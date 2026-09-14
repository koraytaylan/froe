//! Fixtures shared by the reindex test binaries.
//!
//! The stores here are written by the independent encoder of
//! [`super::property_index_layout`], so a reindex is always run against a
//! repository whose bytes no production code produced.

#![allow(dead_code, reason = "each binary uses a different part of this module")]

use std::path::PathBuf;

use froe::store::Repository;
use froe::tooling::digest::digest_repository_excluding;
use froe::writer::index::{ReindexOptions, WorkDirectory};

use super::property_index_layout::{Node, Property, write_repository_with_tree};

pub(crate) struct TestDirectory {
    pub(crate) path: PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-index-reindex-{name}-{}-{:?}",
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

    pub(crate) fn work(&self) -> PathBuf {
        let work = self.path.join("work");
        std::fs::create_dir_all(&work).expect("create the work directory");
        work
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A definition node of `index_type`, flagged for reindex.
pub(crate) fn definition(index_type: &str, extras: Vec<(&str, Property)>) -> Node {
    let mut node = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text(index_type.to_owned()))
        .with("reindex", Property::Boolean(true));
    for (name, value) in extras {
        node = node.with(name, value);
    }
    node
}

/// A store with content and one flagged `jcr:title` property index, plus the
/// `nodetype` definition the path service requires.
pub(crate) fn store_with_a_flagged_title_index(directory: &TestDirectory) -> PathBuf {
    let store = directory.store();
    // Every indexed node declares its type, because the definition below
    // carries a `declaringNodeTypes` that the collector honours.
    let typed = || {
        Node::new().with(
            "jcr:primaryType",
            Property::Name("nt:unstructured".to_owned()),
        )
    };
    let content = Node::new()
        .with_child(
            "content",
            typed()
                .with("jcr:title", Property::Text("Alpha".to_owned()))
                .with_child(
                    "page",
                    typed().with("jcr:title", Property::Text("Beta".to_owned())),
                )
                // An empty value, whose key is the hidden name `:` — a
                // reader that filtered hidden names at the key level would
                // lose it.
                .with_child(
                    "blank",
                    typed().with("jcr:title", Property::Text(String::new())),
                ),
        )
        .with_child(
            "oak:index",
            Node::new()
                .with_child(
                    "nodetype",
                    Node::new()
                        .with(
                            "jcr:primaryType",
                            Property::Name("oak:QueryIndexDefinition".to_owned()),
                        )
                        .with("type", Property::Text("property".to_owned()))
                        .with(
                            "propertyNames",
                            Property::Names(vec!["jcr:primaryType".to_owned()]),
                        ),
                )
                .with_child(
                    "title",
                    definition(
                        "property",
                        vec![
                            (
                                "propertyNames",
                                Property::Names(vec!["jcr:title".to_owned()]),
                            ),
                            ("info", Property::Text("kept verbatim".to_owned())),
                            (
                                "includedPaths",
                                Property::Texts(vec!["/content".to_owned()]),
                            ),
                            (
                                "declaringNodeTypes",
                                Property::Names(vec!["nt:unstructured".to_owned()]),
                            ),
                        ],
                    ),
                ),
        );
    write_repository_with_tree(&store, &content);
    store
}

/// Options spilling under the test's own work directory.
pub(crate) fn options(directory: &TestDirectory) -> ReindexOptions {
    ReindexOptions::new()
        .with_work_directory(WorkDirectory::OperatorNamed(directory.work()))
        // Small, so the sort spills and the merge path is exercised rather
        // than the single-run one.
        .with_sort_budget_bytes(32)
}

/// The whole repository digested with `/oak:index` excluded — the library
/// entry behind `froe digest --exclude-subtree /oak:index`.
///
/// This is the content-tree proof in one value: a reindex may add and
/// rewrite whatever it likes under `/oak:index`, and everything else must
/// come back byte for byte.
pub(crate) fn content_digest(store: &std::path::Path) -> String {
    let repository = Repository::open(store).expect("open the repository");
    let mut rendered = Vec::new();
    digest_repository_excluding(&repository, &["/oak:index".to_owned()], &[], &mut rendered)
        .expect("digest");
    String::from_utf8(rendered).expect("UTF-8")
}

/// The digest lines under `path`, prefix removed.
pub(crate) fn digest_lines(store: &std::path::Path, path: &str) -> Vec<String> {
    let repository = Repository::open(store).expect("open the repository");
    let mut rendered = Vec::new();
    digest_repository_excluding(&repository, &[], &[], &mut rendered).expect("digest");
    let digest = String::from_utf8(rendered).expect("UTF-8");
    digest
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(path)?;
            (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\t'))
                .then(|| rest.to_owned())
        })
        .collect()
}

