//! What a reclamation run intends to do to each archive, and the
//! totals a caller previews before authorizing it.

use super::archive_certificate::certify_active_archives;
use super::providers::CertifiedReclaimSources;
use super::reclaim::{
    ArchiveRewritePolicy, ReclaimRule, analyze_standalone_segment_cleanup,
    reject_duplicate_active_segments,
};
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use std::collections::HashMap;
use std::path::Path;

/// One archive's physical disposition in a standalone cleanup plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlannedArchiveSweep {
    /// Every entry is reclaimable; the archive can be unlinked whole.
    Remove {
        file_name: String,
        segment_count: usize,
        file_bytes: u64,
    },
    /// Enough entries are reclaimable to rewrite the survivors.
    Rewrite {
        file_name: String,
        replacement_name: String,
        segment_count: usize,
        eligible_entry_bytes: u64,
    },
    /// Reclaimable entries exist, but Oak's 25% savings gate keeps the
    /// archive byte-for-byte unchanged.
    DeferredBySavings {
        file_name: String,
        segment_count: usize,
        eligible_entry_bytes: u64,
    },
    /// Reclaimable entries exist, but the archive has exhausted the `a` to
    /// `z` rewrite namespace.
    DeferredAtLastGeneration {
        file_name: String,
        segment_count: usize,
        eligible_entry_bytes: u64,
    },
    /// Another generation pathname blocks a rewrite target or would be
    /// promoted by whole-file removal. Cleanup never truncates or promotes it
    /// implicitly; archive hygiene must classify it first.
    BlockedByOccupiedGeneration {
        file_name: String,
        occupied_name: String,
        segment_count: usize,
        eligible_entry_bytes: u64,
    },
}

impl PlannedArchiveSweep {
    pub(crate) fn file_name(&self) -> &str {
        match self {
            Self::Remove { file_name, .. }
            | Self::Rewrite { file_name, .. }
            | Self::DeferredBySavings { file_name, .. }
            | Self::DeferredAtLastGeneration { file_name, .. }
            | Self::BlockedByOccupiedGeneration { file_name, .. } => file_name,
        }
    }

    pub(crate) fn changes_disk(&self) -> bool {
        matches!(self, Self::Remove { .. } | Self::Rewrite { .. })
    }
}

/// Read-only result of the standalone FULL/retained-two mark phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StandaloneSegmentCompactionPlan {
    pub(crate) archives: Vec<PlannedArchiveSweep>,
    pub(crate) marked_segments: usize,
    pub(super) reclaimable: std::collections::HashSet<SegmentIdentifier>,
}

impl StandaloneSegmentCompactionPlan {
    pub(crate) fn reclaimable_segments(&self) -> &std::collections::HashSet<SegmentIdentifier> {
        &self.reclaimable
    }
}

/// Assembles a comparable plan from a per-archive disposition map.
///
/// Sorted by file name for the same reason the directory-level planner sorts
/// its own: two plans built over the same store must compare equal whatever
/// order the archives were visited in, or the authorization check would refuse
/// runs that agree.
pub(super) fn sorted_sweep_plan(
    planned: &HashMap<String, PlannedArchiveSweep>,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
) -> StandaloneSegmentCompactionPlan {
    let mut archives: Vec<PlannedArchiveSweep> = planned.values().cloned().collect();
    archives.sort_by(|left, right| left.file_name().cmp(right.file_name()));
    StandaloneSegmentCompactionPlan {
        archives,
        marked_segments: reclaimable.len(),
        reclaimable: reclaimable.clone(),
    }
}

/// Physical result of applying a standalone segment cleanup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StandaloneSegmentCompactionOutcome {
    pub(crate) rewritten_archives: usize,
    pub(crate) removed_archives: usize,
    pub(crate) removed_segments: usize,
    pub(crate) archive_bytes_before: u64,
    pub(crate) archive_bytes_after: u64,
    pub(crate) deletion_failures: Vec<DeferredFileDeletion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeferredFileDeletion {
    pub(crate) file_name: String,
    pub(crate) error: String,
    pub(crate) target_was_already_absent: bool,
}

