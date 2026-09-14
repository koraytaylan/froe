//! One test per guard the reindex introduces, reaching the production entry.
//!
//! A refusal is a claim about what froe will *not* do, and a test that only
//! shows the accepting path proves nothing about it. Each test here drives
//! `plan_reindex` or `PreparedReindex::apply` — not `select` or a builder in
//! isolation — and pins the typed refusal or the preserved bytes.
//!
//! The guards whose regression is necessarily in-crate are not here, because
//! a `#[cfg(test)]` seam is absent from the library an integration test links
//! against. The safety case's guards table cites those by name instead:
//! task 0707's three seam-driven tests in `writer/index/apply/tests.rs`, its
//! gate-wiring test in `writer/index/prepared.rs`, task 0704's merge test in
//! `writer/index/property_collector.rs`, and task 0715's identity twins in
//! `writer/maintenance/apply_identity.rs`.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use froe::writer::index::{ReindexWarning, plan_reindex, reindex};
use support::property_index_layout::{Node, Property, write_repository_with_tree};
use support::reindex_fixtures::{
    TestDirectory, digest_lines, options, store_with_a_flagged_title_index,
};

/// The `nodetype` definition the path service reads, which every fixture
/// carries whether or not the run touches it.
fn nodetype_definition() -> Node {
    Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("property".to_owned()))
        .with(
            "propertyNames",
            Property::Names(vec!["jcr:primaryType".to_owned()]),
        )
}

/// A store whose `/oak:index` holds `subject` beside the `nodetype`
/// definition, over one titled content node.
fn store_with(directory: &TestDirectory, subject: Node) -> std::path::PathBuf {
    let store = directory.store();
    write_repository_with_tree(
        &store,
        &Node::new()
            .with_child(
                "content",
                Node::new()
                    .with(
                        "jcr:primaryType",
                        Property::Name("nt:unstructured".to_owned()),
                    )
                    .with("jcr:title", Property::Text("Alpha".to_owned())),
            )
            .with_child(
                "oak:index",
                Node::new()
                    .with_child("nodetype", nodetype_definition())
                    .with_child("subject", subject),
            ),
    );
    store
}

/// A flagged definition of `index_type` carrying `extras`.
fn flagged(index_type: &str, extras: Vec<(&str, Property)>) -> Node {
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

/// Planning with `/oak:index/subject` named explicitly, which is what turns
/// a skip into an answer: an operator who names a definition is always told
/// why it will not be rebuilt.
fn plan_naming_the_subject(
    directory: &TestDirectory,
    store: &std::path::Path,
) -> froe::writer::index::ReindexPlan {
    plan_reindex(
        store,
        &options(directory).with_indexes(["/oak:index/subject".to_owned()]),
    )
    .expect("planning answers a named definition rather than failing")
}

/// The refusal text for `/oak:index/subject`, from a plan that named it.
fn refusal_for_the_subject(directory: &TestDirectory, store: &std::path::Path) -> String {
    let plan = plan_naming_the_subject(directory, store);
    let refusals: Vec<String> = plan
        .warnings
        .iter()
        .filter_map(|warning| match warning {
            ReindexWarning::Skipped { refusal } => Some(refusal.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "a named definition is always answered with exactly one refusal; instead the \
         plan holds actions {:?} and warnings {:?}",
        plan.actions,
        plan.warnings
    );
    assert!(
        plan.is_empty(),
        "a refused definition leaves nothing to do: {:?}",
        plan.actions
    );
    refusals.into_iter().next().expect("one refusal")
}

#[test]
fn an_external_index_type_is_refused_because_its_data_is_not_in_the_repository() {
    let directory = TestDirectory::new("guard-external");
    let store = store_with(&directory, flagged("elasticsearch", Vec::new()));
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("whose data lives outside the repository"),
        "{refusal}"
    );
}

#[test]
fn a_type_with_no_editor_is_refused() {
    let directory = TestDirectory::new("guard-no-editor");
    let store = store_with(&directory, flagged("ordered", Vec::new()));
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(refusal.contains("is of type ordered"), "{refusal}");
}

#[test]
fn a_lucene_definition_is_refused_until_the_lucene_plan_lands() {
    let directory = TestDirectory::new("guard-lucene");
    let store = store_with(&directory, flagged("lucene", Vec::new()));
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("which this froe version does not rebuild"),
        "{refusal}"
    );
}

#[test]
fn a_value_pattern_regular_expression_is_refused_rather_than_ignored() {
    // Oak's editor filters values through the pattern before indexing them.
    // froe evaluates the prefix halves and not the regular expression, so
    // rebuilding would write entries Oak would have left out — a wrong
    // index, which is worse than no index.
    let directory = TestDirectory::new("guard-value-pattern");
    let store = store_with(
        &directory,
        flagged(
            "property",
            vec![
                (
                    "propertyNames",
                    Property::Names(vec!["jcr:title".to_owned()]),
                ),
                ("valuePattern", Property::Text("A.*".to_owned())),
            ],
        ),
    );
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("carries a valuePattern froe cannot evaluate"),
        "{refusal}"
    );
}