/// Every file in the store by name and content, except the lock — which
/// preparing a run creates whether or not the run goes on to write anything.
pub(crate) fn store_contents(store: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut contents: Vec<(String, Vec<u8>)> = std::fs::read_dir(store)
        .expect("read the store directory")
        .filter_map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            (name != "repo.lock").then(|| {
                (
                    name,
                    std::fs::read(entry.path()).expect("read a store file"),
                )
            })
        })
        .collect();
    contents.sort();
    contents
}

/// How many lines `journal.log` holds.
pub(crate) fn journal_lines(store: &std::path::Path) -> usize {
    std::fs::read_to_string(store.join("journal.log"))
        .expect("read the journal")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

/// A store carrying one definition of every type froe rebuilds, each
/// flagged, over content each one covers.
pub(crate) fn store_with_every_supported_type(directory: &TestDirectory) -> PathBuf {
    let store = directory.store();
    let content = Node::new()
        .with_child(
            "content",
            Node::new()
                .with("jcr:title", Property::Text("Alpha".to_owned()))
                .with("uid", Property::Text("u-1".to_owned()))
                .with_child(
                    "page",
                    Node::new()
                        .with("jcr:title", Property::Text("Beta".to_owned()))
                        .with("uid", Property::Text("u-2".to_owned()))
                        .with(
                            "link",
                            Property::Reference("11111111-2222-3333-4444-555555555555".to_owned()),
                        )
                        .with(
                            "soft",
                            Property::WeakReference(
                                "66666666-7777-8888-9999-aaaaaaaaaaaa".to_owned(),
                            ),
                        ),
                ),
        )
        .with_child(
            "oak:index",
            Node::new()
                .with_child(
                    "nodetype",
                    Node::new()
                        .with(
                            "jcr:primaryType",
                            Property::Name("oak:QueryIndexDefinition".to_owned()),
                        )
                        .with("type", Property::Text("property".to_owned()))
                        .with(
                            "propertyNames",
                            Property::Names(vec!["jcr:primaryType".to_owned()]),
                        ),
                )
                .with_child(
                    "title",
                    definition(
                        "property",
                        vec![(
                            "propertyNames",
                            Property::Names(vec!["jcr:title".to_owned()]),
                        )],
                    ),
                )
                .with_child(
                    "uid",
                    definition(
                        "property",
                        vec![
                            ("propertyNames", Property::Names(vec!["uid".to_owned()])),
                            ("unique", Property::Boolean(true)),
                        ],
                    ),
                )
                .with_child("reference", definition("reference", Vec::new()))
                .with_child(
                    "counter",
                    definition(
                        "counter",
                        vec![
                            ("seed", Property::Long(-7_610_761_686_379_641_542)),
                            ("resolution", Property::Long(4)),
                        ],
                    ),
                ),
        );
    write_repository_with_tree(&store, &content);
    store
}

/// A counter parked on a lane that does not exist, already carrying an
/// `:index` from an earlier indexing run.
pub(crate) fn store_with_a_counter_on_an_absent_lane(directory: &TestDirectory) -> PathBuf {
    let store = directory.store();
    let root = Node::new()
        .with_child(
            "content",
            Node::new().with(
                "jcr:primaryType",
                Property::Name("nt:unstructured".to_owned()),
            ),
        )
        .with_child(
            "oak:index",
            Node::new()
                .with_child(
                    "nodetype",
                    Node::new()
                        .with(
                            "jcr:primaryType",
                            Property::Name("oak:QueryIndexDefinition".to_owned()),
                        )
                        .with("type", Property::Text("property".to_owned()))
                        .with(
                            "propertyNames",
                            Property::Names(vec!["jcr:primaryType".to_owned()]),
                        ),
                )
                .with_child(
                    "counter",
                    Node::new()
                        .with(
                            "jcr:primaryType",
                            Property::Name("oak:QueryIndexDefinition".to_owned()),
                        )
                        .with("type", Property::Text("counter".to_owned()))
                        .with("reindex", Property::Boolean(true))
                        .with("async", Property::Text("async".to_owned()))
                        .with("resolution", Property::Long(8))
                        .with("info", Property::Text("kept verbatim".to_owned()))
                        // What an earlier indexing run left, and what a
                        // reset removes.
                        .with_child(":index", Node::new().with(":cnt", Property::Long(40))),
                ),
        );
    write_repository_with_tree(&store, &root);
    store
}

/// Options that authorize proceeding when a lane cannot be resolved.
pub(crate) fn from_head(directory: &TestDirectory) -> ReindexOptions {
    options(directory).with_from_head(true)
}
