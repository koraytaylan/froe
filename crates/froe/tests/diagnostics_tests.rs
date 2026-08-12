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

use froe::PropertyType;
use froe::segment::{MAXIMUM_SEGMENT_SIZE, identifier::SegmentIdentifier};
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
use support::filesystem_snapshot::directory_snapshot;
use support::{
    ArchiveBuilder, SegmentBuilder, TYPE_LIST, TYPE_LIST_BUCKET, TYPE_MAP_BRANCH, TYPE_MAP_LEAF,
    TYPE_NODE, TYPE_TEMPLATE, TYPE_VALUE, TestDirectory, data_segment_uuid, format_uuid,
    independent_map_entry_hash, record_identifier_bytes, string_record, write_repository,
};

const DATA_ARCHIVE: &str = "data00001a.tar";
const BULK_ARCHIVE: &str = "data00000a.tar";
const DATA_BLOCK_ARCHIVE: &str = "data00002a.tar";
const BINARY_BLOCK_COUNT: u32 = 256;
const BINARY_LENGTH: usize = BINARY_BLOCK_COUNT as usize * 4_096;

struct DiagnosticFixture {
    directory: TestDirectory,
    data_identifier: SegmentIdentifier,
    bulk_identifiers: [SegmentIdentifier; 4],
    graph_empty_identifier: Option<SegmentIdentifier>,
    invalid_graph_identifier: Option<SegmentIdentifier>,
}

#[derive(Clone, Copy)]
enum GraphFixture {
    ValidEmpty,
    ValidNonempty,
    HostileReusedList,
    RepeatedList,
    HostileChildName,
    HostileTemplateName,
    Corrupt,
    CorruptWithInvalidHeader,
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
        // Oak's getBulkSegmentIds does not filter by segment kind. This
        // data-kind segment deliberately stores records used as BLOCKs.
        (0x205, 0xA000_0000_0000_0205),
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
    data.add_record(
        1,
        TYPE_VALUE,
        if matches!(graph_fixture, GraphFixture::HostileChildName) {
            0xbfffu16.to_be_bytes().to_vec()
        } else {
            string_record("root")
        },
    );
    data.add_record(2, TYPE_VALUE, string_record("content"));
    data.add_record(
        3,
        TYPE_VALUE,
        if matches!(graph_fixture, GraphFixture::HostileTemplateName) {
            0xbfffu16.to_be_bytes().to_vec()
        } else {
            string_record("data")
        },
    );
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

    // /content has zero children and three properties. Names follow Oak's
    // signed-hash order: data, count, title. `data` is a one-value BINARY
    // array so attribution exercises the counted-list production path.
    let mut property_names = Vec::new();
    for record_number in [3, 12, 13] {
        property_names.extend(record_identifier_bytes(0, record_number));
    }
    data.add_record(16, TYPE_LIST_BUCKET, property_names);
    let mut content_template = ((1u32 << 29) | 3).to_be_bytes().to_vec();
    content_template.extend(record_identifier_bytes(0, 16));
    content_template.extend([(-2i8) as u8, 3, 1]); // BINARIES, LONG, STRING
    data.add_record(6, TYPE_TEMPLATE, content_template);

