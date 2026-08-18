//! Rebuilding a journal from the newest consistent super-root the scan
//! found, keeping whatever journal was there as a backup.

use super::{
    ArchiveSet, Candidate, DiscardedProgress, Error, Path, ProgressObserver, RecordIdentifier,
    RepositoryLock, Result, Step, WorkUnit, Write, collect_super_root_candidates,
    is_fully_consistent, signed_uuid_key,
};

/// The outcome of a journal recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryOutcome {
    /// The record identifier the journal was recovered to.
    pub recovered_head: RecordIdentifier,
    /// How many candidate super-roots were found.
    pub candidates_examined: usize,
    /// The path the previous journal was backed up to, when one existed.
    pub previous_journal_backup: Option<std::path::PathBuf>,
}

/// Rebuilds `journal.log` from the segments on disk. Scans every data
/// segment for super-root candidates, verifies the newest one is fully
/// traversable, and rewrites the journal with the surviving candidates
/// oldest first, so the verified head is the last (winning) line. Backs
/// up any existing journal to `journal.log.bak.NNN`.
///
/// Deliberate deviation from Java, which refuses to recover unless the
/// existing journal already has a resolvable line: this recovery also
/// works when `journal.log` is missing or fully unresolvable — the
/// candidates come from the segments, not the journal, and the newest
/// one must still pass the full consistency gate before anything is
/// written.
pub fn recover_journal(directory: &Path) -> Result<RecoveryOutcome> {
    recover_journal_with_progress(directory, &mut DiscardedProgress)
}

/// Recovers exactly like [`recover_journal`], reporting the candidate
/// scan and the consistency probe to `observer`.
pub fn recover_journal_with_progress(
    directory: &Path,
    observer: &mut dyn ProgressObserver,
) -> Result<RecoveryOutcome> {
    // Deliberate deviation from Java, which recovers locklessly: hold the
    // exclusive repository lock for the whole recovery, so a running AEM
    // instance can never append to a journal this function is about to
    // replace. Strictly safer — the tool fails fast instead of losing the
    // instance's subsequent commits.
    let _repository_lock = RepositoryLock::acquire(directory)?;

    // A read-only view of every archive, opened without needing a
    // resolvable journal.
    let archives = crate::store::open_all_archives_with_progress(directory, observer)?;
    let provider = ArchiveSet::new(archives);
    let mut candidates = collect_super_root_candidates(&provider, observer);
    let candidates_examined = candidates.len();

    // Order candidates newest first — the exact reverse of Oak's
    // ascending sort (timestamp, then segment UUID compared as signed
    // halves like Java's UUID.compareTo, then record number), which Oak
    // then iterates backwards from the newest.
    candidates.sort_by(|first, second| {
        second
            .timestamp_milliseconds
            .cmp(&first.timestamp_milliseconds)
            .then_with(|| signed_uuid_key(second.record).cmp(&signed_uuid_key(first.record)))
            .then_with(|| second.record.record_number.cmp(&first.record.record_number))
    });

    // Take the newest fully consistent candidate. Consistency is checked
    // lazily in newest-first order — with Oak's shared corrupt-path
    // memory: a path found corrupt at a newer candidate is re-probed at
    // every older one and must exist and pass a shallow check there, so
    // a candidate that merely *predates* the corrupted node is rejected
    // exactly as Java rejects it. Matching Oak, only the inconsistent
    // newest suffix is dropped: the surviving older candidates are
    // written below the verified head, unverified, as the fallback lines
    // head resolution skips to when a segment of the last line later
    // goes missing.
    let mut corrupt_memory: Vec<String> = Vec::new();
    observer.step_began(
        &Step::new("probing candidates for consistency", WorkUnit::Revisions)
            .with_total(crate::progress::count(candidates_examined)),
    );
    let consistent_position = candidates
        .iter()
        .enumerate()
        .position(|(probed, candidate)| {
            observer.step_advanced(crate::progress::count(probed));
            is_fully_consistent(&provider, candidate.record, &mut corrupt_memory)
        });
    // Every candidate up to and including the accepted one was probed;
    // without a match, all of them were.
    observer.step_advanced(crate::progress::count(
        consistent_position.map_or(candidates_examined, |position| position + 1),
    ));
    observer.step_ended();
    let consistent_position = consistent_position.ok_or_else(|| Error::InvalidFormat {
        details: format!(
            "no consistent super-root found among {candidates_examined} candidates in {}",
            directory.display()
        ),
    })?;
    let survivors = &candidates[consistent_position..];
    let recovered_head = survivors[0].record;

    let previous_journal_backup = back_up_existing_journal(directory)?;
    // The backup is a copy and the new journal arrives by atomic rename,
    // so `journal.log` exists — old or new — at every moment. On failure,
    // the copy is removed only when the recovered journal was *not*
    // installed (the temporary file still exists); after a successful
    // rename the copy is the sole pre-recovery journal and must survive.
    if let Err(error) = write_recovered_journal(directory, survivors) {
        let temporary_path = directory.join("journal.log.recovered");
        if temporary_path.exists() {
            let _ = std::fs::remove_file(&temporary_path);
            if let Some(backup_path) = &previous_journal_backup {
                let _ = std::fs::remove_file(backup_path);
            }
        }
        return Err(error);
    }

    Ok(RecoveryOutcome {
        recovered_head,
        candidates_examined,
        previous_journal_backup,
    })
}

