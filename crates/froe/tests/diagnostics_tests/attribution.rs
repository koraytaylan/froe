//! Attributing content paths to one archive, and the read-only contract
//! the whole diagnostic holds to.

use super::*;

/// The bulk archive and the archive holding the binary blocks each
/// attribute the same property, one through its graph and one through
/// Oak's block-segment set.
pub(crate) fn assert_bulk_and_block_attribution(
    repository: &Repository,
    fixture: &DiagnosticFixture,
) {
    let bulk_report = debug_archive(repository, BULK_ARCHIVE).expect("bulk attribution");
    let bulk_graph = bulk_report.graph.as_ref().expect("active bulk graph");
    assert_eq!(bulk_graph.origin, ArchiveGraphOrigin::Stored);
    assert_eq!(bulk_graph.rows.len(), 3);
    assert!(bulk_graph.rows.iter().all(|row| matches!(
        row.references,
        ArchiveGraphReferences::Available(ref references) if references.is_empty()
    )));
    let matched_through_block_segment =
        bulk_report
            .references
            .iter()
            .any(|reference| match reference {
                ArchivePathReference::Property {
                    path,
                    name,
                    record_is_in_archive,
                    ..
                } if path == "/root/content/" && name == "data" => {
                    assert!(!record_is_in_archive);
                    true
                }
                _ => false,
            });
    assert!(
        matched_through_block_segment,
        "binary property is attributed through a matching block segment"
    );
    assert!(
        fixture.bulk_identifiers[3].is_data_segment(),
        "the match includes a cross-archive data-kind BLOCK segment"
    );
    assert_eq!(
        bulk_report.work.inspected_binary_blocks,
        u64::from(BINARY_BLOCK_COUNT),
        "Oak validates every block-list entry before testing set membership"
    );

    let data_block_report =
        debug_archive(repository, DATA_BLOCK_ARCHIVE).expect("data-kind block attribution");
    assert!(
        data_block_report
            .references
            .iter()
            .any(|reference| matches!(
                reference,
                ArchivePathReference::Property {
                    name,
                    record_is_in_archive: false,
                    display: ArchivePropertyDisplay::Other(display),
                    ..
                } if name == "data" && display == "[1 binaries]"
            ))
    );
    assert_eq!(
        data_block_report.work.inspected_binary_blocks,
        u64::from(BINARY_BLOCK_COUNT),
        "all block segment IDs count, including a data-kind BLOCK segment"
    );
}

#[expect(
    clippy::cognitive_complexity,
    reason = "31 assertions and no branches; the lint counts each \
              `assert!` expansion as a decision point"
)]
#[test]
pub(crate) fn segment_dump_and_archive_attribution_are_read_only_end_to_end() {
    let fixture = write_diagnostic_fixture("segment-and-archive-debug", GraphFixture::ValidEmpty);
    let before = directory_snapshot(&fixture.directory.path);
    assert!(!fixture.directory.path.join("repo.lock").exists());
    let repository = Repository::open(&fixture.directory.path).expect("open repository");

    let dump = dump_segment(&repository, fixture.data_identifier).expect("segment dump");
    assert!(dump.contains("Info: {\"wid\":\"independent\",\"sno\":1,\"t\":1}"));
    assert!(dump.contains("  TEMPLATE record 00000006:"));
    assert!(dump.contains("      NODE record 0000000b:"));
    assert!(dump.contains("00000000 30 61 4B 0D"));

    let data_report = debug_archive(&repository, DATA_ARCHIVE).expect("data attribution");
    assert_eq!(data_report.state, ArchiveDebugState::Active);
    let data_graph = data_report.graph.as_ref().expect("active graph");
    assert_eq!(data_graph.origin, ArchiveGraphOrigin::Stored);
    assert_eq!(data_graph.rows.len(), 1);
    assert_eq!(
        data_graph.rows[0].references,
        ArchiveGraphReferences::Available(Vec::new()),
        "Oak trusts a valid but semantically empty stored graph"
    );
    assert!(data_report.references.iter().any(|reference| matches!(
        reference,
        ArchivePathReference::Node { path, .. } if path == "/root/content/"
    )));
    assert!(data_report.references.iter().any(|reference| matches!(
        reference,
        ArchivePathReference::Template { path, .. } if path == "/root/content/"
    )));
    assert!(data_report.references.iter().any(|reference| matches!(
        reference,
        ArchivePathReference::Property {
            path,
            name,
            record_is_in_archive: true,
            ..
        } if path == "/root/content/" && name == "data"
    )));
    assert!(data_report.references.iter().any(|reference| matches!(
        reference,
        ArchivePathReference::Property {
            name,
            display: ArchivePropertyDisplay::Other(value),
            ..
        } if name == "count" && value == "-42"
    )));
    assert!(data_report.references.iter().any(|reference| matches!(
        reference,
        ArchivePathReference::Property {
            name,
            display: ArchivePropertyDisplay::String {
                preview_utf16,
                utf16_length,
            },
            ..
        } if name == "title"
            && String::from_utf16_lossy(preview_utf16) == "Hello \"Oak\"\n"
            && *utf16_length == 12
    )));
    assert_eq!(data_report.work.visited_nodes, 3);
    assert_eq!(data_report.work.inspected_properties, 3);
    assert_eq!(
        data_report.work.retained_path_references,
        data_report.references.len() as u64
    );
    assert!(data_report.work.retained_reference_text_bytes > 0);
    assert_eq!(
        data_report.work.inspected_binary_blocks,
        u64::from(BINARY_BLOCK_COUNT),
        "the 255/256 fan-out is traversed and the same-segment first ID is excluded"
    );

    assert_bulk_and_block_attribution(&repository, &fixture);

    drop(repository);
    assert_eq!(directory_snapshot(&fixture.directory.path), before);
    assert!(!fixture.directory.path.join("repo.lock").exists());
}