    // A 256-element list crosses the 255-way list-bucket boundary: record
    // 20 holds the first 255 block identifiers, while top bucket 7 points
    // to record 20 and directly to the final element. Four full bulk
    // segments each hold 64 blocks, so their virtual record numbers are
    // ordinary byte offsets.
    let block_identifier = |block_index: u32| {
        if matches!(
            graph_fixture,
            GraphFixture::HostileReusedList | GraphFixture::RepeatedList
        ) {
            record_identifier_bytes(bulk_references[0], 0)
        } else if block_index == 0 {
            // A block identifier in the property record's own segment does
            // not attribute the property through Oak's block-segment set.
            record_identifier_bytes(0, 0)
        } else {
            let segment_index = (block_index / 64) as usize;
            record_identifier_bytes(bulk_references[segment_index], (block_index % 64) * 4_096)
        }
    };
    let mut first_bucket = Vec::new();
    let first_bucket_entries = if matches!(graph_fixture, GraphFixture::HostileReusedList) {
        11
    } else {
        255
    };
    for block_index in 0..first_bucket_entries {
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

    let mut binary_values = 1u32.to_be_bytes().to_vec();
    binary_values.extend(record_identifier_bytes(0, 8));
    data.add_record(18, TYPE_LIST, binary_values);

    let mut property_values = Vec::new();
    for record_number in [18, 14, 15] {
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
    for bulk_uuid in &bulk_uuids[..3] {
        bulk_archive.add_segment(*bulk_uuid, vec![0x5a; 262_144]);
    }
    let mut data_block_archive = ArchiveBuilder::new();
    for bulk_uuid in &bulk_uuids[3..] {
        data_block_archive.add_segment(*bulk_uuid, vec![0x5a; 262_144]);
    }
    let graph_empty_uuid = data_segment_uuid(0x106);
    let mut data_archive = if matches!(graph_fixture, GraphFixture::Missing) {
        ArchiveBuilder::new().without_index()
    } else if matches!(graph_fixture, GraphFixture::ValidNonempty) {
        ArchiveBuilder::new().with_graph(vec![
            (data_uuid, vec![bulk_uuids[2], bulk_uuids[0], bulk_uuids[2]]),
            // SegmentGraph.parse uses Map.put: this duplicate source row
            // replaces the preceding one, while its targets remain a set.
            (data_uuid, vec![bulk_uuids[3], bulk_uuids[1], bulk_uuids[3]]),
        ])
    } else {
        ArchiveBuilder::new()
    };
    data_archive.add_segment(data_uuid, data.build());
    let graph_empty_identifier = if matches!(graph_fixture, GraphFixture::ValidNonempty) {
        data_archive.add_segment(
            graph_empty_uuid,
            SegmentBuilder::new(graph_empty_uuid).build(),
        );
        Some(SegmentIdentifier::new(
            graph_empty_uuid.0,
            graph_empty_uuid.1,
        ))
    } else {
        None
    };
    let invalid_graph_uuid = data_segment_uuid(0x107);
    let invalid_graph_identifier =
        if matches!(graph_fixture, GraphFixture::CorruptWithInvalidHeader) {
            let mut invalid_header = vec![0u8; 32];
            invalid_header[0..3].copy_from_slice(b"0aK");
            invalid_header[3] = 13;
            invalid_header[14..18].copy_from_slice(&u32::MAX.to_be_bytes());
            data_archive.add_segment(invalid_graph_uuid, invalid_header);
            Some(SegmentIdentifier::new(
                invalid_graph_uuid.0,
                invalid_graph_uuid.1,
            ))
        } else {
            None
        };
    let mut data_archive_bytes = data_archive.build(DATA_ARCHIVE);
    if matches!(
        graph_fixture,
        GraphFixture::Corrupt | GraphFixture::CorruptWithInvalidHeader
    ) {
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
            (
                DATA_BLOCK_ARCHIVE.to_owned(),
                data_block_archive.build(DATA_BLOCK_ARCHIVE),
            ),
        ],
        &[format!("{}:11 root 1", format_uuid(data_uuid))],
    );
    DiagnosticFixture {
        directory,
        data_identifier: SegmentIdentifier::new(data_uuid.0, data_uuid.1),
        bulk_identifiers: bulk_uuids.map(|uuid| SegmentIdentifier::new(uuid.0, uuid.1)),
        graph_empty_identifier,
        invalid_graph_identifier,
    }
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

fn write_deep_wide_production_fixture(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open writable repository");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let leaf = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("leaf");
    let mut names = ["a", "z"];
    names.sort_by(|left, right| {
        independent_map_entry_hash(left)
            .cmp(&independent_map_entry_hash(right))
            .then_with(|| left.encode_utf16().cmp(right.encode_utf16()))
    });
    let mut chain = leaf;
    for _ in 0..4 {
        chain = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Many(vec![
                    (names[0].to_owned(), chain),
                    (names[1].to_owned(), leaf),
                ]),
                &[],
            )
            .expect("branch");
    }
    writer.finish().expect("finish writer");
    assert!(store.set_head(store.head(), chain));
    store.close().expect("close writer");
}

fn write_deep_shared_name_production_fixture(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open writable repository");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let mut chain = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("leaf");
    for _ in 0..5 {
        chain = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "shared-name".to_owned(),
                    node: chain,
                },
                &[],
            )
            .expect("chain node");
    }
    writer.finish().expect("finish writer");
    assert!(store.set_head(store.head(), chain));
    store.close().expect("close writer");
}

fn write_long_scalar_production_fixture(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open writable repository");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let value = writer
        .write_string(&"n".repeat(16_512))
        .expect("long NAME value");
    let content = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "longName".to_owned(),
                property_type: PropertyType::Name,
                values: PropertyValuesToWrite::Single(value),
            }],
        )
        .expect("content node");
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

fn write_rendering_production_fixture(directory: &std::path::Path, array_size: usize) {
    let store = WritableRepository::open(directory).expect("open writable repository");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let seven = writer.write_string("7").expect("array value");
    let minimum_double = writer
        .write_string("4.9E-324")
        .expect("minimum double spelling");
    let long_name_text = "n".repeat(16_512);
    let long_name = writer
        .write_string(&long_name_text)
        .expect("long non-string scalar");
    let mut properties = vec![
        PropertyToWrite {
            name: "numbers".to_owned(),
            property_type: PropertyType::Long,
            values: PropertyValuesToWrite::Multiple(vec![seven; array_size]),
        },
        PropertyToWrite {
            name: "minimumDouble".to_owned(),
            property_type: PropertyType::Double,
            values: PropertyValuesToWrite::Single(minimum_double),
        },
        PropertyToWrite {
            name: "minimumDoubles".to_owned(),
            property_type: PropertyType::Double,
            values: PropertyValuesToWrite::Multiple(vec![minimum_double, minimum_double]),
        },
        PropertyToWrite {
            name: "longName".to_owned(),
            property_type: PropertyType::Name,
            values: PropertyValuesToWrite::Single(long_name),
        },
    ];
    sort_properties_for_template(&mut properties);
    let content = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)
        .expect("rendering content node");
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

