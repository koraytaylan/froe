//! Turning what a run found into the actions it reports: one line per
//! archive, file, and head move, in the order they would happen.

use super::{
    CheckpointPlan, CompactionAction, JournalAnalysis, MaintenanceTask, Path, PlannedArchiveSweep,
    PlannedFileRemoval, RawJournal, RepositoryState, Result, RetainedReclaimable, SegmentWork,
    StandaloneSegmentCompactionPlan, add_estimate, available_filesystem_bytes,
    compaction_target_generation, retained_reclaimable_from,
};

/// What planning found, in the order the action listing reports it.
pub(crate) struct PlanFindings<'findings> {
    pub(crate) directory: &'findings Path,
    pub(crate) options: &'findings crate::writer::maintenance::options::CompactionOptions,
    pub(crate) state: &'findings RepositoryState,
    pub(crate) segment_work: &'findings SegmentWork,
    pub(crate) temporaries: &'findings [PlannedFileRemoval],
    pub(crate) recovery_backups: &'findings [PlannedFileRemoval],
    pub(crate) manifest_upgrade: bool,
    pub(crate) head_nodes: u64,
    /// The selected purge's `(histories, nodes, retained checkpoints)`,
    /// when one is selected.
    pub(crate) version_history_purge: Option<(u64, u64, u64)>,
}

/// The ordered action list an operator confirms, with the totals that
/// accompany it.
#[derive(Default)]
pub(crate) struct PlanListing {
    pub(crate) actions: Vec<CompactionAction>,
    pub(crate) estimated_reclaimable_bytes: u64,
    pub(crate) estimated_archive_rewrite_source_bytes: u64,
    pub(crate) retained_reclaimable: RetainedReclaimable,
}

/// What the archive sweeps in a plan account for.
#[derive(Default)]
pub(crate) struct SweepListing {
    pub(crate) reclaimable_bytes: u64,
    pub(crate) rewrite_source_bytes: u64,
    pub(crate) retained: RetainedReclaimable,
}

/// The running figures a sweep listing charges as it walks the plan.
pub(crate) struct SweepAccounting<'accounting> {
    pub(crate) reclaimable_bytes: &'accounting mut u64,
    pub(crate) rewrite_source_bytes: &'accounting mut u64,
    pub(crate) retained: &'accounting mut RetainedReclaimable,
}

/// Lists what a standalone segment plan does to each archive, and why it
/// keeps whatever it declines to reclaim.
pub(crate) fn list_segment_plan_actions(
    directory: &Path,
    plan: &StandaloneSegmentCompactionPlan,
    actions: &mut Vec<CompactionAction>,
    warnings: &mut Vec<String>,
    accounting: &mut SweepAccounting<'_>,
) -> Result<()> {
    for archive in &plan.archives {
        match archive {
            PlannedArchiveSweep::Remove {
                file_name,
                segment_count,
                file_bytes,
            } => {
                actions.push(CompactionAction::RemoveReclaimableArchive {
                    file_name: file_name.clone(),
                    segments: *segment_count,
                    bytes: *file_bytes,
                });
                add_estimate(accounting.reclaimable_bytes, *file_bytes)?;
            }
            PlannedArchiveSweep::Rewrite {
                file_name,
                replacement_name,
                segment_count,
                eligible_entry_bytes,
            } => {
                add_estimate(
                    accounting.rewrite_source_bytes,
                    std::fs::symlink_metadata(directory.join(file_name))?.len(),
                )?;
                actions.push(CompactionAction::RewriteArchive {
                    file_name: file_name.clone(),
                    replacement_name: replacement_name.clone(),
                    segments: *segment_count,
                    eligible_bytes: *eligible_entry_bytes,
                });
                add_estimate(accounting.reclaimable_bytes, *eligible_entry_bytes)?;
            }
            PlannedArchiveSweep::DeferredBySavings {
                file_name,
                segment_count,
                eligible_entry_bytes,
            } => {
                accounting.retained.below_savings_gate += segment_count;
                add_estimate(&mut accounting.retained.bytes, *eligible_entry_bytes)?;
                warnings.push(format!(
                    "{file_name}: {segment_count} reclaimable segments ({}) retained because savings do not exceed Oak's 25% rewrite gate",
                    crate::units::format_byte_size(*eligible_entry_bytes)
                ));
            }
            PlannedArchiveSweep::DeferredAtLastGeneration {
                file_name,
                segment_count,
                eligible_entry_bytes,
            } => {
                accounting.retained.at_last_generation += segment_count;
                add_estimate(&mut accounting.retained.bytes, *eligible_entry_bytes)?;
                warnings.push(format!(
                    "{file_name}: {segment_count} reclaimable segments ({}) retained because archive generation z cannot be rewritten",
                    crate::units::format_byte_size(*eligible_entry_bytes)
                ));
            }
            PlannedArchiveSweep::BlockedByOccupiedGeneration {
                file_name,
                occupied_name,
                segment_count,
                eligible_entry_bytes,
            } => {
                accounting.retained.blocked_by_occupied_generation += segment_count;
                add_estimate(&mut accounting.retained.bytes, *eligible_entry_bytes)?;
                warnings.push(format!(
                    "{file_name}: {segment_count} reclaimable segments ({}) retained because {occupied_name} already exists",
                    crate::units::format_byte_size(*eligible_entry_bytes)
                ));
            }
        }
    }
    Ok(())
}