/// Backs up an existing `journal.log` to the first free
/// `journal.log.bak.NNN` (000–999). Deliberate deviation from Java's
/// plain rename: the backup is a *copy*, so `journal.log` never
/// disappears — the recovered journal later replaces it atomically.
pub(crate) fn back_up_existing_journal(directory: &Path) -> Result<Option<std::path::PathBuf>> {
    let journal_path = directory.join("journal.log");
    if !journal_path.exists() {
        return Ok(None);
    }
    for counter in 0..1000 {
        let backup = directory.join(format!("journal.log.bak.{counter:03}"));
        if !backup.exists() {
            std::fs::copy(&journal_path, &backup)?;
            std::fs::File::open(&backup)?.sync_all()?;
            return Ok(Some(backup));
        }
    }
    Err(Error::InvalidFormat {
        details: "all journal backup names (000-999) are taken".to_owned(),
    })
}

/// Writes the recovered journal: the surviving candidates oldest first,
/// each line `<uuid>:<record-number> root <segment-info-timestamp>`,
/// exactly as Oak's recovery writes them. The file is assembled beside
/// the journal, fsynced, renamed into place atomically, and the
/// directory entry is fsynced — Java fsyncs nothing here; a crash can
/// therefore never leave the store without a journal.
pub(crate) fn write_recovered_journal(directory: &Path, survivors: &[Candidate]) -> Result<()> {
    let temporary_path = directory.join("journal.log.recovered");
    {
        let mut file = std::fs::File::create(&temporary_path)?;
        for candidate in survivors.iter().rev() {
            let line = format!(
                "{}:{} root {}\n",
                candidate.record.segment,
                candidate.record.record_number as i32,
                candidate.timestamp_milliseconds
            );
            file.write_all(line.as_bytes())?;
        }
        file.sync_all()?;
    }
    std::fs::rename(&temporary_path, directory.join("journal.log"))?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::recover_journal;
    use crate::writer::backup::test_support::{TestDirectory, assert_content, populate};

    #[test]
    fn recover_journal_rebuilds_a_deleted_journal() {
        let directory = TestDirectory::new("recover");
        populate(&directory.path);

        // Delete the journal, then recover it from the segments.
        std::fs::remove_file(directory.path.join("journal.log")).expect("remove journal");
        let outcome = recover_journal(&directory.path).expect("recover");
        assert!(outcome.candidates_examined >= 1);

        // The store reads back with content intact.
        assert_content(&directory.path, "Backup Source");
    }

    #[test]
    fn recover_journal_backs_up_a_corrupt_journal() {
        let directory = TestDirectory::new("recover-backup");
        populate(&directory.path);

        std::fs::write(
            directory.path.join("journal.log"),
            "garbage-with-no-space\n",
        )
        .expect("corrupt journal");
        let outcome = recover_journal(&directory.path).expect("recover");
        assert!(
            outcome.previous_journal_backup.is_some(),
            "the corrupt journal is backed up"
        );
        assert!(directory.path.join("journal.log.bak.000").exists());
        assert_content(&directory.path, "Backup Source");
    }

    #[test]
    fn recover_journal_requires_the_repository_lock() {
        let directory = TestDirectory::new("recover-locked");
        populate(&directory.path);
        let held_lock =
            crate::writer::repository_lock::RepositoryLock::acquire(&directory.path).expect("lock");
        assert!(
            recover_journal(&directory.path).is_err(),
            "recovery must refuse to run while another process holds repo.lock"
        );
        drop(held_lock);
        recover_journal(&directory.path).expect("recovery succeeds once the lock is free");
    }
}
