//! Applying a prepared plan: the compaction phase, then the ordered
//! mutation transaction whose adjacency is the safety argument.

use super::file_removal::{
    PlannedFileRemovalFailureMode, archive_file_bytes, read_optional_regular_file,
    recovery_backup_file_bytes, remove_planned_files,
};
use super::journal_analysis::analyze_journal;
use super::manifest::upgrade_manifest_atomically;
use super::options::MaintenanceTask;
use super::plan::{CompactedGeneration, CompactionOutcome, CompactionPlan, FileDeletionFailure};
use super::planning::{PlannedFileRemoval, directory_fingerprint, verify_exact_super_root};
use super::prepared::PreparedCompaction;
use super::reclamation::compaction_target_generation;
use super::stale_archives::reject_duplicate_active_segments;
use crate::error::{Error, Result};
use crate::progress::{ProgressObserver, Step, WorkUnit};
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::writer::commit::remove_checkpoints;
use crate::writer::compaction::CompactionKind;
use crate::writer::journal_maintenance::{
    JournalRewriteOutcome, RawJournal, RawJournalLineClassification, rewrite_journal_atomically,
    scan_raw_journal,
};
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::{
    ArchiveRewritePolicy, ReclaimRule, StandaloneSegmentCompactionOutcome, WritableRepository,
    apply_standalone_segment_cleanup, certify_active_archives_with_progress, sync_directory_strict,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

mod compaction_phase;
mod journal_phase;

pub(crate) use compaction_phase::*;
pub(crate) use journal_phase::*;

pub(super) fn apply_prepared(
    prepared: PreparedCompaction,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionOutcome> {
    let PreparedCompaction {
        directory,
        options,
        plan,
        repaired,
        repository_lock,
    } = prepared;
    reject_changed_directory(&directory, &plan, &repository_lock)?;
    let gc_log_before = read_optional_regular_file(&directory.join("gc.log"))?;
    let archive_bytes_before = archive_file_bytes(&directory)?;
    if plan.segment_plan.is_some() && plan.manifest_upgrade {
        // This is the final all-source gate before even the manifest may be
        // replaced. Version-two stores do this once inside the segment apply;
        // only the one-time manifest transition needs an additional pass so a
        // bad source cannot leave even a compatible metadata upgrade behind.
        let repository = Repository::open_with_progress(&directory, observer)?;
        reject_duplicate_active_segments(&repository)?;
        certify_active_archives_with_progress(&repository, repository.archives(), observer)?;
    }
    if plan.manifest_upgrade {
        repository_lock.validate_path_identity(&directory)?;
        upgrade_manifest_atomically(&directory)?;
    }

    let PreMutationOutcome {
        segment_outcome,
        removed_stale_archives,
        stale_not_deleted,
    } = apply_pre_copy_mutations(&directory, &plan, &options, &repository_lock, observer)?;
    let (removed_checkpoints, compaction_outcome, expected_head_after) =
        run_compaction_phase(&directory, &plan, &options, &repository_lock, observer)?;
    let journal_outcome = rewrite_journal_for_run(
        &directory,
        &plan,
        &options,
        compaction_outcome.as_ref(),
        &repository_lock,
        observer,
    )?;
    verify_gc_log_delta(
        &directory,
        gc_log_before.as_deref(),
        compaction_outcome.as_ref(),
    )?;
    let head_after = verify_applied_state(&mut AppliedState {
        directory: &directory,
        expected_head_after,
        compaction_outcome: compaction_outcome.as_ref(),
        options: &options,
        observer,
        plan: &plan,
    })?;
    let RetiredResidue {
        removed_temporaries,
        temporary_not_deleted,
        removed_recovery_backups,
        recovery_backup_not_deleted,
    } = retire_run_residue(&directory, &plan, &options, &repository_lock, observer)?;
    let archive_bytes_after = archive_file_bytes(&directory)?;
    let segment_counts = (
        segment_outcome.rewritten_archives,
        segment_outcome.removed_archives,
        segment_outcome.removed_segments,
    );
    let (deletion_failures, files_not_deleted) = collect_deletion_failures(
        segment_outcome,
        vec![
            stale_not_deleted,
            temporary_not_deleted,
            recovery_backup_not_deleted,
        ],
    );
    let removed_journal_lines = journal_outcome.removed_line_count;
    let journal_backup_path = journal_outcome.backup_path;

    let (rewritten_archives, removed_reclaimable_archives, removed_segments) =
        archive_counts(compaction_outcome.as_ref(), segment_counts);
    Ok(CompactionOutcome {
        head_before: plan.current_head,
        head_after,
        removed_checkpoints,
        removed_journal_lines,
        rewritten_archives,
        removed_reclaimable_archives,
        removed_stale_archives,
        removed_temporaries,
        removed_recovery_backups,
        repaired_archives: repaired.len(),
        files_not_deleted,
        archive_bytes_before,
        archive_bytes_after,
        retained_recovery_backup_bytes: recovery_backup_file_bytes(&directory)?,
        compacted: compaction_outcome
            .as_ref()
            .map(|compaction| CompactedGeneration {
                nodes: compaction.copied_nodes,
                generation: compaction.target_generation,
            }),
        removed_segments,
        journal_backup_path,
        deletion_failures,
    })
}

/// Refuses to apply a plan the directory no longer matches.
pub(crate) fn reject_changed_directory(
    directory: &Path,
    plan: &CompactionPlan,
    repository_lock: &Arc<RepositoryLock>,
) -> Result<()> {
    let current_fingerprint = directory_fingerprint(directory)?;
    if current_fingerprint != plan.fingerprint {
        return Err(Error::InvalidFormat {
            details: "the repository changed after the authoritative cleanup plan was built; refusing to apply a stale plan"
                .to_owned(),
        });
    }

    repository_lock.validate_path_identity(directory)?;

    Ok(())
}

/// What the mutations before the copy removed.
pub(crate) struct PreMutationOutcome {
    pub(crate) segment_outcome: StandaloneSegmentCompactionOutcome,
    pub(crate) removed_stale_archives: usize,
    pub(crate) stale_not_deleted: Vec<FileDeletionFailure>,
}

/// Applies the segment plan, the stale archives, and the residue sweep.
///
/// The residue sweep runs before the copy because a killed run's output
/// holds bulk segments alive through its references, and because retiring
/// it first means a retry converges instead of accumulating one more
/// orphan generation.
pub(crate) fn apply_pre_copy_mutations(
    directory: &Path,
    plan: &CompactionPlan,
    options: &super::options::CompactionOptions,
    repository_lock: &Arc<RepositoryLock>,
    observer: &mut dyn ProgressObserver,
) -> Result<PreMutationOutcome> {
    let mut segment_outcome = StandaloneSegmentCompactionOutcome::default();
    if let Some(expected) = &plan.segment_plan {
        repository_lock.validate_path_identity(directory)?;
        let (_, outcome) = apply_standalone_segment_cleanup(
            directory,
            ReclaimRule {
                reference: plan.reference_generation,
                kind: CompactionKind::Full,
                retained_generations: crate::writer::store_writer::RETAINED_GENERATIONS,
            },
            plan.current_head.segment,
            &plan.protected_history_segments,
            options.archive_rewrite_policy,
            Some(expected),
            observer,
        )?;
        segment_outcome = outcome;
    }

    repository_lock.validate_path_identity(directory)?;
    let (removed_stale_archives, stale_not_deleted) =
        if options.contains(MaintenanceTask::StaleArchives) {
            crate::progress::observe(
                observer,
                &Step::new("removing stale archives", WorkUnit::Files)
                    .with_total(crate::progress::count(plan.stale_archives.len())),
                |observer| {
                    remove_planned_files(
                        directory,
                        plan.stale_archives
                            .iter()
                            .map(|archive| PlannedFileRemoval {
                                file_name: archive.file_name.clone(),
                                bytes: archive.bytes,
                                fingerprint: archive.fingerprint.clone(),
                            }),
                        PlannedFileRemovalFailureMode::RequireCertifiedTarget,
                        observer,
                    )
                },
            )?
        } else {
            (0, Vec::new())
        };

    if let Some(expected) = &plan.residue_sweep {
        repository_lock.validate_path_identity(directory)?;
        crate::progress::observe(
            observer,
            &Step::new(
                "retiring interrupted-compaction residue",
                WorkUnit::Archives,
            ),
            |observer| -> Result<()> {
                apply_standalone_segment_cleanup(
                    directory,
                    ReclaimRule {
                        reference: plan.reference_generation,
                        kind: CompactionKind::Full,
                        retained_generations: i32::MAX,
                    },
                    plan.current_head.segment,
                    &HashSet::new(),
                    options.archive_rewrite_policy,
                    Some(expected),
                    observer,
                )
                .map(|_| ())
            },
        )?;
    }

    Ok(PreMutationOutcome {
        segment_outcome,
        removed_stale_archives,
        stale_not_deleted,
    })
}

/// What retiring the run's leftover material removed.
pub(crate) struct RetiredResidue {
    pub(crate) removed_temporaries: usize,
    pub(crate) temporary_not_deleted: Vec<FileDeletionFailure>,
    pub(crate) removed_recovery_backups: usize,
    pub(crate) recovery_backup_not_deleted: Vec<FileDeletionFailure>,
}

/// Removes recovery and staging material, the run's final mutation.
///
/// Never discarded until every repository mutation has passed a fresh
/// exact-head and retained-history verification. These names are outside
/// active archive discovery, so their removal cannot invalidate the
/// verified state.
pub(crate) fn retire_run_residue(
    directory: &Path,
    plan: &CompactionPlan,
    options: &super::options::CompactionOptions,
    repository_lock: &Arc<RepositoryLock>,
    observer: &mut dyn ProgressObserver,
) -> Result<RetiredResidue> {
    repository_lock.validate_path_identity(directory)?;
    let (removed_temporaries, temporary_not_deleted) =
        if options.contains(MaintenanceTask::StaleTemporaries) {
            crate::progress::observe(
                observer,
                &Step::new("removing stale temporary files", WorkUnit::Files)
                    .with_total(crate::progress::count(plan.temporaries.len())),
                |observer| {
                    remove_planned_files(
                        directory,
                        plan.temporaries.iter().cloned(),
                        PlannedFileRemovalFailureMode::Partial,
                        observer,
                    )
                },
            )?
        } else {
            (0, Vec::new())
        };
    let (removed_recovery_backups, backup_not_deleted) =
        if options.contains(MaintenanceTask::RecoveryBackups) {
            crate::progress::observe(
                observer,
                &Step::new("removing old recovery backups", WorkUnit::Files)
                    .with_total(crate::progress::count(plan.recovery_backups.len())),
                |observer| {
                    remove_planned_files(
                        directory,
                        plan.recovery_backups.iter().cloned(),
                        PlannedFileRemovalFailureMode::Partial,
                        observer,
                    )
                },
            )?
        } else {
            (0, Vec::new())
        };
    sync_directory_strict(directory)?;

    Ok(RetiredResidue {
        removed_temporaries,
        temporary_not_deleted,
        removed_recovery_backups,
        recovery_backup_not_deleted: backup_not_deleted,
    })
}

/// Gathers every deletion this run planned but did not achieve, in a
/// stable order, translating the segment layer's failures into the
/// caller-facing ones.
///
/// Returns the failures themselves and the distinct file names they name,
/// which the outcome reports separately.
pub(crate) fn collect_deletion_failures(
    segment_outcome: StandaloneSegmentCompactionOutcome,
    remaining: Vec<Vec<FileDeletionFailure>>,
) -> (Vec<FileDeletionFailure>, Vec<String>) {
    let mut deletion_failures: Vec<_> = segment_outcome
        .deletion_failures
        .into_iter()
        .map(|failure| {
            if failure.target_was_already_absent {
                FileDeletionFailure::already_absent(failure.file_name, failure.error)
            } else {
                FileDeletionFailure::retained(failure.file_name, failure.error)
            }
        })
        .collect();
    for mut failures in remaining {
        deletion_failures.append(&mut failures);
    }
    deletion_failures.sort_by(|left, right| {
        left.file_name
            .cmp(&right.file_name)
            .then_with(|| left.error.cmp(&right.error))
    });
    deletion_failures.dedup();
    let mut files_not_deleted: Vec<_> = deletion_failures
        .iter()
        .map(|failure| failure.file_name.clone())
        .collect();
    files_not_deleted.sort();
    files_not_deleted.dedup();
    (deletion_failures, files_not_deleted)
}

/// Which pass's archive counts the outcome reports.
///
/// A run that compacted reports its sweep's; one that only reclaimed
/// reports the standalone segment pass's.
pub(crate) fn archive_counts(
    compaction_outcome: Option<&CompactionPhaseOutcome>,
    segment_counts: (usize, usize, usize),
) -> (usize, usize, usize) {
    match compaction_outcome {
        Some(compaction) => (
            compaction.sweep.rewritten_archives,
            compaction.sweep.removed_archives,
            compaction.sweep.removed_segments,
        ),
        None => segment_counts,
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::plan::*;
    use crate::writer::maintenance::prepared::*;
    use crate::writer::maintenance::test_support::*;

    #[test]
    fn archive_staging_requires_complete_byte_identity_before_removal() {
        let directory = TestDirectory::repository("archive-staging-proof");
        let exact = directory.path.join("data00000b.tar.cleaning.000");
        std::fs::copy(directory.path.join("data00000a.tar"), &exact)
            .expect("copy exact staging archive");
        let ambiguous = directory.path.join("data00001a.tar.recovering");
        std::fs::write(&ambiguous, b"nonempty recovery evidence")
            .expect("write ambiguous staging archive");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);

        let plan = plan_compaction(&directory.path, &options).expect("plan");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveTemporary { file_name, .. }
                if file_name == "data00000b.tar.cleaning.000"
        )));
        assert!(!plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveTemporary { file_name, .. }
                if file_name == "data00001a.tar.recovering"
        )));

        compact(&directory.path, options).expect("cleanup");
        assert!(!exact.exists());
        assert!(ambiguous.exists());
        Repository::open(&directory.path).expect("healthy repository");
    }

    #[test]
    fn segment_source_certificate_rejects_a_survivor_payload_crc_mismatch() {
        let (directory, source_name, replacement_name, survivor) =
            rewrite_certificate_fixture("source-certificate-survivor-crc");
        corrupt_segment_payload_crc(&directory.path.join(&source_name), survivor);

        assert_source_certificate_refusal(
            &directory,
            &source_name,
            Some(&replacement_name),
            "payload CRC",
        );
    }

    #[test]
    fn segment_source_certificate_rejects_exact_graph_or_brf_omissions() {
        for (name, omitted, expected_error) in [
            (
                "source-certificate-omitted-graph",
                OmittedArchiveMetadata::Graph,
                "segment graph differs",
            ),
            (
                "source-certificate-omitted-brf",
                OmittedArchiveMetadata::BinaryReferences,
                "binary-reference catalog differs",
            ),
        ] {
            let (directory, source_name, replacement_name, _) = rewrite_certificate_fixture(name);
            repack_omitting_archive_metadata(&directory.path, &source_name, omitted);

            assert_source_certificate_refusal(
                &directory,
                &source_name,
                Some(&replacement_name),
                expected_error,
            );
        }
    }

    #[test]
    fn segment_source_certificate_precedes_a_whole_archive_removal() {
        let (directory, source_name, orphan) =
            whole_removal_certificate_fixture("source-certificate-whole-removal");
        change_index_generation(&directory.path.join(&source_name), orphan, -1);

        assert_source_certificate_refusal(
            &directory,
            &source_name,
            None,
            "index/header generation disagreement",
        );
        assert!(
            directory.path.join(source_name).exists(),
            "the whole-removal source must survive a failed certificate"
        );
    }
}
