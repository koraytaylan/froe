//! Rewriting one archive without its reclaimed segments: stage,
//! validate, publish atomically, then unlink the source.

use super::archive_certificate::{
    ExpectedBinaryReferences, ExpectedGraph, validate_exact_archive_trailers,
};
use super::file_identity::{
    FileAccess, RegularFileIdentity, UncommittedArchiveStaging, held_file_identity,
    open_regular_file_no_follow, path_object_identity, preserve_file_metadata,
    remove_published_link_if_same, require_held_file_identity, require_path_file_identity,
    sync_directory_strict,
};
use super::providers::{
    ArchiveSegmentsProvider, FilteredTrailers, archive_segments_provider,
    certify_reopened_active_archive,
};
use super::reclaim::{ArchiveRewritePolicy, plan_archive_sweep};
use super::sweep_plan::DeferredFileDeletion;
use super::sweep_plan::{ArchiveSweepDisposition, ArchiveSweepOutcome, PlannedArchiveSweep};
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::tar_archive::archive::TarArchiveReader;
use crate::writer::compaction::CompactionKind;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::tar_writer::TarArchiveWriter;
use std::path::Path;

mod publish;
mod stage;
mod validate;

pub(crate) use publish::*;
pub(crate) use stage::*;
pub(crate) use validate::*;

#[allow(
    clippy::too_many_arguments,
    reason = "staging, semantic validation, atomic publication, and source unlinking form one deliberately linear safety sequence, and each parameter names one input that sequence must not re-derive"
)]
pub(super) fn sweep_one_archive<'archives>(
    directory: &Path,
    reader: &'archives TarArchiveReader,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
    previously_unavailable_graph_targets: &std::collections::HashSet<SegmentIdentifier>,
    all_archives: &[&'archives TarArchiveReader],
    fallback_provider: &mut Option<ArchiveSegmentsProvider<'archives>>,
    source_certificate_provider: Option<&dyn SegmentProvider>,
    rewrite_policy: ArchiveRewritePolicy,
) -> Result<ArchiveSweepOutcome> {
    let path = directory.join(reader.file_name());
    let Some(planned) = plan_archive_sweep(
        directory,
        reader,
        reclaimable,
        rewrite_policy,
        &std::collections::HashSet::new(),
    )?
    else {
        return Ok(ArchiveSweepOutcome::default());
    };

    let reopened_source = reopen_actionable_source(
        directory,
        &path,
        reader,
        reclaimable,
        &planned,
        source_certificate_provider,
        rewrite_policy,
    )?;
    let reader = reopened_source
        .as_ref()
        .map_or(reader, |(reopened, _, _)| reopened);
    let Some(index) = reader.index() else {
        // Post-compaction cleanup retains Oak's conservative treatment of
        // recovered base archives. Standalone cleanup cannot reach this branch
        // because its source certificate rejects an indexless archive.
        return Ok(ArchiveSweepOutcome::default());
    };
    let planned_unavailable = reclaimable_in_archive(index, reclaimable);
    let (replacement_name, planned_reclaimable_count) = match planned {
        PlannedArchiveSweep::Remove { .. } => {
            return remove_swept_archive(
                &path,
                reader,
                reopened_source.as_ref(),
                planned_unavailable,
            );
        }
        PlannedArchiveSweep::Rewrite {
            replacement_name,
            segment_count,
            ..
        } => (replacement_name, segment_count),
        PlannedArchiveSweep::DeferredBySavings { .. }
        | PlannedArchiveSweep::DeferredAtLastGeneration { .. }
        | PlannedArchiveSweep::BlockedByOccupiedGeneration { .. } => {
            return Ok(ArchiveSweepOutcome::default());
        }
    };

    let survivors = partition_entries(index, reclaimable, planned_reclaimable_count);
    let current_rewrite_targets = planned_unavailable;

    let trailers = source_trailers(
        reader,
        reclaimable,
        previously_unavailable_graph_targets,
        &current_rewrite_targets,
    );
    let scan_provider = scan_provider_for(&trailers, all_archives, fallback_provider)?;
    // Build under a name that cannot participate in archive selection. A
    // crash or validation failure can therefore leave only non-active
    // residue; the healthy source remains the selected generation. Trailer
    // entry names still use the final logical basename.
    let staging_name = next_archive_staging_name(directory, &replacement_name)?;
    let staging_path = directory.join(&staging_name);
    let replacement_path = directory.join(&replacement_name);
    let mut uncommitted_staging = UncommittedArchiveStaging::new(directory, staging_path.clone());
    let (_, source_file, source_identity) = reopened_source
        .as_ref()
        .expect("an archive rewrite always has an actionable reopened source");
    let source_metadata = source_file.metadata()?;
    let mut writer =
        TarArchiveWriter::new_exclusive_staged(directory, &staging_name, &replacement_name);
    let (expected_graph, expected_binary_references) = copy_survivors(
        SurvivorSources {
            reader,
            trailers: &trailers,
            previously_unavailable_graph_targets,
            current_rewrite_targets: &current_rewrite_targets,
            scan_provider,
        },
        &survivors,
        &mut writer,
        &mut uncommitted_staging,
    )?;
    writer.close()?;
    publish_swept_archive(
        SweepPublication {
            directory,
            source_path: &path,
            reader,
            source_file,
            source_identity: *source_identity,
            staging_name: &staging_name,
            staging_path: &staging_path,
            replacement_name: &replacement_name,
            replacement_path: &replacement_path,
            survivors: &survivors,
            expected_graph: &expected_graph,
            expected_binary_references: &expected_binary_references,
            source_metadata: &source_metadata,
            current_rewrite_targets,
        },
        &mut uncommitted_staging,
    )
}

