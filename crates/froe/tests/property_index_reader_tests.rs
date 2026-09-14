//! The property family's readers and consistency check, against `:index`
//! subtrees an independent encoder wrote.
//!
//! The key derivation, the value pattern and the path filter have their own
//! unit tests beside the implementation. What these cover is the reading of a
//! storage subtree and the check over it: that a `match` on an interior node
//! is an entry, that the empty value's `:` key is not mistaken for a hidden
//! child, that the unique strategy's `entry` array is read whole, that each
//! injected defect is reported by path and kind, and that both budgets accept
//! at their limit and refuse at the limit plus one.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::PathBuf;

use froe::index::property::consistency::{EntryCheckBudget, NodeCheckBudget, check, check_entries};
use froe::index::property::{MirrorIndex, TypePredicate, UniqueIndex};
use froe::index::{IndexDefinition, IndexError};
use froe::store::Repository;
use support::property_index_layout::write_repository_with_tree;
use support::property_index_layout::{Node, Property, mirror_storage, unique_storage};

/// A directory that removes itself.
struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-property-index-{name}-{}-{:?}",
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

/// A `property` definition node carrying whatever storage the caller built.
fn property_definition(property_name: &str, unique: bool, storage: Node) -> Node {
    let mut definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("property".to_owned()))
        .with(
            "propertyNames",
            Property::Names(vec![property_name.to_owned()]),
        )
        .with_child(":index", storage);
    if unique {
        definition = definition.with("unique", Property::Boolean(true));
    }
    definition
}

/// A content node carrying one `STRING` property.
fn content_node(property_name: &str, value: &str) -> Node {
    Node::new().with(property_name, Property::Text(value.to_owned()))
}

/// Assembles a store: `/oak:index/<name>` plus whatever content the caller
/// supplies, and returns the opened repository beside the directory that
/// owns it.
fn store(
    directory_name: &str,
    definition_name: &str,
    definition: Node,
    content: Vec<(&str, Node)>,
) -> (TestDirectory, Repository) {
    let directory = TestDirectory::new(directory_name);
    let mut root = Node::new().with_child(
        "oak:index",
        Node::new().with_child(definition_name, definition),
    );
    for (name, node) in content {
        root = root.with_child(name, node);
    }
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open the repository");
    (directory, repository)
}

/// Reads the definition model and the definition node back.
fn read_definition<'repository>(
    repository: &'repository Repository,
    name: &str,
) -> (froe::content::NodeState<'repository>, IndexDefinition) {
    let path = format!("/oak:index/{name}");
    let node = repository
        .node_at_path(&path)
        .expect("resolve the definition")
        .expect("the definition exists");
    let definition = IndexDefinition::read(&node, &path).expect("the definition reads");
    (node, definition)
}

#[test]
fn a_mirror_entry_on_an_interior_node_is_an_entry_and_so_are_its_descendants() {
    // `/content` and `/content/page` both carry the value, so `match` lands
    // on an interior node — the case the real Sling fixture has.
    let storage = mirror_storage(&[("alpha", "/content"), ("alpha", "/content/page")]);
    let (_directory, repository) = store(
        "interior",
        "test",
        property_definition("jcr:title", false, storage),
        Vec::new(),
    );
    let (node, definition) = read_definition(&repository, "test");
    let index = MirrorIndex::open(&node, &definition.path, ":index")
        .expect("open the storage")
        .expect("the storage exists");
    let entries: Vec<(String, String)> = index
        .entries()
        .expect("enumerate")
        .into_iter()
        .map(|entry| (entry.key, entry.path))
        .collect();
    assert_eq!(
        entries,
        [
            ("alpha".to_owned(), "/content".to_owned()),
            ("alpha".to_owned(), "/content/page".to_owned()),
        ]
    );
}

