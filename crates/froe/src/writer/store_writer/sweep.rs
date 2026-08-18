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

/// Everything the survivor copy reads to rebuild each surviving
/// segment's graph and binary-reference entries.
#[derive(Clone, Copy)]
struct SurvivorSources<'sources> {
    reader: &'sources TarArchiveReader,
    trailers: &'sources FilteredTrailers,
    previously_unavailable_graph_targets: &'sources std::collections::HashSet<SegmentIdentifier>,
    current_rewrite_targets: &'sources std::collections::HashSet<SegmentIdentifier>,
    scan_provider: Option<&'sources ArchiveSegmentsProvider<'sources>>,
}

/// Copies every survivor into the staged replacement, rebuilding the
/// trailers it must carry and returning what validation will hold it to.
fn copy_survivors(
    sources: SurvivorSources<'_>,
    survivors: &[crate::tar_archive::index::SegmentIndexEntry],
    writer: &mut TarArchiveWriter,
    uncommitted_staging: &mut UncommittedArchiveStaging,
) -> Result<(ExpectedGraph, ExpectedBinaryReferences)> {
    let SurvivorSources {
        reader,
        trailers,
        previously_unavailable_graph_targets,
        current_rewrite_targets,
        scan_provider,
    } = sources;
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
    for entry in survivors {
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
                current_rewrite_targets,
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
        uncommitted_staging.capture_created_file(writer)?;
        write_result?;
    }
    Ok((expected_graph, expected_binary_references))
}

/// Everything the publication phase reads to prove the staged archive
/// and swap it in for the source.
struct SweepPublication<'publication> {
    directory: &'publication Path,
    source_path: &'publication Path,
    reader: &'publication TarArchiveReader,
    source_file: &'publication std::fs::File,
    source_identity: RegularFileIdentity,
    staging_name: &'publication str,
    staging_path: &'publication Path,
    replacement_name: &'publication str,
    replacement_path: &'publication Path,
    survivors: &'publication [crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &'publication ExpectedGraph,
    expected_binary_references: &'publication ExpectedBinaryReferences,
    source_metadata: &'publication std::fs::Metadata,
    current_rewrite_targets: std::collections::HashSet<SegmentIdentifier>,
}

/// What the staged archive is proved against before publication.
struct StagedValidation<'validation> {
    reader: &'validation TarArchiveReader,
    staging_path: &'validation Path,
    replacement_name: &'validation str,
    survivors: &'validation [crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &'validation ExpectedGraph,
    expected_binary_references: &'validation ExpectedBinaryReferences,
    source_metadata: &'validation std::fs::Metadata,
}

/// Reopens the staged archive through its own no-follow descriptor and
/// holds it to every survivor and trailer the copy was meant to write.
///
/// Returns the validated inode's identity, which publication then
/// requires the linked names to agree with.
fn validate_staged_replacement(validation: &StagedValidation<'_>) -> Result<RegularFileIdentity> {
    let &StagedValidation {
        reader,
        staging_path,
        replacement_name,
        survivors,
        expected_graph,
        expected_binary_references,
        source_metadata,
    } = validation;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "sweep.staging-before-validation-open",
    )?;
    let staging_file = open_regular_file_no_follow(staging_path, FileAccess::ReadWrite)?;
    preserve_file_metadata(&staging_file, source_metadata)?;
    let staging_identity = held_file_identity(&staging_file)?;
    let staged_reader = TarArchiveReader::open_file(staging_path, &staging_file)?;
    if let Err(error) = validate_open_swept_archive(
        reader,
        &staged_reader,
        staging_path,
        survivors,
        expected_graph,
        expected_binary_references,
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
        staging_path,
    )?;
    require_held_file_identity(
        &staging_file,
        staging_identity,
        "validated archive staging file",
    )?;
    require_path_file_identity(
        staging_path,
        staging_identity,
        "validated archive staging file",
    )?;
    Ok(staging_identity)
}

