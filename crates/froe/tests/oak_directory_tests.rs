//! Reading a Lucene index directory out of a store, against `:data` nodes an
//! independent encoder wrote.
//!
//! The layout functions have their own unit tests beside the implementation,
//! because they are pure. What these cover is the storage: both `jcr:data`
//! encodings read back byte-identically to what the helper stored before it
//! appended unique keys, the length rules reproduce the specification's
//! worked example, the listing rules follow `saveDirectoryListing`, and every
//! seek case holds.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use froe::index::lucene::{FileEncoding, OakDirectory};
use froe::index::{IndexDefinition, IndexError};
use froe::store::Repository;
use support::property_index_layout::{Node, Property, write_repository_with_tree};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-oak-directory-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test repository directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The sixteen key bytes, as the thirty-two hexadecimal characters a file
/// node stores and as the bytes a writer appends to every chunk.
const UNIQUE_KEY_HEX: &str = "443be1c792a673d4c26078ee12ba006d";

fn unique_key_bytes() -> Vec<u8> {
    (0..UNIQUE_KEY_HEX.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&UNIQUE_KEY_HEX[index..index + 2], 16).expect("hexadecimal")
        })
        .collect()
}

/// Content of `length` bytes, distinguishable position by position so a seek
/// that lands in the wrong place is visible in the assertion.
fn content(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

/// A streaming file node: one `Binary` of the content with the key appended.
fn streaming_file(payload: &[u8], with_key: bool) -> Node {
    let mut stored = payload.to_vec();
    if with_key {
        stored.extend(unique_key_bytes());
    }
    let mut node = Node::new()
        .with("blobSize", Property::Long(1_047_552))
        .with("jcr:lastModified", Property::Long(1_789_367_067_705))
        .with("jcr:data", Property::Binary(stored));
    if with_key {
        node = node.with("uniqueKey", Property::Text(UNIQUE_KEY_HEX.to_owned()));
    }
    node
}

/// A buffered file node: `blob_size` chunks, each with the key appended.
fn buffered_file(payload: &[u8], blob_size: usize, with_blob_size_property: bool) -> Node {
    let key = unique_key_bytes();
    let chunks: Vec<Vec<u8>> = payload
        .chunks(blob_size)
        .map(|chunk| {
            let mut stored = chunk.to_vec();
            stored.extend(key.iter().copied());
            stored
        })
        .collect();
    let mut node = Node::new()
        .with("uniqueKey", Property::Text(UNIQUE_KEY_HEX.to_owned()))
        .with("jcr:data", Property::Binaries(chunks));
    if with_blob_size_property {
        node = node.with("blobSize", Property::Long(blob_size as i64));
    }
    node
}

/// A `lucene` definition with a `:data` child, plus whatever properties the
/// caller adds to the definition itself.
fn lucene_definition(data: Node, save_listing: Option<bool>) -> Node {
    let mut definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("lucene".to_owned()))
        .with("async", Property::Text("async".to_owned()))
        .with_child(":data", data);
    if let Some(save_listing) = save_listing {
        definition = definition.with("saveDirectoryListing", Property::Boolean(save_listing));
    }
    definition
}

fn store(name: &str, definition: Node) -> (TestDirectory, Repository) {
    let directory = TestDirectory::new(name);
    let root = Node::new().with_child("oak:index", Node::new().with_child("lucene", definition));
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open the repository");
    (directory, repository)
}

fn read_definition(repository: &Repository) -> (froe::content::NodeState<'_>, IndexDefinition) {
    let node = repository
        .node_at_path("/oak:index/lucene")
        .expect("resolve")
        .expect("exists");
    let definition = IndexDefinition::read(&node, "/oak:index/lucene").expect("reads");
    (node, definition)
}

/// Opens `:data` and reads one file whole.
fn read_file(repository: &Repository, file_name: &str) -> (FileEncoding, u64, Vec<u8>) {
    let (node, definition) = read_definition(repository);
    let directory = OakDirectory::open(repository, &node, &definition, ":data")
        .expect("open the directory")
        .expect("the directory exists");
    let file = directory.file(file_name).expect("open the file");
    let mut bytes = Vec::new();
    file.reader().read_to_end(&mut bytes).expect("read");
    (file.encoding(), file.length(), bytes)
}

