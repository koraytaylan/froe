//! Applying a standalone segment-cleanup plan as one ordered mutation
//! sequence under the held lock.

use super::archive_certificate::certify_active_archives_with_progress;
use super::file_identity::{archive_file_bytes, sync_directory_strict};
use super::reclaim::{
    ArchiveRewritePolicy, ReclaimRule, analyze_standalone_segment_cleanup,
    reject_duplicate_active_segments,
};
#[cfg(test)]
use super::sweep::probe_archive_sweep_phase_boundary;
use super::sweep::sweep_one_archive;
use super::sweep_plan::DeferredFileDeletion;
use super::sweep_plan::{
    ArchiveSweepDisposition, PlannedArchiveSweep, StandaloneSegmentCompactionOutcome,
    StandaloneSegmentCompactionPlan,
};
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::tar_archive::archive::TarArchiveReader;
use std::collections::HashMap;
use std::path::Path;

/// Replans under the caller's held repository lock, optionally proves that
/// the authoritative plan is the one previously confirmed, and applies every
/// physically actionable archive sweep. No `gc.log` entry is written: this is
/// standalone cleanup, not a completed compaction cycle.
#[allow(
    clippy::too_many_lines,
    reason = "replanning, mutation, and exact partial-outcome accounting form one locked application sequence"
)]
pub(crate) fn apply_standalone_segment_cleanup(
    directory: &Path,
    rule: ReclaimRule,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    rewrite_policy: ArchiveRewritePolicy,
    expected: Option<&StandaloneSegmentCompactionPlan>,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<(
    StandaloneSegmentCompactionPlan,
    StandaloneSegmentCompactionOutcome,
)> {
    // A cleanup apply is allowed to destroy an entire source archive. Open a
    // fresh, lazy provider over the exact active set and certify every source
    // before the first mutation; recovered/indexless archives and incomplete
    // graph/BRF metadata are never eligible for standalone cleanup.
    let repository = crate::store::Repository::open_with_progress(directory, observer)?;
    reject_duplicate_active_segments(repository.archives())?;
    certify_active_archives_with_progress(&repository, repository.archives(), observer)?;
    apply_standalone_segment_cleanup_from_archives(
        directory,
        repository.archives(),
        Some(&repository),
        rule,
        current_head_segment,
        protected,
        rewrite_policy,
        expected,
        observer,
        #[cfg(test)]
        None,
    )
}

#[cfg(test)]
pub(super) type StandaloneAfterPlanHook<'hook> =
    dyn Fn(&StandaloneSegmentCompactionPlan) -> Result<()> + 'hook;

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the test-only uncertified path and production certified path share one ordered mutation engine"
)]
pub(super) fn apply_standalone_segment_cleanup_from_archives(
    directory: &Path,
    archives: &[TarArchiveReader],
    source_certificate_provider: Option<&dyn SegmentProvider>,
    rule: ReclaimRule,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    rewrite_policy: ArchiveRewritePolicy,
    expected: Option<&StandaloneSegmentCompactionPlan>,
    observer: &mut dyn crate::progress::ProgressObserver,
    #[cfg(test)] after_plan: Option<&StandaloneAfterPlanHook<'_>>,
) -> Result<(
    StandaloneSegmentCompactionPlan,
    StandaloneSegmentCompactionOutcome,
)> {
    let plan = crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "replanning segment reclamation",
            crate::progress::WorkUnit::Archives,
        )
        .with_total(crate::progress::count(archives.len())),
        |observer| {
            analyze_standalone_segment_cleanup(
                directory,
                archives,
                rule,
                current_head_segment,
                protected,
                rewrite_policy,
                observer,
            )
        },
    )?;
    if expected.is_some_and(|expected| expected != &plan) {
        return Err(Error::InvalidFormat {
            details: "the standalone segment-cleanup plan changed after confirmation; refusing \
                      to apply an unconfirmed archive mutation"
                .to_owned(),
        });
    }

    let archive_bytes_before = archive_file_bytes(directory)?;
    #[cfg(test)]
    if let Some(after_plan) = after_plan {
        after_plan(&plan)?;
    }
    let provider_order: Vec<&TarArchiveReader> = archives.iter().collect();
    let mut fallback_provider = None;
    let mut deletion_failures = Vec::new();
    let mut actually_unavailable = std::collections::HashSet::new();
    let mut observed_sweeps = HashMap::new();
    let planned_archives: HashMap<_, _> = plan
        .archives
        .iter()
        .map(|planned| (planned.file_name(), planned))
        .collect();
    // Apply whole removals first, then rewrites. A graph edge is filtered
    // only after its target has really become unavailable (or when the same
    // rewrite is about to make it unavailable). This retains conservative
    // extra edges to deferred, blocked, later, or failed sweep targets.
    crate::progress::observe(
        observer,
        &crate::progress::Step::new("sweeping archives", crate::progress::WorkUnit::Archives)
            .with_total(crate::progress::count(
                plan.archives
                    .iter()
                    .filter(|planned| planned.changes_disk())
                    .count(),
            )),
        |observer| {
            let mut swept = 0usize;
            for rewrite_phase in [false, true] {
                for archive in archives {
                    let Some(planned) = planned_archives.get(archive.file_name()) else {
                        continue;
                    };
                    let is_rewrite = matches!(planned, PlannedArchiveSweep::Rewrite { .. });
                    let is_remove = matches!(planned, PlannedArchiveSweep::Remove { .. });
                    if (!rewrite_phase && !is_remove) || (rewrite_phase && !is_rewrite) {
                        continue;
                    }
                    observer.step_advanced(crate::progress::count(swept));
                    let outcome = sweep_one_archive(
                        directory,
                        archive,
                        &plan.reclaimable,
                        &actually_unavailable,
                        &provider_order,
                        &mut fallback_provider,
                        source_certificate_provider,
                        rewrite_policy,
                    )?;
                    if outcome.disposition != ArchiveSweepDisposition::Unchanged {
                        observed_sweeps.insert(
                            archive.file_name().to_owned(),
                            (outcome.disposition, outcome.newly_unavailable.len()),
                        );
                    }
                    deletion_failures.extend(outcome.deletion_failures);
                    actually_unavailable.extend(outcome.newly_unavailable);
                    swept += 1;
                    observer.step_advanced(crate::progress::count(swept));
                }
                #[cfg(test)]
                if !rewrite_phase {
                    probe_archive_sweep_phase_boundary("sweep.removals-complete-before-rewrites")?;
                }
            }
            Ok::<(), Error>(())
        },
    )?;
    drop(fallback_provider);
    drop(provider_order);
    sync_directory_strict(directory)?;

    let mut outcome = StandaloneSegmentCompactionOutcome {
        archive_bytes_before,
        archive_bytes_after: archive_file_bytes(directory)?,
        deletion_failures,
        ..StandaloneSegmentCompactionOutcome::default()
    };
    for archive in &plan.archives {
        match archive {
            PlannedArchiveSweep::Remove { file_name, .. }
                if observed_sweeps
                    .get(file_name)
                    .is_some_and(|(disposition, _)| {
                        *disposition == ArchiveSweepDisposition::Removed
                    }) =>
            {
                if directory.join(file_name).try_exists()? {
                    if !outcome
                        .deletion_failures
                        .iter()
                        .any(|failure| failure.file_name == *file_name)
                    {
                        outcome.deletion_failures.push(DeferredFileDeletion {
                            file_name: file_name.clone(),
                            error: "file reappeared after the archive unlink succeeded".to_owned(),
                            target_was_already_absent: false,
                        });
                    }
                } else {
                    outcome.removed_archives += 1;
                    outcome.removed_segments += observed_sweeps[file_name].1;
                }
            }
            PlannedArchiveSweep::Rewrite {
                file_name,
                replacement_name,
                ..
            } if observed_sweeps
                .get(file_name)
                .is_some_and(|(disposition, _)| {
                    *disposition == ArchiveSweepDisposition::Rewritten
                }) =>
            {
                if !directory.join(replacement_name).try_exists()? {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "cleanup published rewrite {file_name}, but replacement \
                             {replacement_name} is absent"
                        ),
                    });
                }
                outcome.rewritten_archives += 1;
                outcome.removed_segments += observed_sweeps[file_name].1;
                if directory.join(file_name).try_exists()?
                    && !outcome
                        .deletion_failures
                        .iter()
                        .any(|failure| failure.file_name == *file_name)
                {
                    outcome.deletion_failures.push(DeferredFileDeletion {
                        file_name: file_name.clone(),
                        error: "source archive remained after replacement publication".to_owned(),
                        target_was_already_absent: false,
                    });
                }
            }
            PlannedArchiveSweep::Remove { file_name, .. }
            | PlannedArchiveSweep::Rewrite { file_name, .. } => {
                if observed_sweeps.contains_key(file_name) {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "archive sweep for {file_name} returned a disposition inconsistent with the authoritative plan"
                        ),
                    });
                }
            }
            PlannedArchiveSweep::DeferredBySavings { .. }
            | PlannedArchiveSweep::DeferredAtLastGeneration { .. }
            | PlannedArchiveSweep::BlockedByOccupiedGeneration { .. } => {}
        }
    }
    outcome.deletion_failures.sort_by(|left, right| {
        left.file_name
            .cmp(&right.file_name)
            .then_with(|| left.error.cmp(&right.error))
    });
    outcome.deletion_failures.dedup();
    Ok((plan, outcome))
}
