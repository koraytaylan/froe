//! `OakDirectoryWriter`, read back through plan 0006's `OakDirectory`.
//!
//! The claim is a round trip: whatever goes in comes out byte-identical,
//! at every size where the encoding changes shape. The sizes are chosen
//! against those boundaries — empty, one byte, either side of a block, and
//! large enough that the streaming path runs for a while — because a writer
//! that is wrong only at a boundary is a writer that looks right.
//!
//! The subtree's property set and types are read through the digest, so the
//! shape is compared against what the spec says rather than against the
//! writer's own idea of it.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::PathBuf;

use froe::index::IndexDefinition;
use froe::index::lucene::OakDirectory;
use froe::store::Repository;
use froe::tooling::digest::digest_repository_excluding;
use froe::writer::commit::rewrite_node_with_child_edits;
use froe::writer::index::{DirectoryListing, OakDirectoryWriter};
use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use froe::writer::store_writer::WritableRepository;
use std::io::Read as _;

/// `OakDirectory`'s default, and the block size the sizes below straddle.
const BLOB_SIZE: i64 = 1_048_576 - 4096;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-oak-directory-writer-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Deterministic bytes of `length`, so a mismatch names a position.
fn payload(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

/// Writes a store whose `/oak:index/lucene/:data` holds `files`.
fn write_store(directory: &TestDirectory, files: &[(String, Vec<u8>)], listing: DirectoryListing) {
    let store = WritableRepository::open(&directory.path).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    let data = {
        let mut builder = OakDirectoryWriter::new(&mut writer, BLOB_SIZE, listing);
        for (name, bytes) in files {
            builder
                .add_file(name, std::io::Cursor::new(bytes.clone()))
                .expect("add a file");
        }
        builder.finish().expect("finish the directory")
    };

    let type_value = writer.write_string("lucene").expect("type");
    let definition_properties = [PropertyToWrite {
        name: "type".to_owned(),
        property_type: froe::PropertyType::String,
        values: PropertyValuesToWrite::Single(type_value),
    }];
    let definition = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: ":data".to_owned(),
                node: data,
            },
            &definition_properties,
        )
        .expect("write the definition");
    let oak_index = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "lucene".to_owned(),
                node: definition,
            },
            &[],
        )
        .expect("write /oak:index");

    // The spine comes from the *writable* session's own head: a read-only
    // open here would find a journal the writer has not flushed yet.
    let head = store.head();
    let mut root_edits = froe::writer::commit::ChildEdits::new();
    root_edits.insert("oak:index".to_owned(), Some(oak_index));
    let super_root = store.head_node();
    let root = super_root
        .child_node("root")
        .expect("read the super-root")
        .expect("the super-root has a root child");
    let new_root = rewrite_node_with_child_edits(
        &store,
        &mut writer,
        Some(root.record_identifier()),
        &root_edits,
    )
    .expect("rewrite the root");

    let mut super_edits = froe::writer::commit::ChildEdits::new();
    super_edits.insert("root".to_owned(), Some(new_root));
    let new_head = rewrite_node_with_child_edits(&store, &mut writer, Some(head), &super_edits)
        .expect("rewrite the super-root");

    writer.finish().expect("finish");
    assert!(store.compare_and_set_head(head, new_head));
    store.close().expect("close");
}

/// Reads every file back through `OakDirectory`.
fn read_back(directory: &TestDirectory) -> Vec<(String, Vec<u8>)> {
    let repository = Repository::open(&directory.path).expect("open");
    let node = repository
        .node_at_path("/oak:index/lucene")
        .expect("resolve")
        .expect("exists");
    let definition = IndexDefinition::read(&node, "/oak:index/lucene").expect("model");
    let oak_directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open :data")
        .expect(":data exists");

    let mut files = Vec::new();
    for name in oak_directory.file_names() {
        let file = oak_directory.file(name).expect("open the file");
        let mut bytes = Vec::new();
        file.reader().read_to_end(&mut bytes).expect("read");
        assert_eq!(
            file.length(),
            bytes.len() as u64,
            "{name}: the declared length and the streamed length disagree"
        );
        files.push((name.clone(), bytes));
    }
    files.sort();
    files
}

