//! Load-bearing CLI tests for the read-only segment and archive diagnostics.

use std::collections::BTreeMap;

use froe::PropertyType;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, sort_properties_for_template,
};
use froe::writer::store_writer::WritableRepository;

#[cfg(windows)]
const SEGMENT_DUMP_LINE_SEPARATOR: &str = "\r\n";
#[cfg(not(windows))]
const SEGMENT_DUMP_LINE_SEPARATOR: &str = "\n";

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-cli-diagnostics-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Encodes the checksum used in Oak segment TAR entry names without using
/// the production checksum implementation under test.
fn independent_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let low_bit_mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & low_bit_mask);
        }
    }
    !crc
}

/// Independently encodes a version-13 data segment containing just Oak's
/// conventional record-zero segment info value.
fn independently_encoded_info_segment(info: &str) -> Vec<u8> {
    assert!(info.len() < 128, "fixture uses Oak's small-string form");
    let record_position = 44usize;
    let record_length = 1 + info.len();
    let size = (record_position + record_length.div_ceil(4) * 4).div_ceil(16) * 16;
    let mut bytes = vec![0u8; size];
    bytes[0..3].copy_from_slice(b"0aK");
    bytes[3] = 13;
    bytes[4..8].copy_from_slice(&0x8000_0001u32.to_be_bytes());
    bytes[10..14].copy_from_slice(&1u32.to_be_bytes());
    bytes[18..22].copy_from_slice(&1u32.to_be_bytes());
    bytes[36] = 4; // VALUE
    let virtual_offset = (262_144 - (size - record_position)) as u32;
    bytes[37..41].copy_from_slice(&virtual_offset.to_be_bytes());
    bytes[record_position] = info.len() as u8;
    bytes[record_position + 1..record_position + record_length].copy_from_slice(info.as_bytes());
    bytes
}

/// Writes a prebuilt read-only repository fixture with no production writer.
/// The archive deliberately has no index, exercising the reader's in-memory
/// recovery path without creating Oak's `.ro.bak` side file.
fn write_independent_segment_fixture(
    test_name: &str,
    segment_bytes: &[u8],
) -> (TestDirectory, std::path::PathBuf, froe::SegmentIdentifier) {
    let directory = TestDirectory::new(test_name);
    let store_path = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store_path).expect("create segment store");
    let identifier = froe::SegmentIdentifier::new(0x1234, 0xA000_0000_0000_5678);
    let entry_name = format!("{identifier}.{:08x}", independent_crc32(segment_bytes));

    let mut header = vec![0u8; 512];
    header[..entry_name.len()].copy_from_slice(entry_name.as_bytes());
    header[100..107].copy_from_slice(b"0000400");
    header[108..115].copy_from_slice(b"0000000");
    header[116..123].copy_from_slice(b"0000000");
    header[124..135].copy_from_slice(format!("{:011o}", segment_bytes.len()).as_bytes());
    header[136..147].copy_from_slice(b"00000000000");
    header[148..156].copy_from_slice(b"        ");
    header[156] = b'0';
    let header_checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
    header[148..156].copy_from_slice(format!("{header_checksum:06o}\0 ").as_bytes());

    let mut archive = header;
    archive.extend_from_slice(segment_bytes);
    archive.resize(512 + segment_bytes.len().div_ceil(512) * 512, 0);
    archive.extend_from_slice(&[0u8; 1024]);
    std::fs::write(store_path.join("data00000a.tar"), archive).expect("write prebuilt archive");
    std::fs::write(
        store_path.join("journal.log"),
        format!("{identifier}:0 root 1\n"),
    )
    .expect("write journal");
    std::fs::write(store_path.join("manifest"), "store.version=2\n").expect("write manifest");

    (directory, store_path, identifier)
}

