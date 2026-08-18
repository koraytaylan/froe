//! Whether an archive is worth rewriting once the mark has run: Oak's
//! savings threshold reproduced with its signed 32-bit arithmetic, and
//! the policy froe applies instead.

use super::{
    ArchiveFileName, Error, Path, PlannedArchiveSweep, Result, SegmentIdentifier, TarArchiveReader,
    next_archive_staging_name,
};

/// Which archives a sweep is willing to rewrite to the next generation
/// letter.
///
/// Oak's `TarReader.sweep` rewrites an archive only when the survivors would
/// occupy less than three quarters of the original TAR-entry bytes
/// (`docs/analysis/write-cleanup.md` §4.1). That gate is an input/output
/// economics heuristic for an online collector competing with a running
/// repository, not a format rule: it is evaluated *after* the whole-file
/// removal branch, so Oak already drops one hundred per cent of an archive
/// with no gate at all while refusing to drop twenty-four per cent of one,
/// and the rewrite itself — survivor copy in file-position order, filtered
/// graph and binary-reference trailers, validated publication — is the same
/// operation whatever volume it drops.
///
/// froe reclaims offline, under the exclusive repository lock, because an
/// operator asked it to. Leaving proven garbage on disk to save a copy that
/// operator already authorized is the wrong trade, and it is why a
/// compaction followed by a cleanup could identify hundreds of megabytes of
/// garbage and reclaim none of it: both passes deferred the same archives,
/// forever. [`Self::EveryReclaimableArchive`] is therefore the default; Oak's
/// heuristic stays available, byte-exact, for anyone who wants it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArchiveRewritePolicy {
    /// Rewrite whenever any entry in the archive is reclaimable.
    #[default]
    EveryReclaimableArchive,
    /// Oak's exact Java signed-32-bit `afterSize >= beforeSize * 3 / 4`
    /// heuristic, reproduced including its wrapping multiplication.
    OakSavingsGate,
}

