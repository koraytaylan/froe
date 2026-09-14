//! The synthetic store and dump every Lucene import test works against.
//!
//! Shared by `lucene_import_tests.rs` and `lucene_import_guard_tests.rs`,
//! because a guard test and an outcome test need the same fixture and a
//! second copy of it is a second thing to keep true.

#![allow(
    dead_code,
    reason = "every test binary that links the support module compiles this one too"
)]

use std::path::{Path, PathBuf};

use froe::index::lucene::dump::{DumpOptions, dump_lucene_indexes};
use froe::store::Repository;

use super::property_index_layout::{Node, Property, write_repository_with_tree};

pub(crate) struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-lucene-import-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    pub(crate) fn store(&self) -> PathBuf {
        let store = self.path.join("store");
        std::fs::create_dir_all(&store).expect("create the store directory");
        store
    }

    pub(crate) fn dump(&self) -> PathBuf {
        self.path.join("dump")
    }

    pub(crate) fn input(&self) -> PathBuf {
        self.dump().join("index-dumps")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The committed sample index's files.
pub(crate) fn sample_files() -> Vec<(String, Vec<u8>)> {
    let directory =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lucene-4-7-sample-index");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&directory)
        .expect("read the sample index")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("read"),
            )
        })
        .filter(|(name, _)| name != "README.md")
        .collect();
    files.sort();
    files
}

/// A `:data` child holding `files`.
pub(crate) fn data_directory(files: &[(String, Vec<u8>)]) -> Node {
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

/// How the definition under test is shaped.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    /// The lane, or `None` for a synchronous definition.
    pub(crate) lane: Option<&'static str>,
    /// Whether the lane's checkpoint resolves.
    pub(crate) checkpoint: bool,
    /// Whether the definition also lists `sync`, making it hybrid.
    pub(crate) hybrid: bool,
    /// The `type` of the index a `supersedes` entry names, or `None` for a
    /// definition carrying no `supersedes` at all.
    pub(crate) superseded: Option<&'static str>,
    /// A `compatMode`, which is the one property that can change the
    /// `:version` the import writes.
    pub(crate) compatibility_mode: Option<i64>,
    /// Whether the store also holds a second checkpoint whose root is a
    /// **different** record from the lane's.
    ///
    /// That is what makes the state rule testable: a directory built at a
    /// checkpoint that resolves, and resolves to something other than the
    /// state the lane will resume from.
    pub(crate) rival_checkpoint: bool,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            lane: Some("async"),
            checkpoint: true,
            hybrid: false,
            superseded: None,
            compatibility_mode: None,
            rival_checkpoint: false,
        }
    }
}

/// A store with one Lucene definition holding the sample index.
pub(crate) fn build_store(directory: &TestDirectory, shape: Shape) -> PathBuf {
    let store = directory.store();
    let mut definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("lucene".to_owned()))
        .with_child(":data", data_directory(&sample_files()));
    if let Some(lane) = shape.lane {
        definition = definition.with(
            "async",
            if shape.hybrid {
                Property::Texts(vec![lane.to_owned(), "sync".to_owned()])
            } else {
                Property::Text(lane.to_owned())
            },
        );
    }

    if shape.superseded.is_some() {
        definition = definition.with(
            "supersedes",
            Property::Texts(vec!["/oak:index/old".to_owned()]),
        );
    }
    if let Some(mode) = shape.compatibility_mode {
        definition = definition.with("compatMode", Property::Long(mode));
    }

    let content = Node::new().with_child(
        "page",
        Node::new().with("jcr:title", Property::Text("Alpha".to_owned())),
    );
    let mut definitions = Node::new().with_child("lucene", definition);
    if let Some(kind) = shape.superseded {
        definitions = definitions.with_child(
            "old",
            Node::new()
                .with(
                    "jcr:primaryType",
                    Property::Name("oak:QueryIndexDefinition".to_owned()),
                )
                .with("type", Property::Text(kind.to_owned())),
        );
    }
    let mut root = Node::new()
        .with_child("content", content.clone())
        .with_child("oak:index", definitions);
    if let Some(lane) = shape.lane {
        root = root.with_child(
            ":async",
            Node::new().with(lane, Property::Text("checkpoint-1".to_owned())),
        );
    }

    if shape.checkpoint {
        // The checkpoint's root must be *the content root itself*, because
        // the state rule compares record identity and a checkpoint shares
        // the content root's record by construction.
        let mut checkpoints = vec![("checkpoint-1", root.clone())];
        if shape.rival_checkpoint {
            // A checkpoint that resolves to a different record: one node
            // more under `/content` is enough, since the comparison is by
            // record identity and no two distinct trees share one.
            checkpoints.push((
                "checkpoint-2",
                root.clone().with_child(
                    "other",
                    Node::new().with("jcr:title", Property::Text("Beta".to_owned())),
                ),
            ));
        }
        super::property_index_layout::write_repository_with_checkpoints(
            &store,
            &root,
            &checkpoints,
        );
    } else {
        write_repository_with_tree(&store, &root);
    }
    store
}

/// Dumps `store` into the test directory, returning the input path.
pub(crate) fn dump(directory: &TestDirectory, store: &Path) -> PathBuf {
    dump_lucene_indexes(
        &Repository::open(store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.dump()),
    )
    .expect("dump");
    directory.input()
}

/// The digest lines under `path`.
pub(crate) fn digest_lines(store: &Path, path: &str) -> Vec<String> {
    let repository = Repository::open(store).expect("open");
    let mut rendered = Vec::new();
    froe::tooling::digest::digest_repository_excluding(&repository, &[], &[], &mut rendered)
        .expect("digest");
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

/// The `:data` file bytes, read back through the reader.
pub(crate) fn stored_files(store: &Path) -> Vec<(String, Vec<u8>)> {
    use std::io::Read as _;
    let repository = Repository::open(store).expect("open");
    let node = repository
        .node_at_path("/oak:index/lucene")
        .expect("resolve")
        .expect("exists");
    let definition = froe::index::IndexDefinition::read(&node, "/oak:index/lucene").expect("model");
    let directory =
        froe::index::lucene::OakDirectory::open(&repository, &node, &definition, ":data")
            .expect("open :data")
            .expect(":data exists");
    let mut files = Vec::new();
    for name in directory.file_names() {
        let file = directory.file(name).expect("open");
        let mut bytes = Vec::new();
        file.reader().read_to_end(&mut bytes).expect("read");
        files.push((name.clone(), bytes));
    }
    files.sort();
    files
}
