//! Which definitions a reindex touches, and what it refuses to touch.
//!
//! The other half of task 0707's suite: every supported type in one run, the
//! reset arm, a mixed selection, the work directory's residue rules and the
//! lock. Split from `index_reindex_tests.rs` because one file holding both
//! exceeds the thousand-line gate.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use froe::store::Repository;
use froe::writer::index::{
    DefinitionReport, PreparedReindex, ReindexOptions, ReindexWarning, plan_reindex, reindex,
};
use support::property_index_layout::{
    Node, Property, mirror_storage, unique_storage, write_repository_with_tree,
};
use support::reindex_fixtures::{
    TestDirectory, content_digest, definition, digest_lines, from_head, journal_lines, options,
    store_contents, store_with_a_counter_on_an_absent_lane, store_with_a_flagged_title_index,
    store_with_every_supported_type,
};

#[test]
fn a_counter_on_an_unresolvable_lane_is_refused_rather_than_reset() {
    // froe will not rebuild a counter — Oak's own replay would double it —
    // and Oak will not rebuild one on a lane whose checkpoint is gone:
    // measured on the pinned image, that lane never completes a cycle, so
    // the reset this used to perform removed an index nothing restored.
    let directory = TestDirectory::new("reset");
    let store = store_with_a_counter_on_an_absent_lane(&directory);
    let before = store_contents(&store);

    let plan = plan_reindex(&store, &from_head(&directory)).expect("plan");
    let rendered = format!("{plan:?}");
    assert!(
        rendered.contains("/oak:index/counter") && rendered.contains("CounterOnAnUnresolvableLane"),
        "the plan must answer the counter by name, with the reason: {rendered}"
    );

    let outcome = reindex(&store, from_head(&directory)).expect("reindex");
    assert!(
        !outcome.moved_the_head(),
        "a refused counter leaves the head where it was"
    );
    assert_eq!(
        store_contents(&store),
        before,
        "a refused counter must not remove its index"
    );
    assert!(
        !digest_lines(&store, "/oak:index/counter/:index").is_empty(),
        "the counter's data must still be there"
    );
}

#[test]
fn a_rerun_of_a_refused_counter_still_refuses_and_never_opens_the_store() {
    // The refusal is stable: a second run answers the same way and writes
    // no more than the first did, which is nothing.
    let directory = TestDirectory::new("reset-rerun");
    let store = store_with_a_counter_on_an_absent_lane(&directory);
    reindex(&store, from_head(&directory)).expect("the first run");

    let before = store_contents(&store);
    let head_before = Repository::open(&store)
        .expect("open")
        .head_record_identifier();
    let outcome = reindex(&store, from_head(&directory)).expect("the second run");

    assert!(!outcome.moved_the_head());
    assert!(
        outcome.definitions.is_empty(),
        "a refused definition produces no report: {:?}",
        outcome.definitions
    );
    assert_eq!(
        store_contents(&store),
        before,
        "a rerun with nothing to do wrote to the store"
    );
    assert_eq!(
        Repository::open(&store)
            .expect("open")
            .head_record_identifier(),
        head_before
    );
}

#[test]
fn a_mixed_selection_rebuilds_the_one_it_can_and_refuses_the_counter() {
    let directory = TestDirectory::new("mixed");
    let store = directory.store();
    let root = Node::new()
        .with_child(
            "content",
            Node::new().with("jcr:title", Property::Text("Alpha".to_owned())),
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
                        .with_child(":index", Node::new().with(":cnt", Property::Long(40))),
                ),
        );
    write_repository_with_tree(&store, &root);
    let journal_before = journal_lines(&store);

    let outcome = reindex(&store, from_head(&directory)).expect("reindex");
    assert!(outcome.moved_the_head());
    assert_eq!(
        journal_lines(&store),
        journal_before + 1,
        "one rebuild and one refusal, still one journal line"
    );

    let reports: Vec<&DefinitionReport> = outcome
        .definitions
        .iter()
        .map(|(_, report)| report)
        .collect();
    assert!(
        reports
            .iter()
            .any(|report| matches!(report, DefinitionReport::Rebuilt { .. })),
        "{reports:?}"
    );
    assert!(
        !reports
            .iter()
            .any(|report| matches!(report, DefinitionReport::Reset { .. })),
        "the counter is refused, not reset: {reports:?}"
    );
    assert!(
        !digest_lines(&store, "/oak:index/counter/:index").is_empty(),
        "the refused counter must keep its data"
    );
    assert!(
        !digest_lines(&store, "/oak:index/title/:index").is_empty(),
        "the rebuild arm did not build"
    );
}

