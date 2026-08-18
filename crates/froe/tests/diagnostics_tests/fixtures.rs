//! The repositories these tests read: written by the independent test
//! encoder, so a diagnostic is checked against bytes froe's own writer
//! never produced.

use super::*;

pub(crate) const DATA_ARCHIVE: &str = "data00001a.tar";

pub(crate) const BULK_ARCHIVE: &str = "data00000a.tar";

pub(crate) const DATA_BLOCK_ARCHIVE: &str = "data00002a.tar";

pub(crate) const BINARY_BLOCK_COUNT: u32 = 256;

pub(crate) const BINARY_LENGTH: usize = BINARY_BLOCK_COUNT as usize * 4_096;

pub(crate) struct DiagnosticFixture {
    pub(crate) directory: TestDirectory,
    pub(crate) data_identifier: SegmentIdentifier,
    pub(crate) bulk_identifiers: [SegmentIdentifier; 4],
    pub(crate) graph_empty_identifier: Option<SegmentIdentifier>,
    pub(crate) invalid_graph_identifier: Option<SegmentIdentifier>,
}

#[derive(Clone, Copy)]
pub(crate) enum GraphFixture {
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

/// The 256-element block list of the fixture's long binary value.
///
/// 256 crosses the 255-way list-bucket boundary: record 20 holds the
/// first 255 block identifiers, while top bucket 7 points to record 20
/// and directly to the final element. Four full bulk segments each hold
/// 64 blocks, so their virtual record numbers are ordinary byte offsets.
pub(crate) fn build_long_binary_list(
    graph_fixture: GraphFixture,
    data: &mut SegmentBuilder,
    bulk_references: &[u16],
) {
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
}

/// The three templates the fixture's nodes point at.
///
/// /content has zero children and three properties, named in Oak's
/// signed-hash order — data, count, title — with `data` a one-value
/// BINARY array so attribution exercises the counted-list path.
pub(crate) fn add_fixture_templates(data: &mut SegmentBuilder) {
    // Single-child templates for the super-root and content root.
    let mut super_root_template = 0u32.to_be_bytes().to_vec();
    super_root_template.extend(record_identifier_bytes(0, 1));
    data.add_record(4, TYPE_TEMPLATE, super_root_template);
    let mut root_template = 0u32.to_be_bytes().to_vec();
    root_template.extend(record_identifier_bytes(0, 2));
    data.add_record(5, TYPE_TEMPLATE, root_template);

    let mut property_names = Vec::new();
    for record_number in [3, 12, 13] {
        property_names.extend(record_identifier_bytes(0, record_number));
    }
    data.add_record(16, TYPE_LIST_BUCKET, property_names);
    let mut content_template = ((1u32 << 29) | 3).to_be_bytes().to_vec();
    content_template.extend(record_identifier_bytes(0, 16));
    content_template.extend([(-2i8) as u8, 3, 1]); // BINARIES, LONG, STRING
    data.add_record(6, TYPE_TEMPLATE, content_template);
}

/// The fixture's three archives, plus the identifiers of the segments whose
/// graph trailers the damaged cases target.
pub(crate) struct FixtureArchives {
    pub(crate) bulk: Vec<u8>,
    pub(crate) data_blocks: Vec<u8>,
    pub(crate) data: Vec<u8>,
    pub(crate) graph_empty_identifier: Option<SegmentIdentifier>,
    pub(crate) invalid_graph_identifier: Option<SegmentIdentifier>,
}

/// Packs the fixture's segments into their archives, applying whichever
/// graph-trailer damage the case under test asks for.
pub(crate) fn build_fixture_archives(
    graph_fixture: GraphFixture,
    bulk_uuids: &[support::SegmentUuid],
    data_uuid: support::SegmentUuid,
    data: &SegmentBuilder,
) -> FixtureArchives {
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

    FixtureArchives {
        bulk: bulk_archive.build(BULK_ARCHIVE),
        data_blocks: data_block_archive.build(DATA_BLOCK_ARCHIVE),
        data: data_archive_bytes,
        graph_empty_identifier,
        invalid_graph_identifier,
    }
}

/// Builds this super-root and deliberately stores the binary blocks in a
/// different archive from every node, template, and property record:
///
/// ```text
/// /root/content  data = BINARY(1 MiB), count = -42, title = STRING
/// ```
pub(crate) fn write_diagnostic_fixture(
    test_name: &str,
    graph_fixture: GraphFixture,
) -> DiagnosticFixture {
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

    add_fixture_templates(&mut data);
    build_long_binary_list(graph_fixture, &mut data, &bulk_references);
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

    let archives = build_fixture_archives(graph_fixture, &bulk_uuids, data_uuid, &data);
    write_repository(
        &directory.path,
        &[
            (BULK_ARCHIVE.to_owned(), archives.bulk),
            (DATA_ARCHIVE.to_owned(), archives.data),
            (DATA_BLOCK_ARCHIVE.to_owned(), archives.data_blocks),
        ],
        &[format!("{}:11 root 1", format_uuid(data_uuid))],
    );
    DiagnosticFixture {
        directory,
        data_identifier: SegmentIdentifier::new(data_uuid.0, data_uuid.1),
        bulk_identifiers: bulk_uuids.map(|uuid| SegmentIdentifier::new(uuid.0, uuid.1)),
        graph_empty_identifier: archives.graph_empty_identifier,
        invalid_graph_identifier: archives.invalid_graph_identifier,
    }
}

pub(crate) fn write_wide_production_fixture(directory: &std::path::Path, property_count: usize) {
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
    assert!(store.compare_and_set_head(store.head(), head));
    store.close().expect("close writer");
}

pub(crate) fn write_deep_wide_production_fixture(directory: &std::path::Path) {
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
    assert!(store.compare_and_set_head(store.head(), chain));
    store.close().expect("close writer");
}

pub(crate) fn write_deep_shared_name_production_fixture(directory: &std::path::Path) {
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
    assert!(store.compare_and_set_head(store.head(), chain));
    store.close().expect("close writer");
}

pub(crate) fn write_long_scalar_production_fixture(directory: &std::path::Path) {
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
    assert!(store.compare_and_set_head(store.head(), head));
    store.close().expect("close writer");
}

pub(crate) fn write_rendering_production_fixture(directory: &std::path::Path, array_size: usize) {
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
    assert!(store.compare_and_set_head(store.head(), head));
    store.close().expect("close writer");
}

pub(crate) fn write_external_binary_fixture(test_name: &str) -> TestDirectory {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x301);
    let missing_identifier_uuid = data_segment_uuid(0x399);
    let mut data = SegmentBuilder::new(data_uuid);
    let missing_reference = data.add_referenced_segment(missing_identifier_uuid);
    data.add_record(1, TYPE_VALUE, string_record("root"));
    data.add_record(2, TYPE_VALUE, string_record("content"));
    data.add_record(3, TYPE_VALUE, string_record("shortExternal"));
    data.add_record(4, TYPE_VALUE, string_record("longExternal"));
    data.add_record(15, TYPE_VALUE, string_record("corruptBinary"));