#[test]
fn a_definition_holding_a_composite_mounts_index_is_refused() {
    // A rebuild replaces the definition's hidden children. One of these
    // belongs to another mount of a composite store: froe did not write it
    // and cannot reproduce it, so removing it would be data loss.
    let directory = TestDirectory::new("guard-mount");
    let store = store_with(
        &directory,
        flagged(
            "property",
            vec![(
                "propertyNames",
                Property::Names(vec!["jcr:title".to_owned()]),
            )],
        )
        .with_child(
            ":oak:mount-libs-index",
            Node::new().with("match", Property::Boolean(true)),
        ),
    );
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("a composite-store mount's index data"),
        "{refusal}"
    );

    // And the consequence the refusal exists to prevent: an unnamed run
    // skips the definition, so the other mount's data is still there.
    reindex(&store, options(&directory)).expect("an unnamed run skips it");
    let mount = digest_lines(&store, "/oak:index/subject/:oak:mount-libs-index");
    assert!(
        !mount.is_empty(),
        "the rebuild removed a composite mount's index data, which froe did not \
         write and cannot reproduce"
    );
}

#[test]
fn a_path_filter_oak_cannot_construct_is_refused_as_such() {
    // Oak's `PathFilter` constructor throws on a relative value, so Oak's
    // own cycle skips this definition and leaves its `reindex` flag set.
    // froe must not quietly succeed where Oak fails — and must say which
    // failure it is, since the operator's next move differs.
    let directory = TestDirectory::new("guard-path-filter");
    let store = store_with(
        &directory,
        flagged(
            "property",
            vec![
                (
                    "propertyNames",
                    Property::Names(vec!["jcr:title".to_owned()]),
                ),
                (
                    "includedPaths",
                    Property::Texts(vec!["content/not-absolute".to_owned()]),
                ),
            ],
        ),
    );
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("has a path filter Oak cannot construct"),
        "{refusal}"
    );
}

#[test]
fn a_node_that_is_not_a_definition_is_refused() {
    let directory = TestDirectory::new("guard-not-a-definition");
    let store = store_with(
        &directory,
        Node::new()
            .with(
                "jcr:primaryType",
                Property::Name("nt:unstructured".to_owned()),
            )
            .with("reindex", Property::Boolean(true)),
    );
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("is not an oak:QueryIndexDefinition"),
        "{refusal}"
    );
}

#[test]
fn a_multi_valued_reindex_count_is_refused() {
    // Oak's own increment reads `reindexCount` as a single LONG and its
    // commit fails on a multi-valued one, so froe refuses rather than
    // choosing a value Oak would not have chosen.
    let directory = TestDirectory::new("guard-reindex-count");
    let store = store_with(
        &directory,
        flagged(
            "property",
            vec![
                (
                    "propertyNames",
                    Property::Names(vec!["jcr:title".to_owned()]),
                ),
                (
                    "reindexCount",
                    Property::Texts(vec!["1".to_owned(), "2".to_owned()]),
                ),
            ],
        ),
    );
    let error = reindex(&store, options(&directory))
        .expect_err("a multi-valued reindexCount must be refused");
    assert!(
        error.to_string().contains("reindexCount"),
        "the refusal names the property: {error}"
    );
}