/// Lists the archive rewrites and removals a run would perform: the ones
/// a compaction's own reclaim pass predicts, and the ones a standalone
/// segment plan names.
///
/// Warns where the filesystem may not have room for the rewrites, and
/// where publication's hard-link requirement cannot be preflighted.
pub(crate) fn list_sweep_actions(
    directory: &Path,
    predicted_sweep: Option<&StandaloneSegmentCompactionPlan>,
    segment_plan: Option<&StandaloneSegmentCompactionPlan>,
    actions: &mut Vec<CompactionAction>,
    warnings: &mut Vec<String>,
) -> Result<SweepListing> {
    let mut estimated_reclaimable_bytes = 0u64;
    let mut estimated_archive_rewrite_source_bytes = 0u64;
    let mut retained_reclaimable = RetainedReclaimable::default();
    if let Some(predicted) = predicted_sweep {
        for archive in &predicted.archives {
            match archive {
                PlannedArchiveSweep::Remove {
                    file_name,
                    segment_count,
                    file_bytes,
                } => {
                    actions.push(CompactionAction::RemoveReclaimableArchive {
                        file_name: file_name.clone(),
                        segments: *segment_count,
                        bytes: *file_bytes,
                    });
                    add_estimate(&mut estimated_reclaimable_bytes, *file_bytes)?;
                }
                PlannedArchiveSweep::Rewrite {
                    file_name,
                    replacement_name,
                    segment_count,
                    eligible_entry_bytes,
                } => {
                    actions.push(CompactionAction::RewriteArchive {
                        file_name: file_name.clone(),
                        replacement_name: replacement_name.clone(),
                        segments: *segment_count,
                        eligible_bytes: *eligible_entry_bytes,
                    });
                    add_estimate(&mut estimated_reclaimable_bytes, *eligible_entry_bytes)?;
                }
                other => retained_reclaimable_from(other, &mut retained_reclaimable, warnings)?,
            }
        }
    }
    if let Some(plan) = segment_plan {
        list_segment_plan_actions(
            directory,
            plan,
            actions,
            warnings,
            &mut SweepAccounting {
                reclaimable_bytes: &mut estimated_reclaimable_bytes,
                rewrite_source_bytes: &mut estimated_archive_rewrite_source_bytes,
                retained: &mut retained_reclaimable,
            },
        )?;
    }
    if estimated_archive_rewrite_source_bytes != 0
        && available_filesystem_bytes(directory)
            .is_some_and(|available| available < estimated_archive_rewrite_source_bytes)
    {
        warnings.push(
            "available filesystem space is below the cumulative source size of planned archive rewrites; cleanup remains prefix-safe on ENOSPC, but may need a rerun after space is freed"
                .to_owned(),
        );
    }
    if estimated_archive_rewrite_source_bytes != 0 {
        warnings.push(
            "archive rewrite publication requires same-directory hard-link support, which a read-only plan cannot preflight; an unsupported filesystem fails safely with the source archive intact"
                .to_owned(),
        );
    }
    Ok(SweepListing {
        reclaimable_bytes: estimated_reclaimable_bytes,
        rewrite_source_bytes: estimated_archive_rewrite_source_bytes,
        retained: retained_reclaimable,
    })
}

/// Lists the leftover files a run would remove, charging their bytes to
/// the reclaimable estimate.
pub(crate) fn list_file_removal_actions(
    temporaries: &[PlannedFileRemoval],
    recovery_backups: &[PlannedFileRemoval],
    actions: &mut Vec<CompactionAction>,
    estimated_reclaimable_bytes: &mut u64,
) -> Result<()> {
    for temporary in temporaries {
        actions.push(CompactionAction::RemoveTemporary {
            file_name: temporary.file_name.clone(),
            bytes: temporary.bytes,
        });
        add_estimate(estimated_reclaimable_bytes, temporary.bytes)?;
    }
    for backup in recovery_backups {
        actions.push(CompactionAction::RemoveRecoveryBackup {
            file_name: backup.file_name.clone(),
            bytes: backup.bytes,
        });
        add_estimate(estimated_reclaimable_bytes, backup.bytes)?;
    }
    Ok(())
}