#[test]
fn the_empty_values_key_is_read_rather_than_skipped_as_a_hidden_child() {
    let storage = mirror_storage(&[(":", "/content")]);
    let (_directory, repository) = store(
        "empty-key",
        "test",
        property_definition("jcr:title", false, storage),
        Vec::new(),
    );
    let (node, definition) = read_definition(&repository, "test");
    let index = MirrorIndex::open(&node, &definition.path, ":index")
        .expect("open")
        .expect("exists");
    let entries = index.entries().expect("enumerate");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].key, ":");
    assert_eq!(entries[0].path, "/content");
}

#[test]
fn an_entry_on_the_key_node_itself_is_the_root_path() {
    let storage = mirror_storage(&[("alpha", "")]);
    let (_directory, repository) = store(
        "root-path",
        "test",
        property_definition("jcr:title", false, storage),
        Vec::new(),
    );
    let (node, definition) = read_definition(&repository, "test");
    let index = MirrorIndex::open(&node, &definition.path, ":index")
        .expect("open")
        .expect("exists");
    let entries = index.entries().expect("enumerate");
    assert_eq!(entries[0].content_path(), "/");
}

#[test]
fn the_approximate_counters_are_counted_rather_than_walked_as_content() {
    let storage = mirror_storage(&[("alpha", "/content")])
        .with(":count_0daeb465", Property::Long(200))
        .with(":count_65f31f1f", Property::Long(400));
    let (_directory, repository) = store(
        "counters",
        "test",
        property_definition("jcr:title", false, storage),
        Vec::new(),
    );
    let (node, definition) = read_definition(&repository, "test");
    let index = MirrorIndex::open(&node, &definition.path, ":index")
        .expect("open")
        .expect("exists");
    assert_eq!(index.approximate_counter_count().expect("count"), 2);
    assert_eq!(index.entries().expect("enumerate").len(), 1);
}

#[test]
fn a_unique_index_reads_its_entry_array_whole() {
    let storage = unique_storage(&[("alpha", &["/content/one"]), ("beta", &["/content/two"])]);
    let (_directory, repository) = store(
        "unique",
        "test",
        property_definition("jcr:uuid", true, storage),
        Vec::new(),
    );
    let (node, definition) = read_definition(&repository, "test");
    assert!(definition.property.unique, "the strict BOOLEAN read");
    let index = UniqueIndex::open(&node, ":index")
        .expect("open")
        .expect("exists");
    let entries = index.entries().expect("enumerate");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].key, "alpha");
    assert_eq!(entries[0].paths, ["/content/one"]);
    assert!(!entries[0].is_duplicate());
}

#[test]
fn a_unique_key_with_two_paths_is_a_duplicate_state() {
    let storage = unique_storage(&[("alpha", &["/content/one", "/content/two"])]);
    let (_directory, repository) = store(
        "duplicate",
        "test",
        property_definition("jcr:uuid", true, storage),
        Vec::new(),
    );
    let (node, _) = read_definition(&repository, "test");
    let index = UniqueIndex::open(&node, ":index")
        .expect("open")
        .expect("exists");
    assert!(index.entries().expect("enumerate")[0].is_duplicate());
}

