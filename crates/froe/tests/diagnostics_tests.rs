//! End-to-end diagnostic tests over repositories written by the independent
//! test encoder. These fixtures prove both path attribution across archive
//! boundaries and the strict read-only contract.

#![allow(
    dead_code,
    reason = "the shared independent encoder exposes fixtures used by other integration tests"
)]
#![allow(
    unreachable_pub,
    reason = "test binaries have no external interface; pub only means module-visible"
)]

mod support;

use std::collections::BTreeMap;

use froe::PropertyType;
use froe::segment::identifier::SegmentIdentifier;
use froe::store::Repository;
use froe::tooling::{
    ArchiveDebugError, ArchiveDebugOptions, ArchiveDebugState, ArchiveGraphOrigin,
    ArchiveGraphReferences, ArchivePathReference, ArchivePropertyDisplay, debug_archive,
    debug_archive_with_options, dump_segment,
};
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, sort_properties_for_template,
};
use froe::writer::store_writer::WritableRepository;
use support::{
    ArchiveBuilder, SegmentBuilder, TYPE_LIST_BUCKET, TYPE_NODE, TYPE_TEMPLATE, TYPE_VALUE,
    TestDirectory, data_segment_uuid, format_uuid, record_identifier_bytes, string_record,
    write_repository,
};

const DATA_ARCHIVE: &str = "data00001a.tar";
const BULK_ARCHIVE: &str = "data00000a.tar";
const BINARY_BLOCK_COUNT: u32 = 256;
const BINARY_LENGTH: usize = BINARY_BLOCK_COUNT as usize * 4_096;

struct DiagnosticFixture {
    directory: TestDirectory,
    data_identifier: SegmentIdentifier,
    bulk_identifiers: [SegmentIdentifier; 4],
}

#[derive(Clone, Copy)]
enum GraphFixture {
    ValidEmpty,
    Corrupt,
    Missing,
}