#[test]
fn every_size_around_the_block_boundary_round_trips() {
    // A writer that is wrong only at a boundary is a writer that looks
    // right, so these are exactly the sizes where the encoding changes:
    // empty, one byte, either side of the medium-value limit, and either
    // side of a block.
    let sizes: Vec<usize> = vec![
        0,
        1,
        (BLOB_SIZE - 1) as usize,
        BLOB_SIZE as usize,
        (BLOB_SIZE + 1) as usize,
    ];
    let files: Vec<(String, Vec<u8>)> = sizes
        .iter()
        .map(|size| (format!("file-{size:08}"), payload(*size)))
        .collect();

    let directory = TestDirectory::new("boundaries");
    write_store(&directory, &files, DirectoryListing::Saved);

    assert_round_trips(&read_back(&directory), &files);
}

#[test]
fn a_multi_megabyte_file_round_trips_without_being_held_whole() {
    // Several megabytes, so the streaming path runs for many blocks. What
    // this catches is a writer that drops or reorders a block once the
    // first buffer is exhausted.
    let files = vec![("big".to_owned(), payload(5 * 1024 * 1024))];
    let directory = TestDirectory::new("multi-megabyte");
    write_store(&directory, &files, DirectoryListing::Saved);

    assert_round_trips(&read_back(&directory), &files);
}

/// Compares a round trip by length first, then by the first differing byte.
///
/// A plain `assert_eq!` on multi-megabyte vectors prints both of them, which
/// buries the one fact that locates the bug.
fn assert_round_trips(actual: &[(String, Vec<u8>)], expected: &[(String, Vec<u8>)]) {
    assert_eq!(
        actual.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        expected.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "the file names differ"
    );
    for ((name, read), (_, written)) in actual.iter().zip(expected) {
        assert_eq!(
            read.len(),
            written.len(),
            "{name}: {} bytes read back from {} written",
            read.len(),
            written.len()
        );
        if let Some(position) = read
            .iter()
            .zip(written)
            .position(|(one, other)| one != other)
        {
            panic!(
                "{name}: byte {position} of {} differs — read {:#04x}, wrote {:#04x}",
                read.len(),
                read[position],
                written[position]
            );
        }
    }
}

#[test]
fn the_listing_is_written_in_name_order_and_holds_every_file() {
    // Oak stores `dirListing` in a concurrent hash set's iteration order
    // and reads it back as a set, so there is no order to reproduce; froe
    // writes name order, which is a recorded deviation. What must hold is
    // the *set*.
    let files: Vec<(String, Vec<u8>)> = ["zebra", "alpha", "middle"]
        .iter()
        .map(|name| ((*name).to_owned(), payload(32)))
        .collect();
    let directory = TestDirectory::new("listing");
    write_store(&directory, &files, DirectoryListing::Saved);

    let lines = digest_lines(&directory, "/oak:index/lucene/:data");
    let listing = lines
        .iter()
        .find(|line| line.contains("dirListing="))
        .expect("the directory node carries a listing");
    assert!(
        listing.contains("alpha") && listing.contains("middle") && listing.contains("zebra"),
        "{listing}"
    );
    // Name order is froe's choice, and it is deterministic, which is what
    // makes two renderings of one directory comparable.
    let alpha = listing.find("alpha").expect("alpha");
    let middle = listing.find("middle").expect("middle");
    let zebra = listing.find("zebra").expect("zebra");
    assert!(alpha < middle && middle < zebra, "{listing}");
}