#[test]
fn a_type_predicate_matches_a_mixins_subtypes_but_a_primary_types_only_its_own() {
    let directory = TestDirectory::new("type-predicate");
    let node_types = Node::new()
        .with_child(
            "mix:referenceable",
            Node::new()
                .with("jcr:isMixin", Property::Boolean(true))
                .with(
                    "rep:mixinSubtypes",
                    Property::Names(vec!["mix:versionable".to_owned()]),
                ),
        )
        .with_child(
            "nt:file",
            Node::new()
                .with("jcr:isMixin", Property::Boolean(false))
                .with(
                    "rep:primarySubtypes",
                    Property::Names(vec!["nt:resource".to_owned()]),
                ),
        );
    let root = Node::new()
        .with_child(
            "jcr:system",
            Node::new().with_child("jcr:nodeTypes", node_types),
        )
        .with_child(
            "mixin-node",
            Node::new().with(
                "jcr:mixinTypes",
                Property::Names(vec!["mix:versionable".to_owned()]),
            ),
        )
        .with_child(
            "primary-node",
            Node::new().with("jcr:primaryType", Property::Name("nt:resource".to_owned())),
        )
        .with_child(
            "mixin-only-node",
            Node::new().with(
                "jcr:mixinTypes",
                Property::Names(vec!["nt:file".to_owned()]),
            ),
        );
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open");
    let content_root = repository.content_root().expect("content root");

    let mixin_predicate = TypePredicate::new(&content_root, &["mix:referenceable".to_owned()])
        .expect("build the predicate");
    let node = content_root
        .child_node("mixin-node")
        .expect("resolve")
        .expect("exists");
    assert!(
        mixin_predicate.test(&node).expect("test"),
        "a declared mixin matches its mixin subtypes"
    );

    let primary_predicate =
        TypePredicate::new(&content_root, &["nt:file".to_owned()]).expect("build");
    let primary_node = content_root
        .child_node("primary-node")
        .expect("resolve")
        .expect("exists");
    assert!(
        primary_predicate.test(&primary_node).expect("test"),
        "a declared primary type matches its primary subtypes"
    );
    let mixin_only = content_root
        .child_node("mixin-only-node")
        .expect("resolve")
        .expect("exists");
    assert!(
        !primary_predicate.test(&mixin_only).expect("test"),
        "a declared primary type never matches a node by that node's mixins"
    );
}

#[test]
fn a_definition_whose_declaring_node_types_is_not_names_yields_a_predicate_matching_nothing() {
    let directory = TestDirectory::new("predicate-empty");
    let root = Node::new().with_child(
        "jcr:system",
        Node::new().with_child("jcr:nodeTypes", Node::new()),
    );
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open");
    let content_root = repository.content_root().expect("content root");
    // The empty declared list is what a strict NAMES read of a STRINGS
    // property yields, which is what the definition model hands over.
    let predicate = TypePredicate::new(&content_root, &[]).expect("build");
    assert!(predicate.is_empty());
}

/// A store with two indexed nodes and a mirror index that matches them.
fn consistent_store(name: &str) -> (TestDirectory, Repository) {
    let storage = mirror_storage(&[("alpha", "/one"), ("beta", "/two")]);
    store(
        name,
        "test",
        property_definition("jcr:title", false, storage),
        vec![
            ("one", content_node("jcr:title", "alpha")),
            ("two", content_node("jcr:title", "beta")),
        ],
    )
}

#[test]
fn a_consistent_mirror_index_reports_no_faults() {
    let (_directory, repository) = consistent_store("consistent");
    let (node, definition) = read_definition(&repository, "test");
    let report = check(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
        NodeCheckBudget::of_nodes(100),
    )
    .expect("check");
    assert!(report.is_consistent(), "{report:?}");
    assert_eq!(report.entries_checked, 2);
}

#[test]
fn an_entry_naming_a_removed_content_node_is_reported_as_stale() {
    let storage = mirror_storage(&[("alpha", "/one"), ("beta", "/gone")]);
    let (_directory, repository) = store(
        "stale",
        "test",
        property_definition("jcr:title", false, storage),
        vec![("one", content_node("jcr:title", "alpha"))],
    );
    let (node, definition) = read_definition(&repository, "test");
    let report = check_entries(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
    )
    .expect("check");
    assert_eq!(report.stale_entries.len(), 1, "{report:?}");
    assert_eq!(report.stale_entries[0].path, "/gone");
    assert_eq!(report.stale_entries[0].key, "beta");
}

#[test]
fn an_entry_whose_content_value_changed_is_reported_as_mismatched() {
    let storage = mirror_storage(&[("alpha", "/one")]);
    let (_directory, repository) = store(
        "mismatch",
        "test",
        property_definition("jcr:title", false, storage),
        vec![("one", content_node("jcr:title", "changed"))],
    );
    let (node, definition) = read_definition(&repository, "test");
    let report = check_entries(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
    )
    .expect("check");
    assert_eq!(report.mismatched_entries.len(), 1, "{report:?}");
    assert_eq!(report.mismatched_entries[0].path, "/one");
    assert!(report.stale_entries.is_empty());
}