fn write_external_binary_fixture(test_name: &str) -> TestDirectory {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x301);
    let missing_identifier_uuid = data_segment_uuid(0x399);
    let mut data = SegmentBuilder::new(data_uuid);
    let missing_reference = data.add_referenced_segment(missing_identifier_uuid);
    data.add_record(1, TYPE_VALUE, string_record("root"));
    data.add_record(2, TYPE_VALUE, string_record("content"));
    data.add_record(3, TYPE_VALUE, string_record("shortExternal"));
    data.add_record(4, TYPE_VALUE, string_record("longExternal"));

    let mut super_root_template = 0u32.to_be_bytes().to_vec();
    super_root_template.extend(record_identifier_bytes(0, 1));
    data.add_record(5, TYPE_TEMPLATE, super_root_template);
    let mut root_template = 0u32.to_be_bytes().to_vec();
    root_template.extend(record_identifier_bytes(0, 2));
    data.add_record(6, TYPE_TEMPLATE, root_template);
    let mut property_names = record_identifier_bytes(0, 3);
    property_names.extend(record_identifier_bytes(0, 4));
    data.add_record(7, TYPE_LIST_BUCKET, property_names);
    let mut content_template = ((1u32 << 29) | 2).to_be_bytes().to_vec();
    content_template.extend(record_identifier_bytes(0, 7));
    content_template.extend([2, 2]);
    data.add_record(8, TYPE_TEMPLATE, content_template);

    // The short identifier deliberately declares bytes that are absent; the
    // diagnostic needs only the marker to reproduce Oak's unavailable size.
    data.add_record(9, TYPE_VALUE, 0xE020u16.to_be_bytes().to_vec());
    // The long identifier points into an entirely missing segment. Following
    // it merely to classify the scalar would turn this report into a failure.
    let mut long_external = vec![0xF0];
    long_external.extend(record_identifier_bytes(missing_reference, 99));
    data.add_record(10, TYPE_VALUE, long_external);
    let mut property_values = record_identifier_bytes(0, 9);
    property_values.extend(record_identifier_bytes(0, 10));
    data.add_record(11, TYPE_LIST_BUCKET, property_values);

    let node_record = |record_number: u32, template: u32, extra: Option<u32>| {
        let mut bytes = record_identifier_bytes(0, record_number);
        bytes.extend(record_identifier_bytes(0, template));
        if let Some(extra) = extra {
            bytes.extend(record_identifier_bytes(0, extra));
        }
        bytes
    };
    data.add_record(12, TYPE_NODE, node_record(12, 8, Some(11)));
    data.add_record(13, TYPE_NODE, node_record(13, 6, Some(12)));
    data.add_record(14, TYPE_NODE, node_record(14, 5, Some(13)));
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(data_uuid, data.build());
    write_repository(
        &directory.path,
        &[(DATA_ARCHIVE.to_owned(), archive.build(DATA_ARCHIVE))],
        &[format!("{}:14 root 1", format_uuid(data_uuid))],
    );
    directory
}

/// Independently encodes a corrupt branch whose declared root size differs
/// from the sum of its two leaves. All keys are empty, so concrete entry
/// accounting—not the stored-name-byte limit—must detect the mismatch.
fn write_mismatched_child_map_fixture(
    test_name: &str,
    declared_size: u32,
    leaf_sizes: [u32; 2],
) -> TestDirectory {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x351);
    let mut data = SegmentBuilder::new(data_uuid);
    data.add_record(1, TYPE_VALUE, string_record(""));
    data.add_record(2, TYPE_TEMPLATE, (1u32 << 28).to_be_bytes().to_vec());
    data.add_record(3, TYPE_TEMPLATE, (1u32 << 29).to_be_bytes().to_vec());

    let mut child = record_identifier_bytes(0, 4);
    child.extend(record_identifier_bytes(0, 3));
    data.add_record(4, TYPE_NODE, child);
    for (leaf_record_number, leaf_size) in [5u32, 6].into_iter().zip(leaf_sizes) {
        let mut leaf = ((1u32 << 29) | leaf_size).to_be_bytes().to_vec();
        leaf.extend(std::iter::repeat_n(0u8, leaf_size as usize * 4));
        for _ in 0..leaf_size {
            leaf.extend(record_identifier_bytes(0, 1));
            leaf.extend(record_identifier_bytes(0, 4));
        }
        data.add_record(leaf_record_number, TYPE_MAP_LEAF, leaf);
    }
    let mut branch = declared_size.to_be_bytes().to_vec();
    branch.extend(0b11u32.to_be_bytes());
    branch.extend(record_identifier_bytes(0, 5));
    branch.extend(record_identifier_bytes(0, 6));
    data.add_record(7, TYPE_MAP_BRANCH, branch);

    let mut head = record_identifier_bytes(0, 8);
    head.extend(record_identifier_bytes(0, 2));
    head.extend(record_identifier_bytes(0, 7));
    data.add_record(8, TYPE_NODE, head);
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(data_uuid, data.build());
    write_repository(
        &directory.path,
        &[(DATA_ARCHIVE.to_owned(), archive.build(DATA_ARCHIVE))],
        &[format!("{}:8 root 1", format_uuid(data_uuid))],
    );
    directory
}