/// Binds an actionable source to a no-follow descriptor and retains its
/// inode identity through the destructive syscall.
///
/// Standalone cleanup and ordinary post-compaction cleanup additionally
/// repeat the complete source certificate through this exact
/// descriptor-backed mapping. Replanning prevents a semantically
/// different but still well-formed source from silently proceeding after
/// the locked plan.
#[allow(
    clippy::too_many_arguments,
    reason = "the re-certification repeats every input the original plan was derived from"
)]
pub(crate) fn reopen_actionable_source(
    directory: &Path,
    path: &Path,
    reader: &TarArchiveReader,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
    planned: &PlannedArchiveSweep,
    source_certificate_provider: Option<&dyn SegmentProvider>,
    rewrite_policy: ArchiveRewritePolicy,
) -> Result<Option<(TarArchiveReader, std::fs::File, RegularFileIdentity)>> {
    let reopened_source = if planned.changes_disk() {
        let source_file = open_regular_file_no_follow(path, FileAccess::ReadOnly)?;
        let source_identity = held_file_identity(&source_file)?;
        let reopened = TarArchiveReader::open_file(path, &source_file)?;
        if let Some(provider) = source_certificate_provider {
            certify_reopened_active_archive(provider, &reopened)?;
        }
        if plan_archive_sweep(
            directory,
            &reopened,
            reclaimable,
            rewrite_policy,
            &std::collections::HashSet::new(),
        )?
        .as_ref()
            != Some(planned)
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "the actionable source archive {} changed before its immediate cleanup certificate",
                    reader.file_name()
                ),
            });
        }
        Some((reopened, source_file, source_identity))
    } else {
        None
    };
    Ok(reopened_source)
}

/// The segments this archive holds that the mark phase found reclaimable.
pub(crate) fn reclaimable_in_archive(
    index: &crate::tar_archive::index::SegmentIndex,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
) -> std::collections::HashSet<SegmentIdentifier> {
    index
        .entries()
        .iter()
        .map(|entry| entry.segment_identifier)
        .filter(|identifier| reclaimable.contains(identifier))
        .collect()
}