/// The observed physical result of one archive sweep attempt.
///
/// `newly_unavailable` is populated only by the mutation branch that proved
/// its unlink or higher-generation publication completed. Callers must use
/// this set, rather than the earlier plan, when filtering graph edges in a
/// later rewrite.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ArchiveSweepDisposition {
    #[default]
    Unchanged,
    Removed,
    Rewritten,
}

#[derive(Debug, Default)]
pub(super) struct ArchiveSweepOutcome {
    pub(super) disposition: ArchiveSweepDisposition,
    pub(super) deletion_failures: Vec<DeferredFileDeletion>,
    pub(super) newly_unavailable: std::collections::HashSet<SegmentIdentifier>,
}

/// Everything a post-compaction reclaim pass needs, including the plan it is
/// authorized to carry out.
#[derive(Clone, Copy)]
pub(crate) struct GenerationReclaimRequest<'sources> {
    /// The generation predicate this pass applies.
    pub(crate) rule: ReclaimRule,
    /// Which archives holding reclaimable segments may be rewritten.
    pub(crate) rewrite_policy: ArchiveRewritePolicy,
    /// A proof the caller already certified these sources under the lock.
    pub(crate) certified_sources: Option<&'sources CertifiedReclaimSources>,
    /// The confirmed plan. When present, the pass replans from disk and
    /// refuses — before it unlinks anything — if the two disagree, so a run
    /// can never mutate an archive its operator did not authorize.
    pub(crate) expected: Option<&'sources StandaloneSegmentCompactionPlan>,
}

/// What a reclaim pass did, so a caller can report it rather than guess.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SegmentSweepOutcome {
    /// Archives unlinked whole.
    pub(crate) removed_archives: usize,
    /// Archives republished at the next generation letter.
    pub(crate) rewritten_archives: usize,
    /// Segments the sweep made unavailable, by either disposition.
    pub(crate) removed_segments: usize,
    /// Planned unlinks that did not happen, each with its reason.
    pub(crate) deletion_failures: Vec<DeferredFileDeletion>,
}

/// Generations a froe maintenance run retains behind its reference. One
/// value, read by every phase of the run through [`ReclaimRule`].
///
/// This is the value Oak's own offline tooling uses:
/// `SegmentGCOptions.setOffline()` sets `retainedGenerations = 1`
/// (`docs/analysis/write-compaction.md` §4, lines 486 and 531;
/// `write-backup-restore-recovery.md` line 238). The online default of two
/// protects against two things froe does not have: a concurrent writer, and a
/// head that reuses records in place from an older generation. froe's
/// exclusive `repo.lock` excludes the first; a deep copy that rewrites the
/// head into the reference generation excludes the second.
///
/// At this value head safety no longer follows from the predicate — the
/// predicate only decides which *older* generations are reclaimable, and
/// whether the head reaches one is a property of the store. It is proved per
/// run instead, by `validate_reclaim_reference_invariant`, which re-evaluates
/// this exact rule over the head's transitive closure and refuses before any
/// mutation.
pub(crate) const RETAINED_GENERATIONS: i32 = 1;

