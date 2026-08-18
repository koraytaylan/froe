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

/// What the compaction phase of a merged run did.
#[derive(Clone, Debug)]
pub(super) struct CompactionPhaseOutcome {
    /// The head the copy published. Equals the plan's current head only when
    /// no compaction ran.
    pub(super) head_after: RecordIdentifier,
    /// The generation the copy wrote into.
    pub(super) target_generation: GarbageCollectionGeneration,
    /// Distinct node records the copy rewrote.
    pub(super) copied_nodes: u64,
    /// What the reclaim pass that follows the copy did.
    pub(super) sweep: crate::writer::store_writer::SegmentSweepOutcome,
    /// The exact `gc.log` line this cycle appended.
    pub(super) garbage_collection_entry: String,
}

/// Deep-copies the head into a fresh generation and reclaims what the copy
/// supersedes, in one writable session under the held lock.
///
/// The ordering inside is the whole safety argument. The copy is purely
/// additive — it appends fresh archives and touches no existing byte — so a
/// failure or a refusal anywhere before the head moves leaves the store
/// exactly as it was, minus some orphan archives a later run retires. Only
/// once the copy is committed and the head published does anything get
/// unlinked, and by then every segment the new head reaches lives in the
/// generation the sweep is about to retain.
#[allow(
    clippy::too_many_lines,
    reason = "the copy, the head commit, the reclaim pass and the cycle record are one ordered sequence whose adjacency is the safety argument"
)]
pub(super) fn apply_compaction_phase(
    directory: &Path,
    plan: &CompactionPlan,
    repository_lock: &Arc<RepositoryLock>,
    kind: CompactionKind,
    rewrite_policy: ArchiveRewritePolicy,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionPhaseOutcome> {
    repository_lock.validate_path_identity(directory)?;
    let certified_archive_number =
        plan.checkpoint_archive_number
            .ok_or_else(|| Error::InvalidFormat {
                details: "the compaction phase has no certified output archive number".to_owned(),
            })?;
    let mut store = WritableRepository::open_prepared(
        directory,
        Arc::clone(repository_lock),
        certified_archive_number,
    )?;
    if store.head() != plan.current_head {
        return Err(Error::InvalidFormat {
            details: format!(
                "the compaction phase expected head {}, but strict writable open selected {}",
                plan.current_head,
                store.head()
            ),
        });
    }

    let base_generation = store
        .segment_generation(store.head().segment)
        .ok_or_else(|| Error::InvalidFormat {
            details: format!(
                "the head segment {} carries no generation triple",
                store.head().segment
            ),
        })?;
    let target_generation = compaction_target_generation(base_generation, kind);

    // Refuse a damaged source before the copy appends anything, so a retry
    // against a pre-existing defect does not durably append another full copy
    // before failing. The proof travels to the reclaim pass below.
    let certified_sources = store.preflight_reclaim_sources_with_progress(observer)?;

    let archive_bytes_before = archive_file_bytes(directory)?;
    let omitted_checkpoints: std::collections::BTreeSet<String> =
        plan.checkpoints.names.iter().cloned().collect();
    let mut writer = store.record_writer_with_identifier(target_generation, "c");
    let (new_head, copied_nodes) = crate::progress::observe(
        observer,
        &Step::new("copying nodes into a fresh generation", WorkUnit::Nodes),
        |observer| {
            crate::writer::compaction::deep_copy_super_root_with_progress(
                &store,
                &mut writer,
                plan.current_head,
                &omitted_checkpoints,
                observer,
            )
        },
    )?;
    writer.finish()?;

    if !store.compare_and_set_head(plan.current_head, new_head) {
        return Err(Error::InvalidFormat {
            details: "the head moved during the compaction phase".to_owned(),
        });
    }
    store.flush()?;

    let sweep = crate::progress::observe(
        observer,
        &Step::new("reclaiming old generations", WorkUnit::Archives),
        |_observer| {
            store.reclaim_old_generations_with(
                crate::writer::store_writer::GenerationReclaimRequest {
                    rule: ReclaimRule {
                        reference: target_generation,
                        kind,
                        retained_generations: crate::writer::store_writer::RETAINED_GENERATIONS,
                    },
                    rewrite_policy,
                    certified_sources: Some(&certified_sources),
                    expected: None,
                },
            )
        },
    )?;

    store.close()?;
    sync_directory_strict(directory)?;

    // Oak reads `gc.log` to find the previous cycle when it tail-compacts, so
    // a completed cycle must record itself. The exact bytes are kept, because
    // the line carries a timestamp and the final verification proves the file
    // grew by *these* bytes rather than by something that merely looks alike.
    let archive_bytes_after = archive_file_bytes(directory)?;
    let garbage_collection_entry = crate::writer::compaction::garbage_collection_log_entry(
        archive_bytes_after,
        archive_bytes_before.saturating_sub(archive_bytes_after),
        target_generation,
        copied_nodes,
        new_head,
    );
    crate::writer::compaction::append_garbage_collection_log_entry(
        directory,
        &garbage_collection_entry,
    )?;
    sync_directory_strict(directory)?;

    Ok(CompactionPhaseOutcome {
        head_after: new_head,
        target_generation,
        copied_nodes,
        sweep,
        garbage_collection_entry,
    })
}

/// `gc.log` records completed compaction cycles and nothing else.
///
/// A run that did not compact must leave it byte-identical; a run that did
/// must have appended exactly the one line describing the cycle it
/// completed, and changed nothing already in the file.
fn verify_gc_log_delta(
    directory: &Path,
    gc_log_before: Option<&[u8]>,
    compaction_outcome: Option<&CompactionPhaseOutcome>,
) -> Result<()> {
    let gc_log_after = read_optional_regular_file(&directory.join("gc.log"))?;
    match compaction_outcome {
        None => {
            if gc_log_before != gc_log_after.as_deref() {
                return Err(Error::InvalidFormat {
                    details: "a run that did not compact changed gc.log, which is reserved for completed compaction cycles"
                        .to_owned(),
                });
            }
        }
        Some(compaction) => {
            let before = gc_log_before.unwrap_or_default();
            let after = gc_log_after.as_deref().unwrap_or_default();
            let expected = compaction.garbage_collection_entry.as_bytes();
            if after.len() != before.len() + expected.len()
                || !after.starts_with(before)
                || &after[before.len()..] != expected
            {
                return Err(Error::InvalidFormat {
                    details: "the completed compaction did not append exactly its own gc.log entry"
                        .to_owned(),
                });
            }
        }
    }

    Ok(())
}

/// What a finished run must be able to prove about the store it leaves.
struct AppliedState<'state> {
    directory: &'state Path,
    expected_head_after: RecordIdentifier,
    compaction_outcome: Option<&'state CompactionPhaseOutcome>,
    options: &'state super::options::CompactionOptions,
    plan: &'state CompactionPlan,
    observer: &'state mut dyn ProgressObserver,
}

