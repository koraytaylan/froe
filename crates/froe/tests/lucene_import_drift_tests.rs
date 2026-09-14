//! The import's drift comparison, and its five directional tolerances.
//!
//! The file is never byte-compared with the store, because oak-run prints
//! the already-reindexed *copy* of the definition. What must hold is that
//! every rewrite an honest build performs is accepted **in the direction it
//! happens**, and that the same difference in the other direction is still
//! refused — which a symmetric ignore set cannot express.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use froe::store::Repository;
use froe::writer::index::lucene_import::drift::{DriftVerdict, compare};
use support::property_index_layout::{Node, Property, write_repository_with_tree};

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-import-drift-{name}-{}-{:?}",
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

/// A `lucene` definition carrying `extras`, plus `children`.
fn definition(extras: Vec<(&str, Property)>, children: Vec<(&str, Node)>) -> Node {
    let mut node = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("lucene".to_owned()))
        .with("async", Property::Text("async".to_owned()));
    for (name, value) in extras {
        node = node.with(name, value);
    }
    for (name, child) in children {
        node = node.with_child(name, child);
    }
    node
}

/// Compares `file` against `stored`, both written into one store.
fn verdict(name: &str, file: Node, stored: Node) -> DriftVerdict {
    let directory = TestDirectory::new(name);
    write_repository_with_tree(
        &directory.path,
        &Node::new().with_child(
            "oak:index",
            Node::new()
                .with_child("file", file)
                .with_child("stored", stored),
        ),
    );
    let repository = Repository::open(&directory.path).expect("open");
    let file_node = repository
        .node_at_path("/oak:index/file")
        .expect("resolve")
        .expect("exists");
    let stored_node = repository
        .node_at_path("/oak:index/stored")
        .expect("resolve")
        .expect("exists");
    compare(&file_node, &stored_node).expect("compare")
}

#[test]
fn an_identical_definition_is_clean() {
    let verdict = verdict(
        "identical",
        definition(Vec::new(), Vec::new()),
        definition(Vec::new(), Vec::new()),
    );
    assert!(verdict.is_clean(), "{verdict:?}");
}

#[test]
fn reindex_count_is_ignored_because_the_copy_always_advances_it() {
    let verdict = verdict(
        "reindex-count",
        definition(vec![("reindexCount", Property::Long(8))], Vec::new()),
        definition(vec![("reindexCount", Property::Long(7))], Vec::new()),
    );
    assert!(verdict.is_clean(), "{verdict:?}");
}

#[test]
fn refresh_is_tolerated_on_the_files_side_only() {
    // The lane revert sets it on the copy.
    let accepted = verdict(
        "refresh-file",
        definition(vec![("refresh", Property::Boolean(true))], Vec::new()),
        definition(Vec::new(), Vec::new()),
    );
    assert!(accepted.is_clean(), "{accepted:?}");

    // A store carrying one the file lacks is drift: nothing in an
    // out-of-band build removes it.
    let refused = verdict(
        "refresh-store",
        definition(Vec::new(), Vec::new()),
        definition(vec![("refresh", Property::Boolean(true))], Vec::new()),
    );
    assert!(!refused.is_clean(), "a store-only refresh must be drift");
}

#[test]
fn a_created_seed_is_accepted_and_two_different_seeds_are_drift() {
    // A run that created one is ordinary.
    let created = verdict(
        "seed-created",
        definition(vec![("seed", Property::Long(42))], Vec::new()),
        definition(Vec::new(), Vec::new()),
    );
    assert!(created.is_clean(), "{created:?}");

    // Two seeds that differ are drift, because the counters they drive
    // would disagree. This is the case a symmetric ignore would excuse.
    let differing = verdict(
        "seed-differs",
        definition(vec![("seed", Property::Long(42))], Vec::new()),
        definition(vec![("seed", Property::Long(7))], Vec::new()),
    );
    assert!(
        !differing.is_clean(),
        "two different seeds must be drift, not a tolerated rewrite"
    );

    // The same seed on both sides is no difference at all.
    let same = verdict(
        "seed-same",
        definition(vec![("seed", Property::Long(42))], Vec::new()),
        definition(vec![("seed", Property::Long(42))], Vec::new()),
    );
    assert!(same.is_clean(), "{same:?}");
}

