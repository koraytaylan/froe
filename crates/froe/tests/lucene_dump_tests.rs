//! `froe index dump`, over synthetic stores.
//!
//! The claims here are the safety case's: the store is read-only throughout,
//! the bytes that come out are the bytes that went in, an observed dump
//! equals an unobserved one, and a dump that fails partway leaves its
//! froe-named temporaries removed and the completed file set intact.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::{Path, PathBuf};

use froe::index::lucene::dump::{
    DumpOptions, INDEX_DEFINITIONS_FILE_NAME, INDEX_DUMPS_DIRECTORY_NAME, NoCheckpointReason,
    dump_lucene_indexes, dump_lucene_indexes_with_progress,
};
use froe::index::lucene::layout::{INDEX_DETAILS_FILE_NAME, INDEXER_INFO_FILE_NAME};
use froe::store::Repository;
use support::filesystem_snapshot::directory_snapshot;
use support::property_index_layout::{Node, Property, write_repository_with_tree};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-lucene-dump-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    fn store(&self) -> PathBuf {
        let store = self.path.join("store");
        std::fs::create_dir_all(&store).expect("create the store directory");
        store
    }

    fn output(&self) -> PathBuf {
        self.path.join("output")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The committed sample index's files, as bytes.
fn sample_files() -> Vec<(String, Vec<u8>)> {
    let directory =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lucene-4-7-sample-index");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&directory)
        .expect("read the sample index")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("read a sample file"),
            )
        })
        .filter(|(name, _)| name != "README.md")
        .collect();
    files.sort();
    files
}

/// A `:data` child holding `files`, in the streaming encoding.
fn streaming_data(files: &[(String, Vec<u8>)]) -> Node {
    let mut data = Node::new().with(
        "dirListing",
        Property::Texts(files.iter().map(|(name, _)| name.clone()).collect()),
    );
    for (name, bytes) in files {
        data = data.with_child(
            name,
            Node::new()
                .with("blobSize", Property::Long(1_047_552))
                .with("jcr:data", Property::Binary(bytes.clone())),
        );
    }
    data
}

/// The same, in the buffered encoding: one binary per chunk.
fn buffered_data(files: &[(String, Vec<u8>)], blob_size: usize) -> Node {
    let mut data = Node::new().with(
        "dirListing",
        Property::Texts(files.iter().map(|(name, _)| name.clone()).collect()),
    );
    for (name, bytes) in files {
        let chunks: Vec<Vec<u8>> = bytes.chunks(blob_size.max(1)).map(<[u8]>::to_vec).collect();
        data = data.with_child(
            name,
            Node::new()
                .with("blobSize", Property::Long(blob_size as i64))
                .with("jcr:data", Property::Binaries(chunks)),
        );
    }
    data
}

/// A store whose `/oak:index/lucene` carries `data`, on `lane`.
fn store_with(
    directory: &TestDirectory,
    data: Node,
    lane: Option<&str>,
    checkpoint: bool,
) -> PathBuf {
    let store = directory.store();
    let mut definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("lucene".to_owned()))
        .with_child(":data", data);
    if let Some(lane) = lane {
        definition = definition.with("async", Property::Text(lane.to_owned()));
    }
    let mut root =
        Node::new().with_child("oak:index", Node::new().with_child("lucene", definition));
    if let Some(lane) = lane {
        root = root.with_child(
            ":async",
            Node::new().with(lane, Property::Text("checkpoint-1".to_owned())),
        );
    }
    if lane.is_some() && checkpoint {
        // Checkpoints hang off the **super-root**, not the content root.
        // A `checkpoints` child of `/` is an ordinary content node and is
        // not what Oak — or froe — resolves a lane checkpoint through.
        support::property_index_layout::write_repository_with_checkpoints(
            &store,
            &root,
            &[("checkpoint-1", root.clone())],
        );
    } else {
        write_repository_with_tree(&store, &root);
    }
    store
}

