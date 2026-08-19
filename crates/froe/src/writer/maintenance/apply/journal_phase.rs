//! Rewriting the journal for a run, and proving every root and byte-exact
//! retained line the plan promised is still there afterwards.

use super::{
    Arc, CompactionPhaseOutcome, CompactionPlan, Cow, Error, HashMap, JournalRewriteOutcome,
    MaintenanceTask, Path, ProgressObserver, RawJournal, RawJournalLineClassification,
    RecordIdentifier, Repository, RepositoryLock, Result, analyze_journal,
    rewrite_journal_atomically, scan_raw_journal, sync_directory_strict, verify_exact_super_root,
};

/// Rewrites the journal for whatever this run changed.
///
/// No step wraps this phase: the journal-task branch's head verification
/// and journal analysis report steps of their own, and a step around them
/// would mix nodes and journal lines into one count. The compaction branch
/// is pure file work — its verification happened before the head was
/// published and happens again after this rewrite, where each proves
/// something this phase cannot.
pub(crate) fn rewrite_journal_for_run(
    directory: &Path,
    plan: &CompactionPlan,
    options: &crate::writer::maintenance::options::CompactionOptions,
    compaction_outcome: Option<&CompactionPhaseOutcome>,
    repository_lock: &Arc<RepositoryLock>,
    observer: &mut dyn ProgressObserver,
) -> Result<JournalRewriteOutcome> {
    let journal_outcome = if let Some(compaction) = &compaction_outcome {
        repository_lock.validate_path_identity(directory)?;
        // Byte-level checks only, deliberately: the compacted head was
        // fully walked through the writable session before it was
        // published, and the final verification walks the published store
        // through fresh mappings after this rewrite. A third full walk
        // here defended nothing the numbered `journal.log.bak` does not —
        // by this point the superseded generation is already reclaimed, so
        // the retired lines are dead history either way — and cost minutes
        // per run on a real store.
        let raw = scan_raw_journal(directory)?;
        let retained = retained_compacted_head_line(&raw, compaction.head_after)?;
        verify_retained_journal_lines(&raw, &[raw.lines()[retained].content_bytes().to_vec()])?;
        if raw.lines().len() == 1 {
            JournalRewriteOutcome {
                changed: false,
                backup_path: None,
                retained_record_count: 1,
                removed_line_count: 0,
                bytes_written: raw.source_bytes().len(),
            }
        } else {
            rewrite_journal_atomically(&raw, &[retained])?
        }
    } else if options.contains(MaintenanceTask::Journal) {
        repository_lock.validate_path_identity(directory)?;
        let repository = Repository::open_with_progress(directory, observer)?;
        let head = repository.head_record_identifier();
        verify_exact_super_root(&repository, head, observer)?;
        let raw = scan_raw_journal(directory)?;
        let analysis = analyze_journal(
            &repository,
            &raw,
            head,
            options.journal_revision_retention,
            observer,
        )?;
        verify_retained_journal_roots(
            &plan.journal.retained_record_ids,
            &analysis.retained_record_ids,
        )?;
        verify_retained_journal_lines(&raw, &plan.journal.retained_raw_lines)?;
        if analysis.plan.removed_lines == 0 {
            JournalRewriteOutcome {
                changed: false,
                backup_path: None,
                retained_record_count: analysis.retained_indexes.len(),
                removed_line_count: 0,
                bytes_written: raw.source_bytes().len(),
            }
        } else {
            rewrite_journal_atomically(&raw, &analysis.retained_indexes)?
        }
    } else {
        JournalRewriteOutcome {
            changed: false,
            backup_path: None,
            retained_record_count: 0,
            removed_line_count: 0,
            bytes_written: 0,
        }
    };

    sync_directory_strict(directory)?;
    Ok(journal_outcome)
}