/// Independently encodes a two-record diff chain whose base record is absent.
/// The diagnostic's first scheduling preflight must refuse before attempting
/// to resolve that third map record.
fn write_diff_child_map_fixture(test_name: &str) -> TestDirectory {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x353);
    let mut data = SegmentBuilder::new(data_uuid);
    data.add_record(1, TYPE_VALUE, string_record("child"));
    data.add_record(2, TYPE_TEMPLATE, (1u32 << 28).to_be_bytes().to_vec());
    data.add_record(3, TYPE_TEMPLATE, (1u32 << 29).to_be_bytes().to_vec());

    let mut child = record_identifier_bytes(0, 4);
    child.extend(record_identifier_bytes(0, 3));
    data.add_record(4, TYPE_NODE, child);

    for (record_number, base_record_number) in [(6, 5), (7, 6)] {
        let mut diff = u32::MAX.to_be_bytes().to_vec();
        diff.extend(independent_map_entry_hash("child").to_be_bytes());
        diff.extend(record_identifier_bytes(0, 1));
        diff.extend(record_identifier_bytes(0, 4));
        diff.extend(record_identifier_bytes(0, base_record_number));
        data.add_record(record_number, TYPE_MAP_LEAF, diff);
    }

    let mut head = record_identifier_bytes(0, 8);
    head.extend(record_identifier_bytes(0, 2));
    head.extend(record_identifier_bytes(0, 7));
    data.add_record(8, TYPE_NODE, head);
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(data_uuid, data.build());
    write_repository(
        &directory.path,
        &[(DATA_ARCHIVE.to_owned(), archive.build(DATA_ARCHIVE))],
        &[format!("{}:8 root 1", format_uuid(data_uuid))],
    );
    directory
}

fn write_duplicate_property_fixture(test_name: &str) -> TestDirectory {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x352);
    let mut data = SegmentBuilder::new(data_uuid);
    data.add_record(1, TYPE_VALUE, string_record("dup"));
    data.add_record(2, TYPE_VALUE, string_record("7"));
    let mut names = record_identifier_bytes(0, 1);
    names.extend(record_identifier_bytes(0, 1));
    data.add_record(3, TYPE_LIST_BUCKET, names);
    let mut template = ((1u32 << 29) | 2).to_be_bytes().to_vec();
    template.extend(record_identifier_bytes(0, 3));
    template.extend([3, 3]);
    data.add_record(4, TYPE_TEMPLATE, template);
    let mut values = record_identifier_bytes(0, 2);
    values.extend(record_identifier_bytes(0, 2));
    data.add_record(5, TYPE_LIST_BUCKET, values);
    let mut node = record_identifier_bytes(0, 6);
    node.extend(record_identifier_bytes(0, 4));
    node.extend(record_identifier_bytes(0, 5));
    data.add_record(6, TYPE_NODE, node);
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(data_uuid, data.build());
    write_repository(
        &directory.path,
        &[(DATA_ARCHIVE.to_owned(), archive.build(DATA_ARCHIVE))],
        &[format!("{}:6 root 1", format_uuid(data_uuid))],
    );
    directory
}

