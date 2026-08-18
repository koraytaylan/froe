//! Making the replacement the active archive: certify it where it now
//! lives, publish atomically, then retire the source.

use super::{
    ArchiveSweepDisposition, ArchiveSweepOutcome, DeferredFileDeletion, Error,
    ExpectedBinaryReferences, ExpectedGraph, FileAccess, Path, RegularFileIdentity, Result,
    SegmentIdentifier, StagedValidation, TarArchiveReader, UncommittedArchiveStaging,
    open_regular_file_no_follow, path_object_identity, remove_published_link_if_same,
    require_held_file_identity, require_path_file_identity, sync_directory_strict,
    validate_published_swept_archive, validate_staged_replacement,
};

/// Everything the publication phase reads to prove the staged archive
/// and swap it in for the source.
pub(crate) struct SweepPublication<'publication> {
    pub(crate) directory: &'publication Path,
    pub(crate) source_path: &'publication Path,
    pub(crate) reader: &'publication TarArchiveReader,
    pub(crate) source_file: &'publication std::fs::File,
    pub(crate) source_identity: RegularFileIdentity,
    pub(crate) staging_name: &'publication str,
    pub(crate) staging_path: &'publication Path,
    pub(crate) replacement_name: &'publication str,
    pub(crate) replacement_path: &'publication Path,
    pub(crate) survivors: &'publication [crate::tar_archive::index::SegmentIndexEntry],
    pub(crate) expected_graph: &'publication ExpectedGraph,
    pub(crate) expected_binary_references: &'publication ExpectedBinaryReferences,
    pub(crate) source_metadata: &'publication std::fs::Metadata,
    pub(crate) current_rewrite_targets: std::collections::HashSet<SegmentIdentifier>,
}

/// Validates the staged archive, publishes it under the replacement
/// name, and only then unlinks the source.
///
/// The order is the safety argument: nothing the source holds is removed
/// until a complete, independently reopened copy of every survivor is
/// durable under its final name.
pub(crate) fn publish_swept_archive(
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
    crate::writer::fault_injection::fail_if_armed("sweep.before-publish-link")?;
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
    crate::writer::fault_injection::fail_if_armed("sweep.after-publish-link")?;
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("sweep.before-publish-directory-sync")?;
    sync_directory_strict(directory)?;
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("sweep.after-publish-directory-sync")?;
    #[cfg(test)]
    crate::writer::fault_injection::crash_if_armed("sweep.published-before-source-unlink");
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

/// The replacement, once the hard link that published it exists.
pub(crate) struct PublishedReplacement<'published> {
    pub(crate) directory: &'published Path,
    pub(crate) replacement_path: &'published Path,
    pub(crate) replacement_name: &'published str,
    pub(crate) staging_path: &'published Path,
    pub(crate) staging_identity: RegularFileIdentity,
    pub(crate) reader: &'published TarArchiveReader,
    pub(crate) survivors: &'published [crate::tar_archive::index::SegmentIndexEntry],
    pub(crate) expected_graph: &'published ExpectedGraph,
    pub(crate) expected_binary_references: &'published ExpectedBinaryReferences,
}

/// Proves the two names the publication linked still identify the inode
/// validation certified.
///
/// A pathname substitution in the narrow interval between the pre-link
/// identity check and `hard_link` must not turn an arbitrary inode into
/// the higher active archive generation. If the two names agree with each
/// other but not the validated descriptor, they still identify the link
/// this process just published and can be removed safely.
pub(crate) fn certify_published_replacement(
    published: &PublishedReplacement<'_>,
) -> Result<std::fs::File> {
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

/// The source archive a published rewrite supersedes.
pub(crate) struct RetiredSource<'retired> {
    pub(crate) directory: &'retired Path,
    pub(crate) replacement_path: &'retired Path,
    pub(crate) replacement_file: &'retired std::fs::File,
    pub(crate) staging_identity: RegularFileIdentity,
    pub(crate) path: &'retired Path,
    pub(crate) reader: &'retired TarArchiveReader,
    pub(crate) source_file: &'retired std::fs::File,
    pub(crate) source_identity: RegularFileIdentity,
    pub(crate) staging_name: &'retired str,
    pub(crate) staging_path: &'retired Path,
    pub(crate) current_rewrite_targets: std::collections::HashSet<SegmentIdentifier>,
}