#[test]
fn a_streaming_file_reads_back_its_payload_without_the_trailing_key() {
    let payload = content(225);
    let (_directory, repository) = store(
        "streaming",
        lucene_definition(
            Node::new().with_child("_1.si", streaming_file(&payload, true)),
            None,
        ),
    );
    let (encoding, length, bytes) = read_file(&repository, "_1.si");
    assert_eq!(encoding, FileEncoding::Streaming);
    assert_eq!(
        length, 225,
        "the specification's worked example: a 241-byte blob less the 16 key bytes"
    );
    assert_eq!(bytes, payload);
}

#[test]
fn a_buffered_file_of_three_chunks_reads_back_its_payload() {
    // 250 bytes in chunks of 100: two full chunks and a 50-byte last one.
    let payload = content(250);
    let (_directory, repository) = store(
        "buffered",
        lucene_definition(
            Node::new().with_child("_1.cfs", buffered_file(&payload, 100, true)),
            None,
        ),
    );
    let (encoding, length, bytes) = read_file(&repository, "_1.cfs");
    assert_eq!(encoding, FileEncoding::Buffered);
    assert_eq!(
        length, 250,
        "n * blobSize - (blobSize - last.length()) - key length = 300 - (100 - 66) - 16"
    );
    assert_eq!(bytes, payload);
}

#[test]
fn a_streaming_file_larger_than_its_blob_size_is_one_chunk_not_several() {
    // `blobSize` is the *buffered* encoding's chunking and nothing else. The
    // real Sling fixture's `_0.cfs` is a 1.9 MB single blob beside a
    // `blobSize` of 1,047,552, and dividing its position by `blobSize` would
    // address a second chunk that does not exist. Every earlier streaming
    // fixture here was smaller than its `blobSize`, so the suite passed while
    // the reader could not read a real store; this is the shape that catches
    // it, at a size the test can afford.
    let payload = content(300);
    let mut node = streaming_file(&payload, true);
    node = node.with("blobSize", Property::Long(100));
    let (_directory, repository) = store(
        "streaming-over-blob-size",
        lucene_definition(Node::new().with_child("_0.cfs", node), None),
    );
    let (encoding, length, bytes) = read_file(&repository, "_0.cfs");
    assert_eq!(encoding, FileEncoding::Streaming);
    assert_eq!(length, 300);
    assert_eq!(bytes, payload);
}

#[test]
fn a_seek_inside_a_streaming_file_beyond_its_blob_size_lands_where_it_should() {
    let payload = content(300);
    let node = streaming_file(&payload, true).with("blobSize", Property::Long(100));
    let (_directory, repository) = store(
        "streaming-seek-over-blob-size",
        lucene_definition(Node::new().with_child("_0.cfs", node), None),
    );
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let file = directory.file("_0.cfs").expect("open");
    let mut reader = file.reader();
    reader
        .seek(SeekFrom::Start(250))
        .expect("seek past blobSize");
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).expect("read");
    assert_eq!(tail, &payload[250..]);
}

#[test]
fn a_zero_length_streaming_file_reads_as_empty() {
    let (_directory, repository) = store(
        "empty",
        lucene_definition(
            Node::new().with_child("segments.gen", streaming_file(&[], true)),
            None,
        ),
    );
    let (_, length, bytes) = read_file(&repository, "segments.gen");
    assert_eq!(length, 0, "a blob of only the key is a zero-length file");
    assert!(bytes.is_empty());
}

#[test]
fn a_file_without_a_unique_key_subtracts_no_length() {
    let payload = content(64);
    let (_directory, repository) = store(
        "no-key",
        lucene_definition(
            Node::new().with_child("_0.si", streaming_file(&payload, false)),
            None,
        ),
    );
    let (_, length, bytes) = read_file(&repository, "_0.si");
    assert_eq!(length, 64, "an absent uniqueKey is length-neutral");
    assert_eq!(bytes, payload);
}