fn populate(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open writable repository");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let title = writer.write_string("Hello \"Oak\"\n").expect("title value");
    let count = writer.write_string("-42").expect("count value");
    let first_tag = writer.write_string("first").expect("first tag");
    let second_tag = writer.write_string("second").expect("second tag");
    let minimum_double = writer
        .write_string("4.9E-324")
        .expect("minimum double spelling");
    let mut long_title_text = "x".repeat(59);
    long_title_text.push('\u{1f600}');
    long_title_text.push_str(&"z".repeat(16_449));
    assert_eq!(long_title_text.len(), 16_512);
    let long_title = writer
        .write_string(&long_title_text)
        .expect("long string value");
    let binary = writer
        .write_binary_content(&vec![0x5a; 262_144])
        .expect("long binary");
    let mut properties = vec![
        PropertyToWrite {
            name: "title".to_owned(),
            property_type: PropertyType::String,
            values: PropertyValuesToWrite::Single(title),
        },
        PropertyToWrite {
            name: "count".to_owned(),
            property_type: PropertyType::Long,
            values: PropertyValuesToWrite::Single(count),
        },
        PropertyToWrite {
            name: "tags".to_owned(),
            property_type: PropertyType::String,
            values: PropertyValuesToWrite::Multiple(vec![first_tag, second_tag]),
        },
        PropertyToWrite {
            name: "emptyTags".to_owned(),
            property_type: PropertyType::String,
            values: PropertyValuesToWrite::Multiple(Vec::new()),
        },
        PropertyToWrite {
            name: "blob".to_owned(),
            property_type: PropertyType::Binary,
            values: PropertyValuesToWrite::Single(binary),
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
            name: "longTitle".to_owned(),
            property_type: PropertyType::String,
            values: PropertyValuesToWrite::Single(long_title),
        },
    ];
    sort_properties_for_template(&mut properties);
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Zero,
            &properties,
        )
        .expect("content");
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
        .expect("root");
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
    writer.finish().expect("finish");
    assert!(store.set_head(store.head(), head));
    store.close().expect("close");
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

fn corrupt_stored_graph_checksum(archive_path: &std::path::Path) {
    let mut bytes = std::fs::read(archive_path).expect("read archive");
    let graph_magic = 0x0A30_470Au32.to_be_bytes();
    let magic_position = bytes
        .windows(graph_magic.len())
        .rposition(|window| window == graph_magic)
        .expect("archive has graph footer");
    bytes[magic_position - 12] ^= 0x01;
    std::fs::write(archive_path, bytes).expect("install corrupt-graph fixture");
}

fn assert_missing_archive_cli(store_path: &std::path::Path) {
    let missing = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "debug",
            store_path.to_str().expect("store path"),
            "data99999a.tar",
        ])
        .output()
        .expect("run missing archive debug");
    assert!(missing.status.success());
    assert_eq!(
        String::from_utf8(missing.stdout).expect("UTF-8 missing output"),
        "file doesn't exist, skipping data99999a.tar\n"
    );
}

#[test]
fn segment_hex_cli_sanitizes_hostile_info_from_an_independent_read_only_fixture() {
    let info = "esc=\u{1b};osc=\u{1b}]0;title\u{7};bidi=\u{202e}\u{2066};literal=\\u{1b}";
    let bytes = independently_encoded_info_segment(info);
    let (_directory, store_path, segment_identifier) =
        write_independent_segment_fixture("hostile-segment-info", &bytes);
    let before = directory_snapshot(&store_path);

    let segment = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "segment",
            store_path.to_str().expect("store path"),
            &segment_identifier.to_string(),
            "--hex",
        ])
        .output()
        .expect("run segment dump");
    assert!(
        segment.status.success(),
        "segment dump failed: {}",
        String::from_utf8_lossy(&segment.stderr)
    );
    assert!(segment.stderr.is_empty());
    let output = String::from_utf8(segment.stdout).expect("UTF-8 segment output");
    let expected_info = format!(
        r"Info: esc=\u{{1b}};osc=\u{{1b}}]0;title\u{{7}};bidi=\u{{202e}}\u{{2066}};literal=\\u{{1b}}, Generation: GCGeneration{{generation=1, fullGeneration=1, isCompacted=true}}{SEGMENT_DUMP_LINE_SEPARATOR}"
    );
    assert!(
        output.contains(&expected_info),
        "escaped info line: {output:?}"
    );
    assert!(!output.contains('\u{1b}'), "ESC reached terminal output");
    assert!(
        !output.contains('\u{7}'),
        "OSC terminator reached terminal output"
    );
    assert!(
        !output.contains('\u{202e}'),
        "bidi override reached terminal output"
    );
    assert!(
        !output.contains('\u{2066}'),
        "bidi isolate reached terminal output"
    );
    assert_eq!(directory_snapshot(&store_path), before);
    assert!(!store_path.join("repo.lock").exists());
}