/// Removes the staging alias and then the source, once the replacement
/// is published and durable.
///
/// Each unlink is guarded by the identity the source was certified with,
/// so a path substituted underneath this run removes nothing.
pub(crate) fn retire_swept_source(retired: RetiredSource<'_>) -> Result<ArchiveSweepOutcome> {
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
    crate::writer::fault_injection::fail_if_armed("sweep.before-staging-unlink")?;
    if let Err(error) = std::fs::remove_file(staging_path) {
        deletion_failures.push(DeferredFileDeletion {
            file_name: staging_name.to_owned(),
            error: error.to_string(),
            target_was_already_absent: error.kind() == std::io::ErrorKind::NotFound,
        });
    }
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("sweep.after-staging-unlink")?;
    #[cfg(test)]
    crate::writer::fault_injection::crash_if_armed("sweep.staging-unlinked-before-source-unlink");
    // Deletion failures are consistency-safe: the published higher letter
    // wins and preserves every survivor; the old source is reported later.
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("sweep.before-source-unlink")?;
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
    crate::writer::fault_injection::fail_if_armed("sweep.after-source-unlink")?;
    #[cfg(test)]
    crate::writer::fault_injection::crash_if_armed("sweep.source-unlinked");
    Ok(ArchiveSweepOutcome {
        disposition: ArchiveSweepDisposition::Rewritten,
        deletion_failures,
        newly_unavailable: current_rewrite_targets,
    })
}

#[cfg(test)]
mod tests {
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::parsed_segment::ParsedSegment;
    use crate::segment::record::RecordType;
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::tar_archive::file_name::ArchiveFileName;
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::store_writer::archive_certificate::*;
    use crate::writer::store_writer::reclaim::*;
    use crate::writer::store_writer::repository::*;
    use crate::writer::store_writer::sweep::sweep_one_archive;
    use crate::writer::store_writer::sweep_plan::*;
    use crate::writer::store_writer::test_support::*;
    use crate::writer::tar_writer::TarArchiveWriter;
    use std::collections::HashSet;
    use std::path::Path;

    /// Rebuilds `source` under `swap_name` as a byte-valid archive with the
    /// same UUIDs, index layout, generations, graph, and stale binary-reference
    /// catalog. Only the inline blob identifier changes, at equal length, so the
    /// sweep plan remains unchanged while the segment-entry CRC is recomputed by
    /// the writer. Returns the bytes of the archive it wrote.
    #[cfg(unix)]
    fn rebuild_source_with_swapped_blob(
        directory: &Path,
        source: &TarArchiveReader,
        swap_name: &str,
        source_name: &str,
        blob_segment: SegmentIdentifier,
        original_blob: &[u8],
        swapped_blob: &[u8],
    ) -> Vec<u8> {
        let mut swapped_writer =
            TarArchiveWriter::new_exclusive_staged(directory, swap_name, source_name);
        copy_binary_reference_catalog(&mut swapped_writer, source);
        let mut entries = source.index().expect("source index").entries().to_vec();
        entries.sort_by_key(|entry| entry.position);
        let mut changed_blob = false;
        for entry in &entries {
            let identifier = entry.segment_identifier;
            let mut bytes = source
                .segment_data(identifier)
                .expect("indexed source payload")
                .to_vec();
            let structure = ParsedSegment::parse(identifier, &bytes).expect("source segment");
            if identifier == blob_segment {
                swap_inline_blob_identifier(&mut bytes, &structure, original_blob, swapped_blob);
                changed_blob = true;
            }
            let changed_structure =
                ParsedSegment::parse(identifier, &bytes).expect("same-layout changed segment");
            swapped_writer
                .write_segment(
                    identifier,
                    &bytes,
                    GarbageCollectionGeneration {
                        generation: entry.generation,
                        full_generation: entry.full_generation,
                        is_compacted: entry.is_compacted,
                    },
                    &changed_structure.referenced_segments,
                    &[],
                )
                .expect("write changed source entry");
        }
        assert!(changed_blob, "the fixture must change one blob identifier");
        swapped_writer.close().expect("close changed source");
        std::fs::read(directory.join(swap_name)).expect("read changed source")
    }

    /// Reproduces `source`'s binary-reference catalog in `writer` verbatim, so
    /// the rebuilt archive carries the same stale entries the original did.
    #[cfg(unix)]
    fn copy_binary_reference_catalog(writer: &mut TarArchiveWriter, source: &TarArchiveReader) {
        for catalog_generation in source
            .binary_references()
            .expect("source binary-reference catalog")
            .generations
        {
            let catalog_gc_generation = GarbageCollectionGeneration {
                generation: catalog_generation.generation,
                full_generation: catalog_generation.full_generation,
                is_compacted: catalog_generation.is_compacted,
            };
            for (identifier, references) in catalog_generation.segments {
                writer.add_binary_references(catalog_gc_generation, identifier, references);
            }
        }
    }

