//! Load-bearing CLI tests for the read-only segment and archive diagnostics.

mod support;

use froe::PropertyType;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, sort_properties_for_template,
};
use froe::writer::store_writer::WritableRepository;
use support::filesystem_snapshot::directory_snapshot;

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

/// Returns the write end of an OS pipe after closing its only read end.
/// A child receiving this as standard output therefore encounters `SIGPIPE`
/// on its first write, rather than racing a parent that closes a captured
/// `ChildStdout` after spawning it.
///
/// `std::io::pipe` rather than `libc::pipe`, because the descriptors must be
/// close-on-exec from the instant they exist. Tests in this file run
/// concurrently and spawn eight other `froe` processes; a raw `pipe` leaves
/// both ends inheritable, so a process spawned between the call and the
/// `drop` below inherits the read end and holds the pipe open for as long as
/// it lives. The dump then writes successfully and exits zero, and this test
/// fails claiming froe did not use SIGPIPE. Measured at roughly one run in
/// fifteen before the change, and never when the file runs single-threaded.
#[cfg(unix)]
fn closed_pipe_stdout() -> std::process::Stdio {
    let (read_end, write_end) = std::io::pipe().expect("create closed-stdout test pipe");
    drop(read_end);
    std::process::Stdio::from(write_end)
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

#[cfg(unix)]
const HOSTILE_DEBUG_TEST_NAME: &str = "hostile-debug-\u{1b}]0;repository-title\u{7}-\u{202e}";
#[cfg(unix)]
const ESCAPED_HOSTILE_DEBUG_TEST_NAME: &str =
    r"hostile-debug-\u{1b}]0;repository-title\u{7}-\u{202e}";
#[cfg(windows)]
const HOSTILE_DEBUG_TEST_NAME: &str = "hostile-debug-repository-title-\u{202e}";
#[cfg(windows)]
const ESCAPED_HOSTILE_DEBUG_TEST_NAME: &str = r"hostile-debug-repository-title-\u{202e}";
const HOSTILE_DEBUG_NODE_NAME: &str = "node-\u{1b}]8;;https://example.invalid\u{7}link-\u{202e}";
const ESCAPED_HOSTILE_DEBUG_NODE_NAME: &str =
    r"node-\u{1b}]8;;https://example.invalid\u{7}link-\u{202e}";
const HOSTILE_DEBUG_PROPERTY_NAME: &str = "property-\u{1b}[31m-red-\u{2066}";
const ESCAPED_HOSTILE_DEBUG_PROPERTY_NAME: &str = r"property-\u{1b}[31m-red-\u{2066}";
const HOSTILE_DEBUG_PATH_VALUE: &str = "value-\u{1b}]0;owned\u{7}-\u{202d}";
const ESCAPED_HOSTILE_DEBUG_PATH_VALUE: &str = r"value-\u{1b}]0;owned\u{7}-\u{202d}";

struct HostileDebugFixture {
    head: froe::RecordIdentifier,
    content_root: froe::RecordIdentifier,
    hostile_node: froe::RecordIdentifier,
    path_value: froe::RecordIdentifier,
}

/// Writes terminal-hostile content through the production repository writer.
/// Separate escaped constants above keep the expected CLI text independent of
/// the presentation sanitizer under test.
fn populate_hostile_debug_fixture(directory: &std::path::Path) -> HostileDebugFixture {
    let store = WritableRepository::open(directory).expect("open writable repository");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let path_value = writer
        .write_string(HOSTILE_DEBUG_PATH_VALUE)
        .expect("hostile path value");
    let mut properties = vec![PropertyToWrite {
        name: HOSTILE_DEBUG_PROPERTY_NAME.to_owned(),
        property_type: PropertyType::Path,
        values: PropertyValuesToWrite::Single(path_value),
    }];
    sort_properties_for_template(&mut properties);
    let hostile_node = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Zero,
            &properties,
        )
        .expect("hostile content node");
    let content_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: HOSTILE_DEBUG_NODE_NAME.to_owned(),
                node: hostile_node,
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
                node: content_root,
            },
            &[],
        )
        .expect("super root");
    writer.finish().expect("finish");
    assert!(store.compare_and_set_head(store.head(), head));
    store.close().expect("close");
    HostileDebugFixture {
        head,
        content_root,
        hostile_node,
        path_value,
    }
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
    assert!(store.compare_and_set_head(store.head(), head));
    store.close().expect("close");
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
fn debug_cli_preserves_a_relative_repository_path_in_the_header() {
    let directory = TestDirectory::new("relative-debug-header");
    let store_path = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store_path).expect("create segment store");
    populate(&store_path);
    std::fs::remove_file(store_path.join("repo.lock")).expect("remove bootstrap lock file");
    let repository = froe::Repository::open(&store_path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();
    drop(repository);
    let before = directory_snapshot(&store_path);
    let relative_store_path = std::path::Path::new("segmentstore");

    let debug = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .current_dir(&directory.path)
        .arg("debug")
        .arg(relative_store_path)
        .arg(&archive_file_name)
        .output()
        .expect("run archive debug with a relative repository path");
    assert!(
        debug.status.success(),
        "archive debug failed: {}",
        String::from_utf8_lossy(&debug.stderr)
    );
    assert!(debug.stderr.is_empty());
    let output = String::from_utf8(debug.stdout).expect("UTF-8 debug output");
    let expected_header_prefix = format!(
        "Debug file {}(",
        relative_store_path.join(&archive_file_name).display()
    );
    assert!(
        output.starts_with(&expected_header_prefix),
        "Oak's File.toString preserves the relative input path: {output:?}"
    );
    assert_eq!(directory_snapshot(&store_path), before);
    assert!(!store_path.join("repo.lock").exists());
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
fn debug_cli_escapes_hostile_paths_names_and_non_string_values_end_to_end() {
    let directory = TestDirectory::new(HOSTILE_DEBUG_TEST_NAME);
    let store_path = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store_path).expect("create segment store");
    let fixture = populate_hostile_debug_fixture(&store_path);
    std::fs::remove_file(store_path.join("repo.lock")).expect("remove bootstrap lock file");

    let repository = froe::Repository::open(&store_path).expect("open repository");
    let archive = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(fixture.path_value.segment))
        .expect("archive containing hostile property value");
    let archive_file_name = archive.file_name().to_owned();
    let archive_file_size = archive.file_size();
    let template_identifier = |node: froe::RecordIdentifier| {
        let segment =
            <froe::Repository as froe::SegmentProvider>::segment(&repository, node.segment)
                .expect("node segment");
        segment
            .read_record_identifier(node.record_number, 0, 1)
            .expect("node template identifier")
    };
    let head_template = template_identifier(fixture.head);
    let content_root_template = template_identifier(fixture.content_root);
    let hostile_node_template = template_identifier(fixture.hostile_node);
    drop(repository);
    let before = directory_snapshot(&store_path);

    let debug = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "debug",
            store_path.to_str().expect("store path"),
            &archive_file_name,
        ])
        .output()
        .expect("run hostile archive debug");
    assert!(
        debug.status.success(),
        "archive debug failed: {}",
        String::from_utf8_lossy(&debug.stderr)
    );
    assert!(debug.stderr.is_empty());
    for &raw_control in b"\x07\x1b" {
        assert!(
            !debug.stdout.contains(&raw_control),
            "raw terminal control byte {raw_control:#04x} reached stdout: {:?}",
            String::from_utf8_lossy(&debug.stdout)
        );
    }
    let debug_output = String::from_utf8(debug.stdout).expect("UTF-8 debug output");
    for raw_bidi_control in ['\u{202d}', '\u{202e}', '\u{2066}'] {
        assert!(
            !debug_output.contains(raw_bidi_control),
            "raw bidi control U+{:04X} reached stdout: {debug_output:?}",
            u32::from(raw_bidi_control)
        );
    }

    let archive_path = store_path.join(&archive_file_name);
    let escaped_archive_path = archive_path
        .to_string_lossy()
        .replace(HOSTILE_DEBUG_TEST_NAME, ESCAPED_HOSTILE_DEBUG_TEST_NAME);
    let expected_header = format!("Debug file {escaped_archive_path}({archive_file_size})");
    assert_eq!(debug_output.lines().next(), Some(expected_header.as_str()));

    let expected_references = format!(
        concat!(
            "SegmentNodeState references to {}\n",
            "  / [SegmentNodeState@{}]\n",
            "  /[Template@{}]\n",
            "  /root/ [SegmentNodeState@{}]\n",
            "  /root/[Template@{}]\n",
            "  /root/{}/ [SegmentNodeState@{}]\n",
            "  /root/{}/[Template@{}]\n",
            "  /root/{}/{} = {} [SegmentPropertyState<PATH>@{}]\n",
            "\nTar graph:\n",
        ),
        archive_file_name,
        fixture.head,
        head_template,
        fixture.content_root,
        content_root_template,
        ESCAPED_HOSTILE_DEBUG_NODE_NAME,
        fixture.hostile_node,
        ESCAPED_HOSTILE_DEBUG_NODE_NAME,
        hostile_node_template,
        ESCAPED_HOSTILE_DEBUG_NODE_NAME,
        ESCAPED_HOSTILE_DEBUG_PROPERTY_NAME,
        ESCAPED_HOSTILE_DEBUG_PATH_VALUE,
        fixture.path_value,
    );
    assert!(
        debug_output.contains(&expected_references),
        "exact escaped reference block was absent: {debug_output:?}"
    );

    assert_eq!(directory_snapshot(&store_path), before);
    assert!(!store_path.join("repo.lock").exists());
}

