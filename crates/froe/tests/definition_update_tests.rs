//! The definition rewrite and the selection, over synthetic stores.
//!
//! The rewrite's claim is a negative one — that it changes *only* what the
//! reindex protocol names — so every fixture compares the rewritten subtree
//! against the original through the lines of the content digest, which shows
//! every property, type, arity and value.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::PathBuf;

use froe::index::IndexDefinition;
use froe::store::Repository;
use froe::tooling::digest::digest_repository_excluding;
use froe::writer::index::definition_update::{
    DefinitionEdits, DisablerVerdict, ReindexCount, rewrite_definition,
};
use froe::writer::store_writer::WritableRepository;
use support::property_index_layout::{Node, Property, write_repository_with_tree};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-definition-update-{name}-{}-{:?}",
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

/// A property definition with the given extras.
fn definition(extras: Vec<(&str, Property)>) -> Node {
    let mut node = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("property".to_owned()))
        .with(
            "propertyNames",
            Property::Names(vec!["jcr:title".to_owned()]),
        );
    for (name, value) in extras {
        node = node.with(name, value);
    }
    node
}

/// Rewrites `original` with `edits` and returns the digest lines of the
/// definition before and after.
///
/// Both subtrees live in **one** store, and they have to: the rewrite
/// preserves every property slot it does not name *by record identifier*,
/// which is what makes it safe — and which means the rewritten node
/// references records of the store it was read from. Writing it into a
/// second store would produce a node pointing at a segment that store does
/// not have, and the digest would refuse it with `SegmentNotFound`. That is
/// the identity discipline working, not a defect.
fn rewrite(
    directory: &TestDirectory,
    original: Node,
    edits: &DefinitionEdits,
) -> (Vec<String>, Vec<String>) {
    let store_path = directory.path.join("store");
    std::fs::create_dir_all(&store_path).expect("create the store");
    write_repository_with_tree(
        &store_path,
        &Node::new().with_child("oak:index", Node::new().with_child("test", original)),
    );

    let before = digest_lines(&store_path, "/oak:index/test");

    let store = WritableRepository::open(&store_path).expect("open for writing");
    let generation = store.writing_generation().expect("generation");
    {
        let head = store.head_node();
        let content_root = head
            .child_node("root")
            .expect("read the root")
            .expect("the root exists");
        let definition_state = content_root
            .child_node("oak:index")
            .expect("read /oak:index")
            .expect("exists")
            .child_node("test")
            .expect("read the definition")
            .expect("exists");

        let mut writer = store.record_writer(generation);
        let rewritten = rewrite_definition(&store, &mut writer, &definition_state, edits)
            .expect("rewrite the definition");

        // Published beside the original under `/rewritten`, so the two can
        // be compared line for line in one digest.
        let mut edits = froe::writer::commit::ChildEdits::new();
        edits.insert("rewritten".to_owned(), Some(rewritten));
        let root = froe::writer::commit::rewrite_node_with_child_edits(
            &store,
            &mut writer,
            Some(content_root.record_identifier()),
            &edits,
        )
        .expect("rewrite the content root");
        let mut super_edits = froe::writer::commit::ChildEdits::new();
        super_edits.insert("root".to_owned(), Some(root));
        let super_root = froe::writer::commit::rewrite_node_with_child_edits(
            &store,
            &mut writer,
            Some(store.head()),
            &super_edits,
        )
        .expect("rewrite the super-root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.compare_and_set_head(previous, super_root));
    }
    store.close().expect("close");

    (before, digest_lines(&store_path, "/rewritten"))
}

/// The digest lines of `path` and everything below it, with the prefix
/// removed so two stores are comparable.
fn digest_lines(store: &std::path::Path, path: &str) -> Vec<String> {
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

/// The properties one digest line carries, as `name=Type:value` fields.
fn fields(lines: &[String], node: &str) -> Vec<String> {
    lines
        .iter()
        .find(|line| line.split('\t').next() == Some(node))
        .map(|line| line.split('\t').skip(1).map(str::to_owned).collect())
        .unwrap_or_default()
}

#[test]
fn a_reindex_clears_the_flag_and_increments_the_count() {
    let directory = TestDirectory::new("increment");
    let (before, after) = rewrite(
        &directory,
        definition(vec![
            ("reindex", Property::Boolean(true)),
            ("reindexCount", Property::Long(4)),
        ]),
        &DefinitionEdits::reindexed(DisablerVerdict::Leave, Vec::new()),
    );
    assert!(fields(&before, "").contains(&"reindex=Boolean:true".to_owned()));
    assert!(fields(&after, "").contains(&"reindex=Boolean:false".to_owned()));
    assert!(fields(&after, "").contains(&"reindexCount=Long:5".to_owned()));
}

#[test]
fn an_absent_reindex_count_is_created_at_one() {
    let directory = TestDirectory::new("absent-count");
    let (_, after) = rewrite(
        &directory,
        definition(vec![("reindex", Property::Boolean(true))]),
        &DefinitionEdits::reindexed(DisablerVerdict::Leave, Vec::new()),
    );
    assert!(fields(&after, "").contains(&"reindexCount=Long:1".to_owned()));
}

#[test]
fn a_string_reindex_flags_while_a_string_retain_does_not_retain() {
    // `reindex` is read converting, so a STRING "true" flags the
    // definition. `retainNodeInReindex` is read strictly, so a STRING
    // "true" does *not* keep the hidden child. Oak's own disagreement,
    // observable in a store, reproduced rather than smoothed over.
    let directory = TestDirectory::new("string-typed");
    let original = definition(vec![("reindex", Property::Text("true".to_owned()))])
        .with_child(
            ":index",
            Node::new().with("retainNodeInReindex", Property::Text("true".to_owned())),
        )
        .with_child(
            ":keepme",
            Node::new().with("retainNodeInReindex", Property::Boolean(true)),
        );
    let (before, after) = rewrite(
        &directory,
        original,
        &DefinitionEdits::reindexed(DisablerVerdict::Leave, Vec::new()),
    );
    assert!(before.iter().any(|line| line.starts_with("/:index")));
    assert!(
        !after.iter().any(|line| line.starts_with("/:index")),
        "a STRING retainNodeInReindex does not retain: {after:?}"
    );
    assert!(
        after.iter().any(|line| line.starts_with("/:keepme")),
        "a BOOLEAN one does: {after:?}"
    );
}

#[test]
fn corrupt_is_cleared_and_every_other_property_survives() {
    let directory = TestDirectory::new("corrupt");
    let (before, after) = rewrite(
        &directory,
        definition(vec![
            ("reindex", Property::Boolean(true)),
            (
                "corrupt",
                Property::Date("2026-01-01T00:00:00.000Z".to_owned()),
            ),
            ("info", Property::Text("kept verbatim".to_owned())),
            ("entryCount", Property::Long(42)),
        ]),
        &DefinitionEdits::reindexed(DisablerVerdict::Leave, Vec::new()),
    );
    assert!(
        fields(&before, "")
            .iter()
            .any(|f| f.starts_with("corrupt="))
    );
    assert!(
        !fields(&after, "").iter().any(|f| f.starts_with("corrupt=")),
        "corrupt is cleared"
    );
    for preserved in ["info=String:kept verbatim", "entryCount=Long:42"] {
        assert!(
            fields(&after, "").contains(&preserved.to_owned()),
            "{preserved} must survive: {:?}",
            fields(&after, "")
        );
    }
}

#[test]
fn the_disabler_flag_is_written_only_under_the_verdict() {
    // froe never disables the superseded indexes itself; Oak's *next* cycle
    // does, exactly as after an Oak reindex.
    let leave = TestDirectory::new("disabler-leave");
    let (_, without) = rewrite(
        &leave,
        definition(vec![("reindex", Property::Boolean(true))]),
        &DefinitionEdits::reindexed(DisablerVerdict::Leave, Vec::new()),
    );
    assert!(
        !fields(&without, "")
            .iter()
            .any(|f| f.starts_with(":disableIndexesOnNextCycle=")),
    );

    let flag = TestDirectory::new("disabler-flag");
    let (_, with) = rewrite(
        &flag,
        definition(vec![("reindex", Property::Boolean(true))]),
        &DefinitionEdits::reindexed(DisablerVerdict::Flag, Vec::new()),
    );
    assert!(
        fields(&with, "").contains(&":disableIndexesOnNextCycle=Boolean:true".to_owned()),
        "{:?}",
        fields(&with, "")
    );
}

#[test]
fn a_reset_changes_no_visible_property_at_all() {
    // A reset is not a reindex: Oak's next cycle is what rebuilds and what
    // does the bookkeeping, so `reindex` and `reindexCount` are untouched.
    let directory = TestDirectory::new("reset");
    let original = definition(vec![
        ("reindex", Property::Boolean(false)),
        ("reindexCount", Property::Long(3)),
    ])
    .with_child(":index", Node::new().with("junk", Property::Long(1)));
    let (before, after) = rewrite(&directory, original, &DefinitionEdits::reset());
    assert_eq!(
        fields(&before, ""),
        fields(&after, ""),
        "a reset touches no property of the definition node"
    );
    assert!(before.iter().any(|line| line.starts_with("/:index")));
    assert!(
        !after.iter().any(|line| line.starts_with("/:index")),
        "the hidden children are gone"
    );
}

#[test]
fn a_visible_child_survives_a_reindex_untouched() {
    // Plan 0007 writes no visible child; this is the regression that says
    // so, and the one plan 0010's `facets` subtree will keep honest.
    let directory = TestDirectory::new("visible-child");
    let original = definition(vec![("reindex", Property::Boolean(true))])
        .with_child(
            "indexRules",
            Node::new().with("nested", Property::Text("kept".to_owned())),
        )
        .with_child(":index", Node::new());
    let (before, after) = rewrite(
        &directory,
        original,
        &DefinitionEdits::reindexed(DisablerVerdict::Leave, Vec::new()),
    );
    let visible_before: Vec<&String> = before
        .iter()
        .filter(|line| line.starts_with("/indexRules"))
        .collect();
    let visible_after: Vec<&String> = after
        .iter()
        .filter(|line| line.starts_with("/indexRules"))
        .collect();
    assert_eq!(
        visible_before, visible_after,
        "a visible child is untouched"
    );
}

#[test]
fn a_multi_valued_reindex_count_is_refused_rather_than_guessed() {
    // Oak's own `getLong` throws on a multi-valued property, so its commit
    // fails. A rebuild that picked one of the values would write a count Oak
    // never would.
    let directory = TestDirectory::new("multi-count");
    let source = directory.path.join("source");
    std::fs::create_dir_all(&source).expect("create");
    write_repository_with_tree(
        &source,
        &Node::new().with_child(
            "oak:index",
            Node::new().with_child(
                "test",
                definition(vec![(
                    "reindexCount",
                    Property::Texts(vec!["1".to_owned(), "2".to_owned()]),
                )]),
            ),
        ),
    );
    let repository = Repository::open(&source).expect("open");
    let definition_state = repository
        .node_at_path("/oak:index/test")
        .expect("resolve")
        .expect("exists");

    let target = directory.path.join("target");
    std::fs::create_dir_all(&target).expect("create");
    let store = WritableRepository::open(&target).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let error = rewrite_definition(
        &repository,
        &mut writer,
        &definition_state,
        &DefinitionEdits::reindexed(DisablerVerdict::Leave, Vec::new()),
    )
    .expect_err("a multi-valued reindexCount must be refused");
    assert!(
        error.to_string().contains("multi-valued"),
        "the refusal names what it found: {error}"
    );
}

#[test]
fn an_importers_explicit_count_replaces_the_increment() {
    // Plan 0008's importer sets `reindexCount` to the file's value plus one
    // rather than incrementing what the store holds.
    let directory = TestDirectory::new("set-count");
    let mut edits = DefinitionEdits::reindexed(DisablerVerdict::Leave, Vec::new());
    edits.reindex_count = ReindexCount::Set(99);
    let (_, after) = rewrite(
        &directory,
        definition(vec![("reindexCount", Property::Long(4))]),
        &edits,
    );
    assert!(fields(&after, "").contains(&"reindexCount=Long:99".to_owned()));
}

#[test]
fn the_rewritten_definition_reads_back_as_a_model() {
    // The bookkeeping has to leave a definition froe's own reader still
    // understands — a rewrite that produced a node the model refuses would
    // pass every digest comparison above.
    let directory = TestDirectory::new("readable");
    let (_, _) = rewrite(
        &directory,
        definition(vec![("reindex", Property::Boolean(true))]),
        &DefinitionEdits::reindexed(DisablerVerdict::Flag, Vec::new()),
    );
    let repository = Repository::open(&directory.path.join("store")).expect("open");
    let node = repository
        .node_at_path("/rewritten")
        .expect("resolve")
        .expect("exists");
    let model = IndexDefinition::read(&node, "/oak:index/test").expect("model the rewrite");
    assert!(!model.reindex.flagged, "the flag is cleared");
    assert_eq!(model.reindex.count, 1);
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

use froe::index::inventory::IndexInventory;
use froe::writer::index::selection::{IndexingState, SelectionOptions, SelectionRefusal, select};

/// Builds a store with the given definitions under `/oak:index`, plus the
/// `nodetype` definition the path service requires, and selects over it.
fn selection(
    directory: &TestDirectory,
    definitions: Vec<(&str, Node)>,
    extra_root: Vec<(&str, Node)>,
    options: &SelectionOptions,
) -> (Vec<(String, IndexingState)>, Vec<SelectionRefusal>) {
    let store_path = directory.path.join("store");
    std::fs::create_dir_all(&store_path).expect("create the store");

    let mut oak_index = Node::new().with_child(
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
    );
    for (name, node) in definitions {
        oak_index = oak_index.with_child(name, node);
    }
    let mut root = Node::new().with_child("oak:index", oak_index);
    for (name, node) in extra_root {
        root = root.with_child(name, node);
    }
    write_repository_with_tree(&store_path, &root);

    let repository = Repository::open(&store_path).expect("open");
    let super_root = repository.head();
    let inventory = IndexInventory::collect(&repository, &super_root).expect("inventory");
    let selection = select(&repository, &super_root, &inventory, options).expect("select");
    (
        selection
            .selected
            .into_iter()
            .map(|selected| (selected.definition.path, selected.state))
            .collect(),
        selection.refused,
    )
}

/// A definition of `index_type`, flagged for reindex.
fn typed_definition(index_type: &str, extras: Vec<(&str, Property)>) -> Node {
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

#[test]
fn a_synchronous_definition_selects_the_head() {
    let directory = TestDirectory::new("select-head");
    let (selected, _) = selection(
        &directory,
        vec![(
            "title",
            typed_definition(
                "property",
                vec![(
                    "propertyNames",
                    Property::Names(vec!["jcr:title".to_owned()]),
                )],
            ),
        )],
        Vec::new(),
        &SelectionOptions::default(),
    );
    assert_eq!(
        selected,
        vec![("/oak:index/title".to_owned(), IndexingState::Head)]
    );
}

#[test]
fn an_asynchronous_definition_selects_its_lane_checkpoint() {
    let directory = TestDirectory::new("select-lane");
    let (selected, refused) = selection(
        &directory,
        vec![(
            "title",
            typed_definition(
                "property",
                vec![
                    (
                        "propertyNames",
                        Property::Names(vec!["jcr:title".to_owned()]),
                    ),
                    ("async", Property::Text("async".to_owned())),
                ],
            ),
        )],
        vec![(
            ":async",
            Node::new().with("async", Property::Text("cp-1".to_owned())),
        )],
        &SelectionOptions::default(),
    );
    // The checkpoint does not exist in this store, so the lane is dangling
    // and the definition is refused by name rather than silently rebuilt
    // from the head.
    assert!(selected.is_empty(), "{selected:?}");
    assert!(
        matches!(
            refused.first(),
            Some(SelectionRefusal::DanglingLaneCheckpoint { checkpoint, .. }) if checkpoint == "cp-1"
        ),
        "{refused:?}"
    );
}

#[test]
fn a_lane_with_no_state_at_all_is_its_own_refusal() {
    // Oak's first cycle treats an absent lane like a lost checkpoint and
    // diffs from the missing state, so it is the same hazard under a
    // different shape — and its own variant, so an operator can tell them
    // apart.
    let directory = TestDirectory::new("select-no-lane");
    let (_, refused) = selection(
        &directory,
        vec![(
            "title",
            typed_definition(
                "property",
                vec![
                    (
                        "propertyNames",
                        Property::Names(vec!["jcr:title".to_owned()]),
                    ),
                    ("async", Property::Text("async".to_owned())),
                ],
            ),
        )],
        Vec::new(),
        &SelectionOptions::default(),
    );
    assert!(
        matches!(
            refused.first(),
            Some(SelectionRefusal::LaneAbsent { lane, .. }) if lane == "async"
        ),
        "{refused:?}"
    );
}

#[test]
fn from_head_rebuilds_a_mirror_and_resets_a_counter() {
    // The two side effects `--from-head` authorizes, and no others.
    let directory = TestDirectory::new("from-head");
    let (selected, _) = selection(
        &directory,
        vec![
            (
                "title",
                typed_definition(
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
            (
                "counter",
                typed_definition(
                    "counter",
                    vec![("async", Property::Text("async".to_owned()))],
                ),
            ),
        ],
        Vec::new(),
        &SelectionOptions {
            requested_paths: Vec::new(),
            from_head: true,
        },
    );
    let states: std::collections::BTreeMap<String, IndexingState> = selected.into_iter().collect();
    assert_eq!(states.get("/oak:index/title"), Some(&IndexingState::Head));
    assert_eq!(
        states.get("/oak:index/counter"),
        Some(&IndexingState::ResetForReplay {
            lane: "async".to_owned()
        }),
        "a rebuild would double the counter whether or not froe ran"
    );
}

#[test]
fn every_unsupported_type_is_refused_by_name() {
    let directory = TestDirectory::new("unsupported");
    let (selected, refused) = selection(
        &directory,
        vec![
            ("lucene", typed_definition("lucene", Vec::new())),
            ("elastic", typed_definition("elasticsearch", Vec::new())),
            ("off", typed_definition("disabled", Vec::new())),
            ("old", typed_definition("ordered", Vec::new())),
            ("odd", typed_definition("something-else", Vec::new())),
        ],
        Vec::new(),
        &SelectionOptions::default(),
    );
    assert!(selected.is_empty(), "{selected:?}");
    let kinds: Vec<&str> = refused
        .iter()
        .map(|refusal| match refusal {
            SelectionRefusal::LuceneNotYetSupported { .. } => "lucene",
            SelectionRefusal::ExternalIndex { .. } => "external",
            SelectionRefusal::NoEditor { .. } => "no-editor",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds.iter().filter(|kind| **kind == "lucene").count(), 1);
    assert_eq!(kinds.iter().filter(|kind| **kind == "external").count(), 1);
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "no-editor").count(),
        3,
        "disabled, ordered and an unknown type all have no editor: {refused:?}"
    );
    for refusal in &refused {
        assert!(
            refusal.path().starts_with("/oak:index/"),
            "every refusal names its definition: {refusal:?}"
        );
    }
}

#[test]
fn an_unflagged_definition_is_left_alone_unless_it_is_named() {
    let directory = TestDirectory::new("unflagged");
    let unflagged = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("property".to_owned()))
        .with(
            "propertyNames",
            Property::Names(vec!["jcr:title".to_owned()]),
        );

    let (selected, _) = selection(
        &directory,
        vec![("title", unflagged.clone())],
        Vec::new(),
        &SelectionOptions::default(),
    );
    assert!(
        selected.is_empty(),
        "an unnamed run follows Oak's own cycle"
    );

    let named = TestDirectory::new("unflagged-named");
    let (selected, _) = selection(
        &named,
        vec![("title", unflagged)],
        Vec::new(),
        &SelectionOptions {
            requested_paths: vec!["/oak:index/title".to_owned()],
            from_head: false,
        },
    );
    assert_eq!(selected.len(), 1, "a named definition is rebuilt anyway");
}

#[test]
fn a_definition_parked_on_the_reindex_lane_is_treated_as_synchronous() {
    // The lane runs only when an operator triggers it, and its completion
    // removes `async` again — which is the state this run produces.
    let directory = TestDirectory::new("parked");
    let (selected, _) = selection(
        &directory,
        vec![(
            "title",
            typed_definition(
                "property",
                vec![
                    (
                        "propertyNames",
                        Property::Names(vec!["jcr:title".to_owned()]),
                    ),
                    ("async", Property::Text("async-reindex".to_owned())),
                ],
            ),
        )],
        Vec::new(),
        &SelectionOptions::default(),
    );
    assert_eq!(
        selected,
        vec![("/oak:index/title".to_owned(), IndexingState::Head)]
    );
}

#[test]
fn a_reindex_lane_that_is_mid_run_is_refused() {
    let directory = TestDirectory::new("mid-run");
    let (selected, refused) = selection(
        &directory,
        vec![(
            "title",
            typed_definition(
                "property",
                vec![
                    (
                        "propertyNames",
                        Property::Names(vec!["jcr:title".to_owned()]),
                    ),
                    ("async", Property::Text("async-reindex".to_owned())),
                ],
            ),
        )],
        vec![(
            ":async",
            Node::new().with("async-reindex", Property::Text("cp-9".to_owned())),
        )],
        &SelectionOptions::default(),
    );
    assert!(selected.is_empty(), "{selected:?}");
    assert!(
        matches!(
            refused.first(),
            Some(SelectionRefusal::ReindexLaneInProgress { .. })
        ),
        "{refused:?}"
    );
}