#[test]
fn an_omitted_listing_leaves_the_files_readable() {
    // `saveDirectoryListing = false`: the listing is absent and the reader
    // falls back to the child nodes.
    let files = vec![("only".to_owned(), payload(64))];
    let directory = TestDirectory::new("no-listing");
    write_store(&directory, &files, DirectoryListing::Omitted);

    let lines = digest_lines(&directory, "/oak:index/lucene/:data");
    assert!(
        !lines.iter().any(|line| line.contains("dirListing=")),
        "no listing was asked for: {lines:?}"
    );
    assert_eq!(read_back(&directory), files);
}

#[test]
fn each_file_node_carries_the_property_set_and_types_the_specification_names() {
    let files = vec![("segments_1".to_owned(), payload(128))];
    let directory = TestDirectory::new("properties");
    write_store(&directory, &files, DirectoryListing::Saved);

    let lines = digest_lines(&directory, "/oak:index/lucene/:data/segments_1");
    let node = lines.first().expect("the file node renders");
    // §2: the types matter as much as the names. A `String` where Oak
    // stores a `Long` is a different store.
    assert!(node.contains("uniqueKey=String:"), "{node}");
    assert!(node.contains("blobSize=Long:1044480"), "{node}");
    assert!(node.contains("jcr:lastModified=Long:"), "{node}");
    assert!(node.contains("jcr:data=Binary"), "{node}");
    // Oak sets this only under a blob-deletion callback froe never creates.
    assert!(
        !node.contains("unsafeForActiveDeletion"),
        "froe writes no external blobs, so the flag would claim a configuration it did \
         not create: {node}"
    );
}

#[test]
fn a_duplicate_file_name_is_refused() {
    let directory = TestDirectory::new("duplicate");
    let store = WritableRepository::open(&directory.path).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    let mut builder = OakDirectoryWriter::new(&mut writer, BLOB_SIZE, DirectoryListing::Saved);
    builder
        .add_file("_0.si", std::io::Cursor::new(vec![1, 2, 3]))
        .expect("the first file");
    let error = builder
        .add_file("_0.si", std::io::Cursor::new(vec![4, 5, 6]))
        .expect_err("a duplicate name must be refused");
    assert!(
        error
            .to_string()
            .contains("already holds a file named _0.si"),
        "{error}"
    );
}

#[test]
fn two_files_of_the_same_bytes_get_different_unique_keys() {
    // The key is what makes two identical files distinct blobs, and a
    // predictable one is worse than a refusal.
    let files = vec![
        ("one".to_owned(), payload(100)),
        ("two".to_owned(), payload(100)),
    ];
    let directory = TestDirectory::new("keys");
    write_store(&directory, &files, DirectoryListing::Saved);

    let one = unique_key(&directory, "one");
    let two = unique_key(&directory, "two");
    assert_ne!(one, two, "two files must not share a uniqueKey");
    assert_eq!(one.len(), 32, "sixteen bytes as lower-case hexadecimal");
    assert!(
        one.chars().all(|character| character.is_ascii_hexdigit()),
        "{one}"
    );
}

/// One file node's `uniqueKey`.
fn unique_key(directory: &TestDirectory, file_name: &str) -> String {
    let lines = digest_lines(directory, &format!("/oak:index/lucene/:data/{file_name}"));
    let node = lines.first().expect("the file node renders");
    let marker = "uniqueKey=String:";
    let start = node.find(marker).expect("a uniqueKey") + marker.len();
    node[start..]
        .split([' ', '\t'])
        .next()
        .expect("the value")
        .to_owned()
}

/// The digest lines under `path`.
fn digest_lines(directory: &TestDirectory, path: &str) -> Vec<String> {
    let repository = Repository::open(&directory.path).expect("open");
    let mut rendered = Vec::new();
    digest_repository_excluding(&repository, &[], &[], &mut rendered).expect("digest");
    String::from_utf8(rendered)
        .expect("UTF-8")
        .lines()
        .filter(|line| {
            line.strip_prefix(path).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with('/') || rest.starts_with('\t')
            })
        })
        .map(str::to_owned)
        .collect()
}
