//! `froe index reindex`, end to end over synthetic stores.
//!
//! What these hold is the safety case's claims about a run that rebuilds:
//! the content tree is untouched, the head moves exactly once, the store is
//! byte-identical until the first index record, and the rebuilt indexes pass
//! plan 0006's own consistency check when the store is reopened. The claims
//! about *which* definitions a run touches are in
//! `index_reindex_selection_tests.rs`.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use froe::index::IndexDefinition;
use froe::store::Repository;
use froe::writer::index::{PreparedReindex, ReindexAction, plan_reindex, reindex};
use support::property_index_layout::{Node, Property, write_repository_with_tree};
use support::reindex_fixtures::{
    TestDirectory, content_digest, definition, digest_lines, journal_lines, options,
    store_contents, store_with_a_flagged_title_index,
};

#[test]
fn a_run_rebuilds_the_index_and_leaves_the_content_tree_untouched() {
    let directory = TestDirectory::new("full-run");
    let store = store_with_a_flagged_title_index(&directory);

    let content_before = content_digest(&store);
    let journal_before = journal_lines(&store);

    let outcome = reindex(&store, options(&directory)).expect("reindex");
    assert!(outcome.moved_the_head(), "a rebuild moves the head");

    // The claim the safety case makes first: the content tree survives
    // unconditionally. Everything outside `/oak:index`, byte for byte.
    assert_eq!(
        content_digest(&store),
        content_before,
        "the reindex changed content"
    );
    assert_eq!(
        journal_lines(&store),
        journal_before + 1,
        "exactly one journal line is appended"
    );

    // The index itself: three nodes indexed, one of them under the empty
    // token.
    let index = digest_lines(&store, "/oak:index/title/:index");
    assert!(
        index.iter().any(|line| line.starts_with("/Alpha")),
        "{index:?}"
    );
    assert!(
        index.iter().any(|line| line.starts_with("/Beta")),
        "{index:?}"
    );
    assert!(
        index.iter().any(|line| line.starts_with("/:")),
        "the empty value is indexed under the hidden name `:`: {index:?}"
    );
}

#[test]
fn the_bookkeeping_is_done_and_every_other_property_survives() {
    let directory = TestDirectory::new("bookkeeping");
    let store = store_with_a_flagged_title_index(&directory);
    reindex(&store, options(&directory)).expect("reindex");

    let repository = Repository::open(&store).expect("open");
    let node = repository
        .node_at_path("/oak:index/title")
        .expect("resolve")
        .expect("the definition exists");
    let model = IndexDefinition::read(&node, "/oak:index/title").expect("model");
    assert!(!model.reindex.flagged, "the flag is cleared");
    assert_eq!(model.reindex.count, 1, "the count is incremented");

    let fields = digest_lines(&store, "/oak:index/title");
    let first = fields.first().expect("the definition renders");
    for expected in [
        "info=String:kept verbatim",
        // The identity-preservation guard's wiring proof: a rewrite that
        // rebuilt the node from a model rather than preserving its slots
        // would drop or reorder exactly these.
        "includedPaths=String[]:/content",
        "declaringNodeTypes=Name[]:nt:unstructured",
    ] {
        assert!(
            first.contains(expected),
            "{expected} did not survive the apply path: {first}"
        );
    }
}

#[test]
fn a_run_with_nothing_selected_moves_nothing() {
    let directory = TestDirectory::new("no-work");
    let store = directory.store();
    // No definition carries `reindex = true`.
    write_repository_with_tree(
        &store,
        &Node::new().with_child("content", Node::new()).with_child(
            "oak:index",
            Node::new().with_child(
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
            ),
        ),
    );

    let plan = plan_reindex(&store, &options(&directory)).expect("plan");
    assert!(plan.is_empty(), "{plan:?}");

    let before = store_contents(&store);
    let outcome = reindex(&store, options(&directory)).expect("reindex");
    assert!(!outcome.moved_the_head());
    assert_eq!(
        store_contents(&store),
        before,
        "a run with no work must not write a byte"
    );
}

#[test]
fn the_plan_is_read_only_and_takes_no_lock() {
    let directory = TestDirectory::new("plan-read-only");
    let store = store_with_a_flagged_title_index(&directory);
    let before = store_contents(&store);

    let plan = plan_reindex(&store, &options(&directory)).expect("plan");
    assert_eq!(plan.rebuild_count(), 1, "{plan:?}");
    assert!(
        matches!(
            plan.actions.first(),
            Some(ReindexAction::Rebuild { entries, .. }) if *entries == 3
        ),
        "the plan counts what will be written: {:?}",
        plan.actions
    );
    assert_eq!(
        store_contents(&store),
        before,
        "planning wrote to the store"
    );
    assert!(!store.join("repo.lock").exists(), "planning took the lock");
}

#[test]
fn the_run_subdirectory_is_removed_afterwards() {
    let directory = TestDirectory::new("run-directory");
    let store = store_with_a_flagged_title_index(&directory);
    reindex(&store, options(&directory)).expect("reindex");
    let leftovers: Vec<String> = std::fs::read_dir(directory.work())
        .expect("read the work directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "the run left files behind: {leftovers:?}"
    );
}