#[expect(
    clippy::cognitive_complexity,
    reason = "25 assertions and one branch; the lint counts each \
              `assert!` expansion as a decision point"
)]
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

#[cfg(unix)]
#[test]
fn segment_hex_cli_uses_conventional_sigpipe_for_a_preclosed_stdout() {
    use std::os::unix::process::ExitStatusExt as _;

    let bytes = independently_encoded_info_segment("Oak");
    let (_directory, store_path, segment_identifier) =
        write_independent_segment_fixture("segment-closed-stdout", &bytes);
    let before = directory_snapshot(&store_path);

    let segment = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "segment",
            store_path.to_str().expect("store path"),
            &segment_identifier.to_string(),
            "--hex",
        ])
        .stdout(closed_pipe_stdout())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run segment dump with a preclosed stdout pipe");
    assert_eq!(
        segment.status.signal(),
        Some(libc::SIGPIPE),
        "segment dump must use conventional Unix SIGPIPE termination; status {:?}, stderr {}",
        segment.status,
        String::from_utf8_lossy(&segment.stderr)
    );
    assert!(segment.stderr.is_empty());
    assert_eq!(directory_snapshot(&store_path), before);
    assert!(!store_path.join("repo.lock").exists());
}

#[cfg(unix)]
#[test]
fn debug_cli_uses_conventional_sigpipe_for_a_preclosed_stdout() {
    use std::os::unix::process::ExitStatusExt as _;

    let directory = TestDirectory::new("debug-closed-stdout");
    let store_path = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store_path).expect("create segment store");
    populate(&store_path);
    std::fs::remove_file(store_path.join("repo.lock")).expect("remove bootstrap lock file");
    let repository = froe::Repository::open(&store_path).expect("open repository");
    let archive_file_name = repository.archives()[0].file_name().to_owned();
    drop(repository);
    let before = directory_snapshot(&store_path);

    let debug = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "debug",
            store_path.to_str().expect("store path"),
            &archive_file_name,
        ])
        .stdout(closed_pipe_stdout())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run archive debug with a preclosed stdout pipe");
    assert_eq!(
        debug.status.signal(),
        Some(libc::SIGPIPE),
        "archive debug must use conventional Unix SIGPIPE termination; status {:?}, stderr {}",
        debug.status,
        String::from_utf8_lossy(&debug.stderr)
    );
    assert!(debug.stderr.is_empty());
    assert_eq!(directory_snapshot(&store_path), before);
    assert!(!store_path.join("repo.lock").exists());
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