/// The files a dump wrote for `/oak:index/lucene`, by name.
fn dumped_files(output: &Path) -> Vec<(String, Vec<u8>)> {
    let data = output
        .join(INDEX_DUMPS_DIRECTORY_NAME)
        .join("lucene")
        .join("data");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&data)
        .unwrap_or_else(|error| panic!("read {}: {error}", data.display()))
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("read a dumped file"),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn a_dump_reproduces_the_streaming_bytes_and_leaves_the_store_untouched() {
    let directory = TestDirectory::new("streaming");
    let files = sample_files();
    let store = store_with(&directory, streaming_data(&files), Some("async"), true);
    let before = directory_snapshot(&store);

    let outcome = dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect("dump");

    assert_eq!(dumped_files(&directory.output()), files, "byte for byte");
    assert_eq!(outcome.file_count(), files.len());
    assert!(outcome.is_importable(), "{outcome:?}");
    assert_eq!(outcome.checkpoint.as_deref(), Some("checkpoint-1"));

    // The safety case's testable invariant: the store is byte-identical,
    // `repo.lock` included, because a dump never creates it.
    assert_eq!(
        directory_snapshot(&store),
        before,
        "a dump must not write a byte inside the store"
    );
    assert!(
        !store.join("repo.lock").exists(),
        "a dump must not take the lock"
    );
}

#[test]
fn a_dump_reproduces_the_buffered_bytes_too() {
    // The two encodings of §3 must both come back out as the file that
    // went in; a reader that mishandled the chunked form would differ here
    // and nowhere else.
    let directory = TestDirectory::new("buffered");
    let files = sample_files();
    let store = store_with(&directory, buffered_data(&files, 64), Some("async"), true);

    dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect("dump");

    assert_eq!(dumped_files(&directory.output()), files);
}

#[test]
fn the_three_metadata_files_are_where_oak_runs_reader_expects_them() {
    let directory = TestDirectory::new("metadata");
    let store = store_with(
        &directory,
        streaming_data(&sample_files()),
        Some("async"),
        true,
    );
    dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect("dump");

    let dumps = directory.output().join(INDEX_DUMPS_DIRECTORY_NAME);
    // oak-run's importer scans this directory's direct children for
    // `index-details.txt`, so the per-index file goes one level down and
    // the other two sit here.
    assert!(dumps.join(INDEXER_INFO_FILE_NAME).is_file());
    assert!(dumps.join(INDEX_DEFINITIONS_FILE_NAME).is_file());
    assert!(dumps.join("lucene").join(INDEX_DETAILS_FILE_NAME).is_file());

    let details = std::fs::read_to_string(dumps.join("lucene").join(INDEX_DETAILS_FILE_NAME))
        .expect("read index-details.txt");
    // Java's `Properties.store` escapes `=`, `:`, `#` and `!` in values as
    // well as keys — `saveConvert` does it unconditionally — so a JCR path
    // appears with its colon escaped. That is what Oak's own reader expects
    // to parse back, and froe renders it the same way.
    assert!(
        details.contains(r"indexPath=/oak\:index/lucene"),
        "{details}"
    );
    // The mapping is keyed by the filesystem name and holds the JCR name.
    assert!(details.contains(r"dir.data=\:data"), "{details}");

    // A round trip is the claim that matters: whatever the escaping, Oak's
    // reader and froe's must agree on what comes back.
    let parsed = froe::index::lucene::layout::IndexDetails::parse(&details)
        .expect("parse the file froe just wrote");
    assert_eq!(parsed.index_path, "/oak:index/lucene");
    assert_eq!(parsed.jcr_name_for("data"), Some(":data"));

    let info = std::fs::read_to_string(dumps.join(INDEXER_INFO_FILE_NAME))
        .expect("read indexer-info.properties");
    assert_eq!(
        froe::index::lucene::layout::IndexerInfo::parse(&info)
            .expect("parse indexer-info.properties")
            .checkpoint,
        "checkpoint-1"
    );
}

#[test]
fn a_synchronous_definition_dumps_as_a_backup_without_an_indexer_info() {
    // oak-run cannot import such a directory either. The files are still
    // worth having, so they are written and the reason is reported.
    let directory = TestDirectory::new("synchronous");
    let store = store_with(&directory, streaming_data(&sample_files()), None, false);

    let outcome = dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect("dump");

    assert!(!outcome.is_importable());
    assert!(
        matches!(
            outcome.no_checkpoint_reason,
            Some(NoCheckpointReason::SynchronousDefinition { .. })
        ),
        "{outcome:?}"
    );
    assert!(
        !directory
            .output()
            .join(INDEX_DUMPS_DIRECTORY_NAME)
            .join(INDEXER_INFO_FILE_NAME)
            .exists(),
        "no checkpoint, no indexer-info.properties"
    );
    // The files are there regardless: this is a backup.
    assert_eq!(dumped_files(&directory.output()), sample_files());
}

