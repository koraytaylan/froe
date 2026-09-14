//! The collector walk against an independent oracle.
//!
//! The oracle is a second, naive walk written in this file: it visits the
//! tree without the collector's stack, filter dispatch or sink, and derives
//! the same entries the simplest way it can. Comparing the collector against
//! it catches the class of error a single implementation cannot — a rule
//! applied in the wrong order, or to the wrong node — while the fixtures
//! below pin the individual rules.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use froe::content::node::NodeState;
use froe::index::IndexDefinition;
use froe::index::path_filter::PathVerdict;
use froe::index::property::key_encoding::keys_for_property;
use froe::progress::DiscardedProgress;
use froe::store::Repository;
use froe::writer::index::property_collector::{
    CollectedEntries, CollectedReferences, EntrySink, PropertyCollector, ReferenceCollector,
};
use froe::writer::index::{IndexEntry, RunLocation, SortBudget};
use support::property_index_layout::{Node, Property, write_repository_with_tree};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-index-collector-{name}-{}-{:?}",
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

    fn runs(&self) -> PathBuf {
        let runs = self.path.join("runs");
        std::fs::create_dir_all(&runs).expect("create the run directory");
        runs
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A property definition over `property_names`, with the given extras.
fn definition_node(property_names: &[&str]) -> Node {
    Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("property".to_owned()))
        .with(
            "propertyNames",
            Property::Names(
                property_names
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
            ),
        )
}

/// Builds a store whose content root is `content` and whose definition is at
/// `/oak:index/test`, then collects with both sinks and returns the sorted
/// entries, the counted total, and the visited-node count.
fn collect(
    directory: &TestDirectory,
    content: Node,
    definition_node: Node,
) -> (Vec<IndexEntry>, u64, u64) {
    let store = directory.store();
    let mut root = content;
    root = root.with_child("oak:index", Node::new().with_child("test", definition_node));
    write_repository_with_tree(&store, &root);

    let repository = Repository::open(&store).expect("open the repository");
    let content_root = repository.content_root().expect("content root");
    let definition_state = repository
        .node_at_path("/oak:index/test")
        .expect("resolve")
        .expect("the definition exists");
    let definition =
        IndexDefinition::read(&definition_state, "/oak:index/test").expect("model the definition");

    let (counted, count_accounting) = PropertyCollector::collect(
        &content_root,
        &definition,
        &EntrySink::Count,
        &mut DiscardedProgress,
    )
    .expect("count");
    let CollectedEntries::Counted { entries, .. } = counted else {
        panic!("the counting sink returns a count");
    };

    let (sorted, sort_accounting) = PropertyCollector::collect(
        &content_root,
        &definition,
        &EntrySink::Runs {
            location: RunLocation::new(directory.runs(), "entries"),
            budget: SortBudget::of_bytes(64),
        },
        &mut DiscardedProgress,
    )
    .expect("collect");
    let CollectedEntries::Sorted(sorted) = sorted else {
        panic!("the run sink returns a sorted iterator");
    };
    let collected: Vec<IndexEntry> = sorted
        .collect::<froe::Result<Vec<_>>>()
        .expect("read every entry");

    assert_eq!(
        count_accounting.nodes_visited, sort_accounting.nodes_visited,
        "both sinks walk the same tree"
    );
    assert_eq!(
        entries,
        collected.len() as u64,
        "the counting sink must report what the sorted sink produces"
    );
    (collected, entries, count_accounting.nodes_visited)
}

/// A second, naive derivation of the same entries, and the nodes it walked.
///
/// Deliberately written without the collector's machinery: a recursive
/// descent, a `Vec` of results, and the same public key-derivation call. It
/// shares the key encoder — which is the part plan 0006 already proved
/// against Java vectors — and nothing else.
fn oracle(root: &NodeState<'_>, definition: &IndexDefinition) -> (Vec<IndexEntry>, u64) {
    fn descend(
        node: &NodeState<'_>,
        path: &str,
        definition: &IndexDefinition,
        predicate: &froe::index::property::type_predicate::TypePredicate,
        entries: &mut Vec<IndexEntry>,
        visited: &mut u64,
    ) {
        *visited += 1;
        let verdict = definition.path_filter.filter(path);
        if verdict == PathVerdict::Exclude {
            return;
        }
        if verdict == PathVerdict::Include
            && (predicate.is_empty() || predicate.test(node).expect("test the type"))
        {
            let mut keys = BTreeSet::new();
            for name in &definition.property.property_names {
                if name.starts_with(':') {
                    continue;
                }
                if let Some(property) = node.property(name).expect("read a property") {
                    keys.extend(
                        keys_for_property(
                            &property,
                            &definition.property.value_pattern,
                            &definition.path,
                        )
                        .expect("derive keys"),
                    );
                }
            }
            for key in keys {
                entries.push(IndexEntry::new(key, path));
            }
        }
        for (name, child) in node.child_node_entries().expect("children") {
            if name.starts_with(':') {
                continue;
            }
            let child_path = if path == "/" {
                format!("/{name}")
            } else {
                format!("{path}/{name}")
            };
            descend(&child, &child_path, definition, predicate, entries, visited);
        }
    }

    let predicate = froe::index::property::type_predicate::TypePredicate::new(
        root,
        &definition.property.declaring_node_types,
    )
    .expect("build the predicate");
    let mut entries = Vec::new();
    let mut visited = 0;
    descend(
        root,
        "/",
        definition,
        &predicate,
        &mut entries,
        &mut visited,
    );
    entries.sort();
    (entries, visited)
}

