//! Oak's mark phase: which segments a generation predicate reclaims,
//! and which archives are worth rewriting once it has run.

use super::sweep::{is_reclaimable, next_archive_staging_name};
use super::sweep_plan::{PlannedArchiveSweep, StandaloneSegmentCompactionPlan};
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::compaction::CompactionKind;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use std::collections::HashMap;
use std::path::Path;

pub(super) fn analyze_standalone_segment_cleanup(
    directory: &Path,
    archives: &[TarArchiveReader],
    rule: ReclaimRule,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    rewrite_policy: ArchiveRewritePolicy,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<StandaloneSegmentCompactionPlan> {
    reject_duplicate_active_segments(archives)?;

    let mut references = std::collections::HashSet::new();
    let mut reclaimable = std::collections::HashSet::new();
    let policy = ReclaimPolicy {
        rule,
        protected_data_segments: protected,
    };
    // A skipped standalone compaction uses the exact durable head as Oak's
    // compacted-root boundary. In global reverse write order, compacted
    // entries newer than that root are incomplete/dangling compaction output.
    // One shared state is normative: resetting it per archive could delete
    // valid compacted segments in every older archive.
    let mut ahead_of_root = Some(current_head_segment);
    for (marked, archive) in archives.iter().enumerate() {
        observer.step_advanced(crate::progress::count(marked));
        mark_one_archive(
            archive,
            policy,
            &mut references,
            &mut reclaimable,
            &mut ahead_of_root,
        )?;
    }
    observer.step_advanced(crate::progress::count(archives.len()));
    if let Some(missing_root) = ahead_of_root {
        return Err(Error::InvalidFormat {
            details: format!(
                "current head segment {missing_root} was not encountered in global reverse archive order; refusing to apply the stateful dangling-future rule"
            ),
        });
    }

    let mut planned_archives = Vec::new();
    for archive in archives {
        if let Some(planned) = plan_archive_sweep(
            directory,
            archive,
            &reclaimable,
            rewrite_policy,
            &std::collections::HashSet::new(),
        )? {
            planned_archives.push(planned);
        }
    }
    planned_archives.sort_by(|left, right| left.file_name().cmp(right.file_name()));

    Ok(StandaloneSegmentCompactionPlan {
        archives: planned_archives,
        marked_segments: reclaimable.len(),
        reclaimable,
    })
}

pub(super) fn reject_duplicate_active_segments(archives: &[TarArchiveReader]) -> Result<()> {
    unique_active_segment_locations(archives).map(|_| ())
}

pub(super) fn unique_active_segment_locations(
    archives: &[TarArchiveReader],
) -> Result<HashMap<SegmentIdentifier, &str>> {
    let mut locations: HashMap<SegmentIdentifier, &str> = HashMap::new();
    for archive in archives {
        for identifier in archive.segment_identifiers() {
            if let Some(previous) = locations.insert(identifier, archive.file_name()) {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "segment {identifier} occurs in both active archives {previous} and {}; \
                         refusing cleanup because a store-wide reclaim decision could remove the \
                         authoritative copy",
                        archive.file_name()
                    ),
                });
            }
        }
    }
    Ok(locations)
}

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