#[test]
fn a_dangling_lane_checkpoint_writes_no_indexer_info_and_names_it() {
    let directory = TestDirectory::new("dangling");
    let store = store_with(
        &directory,
        streaming_data(&sample_files()),
        Some("async"),
        false,
    );

    let outcome = dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect("dump");

    match outcome.no_checkpoint_reason {
        Some(NoCheckpointReason::DanglingCheckpoint { lane, checkpoint }) => {
            assert_eq!(lane, "async");
            assert_eq!(checkpoint, "checkpoint-1");
        }
        other => panic!("expected a dangling checkpoint, found {other:?}"),
    }
}

#[test]
fn an_output_directory_inside_the_store_is_refused() {
    let directory = TestDirectory::new("inside");
    let store = store_with(
        &directory,
        streaming_data(&sample_files()),
        Some("async"),
        true,
    );

    let error = dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), store.join("dump-here")),
    )
    .expect_err("an output path inside the store must be refused");
    assert!(
        error
            .to_string()
            .contains("inside the repository directory"),
        "{error}"
    );
}

#[test]
fn an_existing_dump_is_never_written_over() {
    let directory = TestDirectory::new("never-overwrite");
    let store = store_with(
        &directory,
        streaming_data(&sample_files()),
        Some("async"),
        true,
    );
    dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect("the first dump");

    let error = dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect_err("a second dump over the first must be refused");
    assert!(
        error.to_string().contains("already holds a dump"),
        "the refusal names the remedy: {error}"
    );
    assert!(error.to_string().contains("Delete it and rerun"), "{error}");
}

#[test]
fn a_partially_populated_output_is_refused_rather_than_completed() {
    // The residue an interrupted dump leaves. Completing it would produce a
    // directory that is part one dump and part another.
    let directory = TestDirectory::new("partial");
    let store = store_with(
        &directory,
        streaming_data(&sample_files()),
        Some("async"),
        true,
    );
    let dumps = directory.output().join(INDEX_DUMPS_DIRECTORY_NAME);
    std::fs::create_dir_all(dumps.join("lucene")).expect("plant a partial dump");

    let error = dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect_err("a partial dump must be refused");
    assert!(
        error.to_string().contains("already holds a dump"),
        "{error}"
    );
}

#[test]
fn an_observed_dump_equals_an_unobserved_one() {
    let plain = TestDirectory::new("observed-plain");
    let plain_store = store_with(&plain, streaming_data(&sample_files()), Some("async"), true);
    let unobserved = dump_lucene_indexes(
        &Repository::open(&plain_store).expect("open"),
        &DumpOptions::new(Vec::new(), plain.output()),
    )
    .expect("dump");

    let observed_directory = TestDirectory::new("observed");
    let observed_store = store_with(
        &observed_directory,
        streaming_data(&sample_files()),
        Some("async"),
        true,
    );
    let mut log = support::observation_log::ObservationLog::default();
    let observed = dump_lucene_indexes_with_progress(
        &Repository::open(&observed_store).expect("open"),
        &DumpOptions::new(Vec::new(), observed_directory.output()),
        &mut log,
    )
    .expect("observed dump");

    assert_eq!(observed.indexes, unobserved.indexes);
    assert_eq!(observed.checkpoint, unobserved.checkpoint);
    assert!(log.began_and_ended_in_pairs());
    assert_eq!(
        dumped_files(&observed_directory.output()),
        dumped_files(&plain.output()),
        "observation changed what was written"
    );
}

#[test]
fn a_dump_of_nothing_writes_the_directory_and_says_it_is_not_importable() {
    let directory = TestDirectory::new("empty");
    let store = directory.store();
    write_repository_with_tree(&store, &Node::new().with_child("content", Node::new()));

    let outcome = dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.output()),
    )
    .expect("dump");

    assert!(outcome.indexes.is_empty());
    assert!(!outcome.is_importable());
    assert_eq!(outcome.no_checkpoint_reason, None);
}