/// The index of the single physical journal line naming `head`.
///
/// Located by identity rather than by position: the copy appended its line to
/// whatever the journal already held, and a corrupt or duplicated file must be
/// refused rather than guessed at.
pub(in crate::writer::maintenance) fn retained_compacted_head_line(
    raw: &RawJournal,
    head: RecordIdentifier,
) -> Result<usize> {
    let matching: Vec<usize> = raw
        .lines()
        .iter()
        .enumerate()
        .filter(|(_, line)| match line.classification() {
            RawJournalLineClassification::Record(record) => record.record_identifier == head,
            RawJournalLineClassification::ParserSkippedNoSpace
            | RawJournalLineClassification::InvalidRecordIdentifier { .. } => false,
        })
        .map(|(index, _)| index)
        .collect();
    match matching.as_slice() {
        [only] => Ok(*only),
        [] => Err(Error::InvalidFormat {
            details: format!("the journal holds no line naming the compacted head {head}"),
        }),
        many => Err(Error::InvalidFormat {
            details: format!(
                "the journal holds {} lines naming the compacted head {head}; refusing to choose one",
                many.len()
            ),
        }),
    }
}

pub(in crate::writer::maintenance) fn verify_retained_journal_roots(
    expected: &[RecordIdentifier],
    actual_readable: &[RecordIdentifier],
) -> Result<()> {
    let mut counts = HashMap::new();
    for &identifier in actual_readable {
        *counts.entry(identifier).or_insert(0usize) += 1;
    }
    for &identifier in expected {
        let Some(count) = counts.get_mut(&identifier) else {
            return Err(Error::InvalidFormat {
                details: format!(
                    "cleanup made previously readable journal root {identifier} unreadable or removed its journal line"
                ),
            });
        };
        if *count == 0 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "cleanup removed a duplicate readable journal line for root {identifier}"
                ),
            });
        }
        *count -= 1;
    }
    Ok(())
}

pub(in crate::writer::maintenance) fn inject_final_retained_root_fault(
    actual: &mut Vec<RecordIdentifier>,
) {
    #[cfg(test)]
    crate::writer::fault_injection::omit_last_if_armed(
        "cleanup.before-final-retained-root-verification",
        actual,
    );
    #[cfg(not(test))]
    let _ = actual;
}

pub(in crate::writer::maintenance) fn final_expected_retained_lines(
    expected: &[Vec<u8>],
) -> Cow<'_, [Vec<u8>]> {
    #[cfg(test)]
    {
        let mut injected = expected.to_vec();
        crate::writer::fault_injection::append_missing_journal_line_if_armed(
            "cleanup.before-final-retained-line-verification",
            &mut injected,
        );
        Cow::Owned(injected)
    }
    #[cfg(not(test))]
    {
        Cow::Borrowed(expected)
    }
}

pub(in crate::writer::maintenance) fn verify_retained_journal_lines(
    journal: &RawJournal,
    expected: &[Vec<u8>],
) -> Result<()> {
    let mut remaining = expected.iter();
    let mut wanted = remaining.next();
    for line in journal.lines() {
        if wanted.is_some_and(|raw| retained_raw_line_matches(raw, line.raw_bytes())) {
            wanted = remaining.next();
        }
    }
    if wanted.is_some() {
        return Err(Error::InvalidFormat {
            details: "cleanup did not preserve every previously readable physical journal line byte-for-byte, with its original terminator and order"
                .to_owned(),
        });
    }
    Ok(())
}

pub(in crate::writer::maintenance) fn retained_raw_line_matches(
    expected: &[u8],
    actual: &[u8],
) -> bool {
    if actual == expected {
        return true;
    }
    // A checkpoint append and the byte-preserving rewrite both insert the one
    // separator Oak needs after an originally unterminated final record. No
    // other terminator normalization is permitted: LF, CRLF, and bare CR must
    // otherwise remain byte-exact.
    !matches!(expected.last(), Some(b'\n' | b'\r'))
        && actual.len() == expected.len() + 1
        && actual.starts_with(expected)
        && actual.last() == Some(&b'\n')
}