/// Plans one archive's sweep.
///
/// `absent_names` are archive file names this same run has already committed
/// to unlink but has not unlinked yet. Both obstacles this function reads off
/// the live directory — an alternate generation letter that a whole-file
/// removal would promote, and an occupied rewrite target — must treat those
/// names as gone, or a plan built before the removals and a replan built after
/// them would disagree about archives the run never mentioned. Every applying
/// call passes an empty set, because by then the removals really have happened.
/// The sweep a post-compaction reclaim pass will perform, planned read-only.
///
/// Mirrors `reclaim_old_generations_with`'s mark exactly: the same
/// `mark_one_archive` over the same base archives in the same order, with the
/// dangling-future rule disabled and no protected set — because the run it
/// predicts has just published a compacted head, so nothing is dangling and
/// nothing is vetoed. What it cannot observe directly is the reference set the
/// copy's own session archives would seed, so the caller supplies it:
/// `seed_references` are the pre-existing bulk segments the copy will
/// reference where they lie.
///
/// `absent_names` are archives this run removes before it copies, so the
/// prediction and the replan agree about a namespace the prediction can see
/// but the replan will not.
pub(crate) fn predict_post_compaction_reclamation(
    directory: &Path,
    repository: &crate::store::Repository,
    rule: ReclaimRule,
    seed_references: &std::collections::HashSet<SegmentIdentifier>,
    rewrite_policy: ArchiveRewritePolicy,
    absent_names: &std::collections::HashSet<String>,
) -> Result<StandaloneSegmentCompactionPlan> {
    let protected = std::collections::HashSet::new();
    let policy = ReclaimPolicy {
        rule,
        protected_data_segments: &protected,
    };
    let mut references = seed_references.clone();
    let mut reclaimable = std::collections::HashSet::new();
    // No dangling-future root: the run being predicted commits its head before
    // it sweeps, so every compacted entry it will see belongs at or before it.
    let mut ahead_of_root = None;
    for archive in repository.archives() {
        if absent_names.contains(archive.file_name()) {
            continue;
        }
        mark_one_archive(
            archive,
            policy,
            &mut references,
            &mut reclaimable,
            &mut ahead_of_root,
        )?;
    }

    let mut planned_archives = Vec::new();
    for archive in repository.archives() {
        if absent_names.contains(archive.file_name()) {
            continue;
        }
        if let Some(planned) = plan_archive_sweep(
            directory,
            archive,
            &reclaimable,
            rewrite_policy,
            absent_names,
        )? {
            planned_archives.push(planned);
        }
    }
    planned_archives.sort_by(|left, right| left.file_name().cmp(right.file_name()));
    Ok(StandaloneSegmentCompactionPlan {
        archives: planned_archives,
        marked_segments: reclaimable.len(),
        reclaimable,
    })
}

pub(super) fn plan_archive_sweep(
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
pub(super) fn oak_sweep_threshold(before_size: i32) -> i32 {
    before_size.wrapping_mul(3) / 4
}

pub(super) fn oak_sweep_defers(
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

pub(super) fn segment_entry_disk_bytes(archive_name: &str, size: u32) -> Result<u64> {
    512u64
        .checked_add(u64::from(size))
        .and_then(|occupied| {
            occupied.checked_add(crate::writer::tar_writer::padding_size(size as usize) as u64)
        })
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("segment-entry size accounting overflow in {archive_name}"),
        })
}

pub(super) fn alternate_generation_residue(
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

/// Oak's `TarReader.mark` for one archive: entries are visited in
/// *reverse* file order, so a bulk segment — always written before the
/// data segments referencing it — is judged after all of them. Apart from
/// the stateful dangling-future rule, data segments use the generation
/// predicate and non-data segments use membership in the shared `references`
/// set (`remove` both queries and consumes, exactly like Java). Every *kept*
/// data segment protects the non-data segments it references — through the
/// graph trailer when present, else the segment header's reference list —
/// following every target for which Java's `isDataSegmentId` is false.
/// Reclaimable identifiers are
/// accumulated into one store-wide set shared by every archive.
/// The generation predicate one maintenance run applies, everywhere.
///
/// Built once per run and passed by value to the mark phase and to the
/// head-safety guard, so the two can never read different values. They used to
/// read the same pair of constants from five hundred lines apart, which was a
/// coincidence rather than a guarantee — and the moment the retention value
/// became a per-run quantity, that coincidence would have converted a refusal
/// into the silent deletion of head-reachable data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReclaimRule {
    /// The generation every candidate is judged against.
    pub(crate) reference: GarbageCollectionGeneration,
    /// Which of Oak's two generation predicates judges each candidate.
    pub(crate) kind: CompactionKind,
    /// How many generations behind the reference survive.
    pub(crate) retained_generations: i32,
}