/// Independently encodes the old 1,024-item display cutoff's first value
/// above the boundary, together with Java's minimum-double spelling. The
/// repeated list entries are deliberate: the fixture proves parsing and
/// rendering without sharing the production writer's value encoding.
#[allow(
    clippy::too_many_lines,
    reason = "one linear byte fixture keeps the nested list and node relationships auditable"
)]
fn write_independent_rendering_fixture(test_name: &str) -> TestDirectory {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x401);
    let mut data = SegmentBuilder::new(data_uuid);
    data.add_record(1, TYPE_VALUE, string_record("root"));
    data.add_record(2, TYPE_VALUE, string_record("content"));
    data.add_record(3, TYPE_VALUE, string_record("numbers"));
    data.add_record(4, TYPE_VALUE, string_record("minimumDouble"));
    data.add_record(5, TYPE_VALUE, string_record("minimumDoubles"));
    data.add_record(6, TYPE_VALUE, string_record("7"));
    data.add_record(7, TYPE_VALUE, string_record("4.9E-324"));

    // A 1,025-element uncounted list has five top-level children of 255,
    // 255, 255, 255, and 5 entries. All point at the independently encoded
    // scalar `7`; repeated identifiers remain distinct array positions.
    for (bucket_offset, bucket_size) in [255usize, 255, 255, 255, 5].into_iter().enumerate() {
        let mut bucket = Vec::with_capacity(bucket_size * 6);
        for _ in 0..bucket_size {
            bucket.extend(record_identifier_bytes(0, 6));
        }
        data.add_record(20 + bucket_offset as u32, TYPE_LIST_BUCKET, bucket);
    }
    let mut number_top_bucket = Vec::new();
    for record_number in 20..25 {
        number_top_bucket.extend(record_identifier_bytes(0, record_number));
    }
    data.add_record(25, TYPE_LIST_BUCKET, number_top_bucket);
    let mut numbers = 1_025u32.to_be_bytes().to_vec();
    numbers.extend(record_identifier_bytes(0, 25));
    data.add_record(26, TYPE_LIST, numbers);

    let mut double_bucket = record_identifier_bytes(0, 7);
    double_bucket.extend(record_identifier_bytes(0, 7));
    data.add_record(27, TYPE_LIST_BUCKET, double_bucket);
    let mut doubles = 2u32.to_be_bytes().to_vec();
    doubles.extend(record_identifier_bytes(0, 27));
    data.add_record(28, TYPE_LIST, doubles);

    let mut property_names = Vec::new();
    for record_number in [3, 4, 5] {
        property_names.extend(record_identifier_bytes(0, record_number));
    }
    data.add_record(29, TYPE_LIST_BUCKET, property_names);
    let mut content_template = ((1u32 << 29) | 3).to_be_bytes().to_vec();
    content_template.extend(record_identifier_bytes(0, 29));
    content_template.extend([(-3i8) as u8, 4, (-4i8) as u8]);
    data.add_record(30, TYPE_TEMPLATE, content_template);

    let mut property_values = record_identifier_bytes(0, 26);
    property_values.extend(record_identifier_bytes(0, 7));
    property_values.extend(record_identifier_bytes(0, 28));
    data.add_record(31, TYPE_LIST_BUCKET, property_values);

    let mut super_root_template = 0u32.to_be_bytes().to_vec();
    super_root_template.extend(record_identifier_bytes(0, 1));
    data.add_record(32, TYPE_TEMPLATE, super_root_template);
    let mut root_template = 0u32.to_be_bytes().to_vec();
    root_template.extend(record_identifier_bytes(0, 2));
    data.add_record(33, TYPE_TEMPLATE, root_template);
    let node_record = |record_number: u32, template: u32, extra: Option<u32>| {
        let mut bytes = record_identifier_bytes(0, record_number);
        bytes.extend(record_identifier_bytes(0, template));
        if let Some(extra) = extra {
            bytes.extend(record_identifier_bytes(0, extra));
        }
        bytes
    };
    data.add_record(34, TYPE_NODE, node_record(34, 30, Some(31)));
    data.add_record(35, TYPE_NODE, node_record(35, 33, Some(34)));
    data.add_record(36, TYPE_NODE, node_record(36, 32, Some(35)));

    let mut archive = ArchiveBuilder::new();
    archive.add_segment(data_uuid, data.build());
    write_repository(
        &directory.path,
        &[(DATA_ARCHIVE.to_owned(), archive.build(DATA_ARCHIVE))],
        &[format!("{}:36 root 1", format_uuid(data_uuid))],
    );
    directory
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end assertion ties archive attribution, graph, and read-only state together"
)]
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

    let bulk_report = debug_archive(&repository, BULK_ARCHIVE).expect("bulk attribution");
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
        debug_archive(&repository, DATA_BLOCK_ARCHIVE).expect("data-kind block attribution");
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
fn reconstructed_graph_validates_headers_before_reserving_raw_edge_counts() {
    let fixture = write_diagnostic_fixture(
        "corrupt-debug-graph-header",
        GraphFixture::CorruptWithInvalidHeader,
    );
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_graph_edges = fixture.bulk_identifiers.len();
    let report = debug_archive_with_options(&repository, DATA_ARCHIVE, options)
        .expect("invalid header is an unavailable row, not a graph-budget refusal");

    let graph = report.graph.expect("reconstructed graph");
    assert_eq!(graph.origin, ArchiveGraphOrigin::Reconstructed);
    assert_eq!(graph.rows.len(), 2);
    assert_eq!(
        graph.rows[0].references,
        ArchiveGraphReferences::Available(fixture.bulk_identifiers.to_vec())
    );
    let invalid_identifier = fixture
        .invalid_graph_identifier
        .expect("fixture includes an invalid data segment");
    let invalid_row = graph
        .rows
        .iter()
        .find(|row| row.segment_identifier == invalid_identifier)
        .expect("invalid row remains totalized");
    assert!(matches!(
        &invalid_row.references,
        ArchiveGraphReferences::Unavailable { details }
            if details.contains("declares -1 segment references")
    ));
}