pub(in crate::writer::store_writer) fn plan_archive_sweep(
    directory: &Path,
    archive: &TarArchiveReader,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
    rewrite_policy: ArchiveRewritePolicy,
    absent_names: &std::collections::HashSet<String>,
) -> Result<Option<PlannedArchiveSweep>> {
    let Some(name) = ArchiveFileName::parse(archive.file_name()) else {
        return Ok(None);
    };
    let Some(index) = archive.index() else {
        return Ok(None);
    };
    let mut before_entry_bytes = 0u64;
    let mut after_entry_bytes = 0u64;
    let mut eligible_entry_bytes = 0u64;
    let mut reclaimable_count = 0usize;
    for entry in index.entries() {
        let occupied = segment_entry_disk_bytes(archive.file_name(), entry.size)?;
        before_entry_bytes =
            before_entry_bytes
                .checked_add(occupied)
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "archive size accounting overflow in {}",
                        archive.file_name()
                    ),
                })?;
        if reclaimable.contains(&entry.segment_identifier) {
            reclaimable_count += 1;
            eligible_entry_bytes =
                eligible_entry_bytes
                    .checked_add(occupied)
                    .ok_or_else(|| Error::InvalidFormat {
                        details: format!(
                            "cleanup size accounting overflow in {}",
                            archive.file_name()
                        ),
                    })?;
        } else {
            after_entry_bytes =
                after_entry_bytes
                    .checked_add(occupied)
                    .ok_or_else(|| Error::InvalidFormat {
                        details: format!(
                            "archive size accounting overflow in {}",
                            archive.file_name()
                        ),
                    })?;
        }
    }
    if reclaimable_count == 0 {
        return Ok(None);
    }
    if reclaimable_count == index.entries().len() {
        // Another generation normally cannot be active alongside this
        // reader: only one valid winner is selected. Removing that winner,
        // however, would promote any lower stale copy or higher recovered
        // residue on the next open, potentially shadowing healthy segments
        // with obsolete/damaged copies. Archive hygiene must classify every
        // alternate before whole-file deletion proceeds.
        if let Some(occupied_name) = alternate_generation_residue(directory, &name, absent_names)? {
            return Ok(Some(PlannedArchiveSweep::BlockedByOccupiedGeneration {
                file_name: name.file_name,
                occupied_name,
                segment_count: reclaimable_count,
                eligible_entry_bytes,
            }));
        }
        return Ok(Some(PlannedArchiveSweep::Remove {
            file_name: name.file_name,
            segment_count: reclaimable_count,
            file_bytes: archive.file_size(),
        }));
    }
    // Exact Oak gate, when it is the selected policy: both sizes are Java
    // `int`s, multiplication by three wraps in signed 32-bit arithmetic,
    // division truncates toward zero, and equality at 75% is deferred. Prove
    // the accumulated entry sizes fit the source domain before reproducing
    // those arithmetic semantics. The default policy evaluates none of it,
    // which also means an archive whose entry bytes exceed Java's signed
    // domain is rewritten rather than refused.
    if rewrite_policy == ArchiveRewritePolicy::OakSavingsGate
        && oak_sweep_defers(before_entry_bytes, after_entry_bytes, archive.file_name())?
    {
        return Ok(Some(PlannedArchiveSweep::DeferredBySavings {
            file_name: name.file_name,
            segment_count: reclaimable_count,
            eligible_entry_bytes,
        }));
    }
    if name.file_generation >= 'z' {
        return Ok(Some(PlannedArchiveSweep::DeferredAtLastGeneration {
            file_name: name.file_name,
            segment_count: reclaimable_count,
            eligible_entry_bytes,
        }));
    }
    let next_letter = char::from(name.file_generation as u8 + 1);
    let replacement_name = format!("data{:05}{next_letter}.tar", name.archive_number);
    if !absent_names.contains(&replacement_name)
        && directory.join(&replacement_name).try_exists()?
    {
        return Ok(Some(PlannedArchiveSweep::BlockedByOccupiedGeneration {
            file_name: name.file_name,
            occupied_name: replacement_name,
            segment_count: reclaimable_count,
            eligible_entry_bytes,
        }));
    }
    // Applying a multi-archive plan must not discover staging exhaustion only
    // after earlier archives were already swept. This read-only reservation
    // preflight is repeated by the exclusive writer at apply time, where a
    // race still fails safely without touching the source.
    next_archive_staging_name(directory, &replacement_name)?;
    Ok(Some(PlannedArchiveSweep::Rewrite {
        file_name: name.file_name,
        replacement_name,
        segment_count: reclaimable_count,
        eligible_entry_bytes,
    }))
}

/// Java's signed-`int` `beforeSize * 3 / 4` sweep threshold.
pub(in crate::writer::store_writer) fn oak_sweep_threshold(before_size: i32) -> i32 {
    before_size.wrapping_mul(3) / 4
}

pub(in crate::writer::store_writer) fn oak_sweep_defers(
    before_entry_bytes: u64,
    after_entry_bytes: u64,
    archive: &str,
) -> Result<bool> {
    let before_size = i32::try_from(before_entry_bytes).map_err(|_| Error::InvalidFormat {
        details: format!("archive entry bytes exceed Java's signed-i32 domain in {archive}"),
    })?;
    let after_size = i32::try_from(after_entry_bytes).map_err(|_| Error::InvalidFormat {
        details: format!("surviving entry bytes exceed Java's signed-i32 domain in {archive}"),
    })?;
    Ok(after_size >= oak_sweep_threshold(before_size))
}

pub(in crate::writer::store_writer) fn segment_entry_disk_bytes(
    archive_name: &str,
    size: u32,
) -> Result<u64> {
    512u64
        .checked_add(u64::from(size))
        .and_then(|occupied| {
            occupied.checked_add(crate::writer::tar_writer::padding_size(size as usize) as u64)
        })
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("segment-entry size accounting overflow in {archive_name}"),
        })
}

