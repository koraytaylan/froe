//! Proving a replacement holds exactly the survivors the plan named, in
//! the order and with the trailers the source had, before anything is
//! published or unlinked.

use super::{
    Error, ExpectedBinaryReferences, ExpectedGraph, FileAccess, Path, RegularFileIdentity, Result,
    TarArchiveReader, held_file_identity, open_regular_file_no_follow, preserve_file_metadata,
    require_held_file_identity, require_path_file_identity, validate_exact_archive_trailers,
};

/// What the staged archive is proved against before publication.
pub(crate) struct StagedValidation<'validation> {
    pub(crate) reader: &'validation TarArchiveReader,
    pub(crate) staging_path: &'validation Path,
    pub(crate) replacement_name: &'validation str,
    pub(crate) survivors: &'validation [crate::tar_archive::index::SegmentIndexEntry],
    pub(crate) expected_graph: &'validation ExpectedGraph,
    pub(crate) expected_binary_references: &'validation ExpectedBinaryReferences,
    pub(crate) source_metadata: &'validation std::fs::Metadata,
}

/// Reopens the staged archive through its own no-follow descriptor and
/// holds it to every survivor and trailer the copy was meant to write.
///
/// Returns the validated inode's identity, which publication then
/// requires the linked names to agree with.
pub(crate) fn validate_staged_replacement(
    validation: &StagedValidation<'_>,
) -> Result<RegularFileIdentity> {
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

/// What the published name still has to prove after `hard_link`.
///
/// Staging already compared every survivor payload to the source. The
/// published path is a second name for that same inode once identity holds;
/// re-reading every byte would only catch an in-place rewrite, which the
/// segment-store file protocol forbids. A substituted pathname fails the
/// identity checks instead.
#[derive(Clone, Copy)]
pub(crate) enum SurvivorPayloadProof {
    CompareWithSource,
    SameInodeAsValidatedStaging,
}

/// Reopens a swept archive and proves that every survivor's payload and
/// generation metadata exactly match the immutable source before the source
/// may be removed.
#[cfg(test)]
pub(in crate::writer::store_writer) fn validate_swept_archive(
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
pub(in crate::writer::store_writer) fn validate_open_swept_archive(
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
pub(in crate::writer::store_writer) fn validate_published_swept_archive(
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

pub(crate) fn prove_survivor_count_order_and_entries(
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