#[test]
fn crc_valid_nonempty_stored_graph_uses_oak_set_order_and_last_source_row() {
    let fixture = write_diagnostic_fixture("stored-debug-graph", GraphFixture::ValidNonempty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");

    let graph = report.graph.as_ref().expect("stored graph");
    assert_eq!(graph.origin, ArchiveGraphOrigin::Stored);
    assert_eq!(graph.rows.len(), 2, "one total row per archive segment");
    assert_eq!(graph.rows[0].segment_identifier, fixture.data_identifier);
    assert_eq!(
        graph.rows[0].references,
        ArchiveGraphReferences::Available(vec![
            fixture.bulk_identifiers[1],
            fixture.bulk_identifiers[3],
        ]),
        "the duplicate source's last row wins, targets deduplicate and sort"
    );
    assert_eq!(
        graph.rows[1].segment_identifier,
        fixture
            .graph_empty_identifier
            .expect("fixture has an unmentioned archive segment")
    );
    assert_eq!(
        graph.rows[1].references,
        ArchiveGraphReferences::Available(Vec::new()),
        "an archive segment absent from the stored graph receives an empty row"
    );

    let complete_work = report.work.consumed_work_units;
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = complete_work - 1;
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units,
            attempted_work_units,
        }) if maximum_work_units == complete_work - 1
            && attempted_work_units == complete_work
    ));
}

#[test]
fn stored_graph_rows_and_edges_have_independent_typed_caps() {
    let fixture = write_diagnostic_fixture("stored-debug-graph-caps", GraphFixture::ValidNonempty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");

    let mut row_options = ArchiveDebugOptions::default();
    row_options.maximum_graph_rows = 1;
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, row_options),
        Err(ArchiveDebugError::GraphBudgetExceeded {
            maximum_graph_rows: 1,
            attempted_graph_rows: 2,
            ..
        })
    ));

    let mut edge_options = ArchiveDebugOptions::default();
    edge_options.maximum_graph_edges = 2;
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, edge_options),
        Err(ArchiveDebugError::GraphBudgetExceeded {
            maximum_graph_edges: 2,
            attempted_graph_edges: 3,
            ..
        })
    ));
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
fn reconstructed_graph_charges_dense_segment_bytes_before_parsing() {
    let fixture = write_diagnostic_fixture("missing-debug-graph-work", GraphFixture::Missing);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let data_bytes = repository
        .archives()
        .iter()
        .find(|archive| archive.file_name() == DATA_ARCHIVE)
        .and_then(|archive| archive.segment_data(fixture.data_identifier))
        .expect("independent data segment");
    assert_eq!(data_bytes.len(), 2_096);
    // The independently encoded tree consumes 1,087 units before graph
    // reconstruction; graph selection and its row cost two more. Reserving
    // the complete 2,096-byte segment therefore attempts unit 3,185 before
    // parsing. Removing the byte charge makes this absolute threshold pass.
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = 3_184;

    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: 3_184,
            attempted_work_units: 3_185,
        })
    ));
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

    let mut row_options = ArchiveDebugOptions::default();
    row_options.maximum_path_references = 64;
    row_options.maximum_reference_text_bytes = usize::MAX;
    let error = debug_archive_with_options(&repository, &archive_file_name, row_options)
        .expect_err("wide result must stop at the configured limit");
    assert!(matches!(
        error,
        ArchiveDebugError::ResultBudgetExceeded {
            maximum_path_references: 64,
            attempted_path_references: 65,
            ..
        }
    ));

    let mut text_options = ArchiveDebugOptions::default();
    text_options.maximum_path_references = usize::MAX;
    text_options.maximum_reference_text_bytes = 0;
    let text_error = debug_archive_with_options(&repository, &archive_file_name, text_options)
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
fn hostile_reused_block_list_stops_at_the_exact_work_budget() {
    let fixture =
        write_diagnostic_fixture("reused-list-work-budget", GraphFixture::HostileReusedList);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = 79;

    let error = debug_archive_with_options(&repository, DATA_ARCHIVE, options)
        .expect_err("the twelfth reused list entry must not be resolved");
    assert!(
        matches!(
            &error,
            ArchiveDebugError::WorkBudgetExceeded {
                maximum_work_units: 79,
                attempted_work_units: 80,
            }
        ),
        "{error:?}"
    );
    assert!(matches!(
        debug_archive(&repository, DATA_ARCHIVE),
        Err(ArchiveDebugError::Repository(
            froe::Error::InvalidFormat { .. }
        ))
    ));
}