/// Unlinks an archive whose segments all reclaim, after proving the file
/// is still the one certification bound.
///
/// Deletion failures are consistency-safe: ordinarily the old archive
/// remains authoritative for retry, and `NotFound` records that another
/// actor already achieved this exact unlink.
pub(crate) fn remove_swept_archive(
    path: &Path,
    reader: &TarArchiveReader,
    reopened_source: Option<&(TarArchiveReader, std::fs::File, RegularFileIdentity)>,
    planned_unavailable: std::collections::HashSet<SegmentIdentifier>,
) -> Result<ArchiveSweepOutcome> {
    let (_, source_file, source_identity) = reopened_source
        .as_ref()
        .expect("an archive removal always has an actionable reopened source");
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::substitute_path_if_armed(
        "sweep.remove-before-source-identity",
        path,
    )?;
    require_held_file_identity(source_file, *source_identity, "certified removal source")?;
    require_path_file_identity(path, *source_identity, "certified removal source")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::remove_path_if_armed(
        "sweep.remove-before-source-unlink-not-found",
        path,
    )?;
    // Deletion failures are consistency-safe: ordinarily the old
    // archive remains authoritative for retry; `NotFound` records
    // that another actor already achieved this exact unlink.
    Ok(match std::fs::remove_file(path) {
        Ok(()) => ArchiveSweepOutcome {
            disposition: ArchiveSweepDisposition::Removed,
            deletion_failures: Vec::new(),
            newly_unavailable: planned_unavailable,
        },
        Err(error) => ArchiveSweepOutcome {
            disposition: ArchiveSweepDisposition::Unchanged,
            deletion_failures: vec![DeferredFileDeletion {
                file_name: reader.file_name().to_owned(),
                error: error.to_string(),
                target_was_already_absent: error.kind() == std::io::ErrorKind::NotFound,
            }],
            newly_unavailable: std::collections::HashSet::new(),
        },
    })
}

/// Splits an archive's index entries into survivors and reclaimed, in
/// file-position order.
///
/// Accumulates Oak's sweep arithmetic as it goes: `i64` cannot wrap where
/// Java's `int` could not either, because entries are position-bounded
/// below 2 GiB.
pub(crate) fn partition_entries(
    index: &crate::tar_archive::index::SegmentIndex,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
    planned_reclaimable_count: usize,
) -> Vec<crate::tar_archive::index::SegmentIndexEntry> {
    let mut entries: Vec<_> = index.entries().to_vec();
    entries.sort_by_key(|entry| entry.position);
    let mut survivors = Vec::new();
    for entry in entries {
        if !reclaimable.contains(&entry.segment_identifier) {
            survivors.push(entry);
        }
    }
    debug_assert_eq!(
        index.entries().len() - survivors.len(),
        planned_reclaimable_count
    );
    survivors
}

/// The source archive's graph and binary-reference trailers, filtered to
/// what the replacement will carry.
///
/// These two proofs deliberately have different graph scopes. Production
/// active-source certification reconstructs the complete, unfiltered graph
/// from payloads before mutation. A replacement `.gph` is derived
/// subtractively: source entries not copied by this rewrite are left out,
/// while targets are filtered against both identifiers this run previously
/// made unavailable and identifiers belonging to those omitted entries.
/// Staged and published validation compare it with that exact view.
pub(crate) fn source_trailers(
    reader: &TarArchiveReader,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
    previously_unavailable_graph_targets: &std::collections::HashSet<SegmentIdentifier>,
    current_rewrite_targets: &std::collections::HashSet<SegmentIdentifier>,
) -> FilteredTrailers {
    FilteredTrailers::from_archive(
        reader,
        reclaimable,
        previously_unavailable_graph_targets,
        current_rewrite_targets,
    )
}

