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
    // Without the approximate counters. `index-property-storage.md` §11:
    // their name, presence and value are each drawn from a random
    // generator, so *any* two rebuilds of one tree disagree on them —
    // froe's two as much as Oak's two. A comparison of two runs that kept
    // them would be comparing the random draws.
    digest_repository_excluding(
        &repository,
        &[],
        &[froe::writer::index::approximate_counter::COUNT_PROPERTY_PREFIX.to_owned()],
        &mut rendered,
    )
    .expect("digest");
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

/// A store with content and one flagged `lucene` definition of the shape
/// the interop fixture's default definition has: an `nt:base` rule whose
/// catch-all property definition is analyzed and `nodeScopeIndex`, with
/// path restrictions on.
///
/// The node types are written too, because a rule over `nt:base` reaches a
/// node of another type only through the registry.
pub(crate) fn store_with_a_flagged_lucene_index(directory: &TestDirectory) -> PathBuf {
    write_lucene_store(directory, lucene_definition(Vec::new(), Vec::new()))
}

/// The `lucene` definition, with extra definition properties and extra
/// property definitions a case names.
pub(crate) fn lucene_definition(
    extras: Vec<(&str, Property)>,
    properties: Vec<(&str, Node)>,
) -> Node {
    lucene_definition_over("nt:base", extras, properties)
}

/// The same over a named node type, for a case that needs the rule to
/// cover something else — or nothing.
pub(crate) fn lucene_definition_over(
    node_type: &str,
    extras: Vec<(&str, Property)>,
    properties: Vec<(&str, Node)>,
) -> Node {
    let mut catch_all = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("nt:unstructured".to_owned()),
        )
        .with("name", Property::Text("^[^\\/]*$".to_owned()))
        .with("isRegexp", Property::Boolean(true))
        .with("analyzed", Property::Boolean(true))
        .with("nodeScopeIndex", Property::Boolean(true));
    let _ = &mut catch_all;
    let mut property_node = Node::new().with(
        "jcr:primaryType",
        Property::Name("nt:unstructured".to_owned()),
    );
    // The analyzed catch-all is what makes the definition fulltext-enabled,
    // and therefore an `oakCodec` one. A case that needs a definition
    // *without* it states its own property set and passes `no-catch-all`.
    if !extras.iter().any(|(name, _)| *name == "no-catch-all") {
        property_node = property_node.with_child("all", catch_all);
    }
    for (name, node) in properties {
        property_node = property_node.with_child(name, node);
    }
    let rule = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("nt:unstructured".to_owned()),
        )
        .with_child("properties", property_node);
    let rules = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("nt:unstructured".to_owned()),
        )
        .with_child(node_type, rule);
    let mut node = definition(
        "lucene",
        if extras.iter().any(|(name, _)| *name == "no-async") {
            vec![("evaluatePathRestrictions", Property::Boolean(true))]
        } else {
            vec![
                ("async", Property::Text("async".to_owned())),
                ("evaluatePathRestrictions", Property::Boolean(true)),
            ]
        },
    );
    for (name, value) in extras {
        if name == "no-async" || name == "no-catch-all" {
            continue;
        }
        node = node.with(name, value);
    }
    node.with_child("indexRules", rules)
}

/// Writes a store holding `definition` at `/oak:index/lucene`, the
/// `nodetype` definition the path service requires, a small content tree
/// and the node types the rules resolve through.
pub(crate) fn write_lucene_store(directory: &TestDirectory, definition_node: Node) -> PathBuf {
    let store = directory.store();
    // A Lucene definition is rebuilt from its lane's checkpoint, so the
    // fixture carries the lane, the checkpoint it names and the state that
    // checkpoint pins.
    let typed = || {
        Node::new().with(
            "jcr:primaryType",
            Property::Name("nt:unstructured".to_owned()),
        )
    };
    let content_tree = || {
        typed()
            .with("jcr:title", Property::Text("Alpha One".to_owned()))
            .with_child(
                "page",
                typed().with("jcr:title", Property::Text("Beta Two".to_owned())),
            )
    };
    let content = Node::new()
        .with_child("content", content_tree())
        .with_child(
            ":async",
            Node::new().with("async", Property::Text("lane-checkpoint".to_owned())),
        )
        .with_child(
            "jcr:system",
            Node::new().with_child(
                "jcr:nodeTypes",
                Node::new().with_child(
                    "nt:base",
                    Node::new().with(
                        "rep:primarySubtypes",
                        Property::Names(vec!["nt:unstructured".to_owned(), "nt:file".to_owned()]),
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
                .with_child("lucene", definition_node),
        );
    let pinned = Node::new()
        .with_child("content", content_tree())
        .with_child(
            "jcr:system",
            Node::new().with_child(
                "jcr:nodeTypes",
                Node::new().with_child(
                    "nt:base",
                    Node::new().with(
                        "rep:primarySubtypes",
                        Property::Names(vec!["nt:unstructured".to_owned(), "nt:file".to_owned()]),
                    ),
                ),
            ),
        );
    super::property_index_layout::write_repository_with_checkpoints(
        &store,
        &content,
        &[("lane-checkpoint", pinned)],
    );
    store
}

/// The same store, with the lane naming a checkpoint that is not there.
pub(crate) fn write_lucene_store_without_a_checkpoint(directory: &TestDirectory) -> PathBuf {
    let store = directory.store();
    let typed = || {
        Node::new().with(
            "jcr:primaryType",
            Property::Name("nt:unstructured".to_owned()),
        )
    };
    let content = Node::new()
        .with_child("content", typed())
        .with_child(
            ":async",
            Node::new().with("async", Property::Text("lane-checkpoint".to_owned())),
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
                    "lucene",
                    // A `:data` from a previous cycle, which is what a reset
                    // removes: a definition with none is `NothingToDo`.
                    lucene_definition(Vec::new(), Vec::new()).with_child(":data", Node::new()),
                ),
        );
    write_repository_with_tree(&store, &content);
    store
}

/// Options with a binary-text policy, which a Lucene definition cannot be
/// rebuilt without.
pub(crate) fn lucene_options(directory: &TestDirectory) -> ReindexOptions {
    options(directory).with_binary_text_policy(
        froe::index::lucene::documents::binaries::BinaryTextPolicy::new(
            froe::index::lucene::documents::binaries::BinaryTextFallback::Marker,
        ),
    )
}