#[test]
fn a_file_without_a_blob_size_falls_back_to_the_readers_own_constant() {
    // Two chunks of 32 KiB, the reader's fallback — not the definition's
    // 1,047,552, which would compute one chunk and a wrong length.
    let blob_size = 32 * 1024;
    let payload = content(blob_size + 10);
    let (_directory, repository) = store(
        "no-blob-size",
        lucene_definition(
            Node::new().with_child("_0.cfs", buffered_file(&payload, blob_size, false)),
            None,
        ),
    );
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let file = directory.file("_0.cfs").expect("open the file");
    assert_eq!(
        file.blob_size(),
        blob_size as u64,
        "the fallback is the reader's 32 KiB, not the definition's default"
    );
    assert_eq!(file.length(), payload.len() as u64);
    let mut bytes = Vec::new();
    file.reader().read_to_end(&mut bytes).expect("read");
    assert_eq!(bytes, payload);
}

#[test]
fn the_listing_wins_over_the_children_when_saving_it_is_enabled() {
    let payload = content(16);
    let data = Node::new()
        .with(
            "dirListing",
            Property::Texts(vec!["_0.si".to_owned(), "listed-only".to_owned()]),
        )
        .with_child("_0.si", streaming_file(&payload, true))
        .with_child("child-only", streaming_file(&payload, true));
    let (_directory, repository) = store("listing-on", lucene_definition(data, None));
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    assert!(directory.listing_was_read());
    assert_eq!(directory.file_names(), ["_0.si", "listed-only"]);

    let (only_children, only_listed) = directory.listing_disagreements().expect("compare");
    assert_eq!(only_children, ["child-only"]);
    assert_eq!(only_listed, ["listed-only"]);
}

#[test]
fn the_children_win_when_saving_the_listing_is_disabled() {
    let payload = content(16);
    let data = Node::new()
        .with("dirListing", Property::Texts(vec!["stale".to_owned()]))
        .with_child("_0.si", streaming_file(&payload, true));
    let (_directory, repository) = store("listing-off", lucene_definition(data, Some(false)));
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    assert!(
        !directory.listing_was_read(),
        "under saveDirectoryListing = false Oak never reads the property"
    );
    assert_eq!(directory.file_names(), ["_0.si"]);
}

#[test]
fn an_end_relative_seek_works_on_the_streaming_encoding() {
    let payload = content(225);
    let (_directory, repository) = store(
        "seek-end-streaming",
        lucene_definition(
            Node::new().with_child("_1.si", streaming_file(&payload, true)),
            None,
        ),
    );
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let file = directory.file("_1.si").expect("open");
    let mut reader = file.reader();
    assert_eq!(reader.seek(SeekFrom::End(-8)).expect("seek"), 217);
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).expect("read");
    assert_eq!(
        tail,
        &payload[217..],
        "the last block is shortened by the sixteen withheld key bytes, so the end is the \
         file's end rather than the blob's"
    );
}

#[test]
fn an_end_relative_seek_works_on_the_buffered_encoding() {
    let payload = content(250);
    let (_directory, repository) = store(
        "seek-end-buffered",
        lucene_definition(
            Node::new().with_child("_1.cfs", buffered_file(&payload, 100, true)),
            None,
        ),
    );
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let file = directory.file("_1.cfs").expect("open");
    let mut reader = file.reader();
    assert_eq!(reader.seek(SeekFrom::End(-10)).expect("seek"), 240);
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).expect("read");
    assert_eq!(tail, &payload[240..]);
}