#[test]
fn repeated_binary_array_block_segments_produce_one_oak_set_reference() {
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
fn duplicate_rendered_property_lines_use_oak_tree_set_semantics() {
    let directory = write_duplicate_property_fixture("duplicate-property-lines");
    let repository = Repository::open(&directory.path).expect("open repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");
    let duplicate_rows = report
        .references
        .iter()
        .filter(|reference| matches!(reference, ArchivePathReference::Property { name, .. } if name == "dup"))
        .count();

    assert_eq!(duplicate_rows, 1);
    assert_eq!(
        report.work.retained_path_references,
        report.references.len() as u64,
        "only unique rows enter the result ledger"
    );
    let mut exact_unique = ArchiveDebugOptions::default();
    exact_unique.maximum_path_references = report.references.len();
    exact_unique.maximum_reference_text_bytes = report.work.retained_reference_text_bytes as usize;
    let bounded = debug_archive_with_options(&repository, DATA_ARCHIVE, exact_unique)
        .expect("duplicate candidates do not consume aggregate result budget");
    assert_eq!(bounded.references, report.references);
}

#[test]
fn non_string_values_render_fully_across_old_array_and_long_value_cutoffs() {
    for array_size in [1_024usize, 1_025] {
        let directory = TestDirectory::new(&format!("debug-rendering-{array_size}"));
        write_rendering_production_fixture(&directory.path, array_size);
        std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
        let repository = Repository::open(&directory.path).expect("open repository");
        let archive_file_name = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(repository.head_record_identifier().segment))
            .expect("head archive")
            .file_name()
            .to_owned();
        let report = debug_archive(&repository, &archive_file_name).expect("debug report");
        let display = |name: &str| {
            report
                .references
                .iter()
                .find_map(|reference| match reference {
                    ArchivePathReference::Property {
                        name: property_name,
                        display: ArchivePropertyDisplay::Other(text),
                        ..
                    } if property_name == name => Some(text.as_str()),
                    _ => None,
                })
        };

        let numbers = display("numbers").expect("number array display");
        let expected_numbers = format!("[{}]", vec!["7"; array_size].join(", "));
        assert_eq!(numbers, expected_numbers, "{array_size}-element boundary");
        assert!(!numbers.contains("omitted"));
        assert_eq!(display("minimumDouble"), Some("4.9E-324"));
        assert_eq!(display("minimumDoubles"), Some("[4.9E-324, 4.9E-324]"));
        let expected_long_name = "n".repeat(16_512);
        assert_eq!(display("longName"), Some(expected_long_name.as_str()));

        if array_size == 1_025 {
            let mut options = ArchiveDebugOptions::default();
            options.maximum_reference_text_bytes = 1_024;
            assert!(matches!(
                debug_archive_with_options(&repository, &archive_file_name, options),
                Err(ArchiveDebugError::ResultBudgetExceeded { .. })
            ));
        }
    }
}

#[test]
fn independent_records_pin_full_1025_array_and_minimum_double_spelling() {
    let directory = write_independent_rendering_fixture("independent-debug-rendering");
    let repository = Repository::open(&directory.path).expect("open independent repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");
    let display = |name: &str| {
        report
            .references
            .iter()
            .find_map(|reference| match reference {
                ArchivePathReference::Property {
                    name: property_name,
                    display: ArchivePropertyDisplay::Other(text),
                    ..
                } if property_name == name => Some(text.as_str()),
                _ => None,
            })
    };

    let expected_numbers = format!("[{}]", vec!["7"; 1_025].join(", "));
    assert_eq!(display("numbers"), Some(expected_numbers.as_str()));
    assert_eq!(display("minimumDouble"), Some("4.9E-324"));
    assert_eq!(display("minimumDoubles"), Some("[4.9E-324, 4.9E-324]"));
}

#[test]
fn long_non_string_scalar_is_complete_or_a_typed_text_budget_error() {
    let directory = TestDirectory::new("long-scalar-debug-budget");
    write_long_scalar_production_fixture(&directory.path);
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
    let repository = Repository::open(&directory.path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();

    let mut sufficient = ArchiveDebugOptions::default();
    sufficient.maximum_reference_text_bytes = 20_000;
    let report = debug_archive_with_options(&repository, &archive_file_name, sufficient)
        .expect("the configured report budget holds the complete scalar");
    let expected = "n".repeat(16_512);
    assert!(report.references.iter().any(|reference| matches!(
        reference,
        ArchivePathReference::Property {
            name,
            display: ArchivePropertyDisplay::Other(display),
            ..
        } if name == "longName" && display == &expected
    )));

    let mut insufficient = ArchiveDebugOptions::default();
    insufficient.maximum_reference_text_bytes = 1_024;
    assert!(matches!(
        debug_archive_with_options(&repository, &archive_file_name, insufficient),
        Err(ArchiveDebugError::ResultBudgetExceeded {
            maximum_reference_text_bytes: 1_024,
            attempted_reference_text_bytes: 1_025,
            ..
        })
    ));
}

#[test]
fn per_node_child_materialization_cap_is_typed_and_checked_before_expansion() {
    let fixture = write_diagnostic_fixture("debug-child-cap", GraphFixture::ValidEmpty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_scheduled_children_per_node = 0;

    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::NodeChildBudgetExceeded {
            maximum_scheduled_children_per_node: 0,
            attempted_scheduled_children: 1,
        })
    ));
}

