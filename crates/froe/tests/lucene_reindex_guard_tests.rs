//! The refusals of plan 0010's native reindex, one test per guard.
//!
//! Every one of these lands **before the first `:data` record**, so a
//! refused run leaves a store byte-identical to the one it found. The
//! guards table of
//! `docs/plans/0010-lucene-offline-reindex/ARCHITECTURE.md` cites these by
//! name.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use froe::writer::index::{plan_reindex, reindex};
use support::property_index_layout::{Node, Property};
use support::reindex_fixtures::{
    TestDirectory, lucene_definition, lucene_options, options, store_contents,
    store_with_a_flagged_lucene_index, write_lucene_store,
};

/// The refusal a plan reports for the one definition it looked at.
fn refusal(directory: &TestDirectory, store: &std::path::Path, policy: bool) -> String {
    let options = if policy {
        lucene_options(directory)
    } else {
        options(directory)
    };
    let plan = plan_reindex(store, &options).expect("plan");
    assert!(
        plan.actions.is_empty(),
        "the definition must be refused, not planned: {:?}",
        plan.actions
    );
    plan.warnings
        .first()
        .map(std::string::ToString::to_string)
        .expect("a refusal")
}

#[test]
fn a_lucene_definition_without_a_binary_text_policy_is_refused_by_name() {
    let directory = TestDirectory::new("guard-no-policy");
    let store = store_with_a_flagged_lucene_index(&directory);
    let before = store_contents(&store);
    let message = refusal(&directory, &store, false);
    assert!(message.contains("binary-text policy"), "{message}");
    // A refusal costs nothing: not a byte of the store changed.
    assert_eq!(store_contents(&store), before);
}

#[test]
fn a_definition_whose_codec_is_not_oak_codec_is_refused_naming_lucene46() {
    let directory = TestDirectory::new("guard-codec");
    // No analyzed and no nodeScopeIndex property: the rule is not
    // fulltext-enabled, so Oak's own codec verdict is `Lucene46`.
    let definition = lucene_definition(
        vec![("no-catch-all", Property::Boolean(true))],
        vec![(
            "plain",
            Node::new()
                .with(
                    "jcr:primaryType",
                    Property::Name("nt:unstructured".to_owned()),
                )
                .with("name", Property::Text("jcr:title".to_owned()))
                .with("propertyIndex", Property::Boolean(true)),
        )],
    );
    let store = write_lucene_store(&directory, definition);
    let message = refusal(&directory, &store, true);
    assert!(message.contains("Lucene46"), "{message}");
}

#[test]
fn a_definition_level_value_regex_is_refused_by_name() {
    let directory = TestDirectory::new("guard-value-regex");
    let definition = lucene_definition(
        vec![("valueRegex", Property::Text("^keep.*$".to_owned()))],
        Vec::new(),
    );
    let store = write_lucene_store(&directory, definition);
    let message = refusal(&directory, &store, true);
    assert!(message.contains("^keep.*$"), "{message}");
}

#[test]
fn a_definition_using_an_unported_feature_is_refused_by_name() {
    let directory = TestDirectory::new("guard-feature");
    let definition = lucene_definition(
        Vec::new(),
        vec![(
            "dynamic",
            Node::new()
                .with(
                    "jcr:primaryType",
                    Property::Name("nt:unstructured".to_owned()),
                )
                .with("name", Property::Text("jcr:title".to_owned()))
                .with("dynamicBoost", Property::Boolean(true)),
        )],
    );
    let store = write_lucene_store(&directory, definition);
    let message = refusal(&directory, &store, true);
    assert!(message.contains("dynamicBoost"), "{message}");
}

#[test]
fn a_hybrid_definition_is_refused_by_name() {
    let directory = TestDirectory::new("guard-hybrid");
    let definition = lucene_definition(
        vec![(
            "async",
            Property::Texts(vec!["async".to_owned(), "sync".to_owned()]),
        )],
        Vec::new(),
    );
    let store = write_lucene_store(&directory, definition);
    let message = refusal(&directory, &store, true);
    assert!(message.contains("hybrid"), "{message}");
}

#[test]
fn a_definition_without_async_is_refused_by_name() {
    let directory = TestDirectory::new("guard-synchronous");
    let definition = lucene_definition(vec![("no-async", Property::Boolean(true))], Vec::new());
    let store = write_lucene_store(&directory, definition);
    let message = refusal(&directory, &store, true);
    assert!(message.contains("async"), "{message}");
}

/// A refused run writes nothing at all, which is the claim every refusal
/// above rests on.
#[test]
fn a_refused_run_leaves_the_store_byte_identical() {
    let directory = TestDirectory::new("guard-untouched");
    let store = store_with_a_flagged_lucene_index(&directory);
    let before = store_contents(&store);
    let outcome = reindex(&store, options(&directory)).expect("a run with nothing to do");
    assert!(!outcome.moved_the_head(), "nothing was rebuilt");
    assert_eq!(store_contents(&store), before);
}