#[derive(Clone, Copy)]
pub(super) struct ReclaimPolicy<'protected> {
    pub(super) rule: ReclaimRule,
    pub(super) protected_data_segments: &'protected std::collections::HashSet<SegmentIdentifier>,
}

pub(super) fn mark_one_archive(
    reader: &TarArchiveReader,
    policy: ReclaimPolicy<'_>,
    references: &mut std::collections::HashSet<SegmentIdentifier>,
    reclaimable: &mut std::collections::HashSet<SegmentIdentifier>,
    ahead_of_root: &mut Option<SegmentIdentifier>,
) -> Result<()> {
    let mut entries: Vec<(SegmentIdentifier, Option<GarbageCollectionGeneration>, u32)> =
        match reader.index() {
            Some(index) => index
                .entries()
                .iter()
                .copied()
                .map(|entry| {
                    (
                        entry.segment_identifier,
                        Some(GarbageCollectionGeneration {
                            generation: entry.generation,
                            full_generation: entry.full_generation,
                            is_compacted: entry.is_compacted,
                        }),
                        entry.position,
                    )
                })
                .collect(),
            None => reader
                .segment_identifiers()
                .enumerate()
                .map(|(position, identifier)| (identifier, None, position as u32))
                .collect(),
        };
    entries.sort_by_key(|(_, _, position)| *position);

    let graph_adjacency: Option<HashMap<SegmentIdentifier, Vec<SegmentIdentifier>>> = reader
        .segment_graph()
        .map(|graph| graph.adjacency.into_iter().collect());

    for (identifier, generation, _) in entries.iter().rev() {
        let identifier = *identifier;
        let was_referenced = references.remove(&identifier);
        // Oak's `aheadOfRoot &= id != root` both excludes the root itself
        // and switches this rule off permanently for every older entry.
        let reached_root = ahead_of_root.is_some_and(|root| root == identifier);
        if reached_root {
            *ahead_of_root = None;
        }
        let dangling_future =
            ahead_of_root.is_some() && generation.is_some_and(|generation| generation.is_compacted);
        let protected_data =
            identifier.is_data_segment() && policy.protected_data_segments.contains(&identifier);
        let reclaim = if reached_root || protected_data {
            // Readable journal history is an additional conservative veto,
            // including for an otherwise dangling-future data segment. The
            // exact committed root is an unconditional veto too: cleanup's
            // outer generation-invariant check should make this redundant,
            // but a corrupt index must never make this primitive delete it.
            false
        } else if dangling_future {
            // This precedes kind/reachability checks exactly like Oak:
            // compacted bulk entries written after the root are dangling too.
            true
        } else if identifier.is_data_segment() {
            generation.is_some_and(|generation| {
                is_reclaimable(
                    policy.rule.reference,
                    generation,
                    policy.rule.kind,
                    policy.rule.retained_generations,
                )
            })
        } else {
            // Recovered archives cannot be swept, so none of their entries
            // may be marked. They must still participate in reverse-order
            // bulk-reference propagation or an older indexed archive could
            // lose a bulk segment referenced by recovered live data.
            generation.is_some() && !was_referenced
        };
        if reclaim {
            reclaimable.insert(identifier);
        } else if identifier.is_data_segment() {
            let targets = match &graph_adjacency {
                Some(adjacency) => adjacency.get(&identifier).cloned().unwrap_or_default(),
                None => {
                    ParsedSegment::parse(
                        identifier,
                        reader
                            .segment_data(identifier)
                            .ok_or(Error::SegmentNotFound {
                                segment_identifier: identifier,
                            })?,
                    )?
                    .referenced_segments
                }
            };
            for target in targets {
                if !target.is_data_segment() {
                    references.insert(target);
                }
            }
        }
    }
    Ok(())
}