#[test]
fn debug_command_reaches_read_only_production_path() {
    let directory = TestDirectory::new("production-wiring");
    let store_path = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store_path).expect("create segment store");
    populate(&store_path);
    std::fs::remove_file(store_path.join("repo.lock")).expect("remove bootstrap lock file");

    let repository = froe::Repository::open(&store_path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();
    let archive_segment_identifiers: Vec<_> =
        repository.archives()[0].segment_identifiers().collect();
    let stored_graph = repository.archives()[0]
        .segment_graph()
        .expect("production writer graph");
    let (graph_source, graph_target) = stored_graph
        .adjacency
        .iter()
        .find_map(|(source, targets)| targets.first().map(|target| (*source, *target)))
        .expect("long binary creates a real graph edge");
    drop(repository);
    corrupt_stored_graph_checksum(&store_path.join(&archive_file_name));
    let before = directory_snapshot(&store_path);

    let debug = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "debug",
            store_path.to_str().expect("store path"),
            &archive_file_name,
            "data99999a.tar",
        ])
        .output()
        .expect("run archive debug");
    assert!(
        debug.status.success(),
        "archive debug failed: {}",
        String::from_utf8_lossy(&debug.stderr)
    );
    assert!(debug.stderr.is_empty());
    let debug_output = String::from_utf8(debug.stdout).expect("UTF-8 debug output");
    assert!(debug_output.contains("SegmentNodeState references to"));
    assert!(debug_output.contains("/root/content/ [SegmentNodeState@"));
    assert!(debug_output.contains("/root/content/[Template@"));
    assert!(
        debug_output.contains(
            "/root/content/title = \"Hello \\\"Oak\\\"\\n\" [SegmentPropertyState<STRING>@"
        )
    );
    assert!(debug_output.contains("/root/content/count = -42 [SegmentPropertyState<LONG>@"));
    assert!(
        debug_output
            .contains("/root/content/minimumDouble = 4.9E-324 [SegmentPropertyState<DOUBLE>@")
    );
    assert!(debug_output.contains(
        "/root/content/minimumDoubles = [4.9E-324, 4.9E-324] \
         [SegmentPropertyState<DOUBLES>@"
    ));
    assert!(debug_output.contains(&format!(
        "/root/content/longTitle = \"{}\\uD83D... (16510 chars)\" \
         [SegmentPropertyState<STRING>@",
        "x".repeat(59)
    )));
    assert!(
        debug_output.contains("/root/content/tags = \"first\" [SegmentPropertyState<STRINGS>@")
    );
    assert!(debug_output.contains("/root/content/emptyTags =  [SegmentPropertyState<STRINGS>@"));
    assert!(!debug_output.contains("emptyTags = \"\""));
    assert!(!debug_output.contains("tags = [first, second]"));
    assert!(debug_output.contains("Tar graph:"));
    for identifier in archive_segment_identifiers {
        assert!(
            debug_output.contains(&format!("{identifier}=[")),
            "the total graph must contain every archive segment: {identifier}"
        );
    }
    assert!(debug_output.contains(&format!("{graph_source}=[{graph_target}")));
    assert!(debug_output.contains("file doesn't exist, skipping data99999a.tar"));

    assert_missing_archive_cli(&store_path);

    assert_eq!(directory_snapshot(&store_path), before);
    assert!(
        !store_path.join("repo.lock").exists(),
        "read-only commands must not create or touch the repository lock"
    );
}

#[test]
fn segment_hex_cli_dumps_raw_bytes_when_structural_parsing_fails() {
    let mut bytes = independently_encoded_info_segment("Oak");
    bytes[0] = b'x';
    let (_directory, store_path, segment_identifier) =
        write_independent_segment_fixture("corrupt-segment-production-wiring", &bytes);
    let before = directory_snapshot(&store_path);

    let segment = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "segment",
            store_path.to_str().expect("store path"),
            &segment_identifier.to_string(),
            "--hex",
        ])
        .output()
        .expect("run corrupt segment dump");
    assert!(
        segment.status.success(),
        "corrupt segment dump failed: {}",
        String::from_utf8_lossy(&segment.stderr)
    );
    assert!(segment.stderr.is_empty());
    let output = String::from_utf8(segment.stdout).expect("UTF-8 segment output");
    assert!(output.starts_with(&format!("Segment {segment_identifier} (")));
    assert!(output.contains("Parse error: invalid segment-tar data:"));
    assert!(output.contains("00000000 78 61 4B 0D"));
    assert!(output.ends_with(&format!(
        "--------------------------------------------------------------------------{SEGMENT_DUMP_LINE_SEPARATOR}"
    )));

    assert_eq!(directory_snapshot(&store_path), before);
    assert!(!store_path.join("repo.lock").exists());
}