/// Builds this super-root and deliberately stores the binary blocks in a
/// different archive from every node, template, and property record:
///
/// ```text
/// /root/content  data = BINARY(1 MiB), count = -42, title = STRING
/// ```
#[allow(
    clippy::too_many_lines,
    reason = "one linear independent byte fixture keeps every record relationship auditable"
)]
fn write_diagnostic_fixture(test_name: &str, graph_fixture: GraphFixture) -> DiagnosticFixture {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x101);
    let bulk_uuids = [
        (0x202, 0xB000_0000_0000_0202),
        (0x203, 0xB000_0000_0000_0203),
        (0x204, 0xB000_0000_0000_0204),
        (0x205, 0xB000_0000_0000_0205),
    ];
    let mut data = SegmentBuilder::new(data_uuid);
    let bulk_references: Vec<u16> = bulk_uuids
        .iter()
        .map(|uuid| data.add_referenced_segment(*uuid))
        .collect();

    // Record zero is the segment-info convention used by SegmentDump.
    data.add_record(
        0,
        TYPE_VALUE,
        string_record("{\"wid\":\"independent\",\"sno\":1,\"t\":1}"),
    );
    data.add_record(1, TYPE_VALUE, string_record("root"));
    data.add_record(2, TYPE_VALUE, string_record("content"));
    data.add_record(3, TYPE_VALUE, string_record("data"));
    data.add_record(12, TYPE_VALUE, string_record("count"));
    data.add_record(13, TYPE_VALUE, string_record("title"));
    data.add_record(14, TYPE_VALUE, string_record("-42"));
    data.add_record(15, TYPE_VALUE, string_record("Hello \"Oak\"\n"));

    // Single-child templates for the super-root and content root.
    let mut super_root_template = 0u32.to_be_bytes().to_vec();
    super_root_template.extend(record_identifier_bytes(0, 1));
    data.add_record(4, TYPE_TEMPLATE, super_root_template);
    let mut root_template = 0u32.to_be_bytes().to_vec();
    root_template.extend(record_identifier_bytes(0, 2));
    data.add_record(5, TYPE_TEMPLATE, root_template);

    // /content has zero children and three single-valued properties. Names
    // follow Oak's signed-hash order: data, count, title.
    let mut property_names = Vec::new();
    for record_number in [3, 12, 13] {
        property_names.extend(record_identifier_bytes(0, record_number));
    }
    data.add_record(16, TYPE_LIST_BUCKET, property_names);
    let mut content_template = ((1u32 << 29) | 3).to_be_bytes().to_vec();
    content_template.extend(record_identifier_bytes(0, 16));
    content_template.extend([2, 3, 1]); // BINARY, LONG, STRING
    data.add_record(6, TYPE_TEMPLATE, content_template);

    // A 256-element list crosses the 255-way list-bucket boundary: record
    // 20 holds the first 255 block identifiers, while top bucket 7 points
    // to record 20 and directly to the final element. Four full bulk
    // segments each hold 64 blocks, so their virtual record numbers are
    // ordinary byte offsets.
    let block_identifier = |block_index: u32| {
        let segment_index = (block_index / 64) as usize;
        record_identifier_bytes(bulk_references[segment_index], (block_index % 64) * 4_096)
    };
    let mut first_bucket = Vec::new();
    for block_index in 0..255 {
        first_bucket.extend(block_identifier(block_index));
    }
    data.add_record(20, TYPE_LIST_BUCKET, first_bucket);
    let mut top_bucket = record_identifier_bytes(0, 20);
    top_bucket.extend(block_identifier(255));
    data.add_record(7, TYPE_LIST_BUCKET, top_bucket);
    let stored_length = 0xC000_0000_0000_0000u64 | (BINARY_LENGTH as u64 - 16_512);
    let mut binary_value = stored_length.to_be_bytes().to_vec();
    binary_value.extend(record_identifier_bytes(0, 7));
    data.add_record(8, TYPE_VALUE, binary_value);

    let mut property_values = Vec::new();
    for record_number in [8, 14, 15] {
        property_values.extend(record_identifier_bytes(0, record_number));
    }
    data.add_record(17, TYPE_LIST_BUCKET, property_values);

    let node_record = |record_number: u32, template: u32, extra: Option<u32>| {
        let mut bytes = record_identifier_bytes(0, record_number);
        bytes.extend(record_identifier_bytes(0, template));
        if let Some(extra) = extra {
            bytes.extend(record_identifier_bytes(0, extra));
        }
        bytes
    };
    data.add_record(9, TYPE_NODE, node_record(9, 6, Some(17)));
    data.add_record(10, TYPE_NODE, node_record(10, 5, Some(9)));
    data.add_record(11, TYPE_NODE, node_record(11, 4, Some(10)));

    let mut bulk_archive = ArchiveBuilder::new();
    for bulk_uuid in bulk_uuids {
        bulk_archive.add_segment(bulk_uuid, vec![0x5a; 262_144]);
    }
    let mut data_archive = if matches!(graph_fixture, GraphFixture::Missing) {
        ArchiveBuilder::new().without_index()
    } else {
        ArchiveBuilder::new()
    };
    data_archive.add_segment(data_uuid, data.build());
    let mut data_archive_bytes = data_archive.build(DATA_ARCHIVE);
    if matches!(graph_fixture, GraphFixture::Corrupt) {
        let graph_magic = 0x0A30_470Au32.to_be_bytes();
        let magic_position = data_archive_bytes
            .windows(graph_magic.len())
            .rposition(|window| window == graph_magic)
            .expect("independent archive contains graph footer");
        // Damage the stored graph checksum, leaving the segment and index
        // untouched so the repository and attribution still open.
        data_archive_bytes[magic_position - 12] ^= 0x01;
    }

    write_repository(
        &directory.path,
        &[
            (BULK_ARCHIVE.to_owned(), bulk_archive.build(BULK_ARCHIVE)),
            (DATA_ARCHIVE.to_owned(), data_archive_bytes),
        ],
        &[format!("{}:11 root 1", format_uuid(data_uuid))],
    );
    DiagnosticFixture {
        directory,
        data_identifier: SegmentIdentifier::new(data_uuid.0, data_uuid.1),
        bulk_identifiers: bulk_uuids.map(|uuid| SegmentIdentifier::new(uuid.0, uuid.1)),
    }
}

fn directory_snapshot(path: &std::path::Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(path)
        .expect("read directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name(),
                std::fs::read(entry.path()).expect("read file"),
            )
        })
        .collect()
}

fn write_wide_production_fixture(directory: &std::path::Path, property_count: usize) {
    let store = WritableRepository::open(directory).expect("open writable repository");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let mut properties = Vec::with_capacity(property_count);
    for property_index in 0..property_count {
        let value = writer
            .write_string(&format!("value-{property_index}"))
            .expect("property value");
        properties.push(PropertyToWrite {
            name: format!("property-{property_index:04}"),
            property_type: PropertyType::String,
            values: PropertyValuesToWrite::Single(value),
        });
    }
    sort_properties_for_template(&mut properties);
    let content = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)
        .expect("wide content node");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "content".to_owned(),
                node: content,
            },
            &[],
        )
        .expect("content root");
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &[],
        )
        .expect("super root");
    writer.finish().expect("finish writer");
    assert!(store.set_head(store.head(), head));
    store.close().expect("close writer");
}