#[test]
fn a_cleared_corrupt_flag_is_accepted_and_a_file_only_one_is_drift() {
    // The copy's reindex cleared it, and a corrupt-flagged index is the
    // usual reason for an out-of-band build.
    let cleared = verdict(
        "corrupt-cleared",
        definition(Vec::new(), Vec::new()),
        definition(
            vec![(
                "corrupt",
                Property::Date("2026-01-01T00:00:00.000Z".to_owned()),
            )],
            Vec::new(),
        ),
    );
    assert!(cleared.is_clean(), "{cleared:?}");

    // The other direction: a file flagging an index the store considers
    // healthy is drift.
    let flagged = verdict(
        "corrupt-added",
        definition(
            vec![(
                "corrupt",
                Property::Date("2026-01-01T00:00:00.000Z".to_owned()),
            )],
            Vec::new(),
        ),
        definition(Vec::new(), Vec::new()),
    );
    assert!(
        !flagged.is_clean(),
        "a file-only corrupt flag must be drift"
    );
}

#[test]
fn a_cleared_import_state_is_accepted_and_a_file_only_one_is_drift() {
    let cleared = verdict(
        "import-state-cleared",
        definition(Vec::new(), Vec::new()),
        definition(
            vec![("indexImportState", Property::Text("SWITCH_LANE".to_owned()))],
            Vec::new(),
        ),
    );
    assert!(cleared.is_clean(), "{cleared:?}");

    let added = verdict(
        "import-state-added",
        definition(
            vec![("indexImportState", Property::Text("SWITCH_LANE".to_owned()))],
            Vec::new(),
        ),
        definition(Vec::new(), Vec::new()),
    );
    assert!(
        !added.is_clean(),
        "a file-only indexImportState must be drift"
    );
}

#[test]
fn a_facets_child_the_file_adds_is_accepted() {
    // An out-of-band build's document maker persists it.
    let verdict = verdict(
        "facets-added",
        definition(
            Vec::new(),
            vec![(
                "facets",
                Node::new().with("secure", Property::Text("statistical".to_owned())),
            )],
        ),
        definition(Vec::new(), Vec::new()),
    );
    assert!(verdict.is_clean(), "{verdict:?}");
}

#[test]
fn two_equal_facets_children_are_no_difference() {
    // The ordinary case: Oak creates the child the moment any facet
    // configuration is built, and the dump keeps visible children. Dropping
    // the file's side unconditionally would manufacture a *removed* visible
    // child here.
    let facets = || {
        (
            "facets",
            Node::new().with("secure", Property::Text("statistical".to_owned())),
        )
    };
    let verdict = verdict(
        "facets-equal",
        definition(Vec::new(), vec![facets()]),
        definition(Vec::new(), vec![facets()]),
    );
    assert!(verdict.is_clean(), "{verdict:?}");
}

#[test]
fn two_differing_facets_children_are_drift() {
    let verdict = verdict(
        "facets-differ",
        definition(
            Vec::new(),
            vec![(
                "facets",
                Node::new().with("secure", Property::Text("statistical".to_owned())),
            )],
        ),
        definition(
            Vec::new(),
            vec![(
                "facets",
                Node::new().with("secure", Property::Text("insecure".to_owned())),
            )],
        ),
    );
    assert!(
        !verdict.is_clean(),
        "a facets child that differs is drift, not a tolerated addition"
    );
}

#[test]
fn a_store_only_facets_child_is_the_removed_visible_child_it_is() {
    let verdict = verdict(
        "facets-removed",
        definition(Vec::new(), Vec::new()),
        definition(
            Vec::new(),
            vec![(
                "facets",
                Node::new().with("secure", Property::Text("statistical".to_owned())),
            )],
        ),
    );
    assert!(
        !verdict.is_clean(),
        "a store-only facets child is a removed visible child"
    );
}

#[test]
fn a_changed_visible_property_is_drift_and_the_refusal_names_it() {
    let verdict = verdict(
        "changed-property",
        definition(
            vec![(
                "includedPaths",
                Property::Texts(vec!["/content".to_owned()]),
            )],
            Vec::new(),
        ),
        definition(
            vec![("includedPaths", Property::Texts(vec!["/var".to_owned()]))],
            Vec::new(),
        ),
    );
    assert!(!verdict.is_clean());
    let refusal = froe::writer::index::lucene_import::drift::refusal("/oak:index/lucene", &verdict);
    assert!(
        refusal.to_string().contains("includedPaths"),
        "the refusal names the first difference: {refusal}"
    );
    assert!(
        refusal.to_string().contains("oak-run or AEM"),
        "and points at where definition changes belong: {refusal}"
    );
}

#[test]
fn a_removed_visible_child_is_drift() {
    let verdict = verdict(
        "removed-child",
        definition(Vec::new(), Vec::new()),
        definition(
            Vec::new(),
            vec![("indexRules", Node::new().with_child("nt:base", Node::new()))],
        ),
    );
    assert!(!verdict.is_clean());
}