#[test]
fn a_seek_back_across_a_chunk_boundary_reads_the_earlier_chunk_again() {
    let payload = content(250);
    let (_directory, repository) = store(
        "seek-back",
        lucene_definition(
            Node::new().with_child("_1.cfs", buffered_file(&payload, 100, true)),
            None,
        ),
    );
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let file = directory.file("_1.cfs").expect("open");
    let mut reader = file.reader();

    // Read into the third chunk, then seek back into the first.
    reader.seek(SeekFrom::Start(210)).expect("seek forward");
    let mut forward = [0u8; 10];
    reader.read_exact(&mut forward).expect("read forward");
    assert_eq!(forward, payload[210..220]);

    reader.seek(SeekFrom::Start(5)).expect("seek back");
    let mut back = [0u8; 10];
    reader.read_exact(&mut back).expect("read back");
    assert_eq!(back, payload[5..15]);

    // And a read that starts in one chunk and continues into the next.
    reader
        .seek(SeekFrom::Start(95))
        .expect("seek to a boundary");
    let mut across = Vec::new();
    let mut window = [0u8; 10];
    while across.len() < 10 {
        let read = reader.read(&mut window).expect("read across");
        assert!(read > 0, "the reader must make progress");
        across.extend_from_slice(&window[..read.min(10 - across.len())]);
    }
    assert_eq!(across, payload[95..105]);
}

#[test]
fn a_seek_past_the_end_is_refused_by_name() {
    let payload = content(64);
    let (_directory, repository) = store(
        "seek-past-end",
        lucene_definition(
            Node::new().with_child("_0.si", streaming_file(&payload, true)),
            None,
        ),
    );
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let file = directory.file("_0.si").expect("open");
    let mut reader = file.reader();
    let error = reader
        .seek(SeekFrom::Start(65))
        .expect_err("a position past the end is refused");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        error.to_string().contains("65") && error.to_string().contains("64"),
        "{error}"
    );
}

#[test]
fn a_negative_seek_is_refused_by_name() {
    let payload = content(64);
    let (_directory, repository) = store(
        "seek-negative",
        lucene_definition(
            Node::new().with_child("_0.si", streaming_file(&payload, true)),
            None,
        ),
    );
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let file = directory.file("_0.si").expect("open");
    let mut reader = file.reader();
    let error = reader
        .seek(SeekFrom::Current(-1))
        .expect_err("a negative position is refused");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("negative"), "{error}");
}

#[test]
fn a_jcr_data_of_the_wrong_type_is_refused_rather_than_read_as_zero_length() {
    // Oak takes the buffered branch, finds the property is not BINARIES, and
    // reads the file as zero-length with no error at all. froe refuses.
    let data = Node::new().with_child(
        "_0.si",
        Node::new()
            .with("uniqueKey", Property::Text(UNIQUE_KEY_HEX.to_owned()))
            .with("jcr:data", Property::Text("not a binary".to_owned())),
    );
    let (_directory, repository) = store("wrong-type", lucene_definition(data, None));
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let error = directory.file("_0.si").expect_err("refused");
    assert!(
        matches!(error, IndexError::Record(_)) && error.to_string().contains("silent zero"),
        "{error}"
    );
}

#[test]
fn a_chunk_shorter_than_its_unique_key_is_refused() {
    let data = Node::new().with_child(
        "_0.si",
        Node::new()
            .with("uniqueKey", Property::Text(UNIQUE_KEY_HEX.to_owned()))
            .with("blobSize", Property::Long(100))
            .with("jcr:data", Property::Binaries(vec![vec![0u8; 4]])),
    );
    let (_directory, repository) = store("short-chunk", lucene_definition(data, None));
    let (node, definition) = read_definition(&repository);
    let directory = OakDirectory::open(&repository, &node, &definition, ":data")
        .expect("open")
        .expect("exists");
    let error = directory.file("_0.si").expect_err("refused");
    assert!(error.to_string().contains("unique key"), "{error}");
}

#[test]
fn an_absent_suggest_directory_opens_as_none_rather_than_failing() {
    let payload = content(16);
    let (_directory, repository) = store(
        "no-suggest",
        lucene_definition(
            Node::new().with_child("_0.si", streaming_file(&payload, true)),
            None,
        ),
    );
    let (node, definition) = read_definition(&repository);
    assert!(
        OakDirectory::open(&repository, &node, &definition, ":suggest-data")
            .expect("open")
            .is_none(),
        "Oak's writer rebuilds the suggestions whenever their lastUpdated is missing, so an \
         absent :suggest-data is a legal state"
    );
}