pub(in crate::writer::store_writer) fn alternate_generation_residue(
    directory: &Path,
    active: &ArchiveFileName,
    absent_names: &std::collections::HashSet<String>,
) -> Result<Option<String>> {
    Ok(crate::store::list_archive_file_names(directory)?
        .into_iter()
        .filter(|file_name| !absent_names.contains(file_name))
        .filter_map(|file_name| ArchiveFileName::parse(&file_name))
        .filter(|candidate| {
            candidate.archive_number == active.archive_number
                && candidate.file_name != active.file_name
        })
        .max_by_key(|candidate| (candidate.file_generation, candidate.file_name.clone()))
        .map(|candidate| candidate.file_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::store_writer::sweep_plan::*;
    use crate::writer::store_writer::test_support::*;
    use std::collections::HashSet;

    #[test]
    fn the_oak_savings_gate_defers_at_exactly_twenty_five_percent() {
        let directory = TestDirectory::new("savings-gate");
        let entries: Vec<_> = (1..=4)
            .map(|seed| TestArchiveEntry::new(data_identifier(seed), 1, generation(0, 0, false)))
            .collect();
        write_test_archive(&directory, "data00000a.tar", &entries);
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open archive");

        let exactly_one_quarter = HashSet::from([entries[0].identifier]);
        let exact = plan_archive_sweep(
            &directory.path,
            &reader,
            &exactly_one_quarter,
            ArchiveRewritePolicy::OakSavingsGate,
            &std::collections::HashSet::new(),
        )
        .expect("plan exact threshold")
        .expect("one segment is eligible");
        assert!(matches!(
            exact,
            PlannedArchiveSweep::DeferredBySavings {
                segment_count: 1,
                eligible_entry_bytes: 1024,
                ..
            }
        ));

        let more_than_one_quarter = HashSet::from([entries[0].identifier, entries[1].identifier]);
        let rewrite = plan_archive_sweep(
            &directory.path,
            &reader,
            &more_than_one_quarter,
            ArchiveRewritePolicy::OakSavingsGate,
            &std::collections::HashSet::new(),
        )
        .expect("plan above threshold")
        .expect("two segments are eligible");
        assert!(matches!(
            rewrite,
            PlannedArchiveSweep::Rewrite {
                segment_count: 2,
                eligible_entry_bytes: 2048,
                ref replacement_name,
                ..
            } if replacement_name == "data00000b.tar"
        ));
    }

    #[test]
    fn the_default_policy_rewrites_what_the_oak_savings_gate_defers() {
        let directory = TestDirectory::new("savings-gate-default");
        let entries: Vec<_> = (1..=4)
            .map(|seed| TestArchiveEntry::new(data_identifier(seed), 1, generation(0, 0, false)))
            .collect();
        write_test_archive(&directory, "data00000a.tar", &entries);
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open archive");

        // The same one-in-four reclaimable set the gate defers above.
        let exactly_one_quarter = HashSet::from([entries[0].identifier]);
        let planned = plan_archive_sweep(
            &directory.path,
            &reader,
            &exactly_one_quarter,
            ArchiveRewritePolicy::EveryReclaimableArchive,
            &std::collections::HashSet::new(),
        )
        .expect("plan exact threshold")
        .expect("one segment is eligible");
        assert!(
            matches!(
                planned,
                PlannedArchiveSweep::Rewrite {
                    segment_count: 1,
                    eligible_entry_bytes: 1024,
                    ref replacement_name,
                    ..
                } if replacement_name == "data00000b.tar"
            ),
            "the default policy rewrites regardless of how little it frees, got {planned:?}"
        );
    }

    #[test]
    fn sweep_gate_reproduces_java_signed_i32_wrap_and_rejects_larger_domains() {
        assert_eq!(oak_sweep_threshold(4), 3);
        assert!(oak_sweep_defers(4, 3, "boundary").expect("equality defers"));
        assert!(!oak_sweep_defers(4, 2, "boundary").expect("more savings rewrites"));

        let largest_unwrapped = i32::MAX / 3;
        assert_eq!(
            oak_sweep_threshold(largest_unwrapped),
            largest_unwrapped * 3 / 4
        );
        assert_eq!(
            oak_sweep_threshold(largest_unwrapped + 1),
            i32::MIN.saturating_add(1) / 4,
            "the multiplication wraps before Java's truncating division"
        );
        assert!(
            oak_sweep_defers((largest_unwrapped + 1) as u64, 0, "wrapped")
                .expect("wrapped Java arithmetic"),
            "a negative wrapped threshold makes every nonnegative survivor size defer"
        );

        assert!(oak_sweep_defers(i32::MAX as u64 + 1, 0, "oversize").is_err());
        assert!(oak_sweep_defers(1, i32::MAX as u64 + 1, "oversize").is_err());
    }

    /// A stale archive letter this same run has already committed to unlink
    /// must not block the sweep of the archive it shadows. Planning before the
    /// removals and replanning after them would otherwise disagree about an
    /// archive the run never named — which is a plan authorizing one mutation
    /// and an apply performing another.
    #[test]
    fn a_sweep_plan_treats_a_pending_stale_removal_as_already_absent() {
        let directory = TestDirectory::new("pending-stale-removal");
        let old_one = data_identifier(210);
        let old_two = data_identifier(211);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
            ],
        );
        // The superseded-letter condition a stale-archive removal retires.
        std::fs::write(
            directory.path.join("data00000b.tar"),
            b"stale letter this run removes first",
        )
        .expect("write the stale letter");
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open source");
        let cleaned = HashSet::from([old_one, old_two]);

        let blocked = plan_archive_sweep(
            &directory.path,
            &reader,
            &cleaned,
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
        .expect("plan against the live namespace")
        .expect("every entry is reclaimable");
        assert!(
            matches!(
                blocked,
                PlannedArchiveSweep::BlockedByOccupiedGeneration {
                    ref occupied_name,
                    ..
                } if occupied_name == "data00000b.tar"
            ),
            "the live namespace still holds the stale letter, so the removal is blocked: {blocked:?}"
        );

        let absent = HashSet::from(["data00000b.tar".to_owned()]);
        let unblocked = plan_archive_sweep(
            &directory.path,
            &reader,
            &cleaned,
            ArchiveRewritePolicy::default(),
            &absent,
        )
        .expect("plan against the namespace this run will leave behind")
        .expect("every entry is reclaimable");
        assert!(
            matches!(
                unblocked,
                PlannedArchiveSweep::Remove {
                    segment_count: 2,
                    ..
                }
            ),
            "a letter this run removes first cannot block the whole-file removal: {unblocked:?}"
        );
    }

    #[test]
    fn staging_namespace_exhaustion_fails_during_plan_without_mutation() {
        let directory = TestDirectory::new("staging-namespace-exhausted");
        let root = data_identifier(1010);
        let old_one = data_identifier(1011);
        let old_two = data_identifier(1012);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(root, 1, generation(4, 4, false)),
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
            ],
        );
        let source_path = directory.path.join("data00000a.tar");
        let source_before = std::fs::read(&source_path).expect("source before");
        for counter in 0..=999u16 {
            std::fs::write(
                directory
                    .path
                    .join(format!("data00000b.tar.cleaning.{counter:03}")),
                counter.to_be_bytes(),
            )
            .expect("occupy staging name");
        }
        let reader = TarArchiveReader::open(&source_path).expect("open source");
        let error = plan_archive_sweep(
            &directory.path,
            &reader,
            &HashSet::from([old_one, old_two]),
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
        .expect_err("planning must detect that no exclusive staging name exists");
        assert!(
            error
                .to_string()
                .contains("all 1000 exclusive staging names")
        );
        assert_eq!(
            std::fs::read(&source_path).expect("source after refusal"),
            source_before
        );
        assert!(!directory.path.join("data00000b.tar").exists());
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar.cleaning.000"))
                .expect("first residue"),
            0u16.to_be_bytes()
        );
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar.cleaning.999"))
                .expect("last residue"),
            999u16.to_be_bytes()
        );
    }
}
