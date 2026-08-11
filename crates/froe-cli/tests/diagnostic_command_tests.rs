//! Load-bearing CLI tests for the read-only segment and archive diagnostics.

use std::collections::BTreeMap;

use froe::PropertyType;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, sort_properties_for_template,
};
use froe::writer::store_writer::WritableRepository;

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

fn populate(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open writable repository");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let title = writer.write_string("Hello \"Oak\"\n").expect("title value");
    let count = writer.write_string("-42").expect("count value");
    let first_tag = writer.write_string("first").expect("first tag");
    let second_tag = writer.write_string("second").expect("second tag");
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

fn corrupt_one_data_segment_magic(store_path: &std::path::Path) -> froe::SegmentIdentifier {
    let repository = froe::Repository::open(store_path).expect("open repository");
    let segment_identifier = repository
        .segment_identifiers()
        .find(|identifier| identifier.is_data_segment())
        .expect("data segment");
    let archive = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(segment_identifier))
        .expect("segment archive");
    let archive_path = store_path.join(archive.file_name());
    let payload_position = archive
        .index_entry(segment_identifier)
        .expect("indexed segment")
        .position as usize;
    drop(repository);

    let mut bytes = std::fs::read(&archive_path).expect("read archive");
    bytes[payload_position] = b'x';
    std::fs::write(archive_path, bytes).expect("install corrupt-segment fixture");
    segment_identifier
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
fn segment_hex_and_debug_commands_reach_read_only_production_paths() {
    let directory = TestDirectory::new("production-wiring");
    let store_path = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store_path).expect("create segment store");
    populate(&store_path);
    std::fs::remove_file(store_path.join("repo.lock")).expect("remove bootstrap lock file");

    let repository = froe::Repository::open(&store_path).expect("open repository");
    let data_identifier = repository
        .segment_identifiers()
        .find(|identifier| identifier.is_data_segment())
        .expect("data segment");
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

    let segment = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "segment",
            store_path.to_str().expect("store path"),
            &data_identifier.to_string(),
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
    let segment_output = String::from_utf8(segment.stdout).expect("UTF-8 segment output");
    assert!(segment_output.contains(&format!("Segment {data_identifier} (")));
    assert!(segment_output.contains("GCGeneration{generation="));
    assert!(segment_output.contains("00000000 30 61 4B 0D"));

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
    let directory = TestDirectory::new("corrupt-segment-production-wiring");
    let store_path = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store_path).expect("create segment store");
    populate(&store_path);
    std::fs::remove_file(store_path.join("repo.lock")).expect("remove bootstrap lock file");
    let segment_identifier = corrupt_one_data_segment_magic(&store_path);
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
    assert!(
        output.ends_with(
            "--------------------------------------------------------------------------\n"
        )
    );

    assert_eq!(directory_snapshot(&store_path), before);
    assert!(!store_path.join("repo.lock").exists());
}
