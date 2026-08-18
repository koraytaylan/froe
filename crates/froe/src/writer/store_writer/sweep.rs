//! Rewriting one archive without its reclaimed segments: stage,
//! validate, publish atomically, then unlink the source.

use super::archive_certificate::{
    ExpectedBinaryReferences, ExpectedGraph, validate_exact_archive_trailers,
};
use super::file_identity::{
    FileAccess, UncommittedArchiveStaging, held_file_identity, open_regular_file_no_follow,
    path_object_identity, preserve_file_metadata, remove_published_link_if_same,
    require_held_file_identity, require_path_file_identity, sync_directory_strict,
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

pub(super) fn next_archive_staging_name(
    directory: &Path,
    replacement_name: &str,
) -> Result<String> {
    for counter in 0..=999u16 {
        let candidate = format!("{replacement_name}.cleaning.{counter:03}");
        match std::fs::symlink_metadata(directory.join(&candidate)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::InvalidFormat {
        details: format!(
            "all 1000 exclusive staging names for archive {replacement_name} are occupied"
        ),
    })
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
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

    // Bind every actionable source to a no-follow descriptor and retain its
    // inode identity through the destructive syscall. Standalone cleanup and
    // ordinary post-compaction cleanup additionally repeat the complete source
    // certificate through this exact descriptor-backed mapping. Replanning
    // prevents a semantically different but still well-formed source from
    // silently proceeding after the locked plan.
    let reopened_source = if planned.changes_disk() {
        let source_file = open_regular_file_no_follow(&path, FileAccess::ReadOnly)?;
        let source_identity = held_file_identity(&source_file)?;
        let reopened = TarArchiveReader::open_file(&path, &source_file)?;
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
            != Some(&planned)
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
    let reader = reopened_source
        .as_ref()
        .map_or(reader, |(reopened, _, _)| reopened);
    let Some(index) = reader.index() else {
        // Post-compaction cleanup retains Oak's conservative treatment of
        // recovered base archives. Standalone cleanup cannot reach this branch
        // because its source certificate rejects an indexless archive.
        return Ok(ArchiveSweepOutcome::default());
    };
    let planned_unavailable: std::collections::HashSet<_> = index
        .entries()
        .iter()
        .map(|entry| entry.segment_identifier)
        .filter(|identifier| reclaimable.contains(identifier))
        .collect();

    let (replacement_name, planned_reclaimable_count) = match planned {
        PlannedArchiveSweep::Remove { .. } => {
            let (_, source_file, source_identity) = reopened_source
                .as_ref()
                .expect("an archive removal always has an actionable reopened source");
            #[cfg(test)]
            crate::writer::maintenance_fault_injection::substitute_path_if_armed(
                "sweep.remove-before-source-identity",
                &path,
            )?;
            require_held_file_identity(source_file, *source_identity, "certified removal source")?;
            require_path_file_identity(&path, *source_identity, "certified removal source")?;
            #[cfg(test)]
            crate::writer::maintenance_fault_injection::remove_path_if_armed(
                "sweep.remove-before-source-unlink-not-found",
                &path,
            )?;
            // Deletion failures are consistency-safe: ordinarily the old
            // archive remains authoritative for retry; `NotFound` records
            // that another actor already achieved this exact unlink.
            return Ok(match std::fs::remove_file(&path) {
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
            });
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

    // Partition the entries in file-position order, accumulating Oak's
    // sweep arithmetic (`i64` cannot wrap where Java's `int` could not
    // either: entries are position-bounded below 2 GiB).
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
    let current_rewrite_targets = planned_unavailable;

    // These two proofs deliberately have different graph scopes. Production
    // active-source certification above reconstructs the complete, unfiltered
    // graph from payloads before mutation. A replacement `.gph` is derived
    // subtractively: source entries not copied by this rewrite are left out,
    // while targets are filtered against both identifiers this run previously
    // made unavailable and identifiers belonging to those omitted entries.
    // Staged and published validation below compare it with that exact view.
    let trailers = FilteredTrailers::from_archive(
        reader,
        reclaimable,
        previously_unavailable_graph_targets,
        &current_rewrite_targets,
    );
    // When the original archive has no readable catalog, survivor
    // references are reconstructed by a strict scan resolving across
    // every segment of every base archive — and the sweep fails closed
    // on an unresolvable identifier rather than publish an incomplete
    // catalog that could let blob garbage collection delete referenced
    // binaries. (Java would publish an *empty* catalog here; the scan is
    // a strict superset of that.)
    let scan_provider = if trailers.catalog.is_none() {
        if fallback_provider.is_none() {
            *fallback_provider = Some(archive_segments_provider(all_archives)?);
        }
        fallback_provider.as_ref()
    } else {
        None
    };

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
    let mut expected_graph = ExpectedGraph::new();
    let mut expected_binary_references = ExpectedBinaryReferences::new();
    if let Some(catalog_entries) = &trailers.catalog {
        for (generation, segment, references) in catalog_entries {
            writer.add_binary_references(*generation, *segment, references.iter().cloned());
            expected_binary_references
                .entry((
                    generation.generation,
                    generation.full_generation,
                    generation.is_compacted,
                ))
                .or_default()
                .entry(*segment)
                .or_default()
                .extend(references.iter().cloned());
        }
    }
    for entry in &survivors {
        let identifier = entry.segment_identifier;
        let Some(bytes) = reader.segment_data(identifier) else {
            return Err(Error::SegmentNotFound {
                segment_identifier: identifier,
            });
        };
        let generation = GarbageCollectionGeneration {
            generation: entry.generation,
            full_generation: entry.full_generation,
            is_compacted: entry.is_compacted,
        };
        let (references, binary_references) = if identifier.is_data_segment() {
            trailers.for_segment(
                identifier,
                bytes,
                previously_unavailable_graph_targets,
                &current_rewrite_targets,
                scan_provider,
            )?
        } else {
            (Vec::new(), Vec::new())
        };
        if !references.is_empty() {
            expected_graph
                .entry(identifier)
                .or_default()
                .extend(references.iter().copied());
        }
        if trailers.catalog.is_none() && !binary_references.is_empty() {
            expected_binary_references
                .entry((
                    generation.generation,
                    generation.full_generation,
                    generation.is_compacted,
                ))
                .or_default()
                .entry(identifier)
                .or_default()
                .extend(binary_references.iter().cloned());
        }
        let write_result = writer.write_segment(
            identifier,
            bytes,
            generation,
            &references,
            &binary_references,
        );
        // `TarArchiveWriter` creates lazily. Capture the descriptor before
        // propagating the write result so a first-write ENOSPC-style failure
        // cannot strand an unowned `.cleaning` pathname.
        uncommitted_staging.capture_created_file(&writer)?;
        write_result?;
    }
    writer.close()?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "sweep.staging-before-validation-open",
    )?;
    let staging_file = open_regular_file_no_follow(&staging_path, FileAccess::ReadWrite)?;
    preserve_file_metadata(&staging_file, &source_metadata)?;
    let staging_identity = held_file_identity(&staging_file)?;
    let staged_reader = TarArchiveReader::open_file(&staging_path, &staging_file)?;
    if let Err(error) = validate_open_swept_archive(
        reader,
        &staged_reader,
        &staging_path,
        &survivors,
        &expected_graph,
        &expected_binary_references,
    ) {
        return Err(Error::InvalidFormat {
            details: format!(
                "staged rewrite for {} failed complete survivor/trailer validation ({error}); the original {} was left untouched",
                replacement_name,
                reader.file_name()
            ),
        });
    }
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::substitute_path_if_armed(
        "sweep.staging-validated-before-publish",
        &staging_path,
    )?;
    require_held_file_identity(
        &staging_file,
        staging_identity,
        "validated archive staging file",
    )?;
    require_path_file_identity(
        &staging_path,
        staging_identity,
        "validated archive staging file",
    )?;
    // From this point onward the complete validated staging file is useful
    // crash evidence. Publication failures retain it intentionally; ordinary
    // success removes it explicitly below.
    uncommitted_staging.disarm();

    // `hard_link` is an atomic absent-only publication: unlike rename it
    // cannot overwrite a final path created after planning. Both names refer
    // to the already-synced, validated inode until staging cleanup.
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("sweep.before-publish-link")?;
    std::fs::hard_link(&staging_path, &replacement_path)?;

    // A pathname substitution in the narrow interval between the pre-link
    // identity check and `hard_link` must not turn an arbitrary inode into the
    // higher active archive generation. Capture the two names immediately; if
    // they agree with each other but not the validated descriptor, they still
    // identify the link this process just published and can be removed safely.
    let linked_stage_identity = path_object_identity(&staging_path).ok();
    let linked_replacement_identity = path_object_identity(&replacement_path).ok();
    let just_published_identity =
        linked_stage_identity.filter(|identity| Some(*identity) == linked_replacement_identity);
    if linked_stage_identity != Some(staging_identity)
        || linked_replacement_identity != Some(staging_identity)
    {
        remove_published_link_if_same(directory, &replacement_path, just_published_identity)?;
        return Err(Error::InvalidFormat {
            details: format!(
                "archive staging or published path changed identity while publishing {replacement_name}; the source was left untouched"
            ),
        });
    }

    let replacement_file =
        match open_regular_file_no_follow(&replacement_path, FileAccess::ReadOnly) {
            Ok(file) => file,
            Err(error) => {
                remove_published_link_if_same(
                    directory,
                    &replacement_path,
                    Some(staging_identity),
                )?;
                return Err(error);
            }
        };
    let replacement_validation = (|| {
        require_held_file_identity(
            &replacement_file,
            staging_identity,
            "published archive replacement",
        )?;
        require_path_file_identity(
            &replacement_path,
            staging_identity,
            "published archive replacement",
        )?;
        let replacement_reader = TarArchiveReader::open_file(&replacement_path, &replacement_file)?;
        validate_open_swept_archive(
            reader,
            &replacement_reader,
            &replacement_path,
            &survivors,
            &expected_graph,
            &expected_binary_references,
        )
    })();
    if let Err(error) = replacement_validation {
        remove_published_link_if_same(directory, &replacement_path, Some(staging_identity))?;
        return Err(Error::InvalidFormat {
            details: format!(
                "published rewrite {replacement_name} failed descriptor-bound validation ({error}); the source was left untouched"
            ),
        });
    }
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("sweep.after-publish-link")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "sweep.before-publish-directory-sync",
    )?;
    sync_directory_strict(directory)?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "sweep.after-publish-directory-sync",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed(
        "sweep.published-before-source-unlink",
    );
    let mut deletion_failures = Vec::new();
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("sweep.before-staging-unlink")?;
    if let Err(error) = std::fs::remove_file(&staging_path) {
        deletion_failures.push(DeferredFileDeletion {
            file_name: staging_name,
            error: error.to_string(),
            target_was_already_absent: error.kind() == std::io::ErrorKind::NotFound,
        });
    }
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("sweep.after-staging-unlink")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed(
        "sweep.staging-unlinked-before-source-unlink",
    );
    // Deletion failures are consistency-safe: the published higher letter
    // wins and preserves every survivor; the old source is reported later.
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("sweep.before-source-unlink")?;
    let pre_source_unlink_identity = (|| {
        require_held_file_identity(
            &replacement_file,
            staging_identity,
            "published archive replacement",
        )?;
        require_path_file_identity(
            &replacement_path,
            staging_identity,
            "published archive replacement",
        )?;
        require_held_file_identity(source_file, *source_identity, "certified archive source")?;
        require_path_file_identity(&path, *source_identity, "certified archive source")
    })();
    if let Err(error) = pre_source_unlink_identity {
        remove_published_link_if_same(directory, &replacement_path, Some(staging_identity))?;
        return Err(Error::InvalidFormat {
            details: format!(
                "archive identity changed immediately before removing {} ({error}); the source pathname was left untouched",
                reader.file_name()
            ),
        });
    }
    if let Err(error) = std::fs::remove_file(&path) {
        deletion_failures.push(DeferredFileDeletion {
            file_name: reader.file_name().to_owned(),
            error: error.to_string(),
            target_was_already_absent: error.kind() == std::io::ErrorKind::NotFound,
        });
    }
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("sweep.after-source-unlink")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("sweep.source-unlinked");
    Ok(ArchiveSweepOutcome {
        disposition: ArchiveSweepDisposition::Rewritten,
        deletion_failures,
        newly_unavailable: current_rewrite_targets,
    })
}

#[cfg(test)]
pub(super) fn probe_archive_sweep_phase_boundary(cutpoint: &str) -> Result<()> {
    crate::writer::maintenance_fault_injection::fail_if_armed(cutpoint)?;
    crate::writer::maintenance_fault_injection::crash_if_armed(cutpoint);
    Ok(())
}

/// Reopens a swept archive and proves that every survivor's payload and
/// generation metadata exactly match the immutable source before the source
/// may be removed.
#[allow(
    clippy::too_many_lines,
    reason = "payload, generation, order, graph, and BRF checks are one fail-closed archive validation certificate"
)]
#[cfg(test)]
pub(super) fn validate_swept_archive(
    source: &TarArchiveReader,
    swept_path: &Path,
    survivors: &[crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &ExpectedGraph,
    expected_binary_references: &ExpectedBinaryReferences,
) -> Result<()> {
    let swept = TarArchiveReader::open(swept_path)?;
    validate_open_swept_archive(
        source,
        &swept,
        swept_path,
        survivors,
        expected_graph,
        expected_binary_references,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "payload, generation, order, graph, and BRF checks are one fail-closed archive validation certificate"
)]
pub(super) fn validate_open_swept_archive(
    source: &TarArchiveReader,
    swept: &TarArchiveReader,
    swept_path: &Path,
    survivors: &[crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &ExpectedGraph,
    expected_binary_references: &ExpectedBinaryReferences,
) -> Result<()> {
    if swept.is_recovered() {
        return Err(Error::InvalidFormat {
            details: format!("{} has no valid index", swept_path.display()),
        });
    }
    if swept.segment_count() != survivors.len() {
        return Err(Error::InvalidFormat {
            details: format!(
                "{} contains {} segments, expected {}",
                swept_path.display(),
                swept.segment_count(),
                survivors.len()
            ),
        });
    }
    let mut actual_in_file_order = swept
        .index()
        .expect("a non-recovered archive has an index")
        .entries()
        .to_vec();
    actual_in_file_order.sort_by_key(|entry| entry.position);
    let actual_identifier_order: Vec<_> = actual_in_file_order
        .iter()
        .map(|entry| entry.segment_identifier)
        .collect();
    let expected_identifier_order: Vec<_> = survivors
        .iter()
        .map(|entry| entry.segment_identifier)
        .collect();
    if actual_identifier_order != expected_identifier_order {
        return Err(Error::InvalidFormat {
            details: format!(
                "{} changed the physical order of surviving segments",
                swept_path.display()
            ),
        });
    }
    for expected in survivors {
        swept.validate_indexed_segment_entry(expected.segment_identifier)?;
        let actual = swept
            .index_entry(expected.segment_identifier)
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "{} omits surviving segment {}",
                    swept_path.display(),
                    expected.segment_identifier
                ),
            })?;
        if actual.size != expected.size
            || actual.generation != expected.generation
            || actual.full_generation != expected.full_generation
            || actual.is_compacted != expected.is_compacted
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} changed generation metadata for surviving segment {}",
                    swept_path.display(),
                    expected.segment_identifier
                ),
            });
        }
        let source_bytes =
            source
                .segment_data(expected.segment_identifier)
                .ok_or(Error::SegmentNotFound {
                    segment_identifier: expected.segment_identifier,
                })?;
        let swept_bytes =
            swept
                .segment_data(expected.segment_identifier)
                .ok_or(Error::SegmentNotFound {
                    segment_identifier: expected.segment_identifier,
                })?;
        if source_bytes != swept_bytes {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} changed the payload of surviving segment {}",
                    swept_path.display(),
                    expected.segment_identifier
                ),
            });
        }
    }

    validate_exact_archive_trailers(
        swept,
        &swept_path.display().to_string(),
        expected_graph,
        expected_binary_references,
    )
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
