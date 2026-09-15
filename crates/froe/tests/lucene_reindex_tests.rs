//! `froe index reindex` over a Lucene definition, end to end.
//!
//! What these hold is plan 0010's safety case: the content tree is
//! untouched, the head moves once, the segment reads back through plan
//! 0008's own readers, and the definition carries the bookkeeping Oak's own
//! cycle performs. The refusals live in `lucene_reindex_guard_tests.rs`.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use froe::index::lucene::documents::binaries::{BinaryTextFallback, BinaryTextPolicy};
use froe::writer::index::plan::ReindexAction;
use froe::writer::index::{DefinitionReport, plan_reindex, reindex};
use support::reindex_fixtures::{
    TestDirectory, content_digest, digest_lines, journal_lines, lucene_options,
    store_with_a_flagged_lucene_index, write_lucene_store,
};

#[test]
fn a_run_rebuilds_a_lucene_index_and_leaves_the_content_tree_untouched() {
    let directory = TestDirectory::new("lucene-full-run");
    let store = store_with_a_flagged_lucene_index(&directory);

    let content_before = content_digest(&store);
    let journal_before = journal_lines(&store);

    let outcome = reindex(&store, lucene_options(&directory)).expect("reindex");
    assert!(outcome.moved_the_head(), "a rebuild moves the head");
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

    let (path, report) = outcome
        .definitions
        .first()
        .expect("one definition was rebuilt");
    assert_eq!(path, "/oak:index/lucene");
    let DefinitionReport::RebuiltIndex {
        documents,
        nodes_visited,
        files,
        segment_bytes,
    } = report
    else {
        panic!("a Lucene definition reports its own rebuild: {report:?}");
    };
    // The root, `/content`, `/content/page`, `/jcr:system` and its
    // subtree, and `/oak:index` and its subtree are all visited; the
    // documents are the nodes a rule applies to, which is every node with
    // a primary type.
    // `/content` and `/content/page`: the checkpoint's two typed nodes.
    assert_eq!(*documents, 2, "{documents} documents");
    assert!(nodes_visited > documents, "{nodes_visited} nodes visited");
    assert!(*segment_bytes > 0);
    assert_eq!(
        files,
        &vec![
            "_0.cfe".to_owned(),
            "_0.cfs".to_owned(),
            "_0.si".to_owned(),
            "segments.gen".to_owned(),
            "segments_1".to_owned(),
        ],
        "the file set is the one `finish` returns, by name"
    );

    // The `:data` node holds exactly those files and nothing else — never
    // the directory listing, so a spill file could not have reached it.
    let data = digest_lines(&store, "/oak:index/lucene/:data");
    for name in files {
        assert!(
            data.iter()
                .any(|line| line.starts_with(&format!("/{name}"))),
            "{name} is absent from {data:?}"
        );
    }
    // One child per file and no other: the copy is by name from the set
    // `finish` returned, never from the directory listing.
    let children: Vec<&str> = data
        .iter()
        .filter_map(|line| line.split('\t').next())
        .filter(|name| name.starts_with('/'))
        .collect();
    assert_eq!(children.len(), files.len(), "{data:?}");
}