#[test]
fn a_forged_match_on_a_node_that_holds_no_such_value_is_reported_as_mismatched() {
    // The path exists and the node exists, but nothing there keys to
    // `forged`: this is the shape a hand-edited index has.
    let storage = mirror_storage(&[("forged", "/one")]);
    let (_directory, repository) = store(
        "forged",
        "test",
        property_definition("jcr:title", false, storage),
        vec![("one", content_node("jcr:title", "alpha"))],
    );
    let (node, definition) = read_definition(&repository, "test");
    let report = check_entries(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
    )
    .expect("check");
    assert_eq!(report.mismatched_entries.len(), 1, "{report:?}");
    assert_eq!(report.mismatched_entries[0].key, "forged");
}

#[test]
fn a_covered_node_with_no_entry_is_reported_as_missing() {
    let storage = mirror_storage(&[("alpha", "/one")]);
    let (_directory, repository) = store(
        "missing",
        "test",
        property_definition("jcr:title", false, storage),
        vec![
            ("one", content_node("jcr:title", "alpha")),
            ("two", content_node("jcr:title", "beta")),
        ],
    );
    let (node, definition) = read_definition(&repository, "test");
    let report = check(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
        NodeCheckBudget::unlimited(),
    )
    .expect("check");
    assert_eq!(report.missing_entries.len(), 1, "{report:?}");
    assert_eq!(report.missing_entries[0].path, "/two");
    assert_eq!(report.missing_entries[0].key, "beta");
    assert!(!report.is_consistent());
}

#[test]
fn a_duplicate_unique_entry_is_reported_by_key() {
    let storage = unique_storage(&[("alpha", &["/one", "/two"])]);
    let (_directory, repository) = store(
        "duplicate-report",
        "test",
        property_definition("jcr:uuid", true, storage),
        vec![
            ("one", content_node("jcr:uuid", "alpha")),
            ("two", content_node("jcr:uuid", "alpha")),
        ],
    );
    let (node, definition) = read_definition(&repository, "test");
    let report = check_entries(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
    )
    .expect("check");
    assert_eq!(report.duplicate_entries.len(), 1, "{report:?}");
    assert_eq!(report.duplicate_entries[0].key, "alpha");
    assert_eq!(report.duplicate_entries[0].paths.len(), 2);
    assert!(!report.is_consistent());
}

#[test]
fn the_entry_budget_accepts_at_its_limit_and_refuses_at_the_limit_plus_one() {
    let (_directory, repository) = consistent_store("entry-budget");
    let (node, definition) = read_definition(&repository, "test");
    let content_root = repository.content_root().expect("content root");

    let report = check_entries(
        &node,
        &definition,
        &content_root,
        EntryCheckBudget::of_entries(2),
    )
    .expect("two entries fit a budget of two");
    assert_eq!(report.entries_checked, 2);

    let error = check_entries(
        &node,
        &definition,
        &content_root,
        EntryCheckBudget::of_entries(1),
    )
    .expect_err("two entries do not fit a budget of one");
    assert!(
        matches!(&error, IndexError::Record(_))
            && error.to_string().contains("more than 1 entries"),
        "{error}"
    );
}

#[test]
fn the_node_budget_accepts_at_its_limit_and_refuses_at_the_limit_plus_one() {
    let (_directory, repository) = consistent_store("node-budget");
    let (node, definition) = read_definition(&repository, "test");
    let content_root = repository.content_root().expect("content root");

    // The walk visits the content root, `/oak:index`, the definition and the
    // two indexed nodes; `:index` is hidden and is not entered.
    let unbounded = check(
        &node,
        &definition,
        &content_root,
        EntryCheckBudget::of_entries(100),
        NodeCheckBudget::unlimited(),
    )
    .expect("an unlimited budget completes");
    let visited = unbounded.nodes_visited.expect("the covered-node half ran");
    assert!(visited >= 3, "the walk visited {visited} nodes");

    check(
        &node,
        &definition,
        &content_root,
        EntryCheckBudget::of_entries(100),
        NodeCheckBudget::of_nodes(visited),
    )
    .expect("the exact count fits");

    let error = check(
        &node,
        &definition,
        &content_root,
        EntryCheckBudget::of_entries(100),
        NodeCheckBudget::of_nodes(visited - 1),
    )
    .expect_err("one fewer than the walk visits refuses");
    assert!(error.to_string().contains("content nodes"), "{error}");
}

