//! Writing the replacement beside its source: the staging name a rewrite
//! claims, and the survivors copied into it.

use super::{
    ArchiveSegmentsProvider, Error, ExpectedBinaryReferences, ExpectedGraph, FilteredTrailers,
    GarbageCollectionGeneration, Path, Result, SegmentIdentifier, TarArchiveReader,
    TarArchiveWriter, UncommittedArchiveStaging,
};

pub(in crate::writer::store_writer) fn next_archive_staging_name(
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
pub(crate) struct SurvivorSources<'sources> {
    pub(crate) reader: &'sources TarArchiveReader,
    pub(crate) trailers: &'sources FilteredTrailers,
    pub(crate) previously_unavailable_graph_targets:
        &'sources std::collections::HashSet<SegmentIdentifier>,
    pub(crate) current_rewrite_targets: &'sources std::collections::HashSet<SegmentIdentifier>,
    pub(crate) scan_provider: Option<&'sources ArchiveSegmentsProvider<'sources>>,
}

/// Copies every survivor into the staged replacement, rebuilding the
/// trailers it must carry and returning what validation will hold it to.
pub(crate) fn copy_survivors(
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

#[cfg(test)]
mod tests {
    use crate::segment::identifier::SegmentIdentifier;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::store_writer::sweep::validate_swept_archive;
    use crate::writer::store_writer::test_support::*;
    use crate::writer::tar_writer::TarArchiveWriter;
    use std::collections::{HashMap, HashSet};

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