/// Reopens the store from disk and proves the exact newly selected head
/// and every readable retained journal root through fresh mappings.
///
/// All old archive mappings and writable caches are out of scope here.
/// The reopen, the head verification, and the journal analysis each
/// report a step of their own, so no step wraps this phase.
fn verify_applied_state(state: &mut AppliedState<'_>) -> Result<RecordIdentifier> {
    let AppliedState {
        directory,
        expected_head_after,
        compaction_outcome,
        options,
        observer,
        plan,
    } = state;
    let directory = *directory;
    let expected_head_after = *expected_head_after;
    let final_repository = Repository::open_with_progress(directory, observer)?;
    let head_after = final_repository.head_record_identifier();
    if head_after != expected_head_after {
        return Err(Error::InvalidFormat {
            details: format!(
                "cleanup expected final head {expected_head_after}, but fresh reopen selected {head_after}"
            ),
        });
    }
    verify_exact_super_root(&final_repository, head_after, observer)?;
    let final_raw_journal = scan_raw_journal(directory)?;
    let mut final_journal_analysis = analyze_journal(
        &final_repository,
        &final_raw_journal,
        head_after,
        options.journal_revision_retention,
        observer,
    )?;
    inject_final_retained_root_fault(&mut final_journal_analysis.retained_record_ids);
    if let Some(compaction) = &compaction_outcome {
        if final_raw_journal.lines().len() != 1 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a completed compaction left {} journal lines instead of one",
                    final_raw_journal.lines().len()
                ),
            });
        }
        retained_compacted_head_line(&final_raw_journal, compaction.head_after)?;
    } else {
        verify_retained_journal_roots(
            &plan.journal.retained_record_ids,
            &final_journal_analysis.retained_record_ids,
        )?;
        if options.contains(MaintenanceTask::Journal)
            && final_journal_analysis.plan.removed_lines != 0
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "journal cleanup left {} removable physical lines after its atomic rewrite",
                    final_journal_analysis.plan.removed_lines
                ),
            });
        }
        let expected_retained_lines =
            final_expected_retained_lines(&plan.journal.retained_raw_lines);
        verify_retained_journal_lines(&final_raw_journal, &expected_retained_lines)?;
    }

    Ok(head_after)
}