/// The provider that resolves survivor references when the source has no
/// readable catalog.
///
/// The scan resolves across every segment of every base archive, and the
/// sweep fails closed on an unresolvable identifier rather than publish an
/// incomplete catalog that could let blob garbage collection delete
/// referenced binaries. (Java would publish an *empty* catalog here; the
/// scan is a strict superset of that.)
pub(crate) fn scan_provider_for<'fallback, 'archives>(
    trailers: &FilteredTrailers,
    all_archives: &[&'archives TarArchiveReader],
    fallback_provider: &'fallback mut Option<ArchiveSegmentsProvider<'archives>>,
) -> Result<Option<&'fallback ArchiveSegmentsProvider<'archives>>> {
    if trailers.catalog.is_some() {
        return Ok(None);
    }
    if fallback_provider.is_none() {
        *fallback_provider = Some(archive_segments_provider(all_archives)?);
    }
    Ok(fallback_provider.as_ref())
}

pub(crate) fn is_reclaimable(
    reference: GarbageCollectionGeneration,
    segment: GarbageCollectionGeneration,
    kind: CompactionKind,
    retained_generations: i32,
) -> bool {
    // Wrapping subtraction matches Java's `GCGeneration.compareWith`, which
    // uses plain int subtraction; it also cannot panic on the pathological
    // generation values a corrupt archive index might carry.
    match kind {
        CompactionKind::Full => {
            reference
                .full_generation
                .wrapping_sub(segment.full_generation)
                >= retained_generations
                || (reference.generation.wrapping_sub(segment.generation) >= retained_generations
                    && !segment.is_compacted)
        }
        CompactionKind::Tail => {
            reference.generation.wrapping_sub(segment.generation) >= retained_generations
                && !(segment.is_compacted && segment.full_generation == reference.full_generation)
        }
    }
}

#[cfg(test)]
pub(super) fn probe_archive_sweep_phase_boundary(cutpoint: &str) -> Result<()> {
    crate::writer::maintenance_fault_injection::fail_if_armed(cutpoint)?;
    crate::writer::maintenance_fault_injection::crash_if_armed(cutpoint);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::identifier::SegmentIdentifier;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::compaction::CompactionKind;
    use crate::writer::store_writer::reclaim::*;
    use crate::writer::store_writer::sweep_plan::*;
    use crate::writer::store_writer::test_support::*;
    use std::collections::HashSet;
    use std::path::Path;

    #[test]
    fn full_reclaimer_retained_two_honors_exact_and_wrapping_boundaries() {
        let reference = generation(10, 8, true);

        // Full-generation age is decisive for compacted and non-compacted
        // segments, with equality at the retained count included.
        assert!(is_reclaimable(
            reference,
            generation(10, 6, true),
            CompactionKind::Full,
            2
        ));
        assert!(!is_reclaimable(
            reference,
            generation(10, 7, true),
            CompactionKind::Full,
            2
        ));
        // Generation age is an alternate path only for non-compacted data.
        assert!(is_reclaimable(
            reference,
            generation(8, 7, false),
            CompactionKind::Full,
            2
        ));
        assert!(!is_reclaimable(
            reference,
            generation(8, 7, true),
            CompactionKind::Full,
            2
        ));
        assert!(!is_reclaimable(
            reference,
            generation(9, 7, false),
            CompactionKind::Full,
            2
        ));

        // Java subtraction wraps in signed i32 arithmetic. These pairs
        // straddle the boundary and distinguish a wrapped delta of 1 from 2.
        let wrapping_reference = generation(i32::MIN, i32::MIN, false);
        assert!(!is_reclaimable(
            wrapping_reference,
            generation(i32::MAX, i32::MAX, false),
            CompactionKind::Full,
            2
        ));
        assert!(is_reclaimable(
            wrapping_reference,
            generation(i32::MAX - 1, i32::MAX - 1, false),
            CompactionKind::Full,
            2
        ));
        assert!(!is_reclaimable(
            generation(i32::MAX, i32::MAX, false),
            generation(i32::MIN, i32::MIN, false),
            CompactionKind::Full,
            2
        ));
    }

    #[test]
    fn post_compaction_reclaimer_still_retains_exactly_one_generation() {
        let reference = generation(5, 5, true);
        assert!(is_reclaimable(
            reference,
            generation(4, 4, true),
            CompactionKind::Full,
            1
        ));
        assert!(!is_reclaimable(
            reference,
            generation(5, 5, true),
            CompactionKind::Full,
            1
        ));
        assert!(is_reclaimable(
            reference,
            generation(4, 5, false),
            CompactionKind::Tail,
            1
        ));
        assert!(!is_reclaimable(
            reference,
            generation(4, 5, true),
            CompactionKind::Tail,
            1
        ));
    }

    /// The boundary the retention value actually moves, and the store shape
    /// that sits on it: a head Oak tail-compacted to `(1,0,compacted)` over
    /// generation-zero data segments it still reaches. At two retained
    /// generations those segments are spared by arithmetic; at one they are
    /// reclaimable, and only `validate_reclaim_reference_invariant` stands
    /// between the head and its own data.
    #[test]
    fn one_retained_generation_reclaims_what_two_spared() {
        let tail_compacted_head = generation(1, 0, true);
        let untouched_tail = generation(0, 0, false);
        assert!(is_reclaimable(
            tail_compacted_head,
            untouched_tail,
            CompactionKind::Full,
            1
        ));
        assert!(!is_reclaimable(
            tail_compacted_head,
            untouched_tail,
            CompactionKind::Full,
            2
        ));
        assert_eq!(
            crate::writer::store_writer::RETAINED_GENERATIONS,
            1,
            "the run's own retention value is the one this boundary describes"
        );
    }

    #[test]
    fn occupied_next_generation_is_never_truncated_or_rewritten() {
        let directory = TestDirectory::new("occupied-next-generation");
        let root = data_identifier(10);
        let old_one = data_identifier(11);
        let old_two = data_identifier(12);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(root, 1, generation(4, 4, false)),
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
            ],
        );
        let occupied = b"interrupted-cleanup-evidence-must-survive";
        std::fs::write(directory.path.join("data00000b.tar"), occupied)
            .expect("write occupied target");
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open source");
        let cleaned = HashSet::from([old_one, old_two]);

        let planned = plan_archive_sweep(
            &directory.path,
            &reader,
            &cleaned,
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
        .expect("plan")
        .expect("archive has reclaimable entries");
        assert!(matches!(
            planned,
            PlannedArchiveSweep::BlockedByOccupiedGeneration {
                ref occupied_name,
                ..
            } if occupied_name == "data00000b.tar"
        ));
        let mut fallback = None;
        sweep_one_archive(
            &directory.path,
            &reader,
            &cleaned,
            &cleaned,
            &[&reader],
            &mut fallback,
            None,
            ArchiveRewritePolicy::default(),
        )
        .expect("blocked sweep is a safe no-op");
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar")).expect("read occupied target"),
            occupied
        );
        assert!(directory.path.join("data00000a.tar").exists());
    }

    #[test]
    fn occupied_higher_generation_blocks_whole_archive_removal() {
        let directory = TestDirectory::new("occupied-blocks-whole-removal");
        let obsolete = data_identifier(13);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(obsolete, 1, generation(0, 0, false))],
        );
        let occupied = b"damaged-higher-generation-must-not-become-active";
        std::fs::write(directory.path.join("data00000c.tar"), occupied)
            .expect("write recovered residue");
        let source_path = directory.path.join("data00000a.tar");
        let source_before = std::fs::read(&source_path).expect("read source");
        let reader = TarArchiveReader::open(&source_path).expect("open source");
        let cleaned = HashSet::from([obsolete]);

        assert!(matches!(
            plan_archive_sweep(
                &directory.path,
                &reader,
                &cleaned,
                ArchiveRewritePolicy::default(),
                &std::collections::HashSet::new(),
            )
                .expect("plan")
                .expect("eligible archive"),
            PlannedArchiveSweep::BlockedByOccupiedGeneration {
                occupied_name,
                segment_count: 1,
                ..
            } if occupied_name == "data00000c.tar"
        ));
        let mut fallback = None;
        sweep_one_archive(
            &directory.path,
            &reader,
            &cleaned,
            &cleaned,
            &[&reader],
            &mut fallback,
            None,
            ArchiveRewritePolicy::default(),
        )
        .expect("blocked removal is a no-op");
        assert_eq!(
            std::fs::read(source_path).expect("source remains"),
            source_before
        );
        assert_eq!(
            std::fs::read(directory.path.join("data00000c.tar")).expect("residue remains"),
            occupied
        );
    }

    #[test]
    fn lower_stale_generation_blocks_whole_active_archive_removal() {
        let directory = TestDirectory::new("lower-letter-blocks-whole-removal");
        let stale = data_identifier(14);
        let obsolete = data_identifier(15);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(stale, 1, generation(0, 0, false))],
        );
        write_test_archive(
            &directory,
            "data00000b.tar",
            &[TestArchiveEntry::new(obsolete, 1, generation(0, 0, false))],
        );
        let stale_path = directory.path.join("data00000a.tar");
        let active_path = directory.path.join("data00000b.tar");
        let stale_before = std::fs::read(&stale_path).expect("read stale generation");
        let active_before = std::fs::read(&active_path).expect("read active generation");
        let active = TarArchiveReader::open(&active_path).expect("open active generation");
        let cleaned = HashSet::from([obsolete]);

        assert!(matches!(
            plan_archive_sweep(
                &directory.path,
                &active,
                &cleaned,
                ArchiveRewritePolicy::default(),
                &std::collections::HashSet::new(),
            )
                .expect("plan")
                .expect("eligible archive"),
            PlannedArchiveSweep::BlockedByOccupiedGeneration {
                occupied_name,
                segment_count: 1,
                ..
            } if occupied_name == "data00000a.tar"
        ));
        let mut fallback = None;
        sweep_one_archive(
            &directory.path,
            &active,
            &cleaned,
            &cleaned,
            &[&active],
            &mut fallback,
            None,
            ArchiveRewritePolicy::default(),
        )
        .expect("blocked removal is a no-op");
        assert_eq!(
            std::fs::read(active_path).expect("active remains"),
            active_before
        );
        assert_eq!(
            std::fs::read(stale_path).expect("stale remains"),
            stale_before
        );
    }

    #[test]
    fn last_generation_z_is_deferred_without_creating_an_invalid_successor() {
        let directory = TestDirectory::new("generation-z");
        let root = data_identifier(20);
        let old_one = data_identifier(21);
        let old_two = data_identifier(22);
        write_test_archive(
            &directory,
            "data00000z.tar",
            &[
                TestArchiveEntry::new(root, 1, generation(4, 4, false)),
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
            ],
        );
        let path = directory.path.join("data00000z.tar");
        let before = std::fs::read(&path).expect("read source");
        let reader = TarArchiveReader::open(&path).expect("open source");
        let cleaned = HashSet::from([old_one, old_two]);
        assert!(matches!(
            plan_archive_sweep(
                &directory.path,
                &reader,
                &cleaned,
                ArchiveRewritePolicy::default(),
                &std::collections::HashSet::new(),
            )
            .expect("plan")
            .expect("has eligible entries"),
            PlannedArchiveSweep::DeferredAtLastGeneration { .. }
        ));
        let mut fallback = None;
        sweep_one_archive(
            &directory.path,
            &reader,
            &cleaned,
            &cleaned,
            &[&reader],
            &mut fallback,
            None,
            ArchiveRewritePolicy::default(),
        )
        .expect("z sweep is a no-op");
        assert_eq!(std::fs::read(path).expect("read after"), before);
        assert_eq!(
            std::fs::read_dir(&directory.path)
                .expect("list")
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tar"))
                .count(),
            1
        );
    }

    /// The two-archive store the rewrite-replan cases sweep. Each source holds
    /// segments the sweep may reclaim, and the second holds a root whose graph
    /// edge points at the first archive's reclaimable target — the edge whose
    /// survival distinguishes a blocked replan from a published one.
    struct ReplanFixture {
        target: SegmentIdentifier,
        old_one: SegmentIdentifier,
        old_two: SegmentIdentifier,
        root: SegmentIdentifier,
    }

    fn build_replan_fixture(directory: &TestDirectory) -> ReplanFixture {
        let target = data_identifier(83);
        let retained = data_identifier(84);
        let old_one = data_identifier(85);
        let old_two = data_identifier(86);
        let root = data_identifier(87);
        let reference = generation(5, 5, false);
        write_test_archive(
            directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(target, 1, generation(0, 0, false)),
                TestArchiveEntry::new(retained, 1, reference),
            ],
        );
        write_test_archive(
            directory,
            "data00001a.tar",
            &[
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
                TestArchiveEntry::new(root, 1, reference).referencing(&[target]),
            ],
        );
        ReplanFixture {
            target,
            old_one,
            old_two,
            root,
        }
    }

    /// Every source must be an actionable rewrite before the sweeps run, or the
    /// outcomes they report would prove nothing about the replan.
    fn assert_both_sources_plan_rewrites(
        directory: &Path,
        sources: [&TarArchiveReader; 2],
        reclaimable: &HashSet<SegmentIdentifier>,
    ) {
        for source in sources {
            assert!(matches!(
                plan_archive_sweep(
                    directory,
                    source,
                    reclaimable,
                    ArchiveRewritePolicy::default(),
                    &HashSet::new(),
                )
                .expect("initial plan")
                .expect("archive is initially actionable"),
                PlannedArchiveSweep::Rewrite { .. }
            ));
        }
    }

    #[test]
    fn rewrite_replan_noop_reports_no_unavailable_graph_targets() {
        let directory = TestDirectory::new("rewrite-replan-noop-graph-target");
        let ReplanFixture {
            target,
            old_one,
            old_two,
            root,
        } = build_replan_fixture(&directory);

        let first = TarArchiveReader::open(&directory.path.join("data00000a.tar"))
            .expect("open first rewrite source");
        let second = TarArchiveReader::open(&directory.path.join("data00001a.tar"))
            .expect("open second rewrite source");
        let reclaimable = HashSet::from([target, old_one, old_two]);
        assert_both_sources_plan_rewrites(&directory.path, [&first, &second], &reclaimable);

        // Model a pathname appearing after the outer plan but before the
        // immediate per-archive replan. The first sweep must return a proven
        // no-publication outcome, not inherit the stale Rewrite disposition.
        let occupied = b"occupied after outer planning";
        std::fs::write(directory.path.join("data00000b.tar"), occupied)
            .expect("occupy first replacement");
        let provider_order = [&first, &second];
        let mut fallback = None;
        let mut actually_unavailable = HashSet::new();
        let first_outcome = sweep_one_archive(
            &directory.path,
            &first,
            &reclaimable,
            &actually_unavailable,
            &provider_order,
            &mut fallback,
            None,
            ArchiveRewritePolicy::default(),
        )
        .expect("blocked immediate replan is a no-op");
        assert!(first_outcome.deletion_failures.is_empty());
        assert!(
            first_outcome.newly_unavailable.is_empty(),
            "a planned rewrite that never published cannot justify graph filtering"
        );
        assert!(
            directory.path.join("data00000a.tar").exists(),
            "the blocked immediate replan must leave its source available"
        );
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar")).expect("read occupied target"),
            occupied,
            "the blocked immediate replan must not replace the new pathname"
        );
        actually_unavailable.extend(first_outcome.newly_unavailable);

        let second_outcome = sweep_one_archive(
            &directory.path,
            &second,
            &reclaimable,
            &actually_unavailable,
            &provider_order,
            &mut fallback,
            None,
            ArchiveRewritePolicy::default(),
        )
        .expect("second rewrite publishes");
        assert_eq!(
            second_outcome.newly_unavailable,
            HashSet::from([old_one, old_two])
        );

        let rewritten = TarArchiveReader::open(&directory.path.join("data00001b.tar"))
            .expect("open second replacement");
        assert_eq!(
            rewritten.segment_graph().expect("valid graph").as_map()[&root],
            [target],
            "the later rewrite must retain an edge to the still-available first target"
        );
    }
}