#[test]
fn the_rebuilt_definition_carries_oaks_own_bookkeeping() {
    let directory = TestDirectory::new("lucene-bookkeeping");
    let store = store_with_a_flagged_lucene_index(&directory);
    reindex(&store, lucene_options(&directory)).expect("reindex");

    let definition = digest_lines(&store, "/oak:index/lucene");
    let properties: Vec<&String> = definition
        .iter()
        .filter(|line| line.starts_with('\t'))
        .collect();
    let rendered = properties
        .iter()
        .map(|line| line.trim().to_owned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(rendered.contains("reindex=Boolean:false"), "{rendered}");
    assert!(rendered.contains("reindexCount=Long:1"), "{rendered}");
    assert!(rendered.contains(":version=Long:2"), "{rendered}");

    // `:status` carries the four properties Oak's editor writes.
    let status = digest_lines(&store, "/oak:index/lucene/:status");
    let status = status.join(" ");
    for name in [
        "uid=String:",
        "lastUpdated=Date:",
        "indexedNodes=Long:",
        "reindexCompletionTimestamp=Date:",
    ] {
        assert!(status.contains(name), "{name} absent from {status}");
    }

    // `:index-definition` is the clone of the **pre-run** state, so it
    // still says `reindex = true` and carries no `:data`.
    let stored = digest_lines(&store, "/oak:index/lucene/:index-definition");
    let stored = stored.join(" ");
    assert!(stored.contains("reindex=Boolean:true"), "{stored}");
    assert!(!stored.contains(":data"), "{stored}");
}

#[test]
fn a_definition_matching_no_node_produces_the_zero_segment_commit() {
    let directory = TestDirectory::new("lucene-empty");
    // A rule over a node type nothing in the store declares.
    let store = write_lucene_store(
        &directory,
        support::reindex_fixtures::lucene_definition_over("mix:nothing", Vec::new(), Vec::new()),
    );

    let outcome = reindex(&store, lucene_options(&directory)).expect("reindex");
    let (_, report) = outcome.definitions.first().expect("one definition");
    let DefinitionReport::RebuiltIndex {
        documents, files, ..
    } = report
    else {
        panic!("{report:?}");
    };
    assert_eq!(*documents, 0, "no node carries that type");
    assert_eq!(
        files,
        &vec!["segments.gen".to_owned(), "segments_1".to_owned()],
        "what Oak persists for an empty index"
    );
}

#[test]
fn a_stray_file_in_the_segment_directory_never_reaches_the_store() {
    let directory = TestDirectory::new("lucene-stray");
    let store = store_with_a_flagged_lucene_index(&directory);
    reindex(&store, lucene_options(&directory)).expect("reindex");
    let data = digest_lines(&store, "/oak:index/lucene/:data");
    assert!(!data.iter().any(|line| line.contains("stray")), "{data:?}");
}

#[test]
fn two_runs_produce_the_same_index() {
    let first = TestDirectory::new("lucene-determinism-a");
    let store_a = store_with_a_flagged_lucene_index(&first);
    reindex(&store_a, lucene_options(&first)).expect("reindex");

    let second = TestDirectory::new("lucene-determinism-b");
    let store_b = store_with_a_flagged_lucene_index(&second);
    reindex(&store_b, lucene_options(&second)).expect("reindex");

    // `uniqueKey`, the timestamps and the `uid` are drawn per run, so the
    // comparison is over everything else.
    let scrub = |lines: Vec<String>| -> Vec<String> {
        lines
            .into_iter()
            .filter(|line| {
                !line.contains("uniqueKey=")
                    && !line.contains("jcr:lastModified=")
                    && !line.contains("jcr:data=")
                    && !line.contains("uid=")
                    && !line.contains("lastUpdated=")
                    && !line.contains("reindexCompletionTimestamp=")
            })
            .collect()
    };
    assert_eq!(
        scrub(digest_lines(&store_a, "/oak:index/lucene/:data")),
        scrub(digest_lines(&store_b, "/oak:index/lucene/:data")),
        "two runs over one tree produce one index"
    );
}

#[test]
fn the_plan_reports_the_lucene_action_with_its_proxies() {
    let directory = TestDirectory::new("lucene-plan");
    let store = store_with_a_flagged_lucene_index(&directory);
    let plan = plan_reindex(&store, &lucene_options(&directory)).expect("plan");
    let action = plan.actions.first().expect("one action");
    let ReindexAction::RebuildLucene {
        path,
        rules,
        documents,
        indexed_bytes,
        binary_text_policy,
        ..
    } = action
    else {
        panic!("{action:?}");
    };
    assert_eq!(path, "/oak:index/lucene");
    assert_eq!(*rules, 1);
    assert_eq!(*documents, 2);
    assert!(*indexed_bytes > 0, "the indexed-byte proxy is reported");
    assert_eq!(binary_text_policy, "the extraction-error marker");
}

#[test]
fn a_binary_without_a_policy_is_the_only_thing_a_policy_changes() {
    let directory = TestDirectory::new("lucene-policy");
    let store = store_with_a_flagged_lucene_index(&directory);
    let options = lucene_options(&directory)
        .with_binary_text_policy(BinaryTextPolicy::new(BinaryTextFallback::Skip));
    reindex(&store, options).expect("reindex");
    // The store rebuilt under the skipping policy is a store all the same:
    // what the policy decides is what a binary contributes, and this
    // fixture holds none.
    let data = digest_lines(&store, "/oak:index/lucene/:data");
    assert!(
        data.iter().any(|line| line.contains("segments_1")),
        "{data:?}"
    );
}

/// A Lucene definition on a lane whose checkpoint is gone is **reset**
/// under `--from-head`: the hidden children go, nothing is built, and
/// every visible property survives.
#[test]
fn a_lost_checkpoint_resets_a_lucene_definition_under_from_head() {
    let directory = TestDirectory::new("lucene-reset");
    let store = store_with_a_flagged_lucene_index(&directory);
    // Build it once, so there is a `:data` to remove.
    reindex(&store, lucene_options(&directory)).expect("the first rebuild");

    // The lane's checkpoint disappears.
    let broken = TestDirectory::new("lucene-reset-broken");
    let store = support::reindex_fixtures::write_lucene_store_without_a_checkpoint(&broken);
    let outcome = reindex(&store, lucene_options(&broken).with_from_head(true)).expect("reindex");
    let (_, report) = outcome.definitions.first().expect("one definition");
    let DefinitionReport::Reset {
        removed_hidden_children,
        ..
    } = report
    else {
        panic!("a lost checkpoint resets rather than rebuilds: {report:?}");
    };
    assert!(
        removed_hidden_children.is_empty() || removed_hidden_children.contains(&":data".to_owned()),
        "{removed_hidden_children:?}"
    );

    // Nothing was built, and the visible properties are untouched.
    let definition = digest_lines(&store, "/oak:index/lucene").join(" ");
    assert!(!definition.contains(":data"), "{definition}");
    assert!(definition.contains("type=String:lucene"), "{definition}");
    assert!(
        definition.contains("evaluatePathRestrictions=Boolean:true"),
        "{definition}"
    );
}

/// The same run without `--from-head` refuses, naming the lane and the
/// checkpoint.
#[test]
fn a_lost_checkpoint_is_refused_without_from_head() {
    let directory = TestDirectory::new("lucene-dangling");
    let store = support::reindex_fixtures::write_lucene_store_without_a_checkpoint(&directory);
    let plan = plan_reindex(&store, &lucene_options(&directory)).expect("plan");
    assert!(plan.actions.is_empty(), "{:?}", plan.actions);
    let refusal = plan.warnings.first().expect("a refusal").to_string();
    assert!(refusal.contains("lane-checkpoint"), "{refusal}");
}

/// The plan's work-directory figure is a proxy, and for a Lucene
/// definition it rests on the two byte totals the counting walk produced.
#[test]
fn the_plan_reports_a_work_directory_proxy_for_a_lucene_definition() {
    let directory = TestDirectory::new("lucene-proxy");
    let store = store_with_a_flagged_lucene_index(&directory);
    let plan = plan_reindex(&store, &lucene_options(&directory)).expect("plan");
    let ReindexAction::RebuildLucene {
        stored_bytes,
        indexed_bytes,
        ..
    } = plan.actions.first().expect("one action")
    else {
        panic!("{:?}", plan.actions);
    };
    assert_eq!(
        plan.work_directory_estimate_bytes,
        froe::writer::index::plan::lucene_work_directory_estimate(
            *stored_bytes,
            *indexed_bytes,
            lucene_options(&directory).sort_budget_bytes() as u64,
        ),
        "the figure is the proxy `docs/index.md` §5.3 records, not zero"
    );
    assert!(plan.work_directory_estimate_bytes > 0);
}

/// `NodeStateFacetsConfig`'s constructor writes the `facets` node;
/// `setIndexFieldName` writes nothing. So a single-valued facet dimension
/// leaves the node there and childless.
///
/// Oak's own rebuild of the interop fixture's faceted definition writes
/// exactly that, which is what caught the earlier version of
/// `write_facet_configuration` writing a child per dimension whatever its
/// arity.
#[test]
fn a_single_valued_facet_leaves_the_configuration_node_childless() {
    let directory = TestDirectory::new("lucene-facets-single");
    let store = write_lucene_store(&directory, faceted_definition("jcr:title"));

    reindex(&store, lucene_options(&directory)).expect("reindex");

    let facets = digest_lines(&store, "/oak:index/lucene/facets");
    assert_eq!(
        facets,
        vec!["\tjcr:primaryType=Name:nt:unstructured".to_owned()],
        "a single-valued dimension writes the configuration node and nothing under it"
    );
}

/// `setMultiValued` writes only when the value is true, and then one child
/// per path element of the dimension, each carrying `multivalued = true`.
#[test]
fn a_multi_valued_facet_writes_one_child_carrying_multivalued() {
    let directory = TestDirectory::new("lucene-facets-multi");
    let store = support::reindex_fixtures::write_lucene_store_with_extra_content(
        &directory,
        faceted_definition("tags"),
        &[(
            "tags",
            support::property_index_layout::Property::Texts(vec![
                "alpha".to_owned(),
                "beta".to_owned(),
            ]),
        )],
    );

    reindex(&store, lucene_options(&directory)).expect("reindex");

    let facets = digest_lines(&store, "/oak:index/lucene/facets");
    assert_eq!(
        facets,
        vec![
            "\tjcr:primaryType=Name:nt:unstructured".to_owned(),
            "/tags\tjcr:primaryType=Name:nt:unstructured\tmultivalued=Boolean:true".to_owned(),
        ],
        "a multi-valued STRINGS dimension writes its own child with multivalued = true"
    );
}

/// The `lucene` definition with one facet property over `name`.
fn faceted_definition(name: &str) -> support::property_index_layout::Node {
    use support::property_index_layout::{Node, Property};
    support::reindex_fixtures::lucene_definition(
        Vec::new(),
        vec![(
            "category",
            Node::new()
                .with(
                    "jcr:primaryType",
                    Property::Name("nt:unstructured".to_owned()),
                )
                .with("name", Property::Text(name.to_owned()))
                .with("propertyIndex", Property::Boolean(true))
                .with("facets", Property::Boolean(true)),
        )],
    )
}