#[test]
pub(crate) fn missing_archive_is_a_typed_non_fatal_result() {
    let fixture = write_diagnostic_fixture("missing-debug-archive", GraphFixture::ValidEmpty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let report = debug_archive(&repository, "data99999a.tar").expect("missing report");

    assert_eq!(report.state, ArchiveDebugState::Missing);
    assert_eq!(report.file_size, None);
    assert!(report.references.is_empty());
    assert!(report.graph.is_none());
    assert_eq!(report.work.visited_nodes, 0);
}

#[test]
pub(crate) fn superseded_archive_is_distinct_from_a_missing_file() {
    let fixture = write_diagnostic_fixture("inactive-debug-archive", GraphFixture::ValidEmpty);
    let superseding_name = "data00000b.tar";
    std::fs::copy(
        fixture.directory.path.join(BULK_ARCHIVE),
        fixture.directory.path.join(superseding_name),
    )
    .expect("copy a newer file generation");
    let repository = Repository::open(&fixture.directory.path).expect("open repository");

    let inactive = debug_archive(&repository, BULK_ARCHIVE).expect("inactive report");
    assert_eq!(inactive.state, ArchiveDebugState::Inactive);
    assert!(inactive.file_size.is_some());
    assert!(inactive.references.is_empty());
    let active = debug_archive(&repository, superseding_name).expect("active report");
    assert_eq!(active.state, ArchiveDebugState::Active);
}

#[test]
pub(crate) fn repeated_binary_array_block_segments_produce_one_oak_set_reference() {
    let fixture =
        write_diagnostic_fixture("repeated-block-segment-set", GraphFixture::RepeatedList);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let report = debug_archive(&repository, BULK_ARCHIVE).expect("bulk attribution");
    let matching_properties: Vec<_> = report
        .references
        .iter()
        .filter(|reference| {
            matches!(
                reference,
                ArchivePathReference::Property {
                    name,
                    display: ArchivePropertyDisplay::Other(display),
                    ..
                } if name == "data" && display == "[1 binaries]"
            )
        })
        .collect();

    assert_eq!(matching_properties.len(), 1, "Oak localPaths is a set");
    assert_eq!(
        report.work.inspected_binary_blocks,
        u64::from(BINARY_BLOCK_COUNT),
        "repeated IDs deduplicate attribution without hiding corrupt tail entries"
    );
}

#[test]
pub(crate) fn archive_argument_is_a_file_name_not_an_escape_path() {
    let fixture = write_diagnostic_fixture("debug-name-scope", GraphFixture::ValidEmpty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    assert!(debug_archive(&repository, "../data00000a.tar").is_err());
    assert!(debug_archive(&repository, "not-an-archive.tar").is_err());
}

#[test]
pub(crate) fn archive_debug_rejects_an_oversized_indexed_segment() {
    let directory = TestDirectory::new("debug-oversized-indexed-segment");
    let data_uuid = data_segment_uuid(0x501);
    let mut oversized_segment = vec![0u8; MAXIMUM_SEGMENT_SIZE + 1];
    oversized_segment[0..3].copy_from_slice(b"0aK");
    oversized_segment[3] = 13;
    oversized_segment[4..8].copy_from_slice(&(1u32 | 0x8000_0000).to_be_bytes());
    oversized_segment[10..14].copy_from_slice(&1u32.to_be_bytes());

    let mut archive = ArchiveBuilder::new();
    archive.add_segment(data_uuid, oversized_segment);
    write_repository(
        &directory.path,
        &[(DATA_ARCHIVE.to_owned(), archive.build(DATA_ARCHIVE))],
        &[format!("{}:0 root 1", format_uuid(data_uuid))],
    );
    let repository = Repository::open(&directory.path).expect("open indexed repository");
    assert!(repository.archives()[0].index().is_some());

    let error = debug_archive(&repository, DATA_ARCHIVE).expect_err("oversized segment");
    assert!(matches!(
        error,
        ArchiveDebugError::Repository(froe::Error::InvalidFormat { details })
            if details.contains("262145 bytes")
                && details.contains("262144-byte format limit")
    ));
}