#[test]
fn a_hit_less_counter_writes_no_index_node_at_all() {
    // Oak's editor returns before creating `:index` when nothing hits, so a
    // froe rebuild that wrote an empty `:index` would differ from Oak's own
    // reindex of the same store.
    let directory = TestDirectory::new("guard-hitless-counter");
    let store = directory.store();
    write_repository_with_tree(
        &store,
        &Node::new().with_child("content", Node::new()).with_child(
            "oak:index",
            Node::new()
                .with_child("nodetype", nodetype_definition())
                .with_child(
                    "subject",
                    flagged(
                        "counter",
                        vec![
                            ("seed", Property::Long(-7_610_761_686_379_641_542)),
                            // Large enough that nothing hits.
                            ("resolution", Property::Long(1_000_000)),
                        ],
                    ),
                ),
        ),
    );
    reindex(&store, options(&directory)).expect("reindex");
    assert!(
        digest_lines(&store, "/oak:index/subject/:index").is_empty(),
        "a hit-less counter must leave the definition without an :index"
    );
}

#[test]
fn an_empty_reference_set_writes_no_hidden_child() {
    let directory = TestDirectory::new("guard-empty-references");
    let store = store_with(&directory, flagged("reference", Vec::new()));
    reindex(&store, options(&directory)).expect("reindex");
    for name in [":references", ":weakreferences"] {
        assert!(
            digest_lines(&store, &format!("/oak:index/subject/{name}")).is_empty(),
            "an empty reference set must write no {name}"
        );
    }
}

#[test]
fn a_retained_hidden_child_survives_the_rebuild() {
    let directory = TestDirectory::new("guard-retained");
    let store = store_with(
        &directory,
        flagged(
            "property",
            vec![(
                "propertyNames",
                Property::Names(vec!["jcr:title".to_owned()]),
            )],
        )
        .with_child(
            ":keepme",
            Node::new()
                .with("retainNodeInReindex", Property::Boolean(true))
                .with("marker", Property::Text("kept".to_owned())),
        ),
    );
    reindex(&store, options(&directory)).expect("reindex");
    let retained = digest_lines(&store, "/oak:index/subject/:keepme");
    assert!(
        retained
            .first()
            .is_some_and(|line| line.contains("marker=String:kept")),
        "a retained hidden child must survive byte for byte: {retained:?}"
    );
    assert!(
        !digest_lines(&store, "/oak:index/subject/:index").is_empty(),
        "the rebuild still happened"
    );
}

#[test]
fn a_duplicate_unique_key_is_refused_before_anything_is_published() {
    // Oak refuses the commit that would create the second entry, so a store
    // holding both is one Oak could not have produced — and froe must not
    // produce an index that says it did.
    let directory = TestDirectory::new("guard-duplicate-unique");
    let store = directory.store();
    write_repository_with_tree(
        &store,
        &Node::new()
            .with_child(
                "content",
                Node::new()
                    .with("uid", Property::Text("same".to_owned()))
                    .with_child(
                        "other",
                        Node::new().with("uid", Property::Text("same".to_owned())),
                    ),
            )
            .with_child(
                "oak:index",
                Node::new()
                    .with_child("nodetype", nodetype_definition())
                    .with_child(
                        "subject",
                        flagged(
                            "property",
                            vec![
                                ("propertyNames", Property::Names(vec!["uid".to_owned()])),
                                ("unique", Property::Boolean(true)),
                            ],
                        ),
                    ),
            ),
    );
    let head_before = froe::store::Repository::open(&store)
        .expect("open")
        .head_record_identifier();

    let error =
        reindex(&store, options(&directory)).expect_err("a duplicate unique key must be refused");
    assert!(
        error.to_string().contains("unique"),
        "the refusal names the constraint: {error}"
    );
    assert_eq!(
        froe::store::Repository::open(&store)
            .expect("open")
            .head_record_identifier(),
        head_before,
        "the refusal must land before publication"
    );
}

#[test]
fn a_missing_manifest_is_refused_by_the_repository_shape_check() {
    let directory = TestDirectory::new("guard-shape");
    let store = store_with_a_flagged_title_index(&directory);
    std::fs::remove_file(store.join("manifest")).expect("remove the manifest");

    let error = plan_reindex(&store, &options(&directory))
        .expect_err("a store without a manifest is not a repository froe will touch");
    assert!(
        error.to_string().contains("manifest"),
        "the refusal names what is missing: {error}"
    );
}