/// The source archive a published rewrite supersedes.
struct RetiredSource<'retired> {
    directory: &'retired Path,
    replacement_path: &'retired Path,
    replacement_file: &'retired std::fs::File,
    staging_identity: RegularFileIdentity,
    path: &'retired Path,
    reader: &'retired TarArchiveReader,
    source_file: &'retired std::fs::File,
    source_identity: RegularFileIdentity,
    staging_name: &'retired str,
    staging_path: &'retired Path,
    current_rewrite_targets: std::collections::HashSet<SegmentIdentifier>,
}

/// Removes the staging alias and then the source, once the replacement
/// is published and durable.
///
/// Each unlink is guarded by the identity the source was certified with,
/// so a path substituted underneath this run removes nothing.
fn retire_swept_source(retired: RetiredSource<'_>) -> Result<ArchiveSweepOutcome> {
    let RetiredSource {
        directory,
        replacement_path,
        replacement_file,
        staging_identity,
        path,
        reader,
        source_file,
        source_identity,
        staging_name,
        staging_path,
        current_rewrite_targets,
    } = retired;
    let mut deletion_failures = Vec::new();
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("sweep.before-staging-unlink")?;
    if let Err(error) = std::fs::remove_file(staging_path) {
        deletion_failures.push(DeferredFileDeletion {
            file_name: staging_name.to_owned(),
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
            replacement_file,
            staging_identity,
            "published archive replacement",
        )?;
        require_path_file_identity(
            replacement_path,
            staging_identity,
            "published archive replacement",
        )?;
        require_held_file_identity(source_file, source_identity, "certified archive source")?;
        require_path_file_identity(path, source_identity, "certified archive source")
    })();
    if let Err(error) = pre_source_unlink_identity {
        remove_published_link_if_same(directory, replacement_path, Some(staging_identity))?;
        return Err(Error::InvalidFormat {
            details: format!(
                "archive identity changed immediately before removing {} ({error}); the source pathname was left untouched",
                reader.file_name()
            ),
        });
    }
    if let Err(error) = std::fs::remove_file(path) {
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

/// What the published name still has to prove after `hard_link`.
///
/// Staging already compared every survivor payload to the source. The
/// published path is a second name for that same inode once identity holds;
/// re-reading every byte would only catch an in-place rewrite, which the
/// segment-store file protocol forbids. A substituted pathname fails the
/// identity checks instead.
#[derive(Clone, Copy)]
enum SurvivorPayloadProof {
    CompareWithSource,
    SameInodeAsValidatedStaging,
}

/// The replacement, once the hard link that published it exists.
struct PublishedReplacement<'published> {
    directory: &'published Path,
    replacement_path: &'published Path,
    replacement_name: &'published str,
    staging_path: &'published Path,
    staging_identity: RegularFileIdentity,
    reader: &'published TarArchiveReader,
    survivors: &'published [crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &'published ExpectedGraph,
    expected_binary_references: &'published ExpectedBinaryReferences,
}

/// Proves the two names the publication linked still identify the inode
/// validation certified.
///
/// A pathname substitution in the narrow interval between the pre-link
/// identity check and `hard_link` must not turn an arbitrary inode into
/// the higher active archive generation. If the two names agree with each
/// other but not the validated descriptor, they still identify the link
/// this process just published and can be removed safely.
fn certify_published_replacement(published: &PublishedReplacement<'_>) -> Result<std::fs::File> {
    let &PublishedReplacement {
        directory,
        replacement_path,
        replacement_name,
        staging_path,
        staging_identity,
        reader,
        survivors,
        expected_graph,
        expected_binary_references,
    } = published;
    let linked_stage_identity = path_object_identity(staging_path).ok();
    let linked_replacement_identity = path_object_identity(replacement_path).ok();
    let just_published_identity =
        linked_stage_identity.filter(|identity| Some(*identity) == linked_replacement_identity);
    if linked_stage_identity != Some(staging_identity)
        || linked_replacement_identity != Some(staging_identity)
    {
        remove_published_link_if_same(directory, replacement_path, just_published_identity)?;
        return Err(Error::InvalidFormat {
            details: format!(
                "archive staging or published path changed identity while publishing {replacement_name}; the source was left untouched"
            ),
        });
    }

    let replacement_file = match open_regular_file_no_follow(replacement_path, FileAccess::ReadOnly)
    {
        Ok(file) => file,
        Err(error) => {
            remove_published_link_if_same(directory, replacement_path, Some(staging_identity))?;
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
            replacement_path,
            staging_identity,
            "published archive replacement",
        )?;
        let replacement_reader = TarArchiveReader::open_file(replacement_path, &replacement_file)?;
        validate_published_swept_archive(
            reader,
            &replacement_reader,
            replacement_path,
            survivors,
            expected_graph,
            expected_binary_references,
        )
    })();
    if let Err(error) = replacement_validation {
        remove_published_link_if_same(directory, replacement_path, Some(staging_identity))?;
        return Err(Error::InvalidFormat {
            details: format!(
                "published rewrite {replacement_name} failed descriptor-bound validation ({error}); the source was left untouched"
            ),
        });
    }
    Ok(replacement_file)
}