#[test]
fn a_rerun_rebuilds_an_identical_subtree() {
    // Determinism: the same store and the same definition produce the same
    // index, so a comparison against Oak's own rebuild is meaningful.
    let directory = TestDirectory::new("determinism");
    let store = store_with_a_flagged_title_index(&directory);
    reindex(&store, options(&directory)).expect("the first run");
    let first = digest_lines(&store, "/oak:index/title/:index");

    // Flag it again, so the second run has work.
    let second_directory = TestDirectory::new("determinism-second");
    let second = store_with_a_flagged_title_index(&second_directory);
    reindex(&second, options(&second_directory)).expect("the second run");
    assert_eq!(
        digest_lines(&second, "/oak:index/title/:index"),
        first,
        "two runs over the same content produce different indexes"
    );
}

#[test]
fn the_rebuilt_index_passes_the_consistency_check() {
    // Plan 0006's own checker, over the reopened store: the rebuild has to
    // satisfy the reader, not only the writer that produced it.
    let directory = TestDirectory::new("consistency");
    let store = store_with_a_flagged_title_index(&directory);
    reindex(&store, options(&directory)).expect("reindex");

    let repository = Repository::open(&store).expect("open");
    let content_root = repository.content_root().expect("content root");
    let node = repository
        .node_at_path("/oak:index/title")
        .expect("resolve")
        .expect("exists");
    let model = IndexDefinition::read(&node, "/oak:index/title").expect("model");
    let report = froe::index::property::consistency::check(
        &node,
        &model,
        &content_root,
        froe::index::property::consistency::EntryCheckBudget::of_entries(1_000),
        froe::index::property::consistency::NodeCheckBudget::of_nodes(1_000),
    )
    .expect("check the rebuilt index");
    assert!(
        report.is_consistent(),
        "the rebuilt index does not agree with the content it indexes: {report:?}"
    );
}

#[test]
fn an_observed_reindex_plan_equals_an_unobserved_one() {
    // Observation is inert: an observer is told what happened and never
    // decides anything.
    let directory = TestDirectory::new("observed-plan");
    let store = store_with_a_flagged_title_index(&directory);
    let unobserved = plan_reindex(&store, &options(&directory)).expect("plan");
    let mut log = support::observation_log::ObservationLog::default();
    let observed =
        froe::writer::index::plan_reindex_with_progress(&store, &options(&directory), &mut log)
            .expect("observed plan");
    assert_eq!(observed, unobserved);
    assert!(
        log.began_and_ended_in_pairs(),
        "every step the plan opened was closed"
    );
}

#[test]
fn an_observed_reindex_equals_an_unobserved_one() {
    let plain = TestDirectory::new("observed-apply-plain");
    let plain_store = store_with_a_flagged_title_index(&plain);
    let unobserved = reindex(&plain_store, options(&plain)).expect("reindex");

    let observed_directory = TestDirectory::new("observed-apply");
    let observed_store = store_with_a_flagged_title_index(&observed_directory);
    let mut log = support::observation_log::ObservationLog::default();
    let observed = PreparedReindex::prepare_with_progress(
        &observed_store,
        options(&observed_directory),
        &mut log,
    )
    .expect("prepare")
    .apply_with_progress(&mut log)
    .expect("apply");

    assert_eq!(
        observed.definitions, unobserved.definitions,
        "observation changed what the run did"
    );
    assert!(log.began_and_ended_in_pairs());
    assert_eq!(
        digest_lines(&observed_store, "/oak:index/title/:index"),
        digest_lines(&plain_store, "/oak:index/title/:index"),
        "the observed run wrote a different index"
    );
}

#[test]
fn a_counter_is_rebuilt_from_its_own_hash_chain() {
    let directory = TestDirectory::new("counter");
    let store = directory.store();
    let mut content = Node::new();
    for index in 0..40u32 {
        content = content.with_child(
            &format!("n{index}"),
            Node::new().with(
                "jcr:primaryType",
                Property::Name("nt:unstructured".to_owned()),
            ),
        );
    }
    let root = content.with_child(
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
                definition(
                    "counter",
                    vec![
                        ("seed", Property::Long(-7_610_761_686_379_641_542)),
                        ("resolution", Property::Long(8)),
                    ],
                ),
            ),
    );
    write_repository_with_tree(&store, &root);

    let outcome = reindex(&store, options(&directory)).expect("reindex the counter");
    assert!(outcome.moved_the_head());
    let index = digest_lines(&store, "/oak:index/counter/:index");
    assert!(
        index.iter().any(|line| line.contains(":cnt=Long:")),
        "the counter wrote no counts: {index:?}"
    );
}

#[test]
fn the_store_is_untouched_until_the_first_index_record() {
    // `open_prepared` is the side-effect-free open: no manifest rewrite, no
    // archive normalization, no rename before anything is written. What the
    // run appends is new; what was there is byte-identical.
    let directory = TestDirectory::new("additive");
    let store = store_with_a_flagged_title_index(&directory);
    let before = store_contents(&store);
    reindex(&store, options(&directory)).expect("reindex");

    let after = store_contents(&store);
    for (name, bytes) in &before {
        let found = after
            .iter()
            .find(|(other, _)| other == name)
            .unwrap_or_else(|| panic!("{name} is gone after the run"));
        if name == "journal.log" {
            assert!(
                found.1.starts_with(bytes),
                "the journal was rewritten rather than appended to"
            );
        } else {
            assert!(
                found.1.starts_with(bytes),
                "{name} was rewritten rather than appended to"
            );
        }
    }
}