#[test]
fn an_empty_plan_neither_opens_the_store_nor_moves_the_head() {
    // Nothing is flagged, so there is nothing to do — and "nothing to do"
    // has to mean the store is not opened for writing at all, or a run an
    // operator was told was a no-op would still rewrite a manifest.
    let directory = TestDirectory::new("guard-empty-plan");
    let store = directory.store();
    write_repository_with_tree(
        &store,
        &Node::new().with_child("content", Node::new()).with_child(
            "oak:index",
            Node::new().with_child("nodetype", nodetype_definition()),
        ),
    );
    let before: Vec<(std::ffi::OsString, Vec<u8>)> = std::fs::read_dir(&store)
        .expect("read the store")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name(),
                std::fs::read(entry.path()).expect("read"),
            )
        })
        .collect();

    let outcome = reindex(&store, options(&directory)).expect("reindex");
    assert!(!outcome.moved_the_head());
    for (name, bytes) in before {
        if name == "repo.lock" {
            continue;
        }
        assert_eq!(
            std::fs::read(store.join(&name)).expect("read after"),
            bytes,
            "{} changed under an empty plan",
            name.to_string_lossy()
        );
    }
}

/// `/:async` carrying `lane` with `checkpoint`, so a definition parked on
/// that lane resolves — or does not, when the checkpoint is dangling.
fn async_lanes(lane: &str, checkpoint: Option<&str>) -> Node {
    let mut node = Node::new();
    if let Some(checkpoint) = checkpoint {
        node = node.with(lane, Property::Text(checkpoint.to_owned()));
    }
    node
}

#[test]
fn a_dangling_lane_checkpoint_is_refused_without_from_head() {
    // The checkpoint the lane names is gone, so there is no state to index
    // from. Proceeding anyway is a decision only the operator can make.
    let directory = TestDirectory::new("guard-dangling");
    let store = directory.store();
    write_repository_with_tree(
        &store,
        &Node::new()
            .with_child("content", Node::new())
            .with_child(
                ":async",
                async_lanes("async", Some("checkpoint-that-is-gone")),
            )
            .with_child(
                "oak:index",
                Node::new()
                    .with_child("nodetype", nodetype_definition())
                    .with_child(
                        "subject",
                        flagged(
                            "property",
                            vec![
                                (
                                    "propertyNames",
                                    Property::Names(vec!["jcr:title".to_owned()]),
                                ),
                                ("async", Property::Text("async".to_owned())),
                            ],
                        ),
                    ),
            ),
    );
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("checkpoint") && refusal.contains("--from-head"),
        "the refusal names the checkpoint and what would authorize proceeding: {refusal}"
    );
}

#[test]
fn a_lane_absent_from_the_async_node_is_refused_under_its_own_variant() {
    // Oak's first cycle treats this like a lost checkpoint and diffs from
    // the missing state, so it is the same hazard — but an operator needs
    // to be able to tell the two apart.
    let directory = TestDirectory::new("guard-lane-absent");
    let store = directory.store();
    write_repository_with_tree(
        &store,
        &Node::new()
            .with_child("content", Node::new())
            .with_child(":async", async_lanes("other-lane", Some("c1")))
            .with_child(
                "oak:index",
                Node::new()
                    .with_child("nodetype", nodetype_definition())
                    .with_child(
                        "subject",
                        flagged(
                            "property",
                            vec![
                                (
                                    "propertyNames",
                                    Property::Names(vec!["jcr:title".to_owned()]),
                                ),
                                ("async", Property::Text("async".to_owned())),
                            ],
                        ),
                    ),
            ),
    );
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("which has no state on /:async"),
        "the absent lane has its own message: {refusal}"
    );
}

#[test]
fn a_reindex_lane_mid_run_is_refused() {
    // `/:async/async-reindex` carrying a checkpoint means Oak's own
    // operator-triggered lane is running. Two writers rebuilding one index
    // is out of scope, so froe stands down.
    let directory = TestDirectory::new("guard-lane-mid-run");
    let store = directory.store();
    write_repository_with_tree(
        &store,
        &Node::new()
            .with_child("content", Node::new())
            .with_child(":async", async_lanes("async-reindex", Some("running")))
            .with_child(
                "oak:index",
                Node::new()
                    .with_child("nodetype", nodetype_definition())
                    .with_child(
                        "subject",
                        flagged(
                            "property",
                            vec![
                                (
                                    "propertyNames",
                                    Property::Names(vec!["jcr:title".to_owned()]),
                                ),
                                ("async", Property::Text("async-reindex".to_owned())),
                            ],
                        ),
                    ),
            ),
    );
    let refusal = refusal_for_the_subject(&directory, &store);
    assert!(
        refusal.contains("async-reindex") && refusal.contains("in progress"),
        "{refusal}"
    );
}