/// Plans Oak's standalone cleanup predicate: FULL GC, the current committed
/// head generation as reference, and one retained generation. `protected`
/// is a conservative keep-veto for journal history; it never makes a segment
/// reclaimable and therefore cannot weaken Oak's head/checkpoint safety.
pub(crate) fn plan_standalone_segment_cleanup(
    directory: &Path,
    repository: &crate::store::Repository,
    rule: ReclaimRule,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    rewrite_policy: ArchiveRewritePolicy,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<StandaloneSegmentCompactionPlan> {
    reject_duplicate_active_segments(repository.archives())?;
    certify_active_archives(repository, repository.archives())?;
    analyze_standalone_segment_cleanup(
        directory,
        repository.archives(),
        rule,
        current_head_segment,
        protected,
        rewrite_policy,
        observer,
    )
}

/// Segments and bytes a plan's actionable dispositions would physically
/// free. Deferred and blocked archives free nothing and contribute nothing.
///
/// A whole-file removal frees the archive's own size — index and trailers
/// included — while a rewrite frees only the entry bytes it drops.
pub(crate) fn plan_reclaimed_totals(plan: &StandaloneSegmentCompactionPlan) -> (usize, u64) {
    let mut segments = 0usize;
    let mut bytes = 0u64;
    for archive in &plan.archives {
        let (archive_segments, archive_bytes) = match archive {
            PlannedArchiveSweep::Remove {
                segment_count,
                file_bytes,
                ..
            } => (*segment_count, *file_bytes),
            PlannedArchiveSweep::Rewrite {
                segment_count,
                eligible_entry_bytes,
                ..
            } => (*segment_count, *eligible_entry_bytes),
            PlannedArchiveSweep::DeferredBySavings { .. }
            | PlannedArchiveSweep::DeferredAtLastGeneration { .. }
            | PlannedArchiveSweep::BlockedByOccupiedGeneration { .. } => continue,
        };
        segments = segments.saturating_add(archive_segments);
        bytes = bytes.saturating_add(archive_bytes);
    }
    (segments, bytes)
}

/// Replans the same sweep with the journal-history keep-veto lifted, to
/// price what retiring that history would actually release.
///
/// Reusing the real mark and sweep rather than reasoning about the veto
/// separately is the point: the veto holds bulk segments only indirectly —
/// a vetoed data segment keeps seeding its references — and it interacts
/// with the 25% rewrite gate, since releasing more of an archive can push
/// it over the threshold. Only the sweep itself accounts for both, so any
/// hand-rolled estimate would understate the price of the veto, badly on a
/// store whose history holds inline binaries.
///
/// The caller has already certified these archives for the vetoed plan;
/// this is the mark and sweep alone.
pub(crate) fn measure_unvetoed_reclamation(
    directory: &Path,
    repository: &crate::store::Repository,
    rule: ReclaimRule,
    current_head_segment: SegmentIdentifier,
    rewrite_policy: ArchiveRewritePolicy,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<(usize, u64)> {
    let unvetoed = analyze_standalone_segment_cleanup(
        directory,
        repository.archives(),
        rule,
        current_head_segment,
        &std::collections::HashSet::new(),
        rewrite_policy,
        observer,
    )?;
    Ok(plan_reclaimed_totals(&unvetoed))
}

/// Segment identifiers that the actionable archive dispositions in `plan`
/// would make unavailable. Deferred and blocked archives contribute none.
/// Duplicate identifiers have already been rejected while constructing the
/// plan, so each identifier has exactly one physical active copy.
pub(crate) fn planned_unavailable_segments(
    directory: &Path,
    plan: &StandaloneSegmentCompactionPlan,
) -> Result<std::collections::HashSet<SegmentIdentifier>> {
    let actionable: std::collections::HashSet<&str> = plan
        .archives
        .iter()
        .filter(|archive| archive.changes_disk())
        .map(PlannedArchiveSweep::file_name)
        .collect();
    let archives = crate::store::open_all_archives(directory)?;
    let mut unavailable = std::collections::HashSet::new();
    for archive in archives {
        if !actionable.contains(archive.file_name()) {
            continue;
        }
        let Some(index) = archive.index() else {
            return Err(Error::InvalidFormat {
                details: format!(
                    "cleanup planned to mutate recovered archive {}, which has no valid index",
                    archive.file_name()
                ),
            });
        };
        unavailable.extend(
            index
                .entries()
                .iter()
                .map(|entry| entry.segment_identifier)
                .filter(|identifier| plan.reclaimable.contains(identifier)),
        );
    }
    Ok(unavailable)
}