/// Runs the compaction phase when one was selected, returning what it
/// removed, what it did, and the head the run must end at.
fn run_compaction_phase(
    directory: &Path,
    plan: &CompactionPlan,
    options: &super::options::CompactionOptions,
    repository_lock: &Arc<RepositoryLock>,
    observer: &mut dyn ProgressObserver,
) -> Result<(u64, Option<CompactionPhaseOutcome>, RecordIdentifier)> {
    let mut expected_head_after = plan.current_head;
    let mut compaction_outcome: Option<CompactionPhaseOutcome> = None;
    let removed_checkpoints = if let Some(kind) = options.compaction_kind {
        // The head moves exactly once, and the checkpoints this run retires
        // are simply never carried into the fresh generation. Removing them
        // from the live head first would move the head twice, append a second
        // journal line, and strand records at a generation this same run then
        // reclaims — inside a session archive the reclaim pass never sweeps.
        let outcome = apply_compaction_phase(
            directory,
            plan,
            repository_lock,
            kind,
            options.archive_rewrite_policy,
            observer,
        )?;
        expected_head_after = outcome.head_after;
        let omitted = plan.checkpoints.names.len() as u64;
        compaction_outcome = Some(outcome);
        omitted
    } else if plan.checkpoints.names.is_empty() {
        0
    } else {
        let (removed, head_after_checkpoints) = crate::progress::observe(
            observer,
            // No total: the removal is one indivisible commit, so there
            // is nothing to count up to and a declared total would stand
            // at zero for the whole phase.
            &Step::new("removing checkpoints", WorkUnit::Checkpoints),
            |_observer| -> Result<(u64, RecordIdentifier)> {
                repository_lock.validate_path_identity(directory)?;
                let checkpoint_archive_number =
                    plan.checkpoint_archive_number
                        .ok_or_else(|| Error::InvalidFormat {
                            details: "checkpoint cleanup has no certified output archive number"
                                .to_owned(),
                        })?;
                let store = WritableRepository::open_prepared(
                    directory,
                    Arc::clone(repository_lock),
                    checkpoint_archive_number,
                )?;
                if store.head() != plan.current_head {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "cleanup expected checkpoint base head {}, but strict writable open selected {}",
                            plan.current_head,
                            store.head()
                        ),
                    });
                }
                let removed = remove_checkpoints(&store, &plan.checkpoints.names)?;
                if removed != plan.checkpoints.names.len() as u64 {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "cleanup planned to remove {} checkpoints, but the locked head contained {removed}",
                            plan.checkpoints.names.len()
                        ),
                    });
                }
                let head_after_checkpoints = store.head();
                store.close()?;
                sync_directory_strict(directory)?;
                Ok((removed, head_after_checkpoints))
            },
        )?;
        expected_head_after = head_after_checkpoints;
        removed
    };

    Ok((removed_checkpoints, compaction_outcome, expected_head_after))
}

/// Rewrites the journal for whatever this run changed.
///
/// No step wraps this phase: the head verification and the journal
/// analysis inside it report steps of their own, and a step around them
/// would mix nodes and journal lines into one count.
fn rewrite_journal_for_run(
    directory: &Path,
    plan: &CompactionPlan,
    options: &super::options::CompactionOptions,
    compaction_outcome: Option<&CompactionPhaseOutcome>,
    repository_lock: &Arc<RepositoryLock>,
    observer: &mut dyn ProgressObserver,
) -> Result<JournalRewriteOutcome> {
    let journal_outcome = if let Some(compaction) = &compaction_outcome {
        repository_lock.validate_path_identity(directory)?;
        let repository = Repository::open_with_progress(directory, observer)?;
        verify_exact_super_root(&repository, compaction.head_after, observer)?;
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

/// What the mutations before the copy removed.
struct PreMutationOutcome {
    segment_outcome: StandaloneSegmentCompactionOutcome,
    removed_stale_archives: usize,
    stale_not_deleted: Vec<FileDeletionFailure>,
}

/// Applies the segment plan, the stale archives, and the residue sweep.
///
/// The residue sweep runs before the copy because a killed run's output
/// holds bulk segments alive through its references, and because retiring
/// it first means a retry converges instead of accumulating one more
/// orphan generation.
fn apply_pre_copy_mutations(
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
struct RetiredResidue {
    removed_temporaries: usize,
    temporary_not_deleted: Vec<FileDeletionFailure>,
    removed_recovery_backups: usize,
    recovery_backup_not_deleted: Vec<FileDeletionFailure>,
}

/// Removes recovery and staging material, the run's final mutation.
///
/// Never discarded until every repository mutation has passed a fresh
/// exact-head and retained-history verification. These names are outside
/// active archive discovery, so their removal cannot invalidate the
/// verified state.
fn retire_run_residue(
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
fn collect_deletion_failures(
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
fn archive_counts(
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

/// Refuses to apply a plan the directory no longer matches.
fn reject_changed_directory(
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

/// The index of the single physical journal line naming `head`.
///
/// Located by identity rather than by position: the copy appended its line to
/// whatever the journal already held, and a corrupt or duplicated file must be
/// refused rather than guessed at.
pub(super) fn retained_compacted_head_line(
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

pub(super) fn verify_retained_journal_roots(
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

pub(super) fn inject_final_retained_root_fault(actual: &mut Vec<RecordIdentifier>) {
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::omit_last_if_armed(
        "cleanup.before-final-retained-root-verification",
        actual,
    );
    #[cfg(not(test))]
    let _ = actual;
}

pub(super) fn final_expected_retained_lines(expected: &[Vec<u8>]) -> Cow<'_, [Vec<u8>]> {
    #[cfg(test)]
    {
        let mut injected = expected.to_vec();
        crate::writer::maintenance_fault_injection::append_missing_journal_line_if_armed(
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

pub(super) fn verify_retained_journal_lines(
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

pub(super) fn retained_raw_line_matches(expected: &[u8], actual: &[u8]) -> bool {
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