/// Validates the staged archive, publishes it under the replacement
/// name, and only then unlinks the source.
///
/// The order is the safety argument: nothing the source holds is removed
/// until a complete, independently reopened copy of every survivor is
/// durable under its final name.
fn publish_swept_archive(
    publication: SweepPublication<'_>,
    uncommitted_staging: &mut UncommittedArchiveStaging,
) -> Result<ArchiveSweepOutcome> {
    let SweepPublication {
        directory,
        source_path: path,
        reader,
        source_file,
        source_identity,
        staging_name,
        staging_path,
        replacement_name,
        replacement_path,
        survivors,
        expected_graph,
        expected_binary_references,
        source_metadata,
        current_rewrite_targets,
    } = publication;
    let staging_identity = validate_staged_replacement(&StagedValidation {
        reader,
        staging_path,
        replacement_name,
        survivors,
        expected_graph,
        expected_binary_references,
        source_metadata,
    })?;
    // From this point onward the complete validated staging file is useful
    // crash evidence. Publication failures retain it intentionally; ordinary
    // success removes it explicitly below.
    uncommitted_staging.disarm();

    // `hard_link` is an atomic absent-only publication: unlike rename it
    // cannot overwrite a final path created after planning. Both names refer
    // to the already-synced, validated inode until staging cleanup.
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("sweep.before-publish-link")?;
    std::fs::hard_link(staging_path, replacement_path)?;

    let replacement_file = certify_published_replacement(&PublishedReplacement {
        directory,
        replacement_path,
        replacement_name,
        staging_path,
        staging_identity,
        reader,
        survivors,
        expected_graph,
        expected_binary_references,
    })?;
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
    retire_swept_source(RetiredSource {
        directory,
        replacement_path,
        replacement_file: &replacement_file,
        staging_identity,
        path,
        reader,
        source_file,
        source_identity,
        staging_name,
        staging_path,
        current_rewrite_targets,
    })
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
fn reopen_actionable_source(
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

/// Splits an archive's index entries into survivors and reclaimed, in
/// file-position order.
///
/// Accumulates Oak's sweep arithmetic as it goes: `i64` cannot wrap where
/// Java's `int` could not either, because entries are position-bounded
/// below 2 GiB.
fn partition_entries(
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

/// Unlinks an archive whose segments all reclaim, after proving the file
/// is still the one certification bound.
///
/// Deletion failures are consistency-safe: ordinarily the old archive
/// remains authoritative for retry, and `NotFound` records that another
/// actor already achieved this exact unlink.
fn remove_swept_archive(
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
fn source_trailers(
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
fn scan_provider_for<'fallback, 'archives>(
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

/// The segments this archive holds that the mark phase found reclaimable.
fn reclaimable_in_archive(
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

#[cfg(test)]
pub(super) fn probe_archive_sweep_phase_boundary(cutpoint: &str) -> Result<()> {
    crate::writer::maintenance_fault_injection::fail_if_armed(cutpoint)?;
    crate::writer::maintenance_fault_injection::crash_if_armed(cutpoint);
    Ok(())
}

/// Reopens a swept archive and proves that every survivor's payload and
/// generation metadata exactly match the immutable source before the source
/// may be removed.
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

/// Staging certificate: CRC32 plus a full memcmp of every survivor against
/// the source, then generation, order, and trailers.
pub(super) fn validate_open_swept_archive(
    source: &TarArchiveReader,
    swept: &TarArchiveReader,
    swept_path: &Path,
    survivors: &[crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &ExpectedGraph,
    expected_binary_references: &ExpectedBinaryReferences,
) -> Result<()> {
    prove_survivor_count_order_and_entries(
        source,
        swept,
        swept_path,
        survivors,
        SurvivorPayloadProof::CompareWithSource,
    )?;
    validate_exact_archive_trailers(
        swept,
        &swept_path.display().to_string(),
        expected_graph,
        expected_binary_references,
    )
}

/// Published-path certificate after `hard_link` plus inode identity.
///
/// Payload bytes were already compared on the staging inode; this name is
/// a second directory entry for that inode. Count, order, generation, and
/// trailers still run against the published mapping.
pub(super) fn validate_published_swept_archive(
    source: &TarArchiveReader,
    swept: &TarArchiveReader,
    swept_path: &Path,
    survivors: &[crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &ExpectedGraph,
    expected_binary_references: &ExpectedBinaryReferences,
) -> Result<()> {
    prove_survivor_count_order_and_entries(
        source,
        swept,
        swept_path,
        survivors,
        SurvivorPayloadProof::SameInodeAsValidatedStaging,
    )?;
    validate_exact_archive_trailers(
        swept,
        &swept_path.display().to_string(),
        expected_graph,
        expected_binary_references,
    )
}

fn prove_survivor_count_order_and_entries(
    source: &TarArchiveReader,
    swept: &TarArchiveReader,
    swept_path: &Path,
    survivors: &[crate::tar_archive::index::SegmentIndexEntry],
    payload_proof: SurvivorPayloadProof,
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
        if matches!(payload_proof, SurvivorPayloadProof::CompareWithSource) {
            swept.validate_indexed_segment_entry(expected.segment_identifier)?;
        }
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
        if matches!(payload_proof, SurvivorPayloadProof::CompareWithSource) {
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
    }
    Ok(())
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
mod tests {
    use super::*;

    use crate::writer::store_writer::test_support::*;
    use std::collections::HashMap;
    use std::collections::HashSet;

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
    /// A failed staging leaves bytes on disk that no archive selection can
    /// reach, and leaves the healthy source exactly as it was.
    fn assert_staging_is_non_active_residue(
        directory: &TestDirectory,
        missing_graph: &str,
        source_path: &std::path::Path,
        source_before: &[u8],
    ) {
        let staged_bytes = std::fs::read(directory.path.join(missing_graph)).expect("read stage");
        for suffix in ["brf", "gph", "idx"] {
            let logical = format!("data00000b.tar.{suffix}");
            assert!(
                staged_bytes
                    .windows(logical.len())
                    .any(|window| window == logical.as_bytes()),
                "staged trailers must carry the final logical basename"
            );
        }

        write_manifest(directory);
        let selected = crate::store::open_all_archives(&directory.path)
            .expect("non-active staging residue is ignored");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].file_name(), "data00000a.tar");
        assert_eq!(
            std::fs::read(source_path).expect("healthy source remains"),
            source_before
        );
        assert!(!directory.path.join("data00000b.tar").exists());
    }

    /// The two-segment source archive every staged-rewrite validation case
    /// starts from.
    struct StagedRewriteFixture {
        directory: TestDirectory,
        first: SegmentIdentifier,
        second: SegmentIdentifier,
        generation: GarbageCollectionGeneration,
        first_bytes: Vec<u8>,
        second_bytes: Vec<u8>,
        live_blob: String,
    }

    fn build_staged_rewrite_fixture() -> StagedRewriteFixture {
        let directory = TestDirectory::new("staged-semantic-validation");
        let first = data_identifier(83);
        let second = data_identifier(84);
        let generation = generation(7, 6, true);
        let first_bytes = vec![0x83; 64];
        let second_bytes = vec![0x84; 64];
        let live_blob = "live-blob".to_owned();

        let mut source_writer = TarArchiveWriter::new(&directory.path, "data00000a.tar");
        source_writer
            .write_segment(
                first,
                &first_bytes,
                generation,
                &[second],
                std::slice::from_ref(&live_blob),
            )
            .expect("write source first");
        source_writer
            .write_segment(second, &second_bytes, generation, &[], &[])
            .expect("write source second");
        source_writer.close().expect("close source");
        StagedRewriteFixture {
            directory,
            first,
            second,
            generation,
            first_bytes,
            second_bytes,
            live_blob,
        }
    }

    #[test]
    fn staged_rewrite_validation_requires_exact_trailers_and_physical_order() {
        let StagedRewriteFixture {
            directory,
            first,
            second,
            generation,
            first_bytes,
            second_bytes,
            live_blob,
        } = build_staged_rewrite_fixture();
        let source_path = directory.path.join("data00000a.tar");
        let source_before = std::fs::read(&source_path).expect("read source");
        let source = TarArchiveReader::open(&source_path).expect("open source");
        let mut survivors = source.index().expect("source index").entries().to_vec();
        survivors.sort_by_key(|entry| entry.position);

        let expected_graph = HashMap::from([(first, HashSet::from([second]))]);
        let expected_binary_references = HashMap::from([(
            (
                generation.generation,
                generation.full_generation,
                generation.is_compacted,
            ),
            HashMap::from([(first, HashSet::from([live_blob.clone()]))]),
        )]);

        let write_staged =
            |physical_name: &str, order: &[SegmentIdentifier], graph: bool, catalog: bool| {
                let mut writer = TarArchiveWriter::new_exclusive_staged(
                    &directory.path,
                    physical_name,
                    "data00000b.tar",
                );
                for identifier in order {
                    let bytes = if *identifier == first {
                        &first_bytes
                    } else {
                        &second_bytes
                    };
                    let references = if *identifier == first && graph {
                        vec![second]
                    } else {
                        Vec::new()
                    };
                    let binary_references = if *identifier == first && catalog {
                        vec![live_blob.clone()]
                    } else {
                        Vec::new()
                    };
                    writer
                        .write_segment(
                            *identifier,
                            bytes,
                            generation,
                            &references,
                            &binary_references,
                        )
                        .expect("write staged survivor");
                }
                writer.close().expect("close staged archive");
            };

        let missing_graph = "data00000b.tar.cleaning.000";
        write_staged(missing_graph, &[first, second], false, true);
        let graph_error = validate_swept_archive(
            &source,
            &directory.path.join(missing_graph),
            &survivors,
            &expected_graph,
            &expected_binary_references,
        )
        .expect_err("a valid-checksum rewrite may not omit a live graph edge");
        assert!(graph_error.to_string().contains("graph differs"));

        let missing_catalog = "data00000b.tar.cleaning.001";
        write_staged(missing_catalog, &[first, second], true, false);
        let catalog_error = validate_swept_archive(
            &source,
            &directory.path.join(missing_catalog),
            &survivors,
            &expected_graph,
            &expected_binary_references,
        )
        .expect_err("a valid-checksum rewrite may not omit a live BRF entry");
        assert!(catalog_error.to_string().contains("catalog differs"));

        let reordered = "data00000b.tar.cleaning.002";
        write_staged(reordered, &[second, first], true, true);
        let order_error = validate_swept_archive(
            &source,
            &directory.path.join(reordered),
            &survivors,
            &expected_graph,
            &expected_binary_references,
        )
        .expect_err("a semantically equal rewrite may not reorder physical segments");
        assert!(order_error.to_string().contains("physical order"));

        assert_staging_is_non_active_residue(
            &directory,
            missing_graph,
            &source_path,
            &source_before,
        );
    }
}