    let mut super_root_template = 0u32.to_be_bytes().to_vec();
    super_root_template.extend(record_identifier_bytes(0, 1));
    data.add_record(5, TYPE_TEMPLATE, super_root_template);
    let mut root_template = 0u32.to_be_bytes().to_vec();
    root_template.extend(record_identifier_bytes(0, 2));
    data.add_record(6, TYPE_TEMPLATE, root_template);
    let mut property_names = record_identifier_bytes(0, 4);
    property_names.extend(record_identifier_bytes(0, 15));
    property_names.extend(record_identifier_bytes(0, 3));
    data.add_record(7, TYPE_LIST_BUCKET, property_names);
    let mut content_template = ((1u32 << 29) | 3).to_be_bytes().to_vec();
    content_template.extend(record_identifier_bytes(0, 7));
    content_template.extend([2, 2, 2]);
    data.add_record(8, TYPE_TEMPLATE, content_template);

    // The short identifier deliberately declares bytes that are absent; the
    // diagnostic needs only the marker to reproduce Oak's unavailable size.
    data.add_record(9, TYPE_VALUE, 0xE020u16.to_be_bytes().to_vec());
    // The long identifier points into an entirely missing segment. Following
    // it merely to classify the scalar would turn this report into a failure.
    let mut long_external = vec![0xF0];
    long_external.extend(record_identifier_bytes(missing_reference, 99));
    data.add_record(10, TYPE_VALUE, long_external);
    // SegmentBlob.length throws for 11111xxx. Oak's diagnostic catches that
    // exception in AbstractPropertyState.getBinarySize and prints -1.
    data.add_record(16, TYPE_VALUE, vec![0xF8]);
    let mut property_values = record_identifier_bytes(0, 10);
    property_values.extend(record_identifier_bytes(0, 16));
    property_values.extend(record_identifier_bytes(0, 9));
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
pub(crate) fn write_mismatched_child_map_fixture(
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
pub(crate) fn write_diff_child_map_fixture(test_name: &str) -> TestDirectory {
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

/// Independently encodes one many-arity child map so the archive diagnostic's
/// combined scheduling budget must include both map scans as well as the
/// child count and stored name bytes.
pub(crate) fn write_one_child_map_fixture(test_name: &str) -> TestDirectory {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x354);
    let mut data = SegmentBuilder::new(data_uuid);
    data.add_record(1, TYPE_VALUE, string_record("child"));
    data.add_record(2, TYPE_TEMPLATE, (1u32 << 28).to_be_bytes().to_vec());
    data.add_record(3, TYPE_TEMPLATE, (1u32 << 29).to_be_bytes().to_vec());

    let mut child = record_identifier_bytes(0, 4);
    child.extend(record_identifier_bytes(0, 3));
    data.add_record(4, TYPE_NODE, child);

    let mut child_map = 1u32.to_be_bytes().to_vec();
    child_map.extend(independent_map_entry_hash("child").to_be_bytes());
    child_map.extend(record_identifier_bytes(0, 1));
    child_map.extend(record_identifier_bytes(0, 4));
    data.add_record(5, TYPE_MAP_LEAF, child_map);

    let mut head = record_identifier_bytes(0, 6);
    head.extend(record_identifier_bytes(0, 2));
    head.extend(record_identifier_bytes(0, 5));
    data.add_record(6, TYPE_NODE, head);
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(data_uuid, data.build());
    write_repository(
        &directory.path,
        &[(DATA_ARCHIVE.to_owned(), archive.build(DATA_ARCHIVE))],
        &[format!("{}:6 root 1", format_uuid(data_uuid))],
    );
    directory
}

pub(crate) fn write_duplicate_property_fixture(test_name: &str) -> TestDirectory {
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

/// One root node whose template contains one property name, stored property,
/// and scalar value. The independent encoding keeps the archive-debug work
/// threshold sensitive to the template's name-list entry lookup.
pub(crate) fn write_template_lookup_work_fixture(test_name: &str) -> TestDirectory {
    let directory = TestDirectory::new(test_name);
    let data_uuid = data_segment_uuid(0x353);
    let mut data = SegmentBuilder::new(data_uuid);
    data.add_record(1, TYPE_VALUE, string_record("answer"));
    data.add_record(2, TYPE_VALUE, string_record("42"));
    data.add_record(3, TYPE_LIST_BUCKET, record_identifier_bytes(0, 1));
    let mut template = ((1u32 << 29) | 1).to_be_bytes().to_vec();
    template.extend(record_identifier_bytes(0, 3));
    template.push(3); // LONG
    data.add_record(4, TYPE_TEMPLATE, template);
    let mut node = record_identifier_bytes(0, 6);
    node.extend(record_identifier_bytes(0, 4));
    node.extend(record_identifier_bytes(0, 2));
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
pub(crate) fn write_independent_rendering_fixture(test_name: &str) -> TestDirectory {
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