/// Lists the actions that move the head: checkpoint removal, the copy
/// into a fresh generation, and the journal history that copy retires.
pub(crate) fn list_head_moving_actions(
    options: &crate::writer::maintenance::options::CompactionOptions,
    effective_compaction_kind: Option<super::CompactionKind>,
    checkpoints: &CheckpointPlan,
    journal_analysis: &JournalAnalysis,
    raw_journal: &RawJournal,
    actions: &mut Vec<CompactionAction>,
) {
    if !checkpoints.names.is_empty() {
        actions.push(CompactionAction::RemoveCheckpoints {
            names: checkpoints.names.clone(),
            expired: checkpoints.expired,
            unreferenced: checkpoints.unreferenced,
        });
    }
    if effective_compaction_kind.is_some() {
        // Every revision goes, whether or not it still resolves: the run keeps
        // only the line naming the head it is about to write. This is the
        // irreversible half of a maintenance run, so the plan states it plainly
        // rather than leaving the operator to infer it from a prune count that
        // describes a different rule.
        actions.push(CompactionAction::RetireJournalHistory {
            revisions: raw_journal.lines().len(),
        });
    } else if options.contains(MaintenanceTask::Journal) && journal_analysis.plan.removed_lines != 0
    {
        actions.push(CompactionAction::PruneJournal {
            lines: journal_analysis.plan.removed_lines,
            parser_ignored: journal_analysis.plan.parser_ignored,
            missing_segments: journal_analysis.plan.missing_segments,
            unreadable_revisions: journal_analysis.plan.unreadable_revisions,
            beyond_retention: journal_analysis.plan.beyond_retention,
        });
    }
}

/// Turns what planning found into the actions it will take, accumulating
/// the byte estimates and the retained-reclaimable accounting as it goes.
///
/// Warnings are appended rather than returned: the caller keeps its own
/// copy so a failure after this point still carries them out.
pub(crate) fn list_planned_actions(
    findings: &PlanFindings<'_>,
    warnings: &mut Vec<String>,
) -> Result<PlanListing> {
    let &PlanFindings {
        directory,
        options,
        state,
        segment_work,
        temporaries,
        recovery_backups,
        manifest_upgrade,
        head_nodes,
        version_history_purge,
    } = findings;
    let RepositoryState {
        raw_journal,
        journal_analysis,
        pending_repairs,
        checkpoints,
        ..
    } = state;
    let SegmentWork {
        stale_archives,
        reference_generation,
        segment_plan,
        residue_sweep,
        predicted_sweep,
        residue_segments,
        effective_compaction_kind,
        ..
    } = segment_work;
    let (reference_generation, residue_segments) = (*reference_generation, *residue_segments);
    let (segment_plan, residue_sweep, predicted_sweep) = (
        segment_plan.as_ref(),
        residue_sweep.as_ref(),
        predicted_sweep.as_ref(),
    );
    let mut actions = Vec::new();
    let mut estimated_reclaimable_bytes = 0u64;
    // First, because everything else in a repairing run is downstream of it —
    // and because it is the one action here that *adds* bytes rather than
    // reclaiming them, so it stays out of the reclaimable estimate.
    actions.extend(pending_repairs.iter().cloned());
    if manifest_upgrade {
        actions.push(CompactionAction::UpgradeManifest);
    }
    if residue_sweep.is_some() {
        actions.push(CompactionAction::RetireInterruptedCompactionResidue {
            segments: residue_segments,
        });
    }
    if let Some((histories, nodes, retained_checkpoints)) = version_history_purge {
        actions.push(CompactionAction::PurgeOrphanedVersionHistories {
            histories,
            nodes,
            retained_checkpoints,
        });
    }
    if let Some(kind) = *effective_compaction_kind {
        actions.push(CompactionAction::CopyHeadIntoFreshGeneration {
            head_nodes,
            target_generation: compaction_target_generation(reference_generation, kind),
            kind,
        });
    }
    let sweeps = list_sweep_actions(
        directory,
        predicted_sweep,
        segment_plan,
        &mut actions,
        warnings,
    )?;
    estimated_reclaimable_bytes =
        estimated_reclaimable_bytes.saturating_add(sweeps.reclaimable_bytes);
    let estimated_archive_rewrite_source_bytes = sweeps.rewrite_source_bytes;
    let retained_reclaimable = sweeps.retained;
    for stale in stale_archives {
        actions.push(CompactionAction::RemoveStaleArchive {
            file_name: stale.file_name.clone(),
            reason: stale.reason,
            bytes: stale.bytes,
        });
        add_estimate(&mut estimated_reclaimable_bytes, stale.bytes)?;
    }
    list_head_moving_actions(
        options,
        *effective_compaction_kind,
        checkpoints,
        journal_analysis,
        raw_journal,
        &mut actions,
    );
    list_file_removal_actions(
        temporaries,
        recovery_backups,
        &mut actions,
        &mut estimated_reclaimable_bytes,
    )?;
    Ok(PlanListing {
        actions,
        estimated_reclaimable_bytes,
        estimated_archive_rewrite_source_bytes,
        retained_reclaimable,
    })
}