    /// Overwrites the one inline (`0xE0`-class) external-blob identifier in
    /// `bytes` with `swapped_blob`, asserting the record is the shape the
    /// fixture depends on and that the replacement is the same length.
    #[cfg(unix)]
    fn swap_inline_blob_identifier(
        bytes: &mut [u8],
        structure: &ParsedSegment,
        original_blob: &[u8],
        swapped_blob: &[u8],
    ) {
        let external = structure
            .record_table()
            .iter()
            .find(|record| record.record_type() == Some(RecordType::ExternalBlobIdentifier))
            .expect("inline external-blob record");
        let position = structure
            .buffer_position(external.offset)
            .expect("external-blob record position");
        let encoded_length = u16::from_be_bytes([bytes[position], bytes[position + 1]]);
        assert_eq!(encoded_length & 0xF000, 0xE000);
        assert_eq!(usize::from(encoded_length & 0x0FFF), original_blob.len());
        assert_eq!(
            &bytes[position + 2..position + 2 + original_blob.len()],
            original_blob
        );
        bytes[position + 2..position + 2 + swapped_blob.len()].copy_from_slice(swapped_blob);
    }

    #[cfg(unix)]
    #[test]
    fn immediate_source_certificate_uses_the_reopened_blob_payload() {
        const ORIGINAL_BLOB: &[u8] = b"live-external-blob";
        const SWAPPED_BLOB: &[u8] = b"evil-external-blob";
        assert_eq!(ORIGINAL_BLOB.len(), SWAPPED_BLOB.len());

        let directory = TestDirectory::new("reopened-source-provider");
        let blob_segment = {
            let store = WritableRepository::open(&directory.path).expect("bootstrap writer");
            let previous = store.head();
            let write_generation = store.writing_generation().expect("write generation");
            let (head, child) = write_session_semantic_fixture(&store, write_generation);
            assert!(store.compare_and_set_head(previous, head));
            store.close().expect("close blob-bearing source");
            child.segment
        };

        // Keep this repository open across the path replacement. It models the
        // complete provider captured before an actionable source is reopened.
        let stale_repository = Repository::open(&directory.path).expect("open original mapping");
        let source = stale_repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(blob_segment))
            .expect("archive containing blob segment");
        certify_active_archive(&stale_repository, source).expect("original source is certified");
        let source_name = source.file_name().to_owned();
        let source_path = directory.path.join(&source_name);
        let swap_name = format!("{source_name}.swapped");
        let swap_path = directory.path.join(&swap_name);

        let swapped_bytes = rebuild_source_with_swapped_blob(
            &directory.path,
            source,
            &swap_name,
            &source_name,
            blob_segment,
            ORIGINAL_BLOB,
            SWAPPED_BLOB,
        );
        std::fs::rename(&swap_path, &source_path).expect("replace source pathname");

        let reopened = TarArchiveReader::open(&source_path).expect("reopen changed source");
        // The certificate reconstructs from the payload bytes of the archive
        // it was handed, so an inline (`0xE0`-class) identifier is caught
        // whatever provider is passed — a stale one no longer masks it. The
        // provider still resolves every UUID the segment *references*, which
        // is what the reopened-source shadowing below remains needed for.
        let stale_provider_error = certify_active_archive(&stale_repository, &reopened)
            .expect_err("the reopened payload is certified against itself, not the stale mapping");
        assert!(
            stale_provider_error.to_string().contains("catalog differs"),
            "{stale_provider_error}"
        );

        let cleaned: HashSet<_> = source
            .segment_identifiers()
            .filter(|identifier| *identifier != blob_segment)
            .collect();
        assert!(
            !cleaned.is_empty(),
            "the fixture must request a partial rewrite"
        );
        assert!(matches!(
            plan_archive_sweep(
                &directory.path,
                source,
                &cleaned,
                ArchiveRewritePolicy::default(),
                &std::collections::HashSet::new(),
            )
            .expect("source sweep plan"),
            Some(PlannedArchiveSweep::Rewrite { .. })
        ));
        let mut fallback_provider = None;
        let error = sweep_one_archive(
            &directory.path,
            source,
            &cleaned,
            &cleaned,
            &[source],
            &mut fallback_provider,
            Some(&stale_repository),
            ArchiveRewritePolicy::default(),
        )
        .expect_err("fresh source payload must invalidate its stale BRF before publication");

        assert!(error.to_string().contains("catalog differs"), "{error}");
        assert_eq!(
            std::fs::read(&source_path).expect("changed source remains"),
            swapped_bytes
        );
        let parsed_name = ArchiveFileName::parse(&source_name).expect("source archive name");
        let next_generation = char::from(parsed_name.file_generation as u8 + 1);
        assert!(
            !directory
                .path
                .join(format!(
                    "data{:05}{next_generation}.tar",
                    parsed_name.archive_number
                ))
                .exists(),
            "no replacement may be published after the fresh certificate fails"
        );
    }
}