#[test]
fn a_confirmed_run_walks_the_content_exactly_twice_per_definition() {
    // What this pins is the number of *walks*, not the number of steps: the
    // counting walk the plan performs so an operator confirms a number, and
    // the collecting walk the apply performs. The tail adds no third walk —
    // it checks the entries it was given against the content they name,
    // which is why `check`'s covered-node half is deliberately not run.
    let directory = TestDirectory::new("walks");
    let store = store_with_a_flagged_title_index(&directory);

    let mut log = support::observation_log::ObservationLog::default();
    PreparedReindex::prepare_with_progress(&store, options(&directory), &mut log)
        .expect("prepare")
        .apply_with_progress(&mut log)
        .expect("apply");

    let collecting = log
        .descriptions()
        .into_iter()
        .filter(|description| *description == "collecting index entries")
        .count();
    assert_eq!(
        collecting,
        2,
        "one definition, one counting walk and one collecting walk: {:?}",
        log.descriptions()
    );
    for once in ["writing index records", "verifying the rebuilt indexes"] {
        assert_eq!(
            log.descriptions()
                .into_iter()
                .filter(|description| *description == once)
                .count(),
            1,
            "{once} opened more than once: {:?}",
            log.descriptions()
        );
    }
    assert!(log.began_and_ended_in_pairs());
}

#[test]
fn one_run_rebuilds_every_supported_type() {
    let directory = TestDirectory::new("every-type");
    let store = store_with_every_supported_type(&directory);
    let content_before = content_digest(&store);
    let journal_before = journal_lines(&store);

    let outcome = reindex(&store, options(&directory)).expect("reindex");
    assert!(outcome.moved_the_head());
    assert_eq!(
        content_digest(&store),
        content_before,
        "the reindex changed content"
    );
    assert_eq!(
        journal_lines(&store),
        journal_before + 1,
        "four definitions, one journal line"
    );

    // Each type wrote the shape its own storage strategy calls for.
    let mirror = digest_lines(&store, "/oak:index/title/:index");
    assert!(
        mirror.iter().any(|line| line.starts_with("/Alpha")),
        "{mirror:?}"
    );
    let unique = digest_lines(&store, "/oak:index/uid/:index");
    assert!(
        unique.iter().any(|line| line.contains("entry=")),
        "a unique index stores its paths in `entry`: {unique:?}"
    );
    let references = digest_lines(&store, "/oak:index/reference/:references");
    assert!(
        references
            .iter()
            .any(|line| line.contains("11111111-2222-3333-4444-555555555555")),
        "{references:?}"
    );
    let weak = digest_lines(&store, "/oak:index/reference/:weakreferences");
    assert!(
        weak.iter()
            .any(|line| line.contains("66666666-7777-8888-9999-aaaaaaaaaaaa")),
        "{weak:?}"
    );
    let counter = digest_lines(&store, "/oak:index/counter/:index");
    assert!(
        counter.iter().any(|line| line.contains(":cnt=Long:")),
        "{counter:?}"
    );
}