#[test]
fn a_nested_definition_is_refused_rather_than_approximated() {
    // Oak scopes a definition under a content node to that node and runs a
    // child cycle whose paths are relative to it. froe indexes absolute
    // paths, so approximating would write an index that answers wrongly.
    let directory = TestDirectory::new("guard-nested");
    let store = directory.store();
    write_repository_with_tree(
        &store,
        &Node::new()
            .with_child(
                "content",
                Node::new().with_child(
                    "oak:index",
                    Node::new().with_child(
                        "nested",
                        flagged(
                            "property",
                            vec![(
                                "propertyNames",
                                Property::Names(vec!["jcr:title".to_owned()]),
                            )],
                        ),
                    ),
                ),
            )
            .with_child(
                "oak:index",
                Node::new().with_child("nodetype", nodetype_definition()),
            ),
    );
    let plan = plan_reindex(
        &store,
        &options(&directory).with_indexes(["/content/oak:index/nested".to_owned()]),
    )
    .expect("planning answers a named definition");
    let refusals: Vec<String> = plan
        .warnings
        .iter()
        .filter_map(|warning| match warning {
            ReindexWarning::Skipped { refusal } => Some(refusal.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.contains("nested under a content node")),
        "{refusals:?}"
    );
    assert!(plan.is_empty(), "{:?}", plan.actions);
}

#[test]
fn a_named_path_with_no_node_is_answered_rather_than_ignored() {
    let directory = TestDirectory::new("guard-absent-path");
    let store = store_with_a_flagged_title_index(&directory);
    let plan = plan_reindex(
        &store,
        &options(&directory).with_indexes(["/oak:index/does-not-exist".to_owned()]),
    )
    .expect("planning answers every named path");
    let refusals: Vec<String> = plan
        .warnings
        .iter()
        .filter_map(|warning| match warning {
            ReindexWarning::Skipped { refusal } => Some(refusal.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.contains("there is no node at this path")),
        "a named path that does not exist must be answered: {refusals:?}"
    );
}

#[test]
fn an_asynchronous_definition_is_indexed_from_its_lane_checkpoint_not_the_head() {
    // An async index is as current as its lane, and rebuilding it from the
    // head would silently move it forward — entries Oak's own lane has not
    // reached yet, in an index whose lane state still says otherwise. The
    // checkpoint's root is the state, and that is what this pins: content
    // added after the checkpoint must not be indexed.
    let directory = TestDirectory::new("guard-lane-state");
    let store = directory.store();

    let titled = |title: &str| {
        Node::new()
            .with(
                "jcr:primaryType",
                Property::Name("nt:unstructured".to_owned()),
            )
            .with("jcr:title", Property::Text(title.to_owned()))
    };
    let definitions = Node::new()
        .with_child("nodetype", nodetype_definition())
        .with_child(
            "subject",
            flagged(
                "property",
                vec![
                    (
                        "propertyNames",
                        Property::Names(vec!["jcr:title".to_owned()]),
                    ),
                    ("async", Property::Text("async".to_owned())),
                ],
            ),
        );

    // `/content/early` is in the checkpoint's root; `/content/late` is only
    // in the head. Both carry the indexed property.
    let head = Node::new()
        .with_child(
            "content",
            Node::new()
                .with_child("early", titled("Early"))
                .with_child("late", titled("Late")),
        )
        .with_child(":async", async_lanes("async", Some("lane-checkpoint")))
        .with_child("oak:index", definitions.clone());
    let pinned = Node::new()
        .with_child("content", Node::new().with_child("early", titled("Early")))
        .with_child("oak:index", definitions);

    support::property_index_layout::write_repository_with_checkpoints(
        &store,
        &head,
        &[("lane-checkpoint", pinned)],
    );

    reindex(&store, options(&directory)).expect("reindex from the lane checkpoint");
    let index = digest_lines(&store, "/oak:index/subject/:index");
    assert!(
        index.iter().any(|line| line.starts_with("/Early")),
        "the checkpoint's own content must be indexed: {index:?}"
    );
    assert!(
        !index.iter().any(|line| line.starts_with("/Late")),
        "content the lane has not reached must not be indexed: {index:?}"
    );
}

#[test]
fn a_missing_journal_is_refused_by_the_repository_shape_check() {
    let directory = TestDirectory::new("guard-shape-journal");
    let store = store_with_a_flagged_title_index(&directory);
    std::fs::remove_file(store.join("journal.log")).expect("remove the journal");

    let error = plan_reindex(&store, &options(&directory))
        .expect_err("a store without a journal is not a repository froe will touch");
    assert!(
        error.to_string().contains("journal"),
        "the refusal names what is missing: {error}"
    );
}

#[cfg(unix)]
#[test]
fn a_managed_file_that_is_a_symlink_is_refused_without_being_followed() {
    // The run resolves the repository path once and every later check is
    // about *that* directory — so a root the operator names through a
    // symlink is fine, and is canonicalized. A managed file inside the
    // store that is a symlink is not: writing through it would put bytes
    // somewhere the shape check never certified.
    let directory = TestDirectory::new("guard-shape-symlink");
    let store = store_with_a_flagged_title_index(&directory);
    let elsewhere = directory.path.join("journal-elsewhere.log");
    std::fs::rename(store.join("journal.log"), &elsewhere).expect("move the journal aside");
    std::os::unix::fs::symlink(&elsewhere, store.join("journal.log"))
        .expect("replace it with a symlink");

    let error = plan_reindex(&store, &options(&directory))
        .expect_err("a symlinked managed file must be refused");
    assert!(
        error.to_string().contains("is not a regular file"),
        "the refusal says the managed path is not what it must be: {error}"
    );
}

#[test]
fn a_directory_that_changed_during_confirmation_is_refused_before_any_record() {
    // The plan an operator confirmed described a store that no longer
    // exists. Applying it anyway would act on a state nobody approved.
    let directory = TestDirectory::new("guard-fingerprint");
    let store = store_with_a_flagged_title_index(&directory);
    let prepared = froe::writer::index::PreparedReindex::prepare(&store, options(&directory))
        .expect("prepare");

    // A new file in the store directory, which is what the fingerprint is
    // over. `repo.lock` is excluded by design, so this has to be a name the
    // fingerprint covers.
    std::fs::write(store.join("data00099a.tar"), b"not an archive\n")
        .expect("plant a change during confirmation");

    let error = match prepared.apply() {
        Ok(_) => panic!("a changed directory must be refused"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("changed between planning and applying"),
        "the refusal says what changed and when: {error}"
    );
}

#[test]
fn the_index_records_go_to_an_archive_number_above_every_physical_name() {
    // The run is additive: it must never write into an archive number that
    // already exists on disk, whether or not that archive is active.
    let directory = TestDirectory::new("guard-archive-number");
    let store = store_with_a_flagged_title_index(&directory);
    let numbers_before = archive_numbers(&store);
    let highest_before = *numbers_before
        .iter()
        .max()
        .expect("the fixture has an archive");

    reindex(&store, options(&directory)).expect("reindex");

    let added: Vec<u32> = archive_numbers(&store)
        .into_iter()
        .filter(|number| !numbers_before.contains(number))
        .collect();
    assert!(!added.is_empty(), "the run wrote no archive");
    for number in added {
        assert!(
            number > highest_before,
            "the run wrote archive {number}, at or below the {highest_before} already there"
        );
    }
}

/// The numeric part of every `dataNNNNNx.tar` in the store.
fn archive_numbers(store: &std::path::Path) -> Vec<u32> {
    std::fs::read_dir(store)
        .expect("read the store")
        .filter_map(|entry| {
            let name = entry.expect("entry").file_name();
            if std::path::Path::new(&name).extension()? != "tar" {
                return None;
            }
            name.to_string_lossy()
                .strip_prefix("data")?
                .get(..5)?
                .parse()
                .ok()
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn a_replaced_lock_file_is_refused_before_the_store_is_opened() {
    // The fingerprint skips `repo.lock` by design — the run creates it — so
    // a lock file swapped during confirmation is invisible to it. The
    // path-identity recheck is the only thing that sees it, and what it
    // sees is that the lock this run holds is no longer the lock at that
    // path: another writer could hold the new one.
    let directory = TestDirectory::new("guard-lock-identity");
    let store = store_with_a_flagged_title_index(&directory);
    let prepared = froe::writer::index::PreparedReindex::prepare(&store, options(&directory))
        .expect("prepare");

    std::fs::remove_file(store.join("repo.lock")).expect("remove the lock we hold");
    std::fs::write(store.join("repo.lock"), b"").expect("put a different inode there");

    let error = match prepared.apply() {
        Ok(_) => panic!("a replaced lock file must be refused"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("lock"),
        "the refusal names the lock: {error}"
    );
}