#[test]
fn corrupt_map_cannot_enumerate_past_its_preflighted_child_limit() {
    for (fixture_name, declared_size, leaf_sizes, expected_details) in [
        (
            "debug-corrupt-map-over-count",
            33,
            [17, 17],
            "child map declared 33 entries but enumerated at least 34",
        ),
        (
            "debug-corrupt-map-under-count",
            34,
            [17, 16],
            "child map declared 34 entries but enumerated 33",
        ),
    ] {
        let directory = write_mismatched_child_map_fixture(fixture_name, declared_size, leaf_sizes);
        let repository = Repository::open(&directory.path).expect("open repository");
        for child_limit in [
            ArchiveDebugOptions::default().maximum_scheduled_children_per_node,
            u64::MAX,
        ] {
            let mut options = ArchiveDebugOptions::default();
            options.maximum_scheduled_children_per_node = child_limit;
            let error = debug_archive_with_options(&repository, DATA_ARCHIVE, options)
                .expect_err("the concrete entry mismatch is repository corruption");
            assert!(
                matches!(
                    error,
                    ArchiveDebugError::Repository(froe::Error::InvalidFormat { ref details })
                        if details == expected_details
                ),
                "fixture {fixture_name}, child limit {child_limit}: {error:?}"
            );
        }
    }
}

#[test]
fn archive_work_budget_charges_each_child_map_diff_record_at_an_absolute_threshold() {
    let directory = write_diff_child_map_fixture("debug-map-diff-work");
    let repository = Repository::open(&directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = 2;

    // Unit one selects the root traversal step. Only one further unit
    // remains: the first diff fits, but following the second diff attempts
    // absolute unit three before any child-entry materialization.
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: 2,
            attempted_work_units: 3,
        })
    ));
}

#[test]
fn deep_wide_tree_hits_total_pending_node_cap() {
    let directory = TestDirectory::new("debug-pending-cap");
    write_deep_wide_production_fixture(&directory.path);
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
    let repository = Repository::open(&directory.path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();
    let mut options = ArchiveDebugOptions::default();
    options.maximum_pending_nodes = 2;

    assert!(matches!(
        debug_archive_with_options(&repository, &archive_file_name, options),
        Err(ArchiveDebugError::PendingNodeBudgetExceeded {
            maximum_pending_nodes: 2,
            attempted_pending_nodes: 3,
        })
    ));
}

#[test]
fn deep_shared_name_paths_charge_each_full_path_copy() {
    let directory = TestDirectory::new("debug-deep-path-work");
    write_deep_shared_name_production_fixture(&directory.path);
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove bootstrap lock");
    let repository = Repository::open(&directory.path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = 1_116;

    // The fifth nested node's Oak path is five copies of "/shared-name"
    // plus the trailing slash used for its rows: 5 * 12 + 1 = 61 bytes.
    // The independently fixed threshold catches that whole copy as work;
    // charging only the newly scheduled name would not reach 1_177.
    assert!(matches!(
        debug_archive_with_options(&repository, &archive_file_name, options),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: 1_116,
            attempted_work_units: 1_177,
        })
    ));
}

#[test]
fn hostile_child_name_is_refused_from_its_length_before_materialization() {
    let fixture = write_diagnostic_fixture("debug-child-name-cap", GraphFixture::HostileChildName);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_name_bytes_per_node = 64;

    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::NodeNameBudgetExceeded {
            maximum_name_bytes_per_node: 64,
            attempted_name_bytes: 16_511,
        })
    ));
}

#[test]
fn hostile_template_property_name_is_refused_before_cache_materialization() {
    let fixture =
        write_diagnostic_fixture("debug-template-name-cap", GraphFixture::HostileTemplateName);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_name_bytes_per_node = 64;

    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::NodeNameBudgetExceeded {
            maximum_name_bytes_per_node: 64,
            attempted_name_bytes: 16_511,
        })
    ));
}

#[test]
fn external_binary_scalars_render_oak_unavailable_size_without_reading_identifiers() {
    let directory = write_external_binary_fixture("external-debug-display");
    let repository = Repository::open(&directory.path).expect("open repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");
    let displays: Vec<(&str, &str)> = report
        .references
        .iter()
        .filter_map(|reference| match reference {
            ArchivePathReference::Property {
                name,
                display: ArchivePropertyDisplay::Other(display),
                ..
            } if name.ends_with("External") => Some((name.as_str(), display.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        displays,
        [
            ("longExternal", "{-1 bytes}"),
            ("shortExternal", "{-1 bytes}"),
        ]
    );
}

#[test]
fn archive_argument_is_a_file_name_not_an_escape_path() {
    let fixture = write_diagnostic_fixture("debug-name-scope", GraphFixture::ValidEmpty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    assert!(debug_archive(&repository, "../data00000a.tar").is_err());
    assert!(debug_archive(&repository, "not-an-archive.tar").is_err());
}

#[test]
fn archive_debug_rejects_an_oversized_indexed_segment() {
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