#[test]
fn segment_dump_and_archive_attribution_are_read_only_end_to_end() {
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
            display: ArchivePropertyDisplay::String(value),
            ..
        } if name == "title" && value == "Hello \"Oak\"\n"
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
        "the 255/256 list fan-out boundary is traversed exactly once"
    );

    let bulk_report = debug_archive(&repository, BULK_ARCHIVE).expect("bulk attribution");
    let bulk_graph = bulk_report.graph.as_ref().expect("active bulk graph");
    assert_eq!(bulk_graph.origin, ArchiveGraphOrigin::Stored);
    assert_eq!(bulk_graph.rows.len(), fixture.bulk_identifiers.len());
    assert!(bulk_graph.rows.iter().all(|row| matches!(
        row.references,
        ArchiveGraphReferences::Available(ref references) if references.is_empty()
    )));
    let matching_block_count = bulk_report
        .references
        .iter()
        .find_map(|reference| match reference {
            ArchivePathReference::Property {
                path,
                name,
                record_is_in_archive,
                binary_bulk_block_count,
                ..
            } if path == "/root/content/" && name == "data" => {
                assert!(!record_is_in_archive);
                Some(binary_bulk_block_count)
            }
            _ => None,
        })
        .expect("binary property attributed through its bulk blocks");
    assert_eq!(*matching_block_count, u64::from(BINARY_BLOCK_COUNT));
    assert_eq!(
        bulk_report.work.inspected_binary_blocks,
        u64::from(BINARY_BLOCK_COUNT)
    );

    drop(repository);
    assert_eq!(directory_snapshot(&fixture.directory.path), before);
    assert!(!fixture.directory.path.join("repo.lock").exists());
}

#[test]
fn missing_archive_is_a_typed_non_fatal_result() {
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
fn superseded_archive_is_distinct_from_a_missing_file() {
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
fn corrupt_graph_is_reconstructed_and_does_not_hide_content_attribution() {
    let fixture = write_diagnostic_fixture("corrupt-debug-graph", GraphFixture::Corrupt);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");

    assert_eq!(report.state, ArchiveDebugState::Active);
    let graph = report.graph.as_ref().expect("reconstructed graph");
    assert_eq!(graph.origin, ArchiveGraphOrigin::Reconstructed);
    assert_eq!(graph.rows.len(), 1);
    assert_eq!(graph.rows[0].segment_identifier, fixture.data_identifier);
    assert_eq!(
        graph.rows[0].references,
        ArchiveGraphReferences::Available(fixture.bulk_identifiers.to_vec())
    );
    assert!(
        report
            .references
            .iter()
            .any(|reference| matches!(reference, ArchivePathReference::Node { .. })),
        "path scan remains useful even when the optional graph is corrupt"
    );
}

#[test]
fn missing_graph_is_reconstructed_from_recovered_archive_bytes() {
    let fixture = write_diagnostic_fixture("missing-debug-graph", GraphFixture::Missing);
    let before = directory_snapshot(&fixture.directory.path);
    let repository = Repository::open(&fixture.directory.path).expect("open recovered repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");

    let graph = report.graph.as_ref().expect("reconstructed graph");
    assert_eq!(graph.origin, ArchiveGraphOrigin::Reconstructed);
    assert_eq!(graph.rows.len(), 1);
    assert_eq!(
        graph.rows[0].references,
        ArchiveGraphReferences::Available(fixture.bulk_identifiers.to_vec())
    );
    drop(repository);
    assert_eq!(directory_snapshot(&fixture.directory.path), before);
    assert!(!fixture.directory.path.join("repo.lock").exists());
}

#[test]
fn wide_production_tree_hits_typed_result_budget_before_retention_grows_unbounded() {
    let directory = TestDirectory::new("wide-debug-budget");
    write_wide_production_fixture(&directory.path, 128);
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
    let before = directory_snapshot(&directory.path);
    let repository = Repository::open(&directory.path).expect("open repository");
    let archive_file_name = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(repository.head_record_identifier().segment))
        .expect("head archive")
        .file_name()
        .to_owned();

    let error = debug_archive_with_options(
        &repository,
        &archive_file_name,
        ArchiveDebugOptions {
            maximum_path_references: 64,
            maximum_reference_text_bytes: usize::MAX,
        },
    )
    .expect_err("wide result must stop at the configured limit");
    assert!(matches!(
        error,
        ArchiveDebugError::ResultBudgetExceeded {
            maximum_path_references: 64,
            attempted_path_references: 65,
            ..
        }
    ));

    let text_error = debug_archive_with_options(
        &repository,
        &archive_file_name,
        ArchiveDebugOptions {
            maximum_path_references: usize::MAX,
            maximum_reference_text_bytes: 0,
        },
    )
    .expect_err("retained text has an independent limit");
    assert!(matches!(
        text_error,
        ArchiveDebugError::ResultBudgetExceeded {
            maximum_reference_text_bytes: 0,
            attempted_path_references: 1,
            attempted_reference_text_bytes: 1..,
            ..
        }
    ));

    drop(repository);
    assert_eq!(directory_snapshot(&directory.path), before);
    assert!(!directory.path.join("repo.lock").exists());
}

#[test]
fn archive_argument_is_a_file_name_not_an_escape_path() {
    let fixture = write_diagnostic_fixture("debug-name-scope", GraphFixture::ValidEmpty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    assert!(debug_archive(&repository, "../data00000a.tar").is_err());
    assert!(debug_archive(&repository, "not-an-archive.tar").is_err());
}
