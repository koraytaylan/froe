//! Independent-encoder tests for public streaming binary reads.

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
use std::io::Read;

use froe::content::value::read_binary_content;
use froe::error::Error;
use froe::read_binary_stream;
use froe::segment::identifier::SegmentIdentifier;
use froe::segment::record::RecordIdentifier;
use froe::store::Repository;
use support::{
    ArchiveBuilder, SegmentBuilder, TYPE_EXTERNAL_BLOB_IDENTIFIER, TYPE_LIST_BUCKET, TYPE_VALUE,
    TestDirectory, data_segment_uuid, format_uuid, record_identifier_bytes, string_record,
    write_repository,
};

const DATA_ARCHIVE: &str = "data00001a.tar";
const BULK_ARCHIVE: &str = "data00000a.tar";

fn bulk_segment_uuid(seed: u64) -> support::SegmentUuid {
    (seed, 0xB000_0000_0000_0000 | seed)
}

fn direct_record(content: &[u8]) -> Vec<u8> {
    let mut record = match content.len() {
        0..=127 => vec![content.len() as u8],
        128..=16_511 => {
            let stored = 0x8000u16 | (content.len() as u16 - 128);
            stored.to_be_bytes().to_vec()
        }
        length => panic!("{length} is not a direct-value fixture length"),
    };
    record.extend_from_slice(content);
    record
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

#[test]
fn public_stream_reads_independently_encoded_direct_and_partial_bulk_values() {
    let directory = TestDirectory::new("binary-stream");
    let data_uuid = data_segment_uuid(0x501);
    let bulk_uuid = bulk_segment_uuid(0x502);
    let data_identifier = SegmentIdentifier::new(data_uuid.0, data_uuid.1);
    let bulk_content: Vec<u8> = (0..16_512).map(|index| (index % 251) as u8).collect();
    let small_content: Vec<u8> = (0..127).map(|index| index as u8).collect();
    let first_medium_content: Vec<u8> = (0..128).map(|index| index as u8).collect();
    let last_medium_content: Vec<u8> = (0..16_511).map(|index| (index % 239) as u8).collect();

    let mut data = SegmentBuilder::new(data_uuid);
    let bulk_reference = data.add_referenced_segment(bulk_uuid);
    data.add_record(0, TYPE_VALUE, string_record("{\"wid\":\"independent\"}"));
    data.add_record(1, TYPE_VALUE, direct_record(&small_content));
    data.add_record(2, TYPE_VALUE, direct_record(&first_medium_content));
    data.add_record(3, TYPE_VALUE, direct_record(&last_medium_content));

    let external_identifier = "external-blob-1";
    let mut external_record = (0xE000u16 | external_identifier.len() as u16)
        .to_be_bytes()
        .to_vec();
    external_record.extend_from_slice(external_identifier.as_bytes());
    data.add_record(4, TYPE_EXTERNAL_BLOB_IDENTIFIER, external_record);

    // Literal values come from record-layer.md: 4 KiB blocks, a 256 KiB
    // virtual segment, and 16,512 as the first long-value length. Oak writes
    // a partial binary tail into one bulk segment at virtual offset
    // 262144-length, with a five-entry block list in the data segment.
    let first_virtual_offset = 262_144u32 - 16_512u32;
    let mut block_list = Vec::new();
    for block_index in 0..5u32 {
        block_list.extend_from_slice(&record_identifier_bytes(
            bulk_reference,
            first_virtual_offset + block_index * 4096,
        ));
    }
    data.add_record(7, TYPE_LIST_BUCKET, block_list);
    let mut long_record = 0xC000_0000_0000_0000u64.to_be_bytes().to_vec();
    long_record.extend_from_slice(&record_identifier_bytes(0, 7));
    data.add_record(8, TYPE_VALUE, long_record);

    let mut bulk_archive = ArchiveBuilder::new();
    bulk_archive.add_segment(bulk_uuid, bulk_content.clone());
    let mut data_archive = ArchiveBuilder::new();
    data_archive.add_segment(data_uuid, data.build());
    write_repository(
        &directory.path,
        &[
            (BULK_ARCHIVE.to_owned(), bulk_archive.build(BULK_ARCHIVE)),
            (DATA_ARCHIVE.to_owned(), data_archive.build(DATA_ARCHIVE)),
        ],
        &[format!("{}:8 root 1", format_uuid(data_uuid))],
    );
    let before = directory_snapshot(&directory.path);
    let repository = Repository::open(&directory.path).expect("open independent repository");

    for (record_number, expected) in [
        (1, small_content.as_slice()),
        (2, first_medium_content.as_slice()),
        (3, last_medium_content.as_slice()),
    ] {
        let identifier = RecordIdentifier::new(data_identifier, record_number);
        let mut stream = read_binary_stream(&repository, identifier).expect("direct stream");
        let mut actual = Vec::new();
        stream.read_to_end(&mut actual).expect("read direct value");
        assert_eq!(actual, expected);
    }

    let long_identifier = RecordIdentifier::new(data_identifier, 8);
    let mut stream = read_binary_stream(&repository, long_identifier).expect("long stream");
    let mut first_block = [0u8; 5000];
    assert_eq!(
        stream.read(&mut first_block).expect("first long block"),
        4096,
        "the public Read implementation stops at Oak's block boundary"
    );
    let mut actual = first_block[..4096].to_vec();
    stream
        .read_to_end(&mut actual)
        .expect("remaining long value");
    assert_eq!(actual, bulk_content);
    assert_eq!(
        read_binary_content(&repository, long_identifier).expect("compatibility helper"),
        bulk_content
    );

    match read_binary_stream(&repository, RecordIdentifier::new(data_identifier, 4)) {
        Err(Error::ExternalBinaryContentUnavailable { blob_identifier }) => {
            assert_eq!(blob_identifier, external_identifier);
        }
        _ => panic!("expected external binary rejection"),
    }

    drop(repository);
    assert_eq!(directory_snapshot(&directory.path), before);
    assert!(!directory.path.join("repo.lock").exists());
}