/// Asserts the collector agrees with the oracle over `content`.
fn assert_agrees_with_the_oracle(name: &str, content: Node, definition_node: Node) {
    let directory = TestDirectory::new(name);
    let store = directory.store();
    let mut root = content.clone();
    root = root.with_child(
        "oak:index",
        Node::new().with_child("test", definition_node.clone()),
    );
    write_repository_with_tree(&store, &root);
    let repository = Repository::open(&store).expect("open");
    let content_root = repository.content_root().expect("content root");
    let definition_state = repository
        .node_at_path("/oak:index/test")
        .expect("resolve")
        .expect("exists");
    let definition = IndexDefinition::read(&definition_state, "/oak:index/test").expect("model");
    let (expected, walked) = oracle(&content_root, &definition);

    let second = TestDirectory::new(&format!("{name}-collected"));
    let (collected, _, visited) = collect(&second, content, definition_node);
    assert_eq!(
        collected, expected,
        "{name}: the collector differs from the oracle"
    );
    assert_eq!(
        visited, walked,
        "{name}: the two walks visited different node counts"
    );
}

#[test]
fn hidden_children_and_hidden_properties_are_never_indexed() {
    // Every editor runs inside `VisibleEditor`, so neither is ever visited.
    let content = Node::new().with_child(
        "content",
        Node::new()
            .with("title", Property::Text("visible".to_owned()))
            .with(":hidden", Property::Text("invisible".to_owned()))
            .with_child(
                ":storage",
                Node::new().with("title", Property::Text("invisible".to_owned())),
            ),
    );
    let (collected, _, _) = collect(
        &TestDirectory::new("hidden"),
        content,
        definition_node(&["title"]),
    );
    assert_eq!(collected, vec![IndexEntry::new("visible", "/content")]);
}

#[test]
fn two_indexed_properties_sharing_one_value_contribute_one_entry() {
    // Oak's editor derives one key *set* per node, so the union is what is
    // indexed — not one entry per property.
    let content = Node::new().with_child(
        "content",
        Node::new()
            .with("first", Property::Text("shared".to_owned()))
            .with("second", Property::Text("shared".to_owned())),
    );
    let (collected, _, _) = collect(
        &TestDirectory::new("shared-value"),
        content,
        definition_node(&["first", "second"]),
    );
    assert_eq!(collected, vec![IndexEntry::new("shared", "/content")]);
}

#[test]
fn an_excluded_subtree_is_pruned_and_a_traversed_one_is_descended() {
    let content = Node::new()
        .with_child(
            "keep",
            Node::new()
                .with("title", Property::Text("kept".to_owned()))
                .with_child(
                    "deep",
                    Node::new().with("title", Property::Text("deeper".to_owned())),
                ),
        )
        .with_child(
            "drop",
            Node::new().with("title", Property::Text("dropped".to_owned())),
        );
    let definition = definition_node(&["title"])
        .with("includedPaths", Property::Texts(vec!["/keep".to_owned()]));
    let (collected, _, _) = collect(&TestDirectory::new("filter"), content, definition);
    // `/` is `Traverse` — descended, not indexed — and `/drop` is `Exclude`.
    assert_eq!(
        collected,
        vec![
            IndexEntry::new("deeper", "/keep/deep"),
            IndexEntry::new("kept", "/keep"),
        ]
    );
}