#[test]
fn the_content_tree_still_checks_consistent_after_a_run() {
    // The library entry behind `froe check`, over the whole store: a
    // reindex that damaged a record the content shares would show here and
    // in nothing the index tests look at.
    let directory = TestDirectory::new("check-consistency");
    let store = store_with_every_supported_type(&directory);
    reindex(&store, options(&directory)).expect("reindex");

    let report = froe::tooling::check_consistency(
        &store,
        &["/".to_owned()],
        froe::tooling::BinaryCheck::EveryBlock,
        1,
    )
    .expect("check the reindexed store");
    assert!(
        report.has_good_revision(),
        "the reindexed store does not check consistent: {report:?}"
    );
}

#[test]
fn the_rebuilt_subtrees_render_as_the_independent_encoder_writes_them() {
    // The comparison that shares no code with the builders: the expected
    // `:index` is described here as entries and written by the test-only
    // encoder, and the two stores are compared through the digest.
    let directory = TestDirectory::new("independent");
    let store = store_with_every_supported_type(&directory);
    reindex(&store, options(&directory)).expect("reindex");

    let expected_directory = TestDirectory::new("independent-expected");
    let expected = expected_directory.store();
    write_repository_with_tree(
        &expected,
        &Node::new().with_child(
            "oak:index",
            Node::new()
                .with_child(
                    "title",
                    Node::new().with_child(
                        ":index",
                        mirror_storage(&[("Alpha", "content"), ("Beta", "content/page")]),
                    ),
                )
                .with_child(
                    "uid",
                    Node::new().with_child(
                        ":index",
                        // Absolute, against the mirror's relative path:
                        // §12.2 of the storage spec.
                        unique_storage(&[("u-1", &["/content"]), ("u-2", &["/content/page"])]),
                    ),
                ),
        ),
    );

    for path in ["/oak:index/title/:index", "/oak:index/uid/:index"] {
        assert_eq!(
            digest_lines(&store, path),
            digest_lines(&expected, path),
            "the rebuilt {path} is not what the independent encoder writes"
        );
    }
}

#[test]
fn a_concurrent_holder_of_the_lock_is_refused() {
    let directory = TestDirectory::new("concurrent");
    let store = store_with_a_flagged_title_index(&directory);
    let held = PreparedReindex::prepare(&store, options(&directory)).expect("prepare");

    let message = match PreparedReindex::prepare(&store, options(&directory)) {
        Ok(_) => panic!("a second prepare must be refused while the first holds the lock"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("lock"),
        "the refusal names the lock: {message}"
    );
    drop(held);
}

#[test]
fn residue_under_the_default_work_directory_is_a_warning_rather_than_a_refusal() {
    // The operator did not choose the default directory, so something froe
    // left there is froe's to report, not the operator's to explain.
    let directory = TestDirectory::new("default-residue");
    let store = store_with_a_flagged_title_index(&directory);
    let options = ReindexOptions::new().with_sort_budget_bytes(32);
    let plan = plan_reindex(&store, &options).expect("plan");
    let default_work = plan.work_directory.clone();
    std::fs::create_dir_all(default_work.join("froe-reindex-0000000000000000"))
        .expect("plant the residue");

    let plan =
        plan_reindex(&store, &options).expect("a default directory warns rather than refuses");
    assert!(
        plan.warnings.iter().any(|warning| matches!(
            warning,
            ReindexWarning::ResidueUnderDefaultWorkDirectory { .. }
        )),
        "{:?}",
        plan.warnings
    );
    let _ = std::fs::remove_dir_all(default_work.join("froe-reindex-0000000000000000"));
}

#[test]
fn a_residue_directory_is_refused_under_an_operator_named_work_directory() {
    // The operator chose the directory, so something they did not expect
    // being in it is a refusal rather than a warning.
    let directory = TestDirectory::new("residue");
    let store = store_with_a_flagged_title_index(&directory);
    std::fs::create_dir_all(directory.work().join("froe-reindex-0000000000000000"))
        .expect("plant the residue");

    let error = plan_reindex(&store, &options(&directory))
        .expect_err("a leftover run directory must be refused");
    assert!(
        error
            .to_string()
            .contains("left over from an earlier froe reindex"),
        "the refusal says what it found: {error}"
    );
}