#[test]
fn the_path_filter_bounds_the_covered_node_half() {
    let storage = mirror_storage(&[("alpha", "/content/one")]);
    let definition = property_definition("jcr:title", false, storage).with(
        "includedPaths",
        Property::Texts(vec!["/content".to_owned()]),
    );
    let (_directory, repository) = store(
        "filtered",
        "test",
        definition,
        vec![
            (
                "content",
                Node::new().with_child("one", content_node("jcr:title", "alpha")),
            ),
            // Outside the include set: carries the property, and must not be
            // reported as missing.
            ("etc", content_node("jcr:title", "beta")),
        ],
    );
    let (node, definition) = read_definition(&repository, "test");
    let report = check(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
        NodeCheckBudget::unlimited(),
    )
    .expect("check");
    assert!(
        report.is_consistent(),
        "a node outside the include set is not covered — {report:?}"
    );
}

#[test]
fn a_reference_entry_resolves_its_property_path_rather_than_a_node_path() {
    // `:references/<uuid>/content/one/ref` means "`/content/one/@ref` holds
    // this identifier" — the path is the *property's*, made relative.
    let storage = mirror_storage(&[("11111111-2222-3333-4444-555555555555", "/content/one/ref")]);
    let definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("reference".to_owned()))
        .with_child(":references", storage);
    let (_directory, repository) = store(
        "reference",
        "reference",
        definition,
        vec![(
            "content",
            Node::new().with_child(
                "one",
                Node::new().with(
                    "ref",
                    Property::Text("11111111-2222-3333-4444-555555555555".to_owned()),
                ),
            ),
        )],
    );
    let (node, definition) = read_definition(&repository, "reference");
    let report = check_entries(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
    )
    .expect("check");
    assert!(report.is_consistent(), "{report:?}");
    assert_eq!(report.entries_checked, 1);
}

#[test]
fn a_reference_entry_whose_property_no_longer_holds_the_identifier_is_mismatched() {
    let storage = mirror_storage(&[("11111111-2222-3333-4444-555555555555", "/content/one/ref")]);
    let definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("reference".to_owned()))
        .with_child(":references", storage);
    let (_directory, repository) = store(
        "reference-mismatch",
        "reference",
        definition,
        vec![(
            "content",
            Node::new().with_child(
                "one",
                Node::new().with("ref", Property::Text("a-different-identifier".to_owned())),
            ),
        )],
    );
    let (node, definition) = read_definition(&repository, "reference");
    let report = check_entries(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
    )
    .expect("check");
    assert_eq!(report.mismatched_entries.len(), 1, "{report:?}");
}

#[test]
fn an_absent_reference_child_enumerates_as_empty_rather_than_failing() {
    // Oak creates `:references` and `:weakreferences` on the first insert,
    // so a store with no weak references has no `:weakreferences` at all.
    let definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("reference".to_owned()));
    let (_directory, repository) = store("reference-absent", "reference", definition, Vec::new());
    let (node, definition) = read_definition(&repository, "reference");
    assert!(
        MirrorIndex::open(&node, &definition.path, ":weakreferences")
            .expect("open")
            .is_none()
    );
    let report = check_entries(
        &node,
        &definition,
        &repository.content_root().expect("content root"),
        EntryCheckBudget::of_entries(100),
    )
    .expect("check");
    assert!(report.is_consistent(), "{report:?}");
    assert_eq!(report.entries_checked, 0);
}