#[test]
fn the_collector_agrees_with_an_independent_walk() {
    let content = Node::new()
        .with_child(
            "content",
            Node::new()
                .with("title", Property::Text("a b".to_owned()))
                .with_child(
                    "page",
                    Node::new().with(
                        "title",
                        Property::Texts(vec!["x".to_owned(), "y".to_owned(), "x".to_owned()]),
                    ),
                )
                .with_child(
                    "empty",
                    Node::new().with("title", Property::Text(String::new())),
                ),
        )
        .with_child(
            "other",
            Node::new().with("title", Property::Text("slash/value".to_owned())),
        );
    assert_agrees_with_the_oracle("oracle", content, definition_node(&["title"]));
}

#[test]
fn a_strong_reference_under_version_storage_is_skipped_and_a_weak_one_is_kept() {
    // `isVersionStorePath` is a plain string prefix test, so a sibling named
    // `jcr:versionStorage2` is excluded too. Oak's quirk, reproduced.
    let identifier = "aaaaaaaa-1111-4111-8111-111111111111";
    let content = Node::new()
        .with_child(
            "jcr:system",
            Node::new()
                .with_child(
                    "jcr:versionStorage",
                    Node::new()
                        .with("strong", Property::Reference(identifier.to_owned()))
                        .with("weak", Property::WeakReference(identifier.to_owned())),
                )
                .with_child(
                    "jcr:versionStorage2",
                    Node::new().with("strong", Property::Reference(identifier.to_owned())),
                ),
        )
        .with_child(
            "content",
            Node::new().with("strong", Property::Reference(identifier.to_owned())),
        );

    let directory = TestDirectory::new("version-storage");
    let store = directory.store();
    write_repository_with_tree(&store, &content);
    let repository = Repository::open(&store).expect("open");
    let content_root = repository.content_root().expect("content root");
    let (collected, _) =
        ReferenceCollector::collect(&content_root, &EntrySink::Count, &mut DiscardedProgress)
            .expect("collect");
    let CollectedReferences::Counted { strong, weak, .. } = collected else {
        panic!("the counting sink returns counts");
    };
    assert_eq!(
        strong, 1,
        "only /content/@strong survives the version-store test"
    );
    assert_eq!(weak, 1, "a weak reference under version storage is kept");
}

#[test]
fn a_multi_valued_reference_listing_one_identifier_twice_is_one_entry() {
    let identifier = "bbbbbbbb-2222-4222-8222-222222222222";
    let content = Node::new().with_child(
        "content",
        Node::new().with(
            "refs",
            Property::References(vec![identifier.to_owned(), identifier.to_owned()]),
        ),
    );
    let directory = TestDirectory::new("repeated-reference");
    let store = directory.store();
    write_repository_with_tree(&store, &content);
    let repository = Repository::open(&store).expect("open");
    let content_root = repository.content_root().expect("content root");
    let (collected, _) =
        ReferenceCollector::collect(&content_root, &EntrySink::Count, &mut DiscardedProgress)
            .expect("collect");
    let CollectedReferences::Counted { strong, weak, .. } = collected else {
        panic!("counts");
    };
    assert_eq!(strong, 1, "Oak collects the values into a set");
    assert_eq!(weak, 0);
}

#[test]
fn a_store_with_no_weak_reference_reports_its_weak_set_empty() {
    // Oak creates `:weakreferences` only on the first insert, so a set that
    // stayed empty must produce no hidden child at all.
    let identifier = "cccccccc-3333-4333-8333-333333333333";
    let content = Node::new().with_child(
        "content",
        Node::new().with("ref", Property::Reference(identifier.to_owned())),
    );
    let directory = TestDirectory::new("no-weak");
    let store = directory.store();
    write_repository_with_tree(&store, &content);
    let repository = Repository::open(&store).expect("open");
    let content_root = repository.content_root().expect("content root");
    let (collected, _) = ReferenceCollector::collect(
        &content_root,
        &EntrySink::Runs {
            location: RunLocation::new(directory.runs(), "references"),
            budget: SortBudget::of_bytes(1 << 20),
        },
        &mut DiscardedProgress,
    )
    .expect("collect");
    let CollectedReferences::Sorted(mut sets) = collected else {
        panic!("the run sink returns sorted sets");
    };
    assert!(sets.has_strong(), "the strong set received an entry");
    assert!(!sets.has_weak(), "the weak set stayed empty");

    let strong: Vec<IndexEntry> = sets
        .strong()
        .expect("open the strong set")
        .expect("the strong set exists")
        .collect::<froe::Result<Vec<_>>>()
        .expect("read");
    // The key is the identifier unencoded; the value is the property's path
    // made relative by stripping the leading `/`.
    assert_eq!(strong, vec![IndexEntry::new(identifier, "content/ref")]);
}
