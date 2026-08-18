//! The compaction a run performs before its mutations: what it rewrites,
//! what it leaves in `gc.log`, and the state the store must be in
//! afterwards.

use super::{
    Arc, ArchiveRewritePolicy, CompactionKind, CompactionPlan, Error, GarbageCollectionGeneration,
    MaintenanceTask, Path, ProgressObserver, ReclaimRule, RecordIdentifier, Repository,
    RepositoryLock, Result, Step, WorkUnit, WritableRepository, analyze_journal,
    compaction_target_generation, final_expected_retained_lines, inject_final_retained_root_fault,
    read_optional_regular_file, remove_checkpoints, retained_compacted_head_line, scan_raw_journal,
    sync_directory_strict, verify_exact_super_root, verify_retained_journal_lines,
    verify_retained_journal_roots,
};
use crate::writer::store_writer::archive_file_bytes;

/// What the compaction phase of a merged run did.
#[derive(Clone, Debug)]
pub(in crate::writer::maintenance) struct CompactionPhaseOutcome {
    /// The head the copy published. Equals the plan's current head only when
    /// no compaction ran.
    pub(in crate::writer::maintenance) head_after: RecordIdentifier,
    /// The generation the copy wrote into.
    pub(in crate::writer::maintenance) target_generation: GarbageCollectionGeneration,
    /// Distinct node records the copy rewrote.
    pub(in crate::writer::maintenance) copied_nodes: u64,
    /// What the reclaim pass that follows the copy did.
    pub(in crate::writer::maintenance) sweep: crate::writer::store_writer::SegmentSweepOutcome,
    /// The exact `gc.log` line this cycle appended.
    pub(in crate::writer::maintenance) garbage_collection_entry: String,
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
pub(in crate::writer::maintenance) fn apply_compaction_phase(
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
pub(crate) fn verify_gc_log_delta(
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
pub(crate) struct AppliedState<'state> {
    pub(crate) directory: &'state Path,
    pub(crate) expected_head_after: RecordIdentifier,
    pub(crate) compaction_outcome: Option<&'state CompactionPhaseOutcome>,
    pub(crate) options: &'state crate::writer::maintenance::options::CompactionOptions,
    pub(crate) plan: &'state CompactionPlan,
    pub(crate) observer: &'state mut dyn ProgressObserver,
}

/// Reopens the store from disk and proves the exact newly selected head
/// and every readable retained journal root through fresh mappings.
///
/// All old archive mappings and writable caches are out of scope here.
/// The reopen, the head verification, and the journal analysis each
/// report a step of their own, so no step wraps this phase.
pub(crate) fn verify_applied_state(state: &mut AppliedState<'_>) -> Result<RecordIdentifier> {
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
pub(crate) fn run_compaction_phase(
    directory: &Path,
    plan: &CompactionPlan,
    options: &crate::writer::maintenance::options::CompactionOptions,
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
