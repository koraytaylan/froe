//! Orphaned version histories, end to end: an independent oracle for the
//! detection, the purge's exact removals, its exclusions, and the
//! checkpoint scoping.
//!
//! The oracle walks the store through the read-only content API and applies
//! the field query's logic directly — version histories whose
//! `jcr:versionableUuid` matches no live `jcr:uuid` outside version storage
//! — so the planner's collector machinery is checked by a second,
//! independent implementation, never by itself.

use froe::writer::commit::create_checkpoint;
use froe::writer::{
    ChildNodesToWrite, CompactionKind, CompactionOptions, PropertyToWrite, PropertyValuesToWrite,
    WritableRepository, compact, plan_compaction,
};
use froe::{CompactionAction, NodeState, PropertyType, Repository};

/// A scratch repository directory, removed when the test drops it.
struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-orphaned-histories-{name}"));
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

const LIVE_VERSIONABLE: &str = "aaaaaaaa-1111-4111-8111-111111111111";
const ORPHAN_VERSIONABLE: &str = "bbbbbbbb-2222-4222-8222-222222222222";
const CONFIGURATION_VERSIONABLE: &str = "cccccccc-3333-4333-8333-333333333333";
const ORPHAN_VERSION: &str = "bbbbbbbb-2222-4222-8222-aaaaaaaaaaaa";

/// What the fixture varies per scenario.
struct Fixture {
    /// A live REFERENCE property naming the orphan's version, which the
    /// advisory pass must then treat as a demotion.
    reference_into_orphan: bool,
    /// A checkpoint sharing the pre-deletion state, to prove scoping.
    checkpoint: bool,
}

/// One version history subtree: the history node, its root version, and
/// the frozen node.
struct HistoryToWrite<'fixture> {
    versionable: &'fixture str,
    history_identifier: &'fixture str,
    version_identifier: &'fixture str,
    frozen_primary_type: &'fixture str,
    /// A record the frozen node also holds — the shape that once made the
    /// planner mistake a live history for an orphan, when the shared
    /// record was certified inside version storage first.
    frozen_child: Option<(&'fixture str, froe::RecordIdentifier)>,
}

fn string_property(
    writer: &mut froe::writer::record_writer::RecordWriter<
        impl froe::writer::record_writer::SegmentSink,
    >,
    name: &str,
    property_type: PropertyType,
    text: &str,
) -> PropertyToWrite {
    let value = writer.write_string(text).expect("write the property value");
    PropertyToWrite {
        name: name.to_owned(),
        property_type,
        values: PropertyValuesToWrite::Single(value),
    }
}

fn write_history(
    writer: &mut froe::writer::record_writer::RecordWriter<
        impl froe::writer::record_writer::SegmentSink,
    >,
    history: &HistoryToWrite<'_>,
) -> froe::RecordIdentifier {
    let frozen_properties = vec![
        string_property(
            writer,
            "jcr:frozenPrimaryType",
            PropertyType::Name,
            history.frozen_primary_type,
        ),
        string_property(
            writer,
            "jcr:frozenUuid",
            PropertyType::String,
            history.versionable,
        ),
    ];
    let frozen_children = match &history.frozen_child {
        Some((name, node)) => ChildNodesToWrite::One {
            name: (*name).to_owned(),
            node: *node,
        },
        None => ChildNodesToWrite::Zero,
    };
    let frozen = writer
        .write_node(
            Some("nt:frozenNode"),
            &[],
            &frozen_children,
            &frozen_properties,
        )
        .expect("write the frozen node");
    let version_properties = vec![
        string_property(
            writer,
            "jcr:uuid",
            PropertyType::String,
            history.version_identifier,
        ),
        string_property(
            writer,
            "jcr:created",
            PropertyType::Date,
            "2020-01-01T00:00:00.000Z",
        ),
    ];
    let root_version = writer
        .write_node(
            Some("nt:version"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:frozenNode".to_owned(),
                node: frozen,
            },
            &version_properties,
        )
        .expect("write the root version");
    let history_properties = vec![
        string_property(
            writer,
            "jcr:uuid",
            PropertyType::String,
            history.history_identifier,
        ),
        string_property(
            writer,
            "jcr:versionableUuid",
            PropertyType::String,
            history.versionable,
        ),
    ];
    writer
        .write_node(
            Some("nt:versionHistory"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:rootVersion".to_owned(),
                node: root_version,
            },
            &history_properties,
        )
        .expect("write the history")
}

/// Wraps `node` in the version-storage hash directories `a/b/c`.
fn wrap_in_intermediates(
    writer: &mut froe::writer::record_writer::RecordWriter<
        impl froe::writer::record_writer::SegmentSink,
    >,
    first: &str,
    second: &str,
    third: &str,
    name: &str,
    node: froe::RecordIdentifier,
) -> (String, froe::RecordIdentifier) {
    let mut wrapped = node;
    let mut wrapped_name = name.to_owned();
    for level in [third, second, first] {
        wrapped = writer
            .write_node(
                Some("rep:versionStorage"),
                &[],
                &ChildNodesToWrite::One {
                    name: wrapped_name.clone(),
                    node: wrapped,
                },
                &[],
            )
            .expect("write the intermediate");
        level.clone_into(&mut wrapped_name);
    }
    (wrapped_name, wrapped)
}

/// The three histories under their intermediate chains, joined under one
/// version-storage node inside `jcr:system`.
fn write_version_storage_system(
    writer: &mut froe::writer::record_writer::RecordWriter<
        impl froe::writer::record_writer::SegmentSink,
    >,
) -> froe::RecordIdentifier {
    let live_history = write_history(
        writer,
        &HistoryToWrite {
            versionable: LIVE_VERSIONABLE,
            history_identifier: "aaaaaaaa-1111-4111-8111-999999999999",
            version_identifier: "aaaaaaaa-1111-4111-8111-888888888888",
            frozen_primary_type: "nt:unstructured",
            frozen_child: None,
        },
    );
    let orphan_history = write_history(
        writer,
        &HistoryToWrite {
            versionable: ORPHAN_VERSIONABLE,
            history_identifier: "bbbbbbbb-2222-4222-8222-999999999999",
            version_identifier: ORPHAN_VERSION,
            frozen_primary_type: "nt:unstructured",
            frozen_child: None,
        },
    );
    let configuration_history = write_history(
        writer,
        &HistoryToWrite {
            versionable: CONFIGURATION_VERSIONABLE,
            history_identifier: "cccccccc-3333-4333-8333-999999999999",
            version_identifier: "cccccccc-3333-4333-8333-aaaaaaaaaaaa",
            frozen_primary_type: "nt:configuration",
            frozen_child: None,
        },
    );
    let (live_name, live_wrapped) =
        wrap_in_intermediates(writer, "aa", "ab", "ac", LIVE_VERSIONABLE, live_history);
    let (orphan_name, orphan_wrapped) =
        wrap_in_intermediates(writer, "ba", "bb", "bc", ORPHAN_VERSIONABLE, orphan_history);
    let (configuration_name, configuration_wrapped) = wrap_in_intermediates(
        writer,
        "ca",
        "cb",
        "cc",
        CONFIGURATION_VERSIONABLE,
        configuration_history,
    );
    let version_storage = writer
        .write_node(
            Some("rep:versionStorage"),
            &[],
            &ChildNodesToWrite::Many(vec![
                (live_name, live_wrapped),
                (orphan_name, orphan_wrapped),
                (configuration_name, configuration_wrapped),
            ]),
            &[],
        )
        .expect("write the version storage");
    writer
        .write_node(
            Some("rep:system"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:versionStorage".to_owned(),
                node: version_storage,
            },
            &[],
        )
        .expect("write jcr:system")
}

/// Builds the store: one live versionable with its history, one orphaned
/// history (its versionable deleted), and one orphaned configuration
/// history — each under its own intermediate chain.
fn write_store(directory: &std::path::Path, fixture: &Fixture) {
    let store = WritableRepository::open(directory).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);
    let jcr_system = write_version_storage_system(&mut writer);

    let mut page_properties = vec![string_property(
        &mut writer,
        "jcr:uuid",
        PropertyType::String,
        LIVE_VERSIONABLE,
    )];
    if fixture.reference_into_orphan {
        page_properties.push(string_property(
            &mut writer,
            "heldVersion",
            PropertyType::Reference,
            ORPHAN_VERSION,
        ));
    }
    let live_page = writer
        .write_node(
            Some("nt:unstructured"),
            &["mix:versionable".to_owned()],
            &ChildNodesToWrite::Zero,
            &page_properties,
        )
        .expect("write the live page");
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::One {
                name: "page".to_owned(),
                node: live_page,
            },
            &[],
        )
        .expect("write the content");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("content".to_owned(), content),
                ("jcr:system".to_owned(), jcr_system),
            ]),
            &[],
        )
        .expect("write the root");
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &[],
        )
        .expect("write the super root");
    writer.finish().expect("finish the writer");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush the store");
    if fixture.checkpoint {
        create_checkpoint(&store, i64::MAX / 4, &[]).expect("create the checkpoint");
    }
    store.close().expect("close the store");
}

/// The field query's logic over the read-only content API: version
/// histories whose versionable identifier matches no live `jcr:uuid`.
fn oracle_orphans(directory: &std::path::Path) -> Vec<String> {
    let repository = Repository::open(directory).expect("open for the oracle");
    let root = repository
        .node_at_path("/")
        .expect("resolve the root")
        .expect("the root exists");
    let mut live = std::collections::BTreeSet::new();
    collect_live_identifiers(&root, "", &mut live);
    let mut orphans = Vec::new();
    let storage = repository
        .node_at_path("/jcr:system/jcr:versionStorage")
        .expect("resolve version storage")
        .expect("version storage exists");
    collect_orphan_histories(&storage, &live, &mut orphans);
    orphans.sort();
    orphans
}

fn collect_live_identifiers(
    node: &NodeState<'_>,
    path: &str,
    live: &mut std::collections::BTreeSet<String>,
) {
    if path.starts_with("/jcr:system/jcr:versionStorage") {
        return;
    }
    for property in node.properties().expect("read properties") {
        if property.name == "jcr:uuid"
            && let froe::PropertyValues::Single(froe::PropertyValue::String(text)) =
                &property.values
        {
            live.insert(text.to_ascii_lowercase());
        }
    }
    for (name, child) in node.child_node_entries().expect("read children") {
        collect_live_identifiers(&child, &format!("{path}/{name}"), live);
    }
}

fn collect_orphan_histories(
    node: &NodeState<'_>,
    live: &std::collections::BTreeSet<String>,
    orphans: &mut Vec<String>,
) {
    let properties = node.properties().expect("read properties");
    let primary_type = properties.iter().find_map(|property| {
        (property.name == "jcr:primaryType").then(|| match &property.values {
            froe::PropertyValues::Single(froe::PropertyValue::Name(name)) => name.clone(),
            _ => String::new(),
        })
    });
    if primary_type.as_deref() == Some("nt:versionHistory") {
        for property in &properties {
            if property.name == "jcr:versionableUuid"
                && let froe::PropertyValues::Single(froe::PropertyValue::String(text)) =
                    &property.values
                && !live.contains(&text.to_ascii_lowercase())
            {
                orphans.push(text.to_ascii_lowercase());
            }
        }
        return;
    }
    for (_, child) in node.child_node_entries().expect("read children") {
        collect_orphan_histories(&child, live, orphans);
    }
}

/// The planner's orphan count must equal the oracle's, on the same store.
#[test]
fn detection_agrees_with_the_independent_oracle() {
    let directory = TestDirectory::new("oracle");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let orphans = oracle_orphans(&directory.path);
    assert_eq!(
        orphans,
        vec![
            ORPHAN_VERSIONABLE.to_owned(),
            CONFIGURATION_VERSIONABLE.to_owned()
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>(),
        "the oracle itself must find exactly the two orphans"
    );

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the store");
    let report = plan.orphaned_version_histories();
    assert_eq!(report.orphaned_histories, orphans.len() as u64);
    assert_eq!(
        report.orphaned_nodes, 6,
        "two orphan histories of three nodes each"
    );
    assert_eq!(report.malformed_identifiers, 0);
}

/// The purge removes exactly the plain orphan: the configuration history
/// is kept with a warning, the live history survives, the emptied
/// intermediates vanish, and a repeat run converges to nothing.
#[test]
fn a_purge_removes_the_orphan_and_converges() {
    let directory = TestDirectory::new("purge");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    let plan = plan_compaction(&directory.path, &options).expect("plan the purge");
    let repeat_options = options.clone();
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories {
                histories: 1,
                nodes: 3,
                ..
            }
        )),
        "exactly the plain orphan is selected: {:?}",
        plan.actions()
    );
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("nt:configuration")),
        "the configuration exclusion is warned about: {:?}",
        plan.warnings()
    );

    compact(&directory.path, options).expect("apply the purge");

    let repository = Repository::open(&directory.path).expect("reopen after the purge");
    assert!(
        repository
            .node_at_path(&format!(
                "/jcr:system/jcr:versionStorage/ba/bb/bc/{ORPHAN_VERSIONABLE}"
            ))
            .expect("resolve the purged history")
            .is_none(),
        "the purged history must be gone from the head"
    );
    assert!(
        repository
            .node_at_path("/jcr:system/jcr:versionStorage/ba")
            .expect("resolve the emptied intermediate")
            .is_none(),
        "an intermediate whose only history was purged must be gone"
    );
    for surviving in [
        format!("/jcr:system/jcr:versionStorage/aa/ab/ac/{LIVE_VERSIONABLE}"),
        format!("/jcr:system/jcr:versionStorage/ca/cb/cc/{CONFIGURATION_VERSIONABLE}"),
        "/content/page".to_owned(),
    ] {
        assert!(
            repository
                .node_at_path(&surviving)
                .expect("resolve a survivor")
                .is_some(),
            "{surviving} must survive the purge"
        );
    }
    drop(repository);
    assert_eq!(
        oracle_orphans(&directory.path),
        vec![CONFIGURATION_VERSIONABLE.to_owned()]
    );

    let repeat = plan_compaction(&directory.path, &repeat_options).expect("plan the repeat");
    assert!(
        repeat.is_empty(),
        "with only the excluded configuration left, the repeat converges: {:?}",
        repeat.actions()
    );
    assert!(repeat.already_fully_compacted());
}

/// A REFERENCE value outside version storage naming a record inside a
/// candidate demotes it: the advisory pass fails safe.
#[test]
fn an_inbound_reference_demotes_the_candidate() {
    let directory = TestDirectory::new("reference-demotion");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: true,
            checkpoint: false,
        },
    );

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    let plan = plan_compaction(&directory.path, &options).expect("plan the demoted purge");
    assert!(
        !plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories { .. }
        )),
        "a referenced history must not be selected: {:?}",
        plan.actions()
    );
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("REFERENCE")),
        "the demotion is warned about: {:?}",
        plan.warnings()
    );
}

/// The age bound keeps a history whose newest version is younger than the
/// bound — proven with a bound far larger than the fixture's age, so the
/// test needs no clock arithmetic of its own.
#[test]
fn the_age_bound_keeps_young_histories() {
    let directory = TestDirectory::new("age-bound");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let two_centuries = std::time::Duration::from_secs(200 * 365 * 24 * 60 * 60);
    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge()
            .with_purged_history_minimum_age(two_centuries),
    )
    .expect("plan with the age bound");
    assert!(
        !plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories { .. }
        )),
        "nothing is two centuries old: {:?}",
        plan.actions()
    );
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("age bound")),
        "the age exclusion is warned about: {:?}",
        plan.warnings()
    );
}

/// A retained checkpoint's snapshot keeps its own version storage: the
/// purge removes the history from the head, and the checkpoint still
/// resolves it.
#[test]
fn a_checkpoint_snapshot_keeps_what_the_head_purges() {
    let directory = TestDirectory::new("checkpoint-scope");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: true,
        },
    );

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    compact(&directory.path, options).expect("apply the purge");

    let repository = Repository::open(&directory.path).expect("reopen after the purge");
    assert!(
        repository
            .node_at_path(&format!(
                "/jcr:system/jcr:versionStorage/ba/bb/bc/{ORPHAN_VERSIONABLE}"
            ))
            .expect("resolve the purged history")
            .is_none(),
        "the head loses the history"
    );
    let checkpoints = repository.checkpoints().expect("list checkpoints");
    assert_eq!(checkpoints.len(), 1, "the checkpoint survives the run");
    let (_, checkpoint) = &checkpoints[0];
    let mut snapshot = checkpoint
        .child_node("root")
        .expect("read the snapshot root")
        .expect("the snapshot has a root");
    for name in [
        "jcr:system",
        "jcr:versionStorage",
        "ba",
        "bb",
        "bc",
        ORPHAN_VERSIONABLE,
    ] {
        snapshot = snapshot
            .child_node(name)
            .expect("descend the snapshot")
            .unwrap_or_else(|| panic!("the checkpoint's snapshot must keep resolving {name}"));
    }
}

/// Joins prepared version-storage children under `jcr:system`, adds any
/// further root children, and publishes the head — the finalization every
/// hand-built fixture shares. The caller closes the store, whose borrow
/// the writer holds until this returns.
fn publish_version_storage(
    store: &WritableRepository,
    mut writer: froe::writer::record_writer::RecordWriter<
        impl froe::writer::record_writer::SegmentSink,
    >,
    version_storage_children: Vec<(String, froe::RecordIdentifier)>,
    extra_root_children: Vec<(String, froe::RecordIdentifier)>,
) {
    let version_storage = writer
        .write_node(
            Some("rep:versionStorage"),
            &[],
            &ChildNodesToWrite::Many(version_storage_children),
            &[],
        )
        .expect("write the version storage");
    let jcr_system = writer
        .write_node(
            Some("rep:system"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:versionStorage".to_owned(),
                node: version_storage,
            },
            &[],
        )
        .expect("write jcr:system");
    let mut root_children = extra_root_children;
    root_children.push(("jcr:system".to_owned(), jcr_system));
    let root = writer
        .write_node(None, &[], &ChildNodesToWrite::Many(root_children), &[])
        .expect("write the root");
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &[],
        )
        .expect("write the super root");
    writer.finish().expect("finish the writer");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush the store");
}

/// A record reachable both under live content and inside a frozen node
/// must keep its history live. This is the regression the walk order
/// exists to prevent: when version storage was certified before the
/// content tree, the shared record was memo-skipped out of the live walk,
/// its `jcr:uuid` never matched, and the live history read as an orphan —
/// which a purge would then have removed.
#[test]
fn a_record_shared_into_a_frozen_subtree_stays_live() {
    let directory = TestDirectory::new("shared-frozen-record");
    let store = WritableRepository::open(&directory.path).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    // The live page first, so the same record can be attached in both
    // places: under the content tree and inside the live history's frozen
    // node.
    let page_properties = vec![string_property(
        &mut writer,
        "jcr:uuid",
        PropertyType::String,
        LIVE_VERSIONABLE,
    )];
    let live_page = writer
        .write_node(
            Some("nt:unstructured"),
            &["mix:versionable".to_owned()],
            &ChildNodesToWrite::Zero,
            &page_properties,
        )
        .expect("write the live page");

    let live_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: LIVE_VERSIONABLE,
            history_identifier: "aaaaaaaa-1111-4111-8111-999999999999",
            version_identifier: "aaaaaaaa-1111-4111-8111-888888888888",
            frozen_primary_type: "nt:unstructured",
            frozen_child: Some(("sharedPage", live_page)),
        },
    );
    let orphan_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: ORPHAN_VERSIONABLE,
            history_identifier: "bbbbbbbb-2222-4222-8222-999999999999",
            version_identifier: ORPHAN_VERSION,
            frozen_primary_type: "nt:unstructured",
            frozen_child: None,
        },
    );
    let (live_name, live_wrapped) = wrap_in_intermediates(
        &mut writer,
        "aa",
        "ab",
        "ac",
        LIVE_VERSIONABLE,
        live_history,
    );
    let (orphan_name, orphan_wrapped) = wrap_in_intermediates(
        &mut writer,
        "ba",
        "bb",
        "bc",
        ORPHAN_VERSIONABLE,
        orphan_history,
    );
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::One {
                name: "page".to_owned(),
                node: live_page,
            },
            &[],
        )
        .expect("write the content");
    publish_version_storage(
        &store,
        writer,
        vec![(live_name, live_wrapped), (orphan_name, orphan_wrapped)],
        vec![("content".to_owned(), content)],
    );
    store.close().expect("close the store");

    assert_eq!(
        oracle_orphans(&directory.path),
        vec![ORPHAN_VERSIONABLE.to_owned()],
        "the oracle sees exactly the deliberate orphan"
    );
    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the store");
    let report = plan.orphaned_version_histories();
    assert_eq!(
        report.orphaned_histories, 1,
        "the live history shares a record into its frozen node and must not read as orphaned"
    );
}

/// A history whose `jcr:versionableUuid` does not parse shares its
/// intermediate chain with a selected orphan. The purge must remove the
/// orphan and keep the chain, because pruning it would delete the
/// unclassifiable history with it.
#[test]
fn a_malformed_sibling_under_a_shared_intermediate_survives_the_purge() {
    let directory = TestDirectory::new("malformed-sibling");
    let store = WritableRepository::open(&directory.path).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    let orphan_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: ORPHAN_VERSIONABLE,
            history_identifier: "bbbbbbbb-2222-4222-8222-999999999999",
            version_identifier: ORPHAN_VERSION,
            frozen_primary_type: "nt:unstructured",
            frozen_child: None,
        },
    );
    let malformed_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: "not-an-identifier",
            history_identifier: "dddddddd-4444-4444-8444-999999999999",
            version_identifier: "dddddddd-4444-4444-8444-aaaaaaaaaaaa",
            frozen_primary_type: "nt:unstructured",
            frozen_child: None,
        },
    );
    // Both histories under the same deepest intermediate, so the chain is
    // only removable if *everything* under it goes.
    let deepest = writer
        .write_node(
            Some("rep:versionStorage"),
            &[],
            &ChildNodesToWrite::Many(vec![
                (ORPHAN_VERSIONABLE.to_owned(), orphan_history),
                ("malformed".to_owned(), malformed_history),
            ]),
            &[],
        )
        .expect("write the deepest intermediate");
    let mut wrapped = deepest;
    let mut wrapped_name = "bc".to_owned();
    for level in ["bb", "ba"] {
        wrapped = writer
            .write_node(
                Some("rep:versionStorage"),
                &[],
                &ChildNodesToWrite::One {
                    name: wrapped_name.clone(),
                    node: wrapped,
                },
                &[],
            )
            .expect("write the intermediate");
        level.clone_into(&mut wrapped_name);
    }
    publish_version_storage(&store, writer, vec![(wrapped_name, wrapped)], Vec::new());
    store.close().expect("close the store");

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    let plan = plan_compaction(&directory.path, &options).expect("plan the purge");
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories { histories: 1, .. }
        )),
        "exactly the parseable orphan is selected: {:?}",
        plan.actions()
    );
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("do not parse")),
        "the unclassifiable history is warned about: {:?}",
        plan.warnings()
    );

    compact(&directory.path, options).expect("apply the purge");

    let repository = Repository::open(&directory.path).expect("reopen after the purge");
    assert!(
        repository
            .node_at_path(&format!(
                "/jcr:system/jcr:versionStorage/ba/bb/bc/{ORPHAN_VERSIONABLE}"
            ))
            .expect("resolve the purged history")
            .is_none(),
        "the selected orphan must be gone"
    );
    let survivor = repository
        .node_at_path("/jcr:system/jcr:versionStorage/ba/bb/bc/malformed")
        .expect("resolve the malformed history")
        .expect("the malformed history must survive: its intermediate chain was shared");
    let kept = survivor
        .properties()
        .expect("read the survivor's properties")
        .iter()
        .any(|property| property.name == "jcr:versionableUuid");
    assert!(
        kept,
        "the survivor keeps its unparseable identifier property"
    );
}

/// The copy estimate subtracts what the purge omits: planning the same
/// store with and without the purge must differ by exactly the report's
/// node-record estimate.
#[test]
fn a_purged_copy_is_predicted_cheaper_by_the_node_record_estimate() {
    let directory = TestDirectory::new("purged-copy-estimate");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let without_purge = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan without the purge");
    let with_purge = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge(),
    )
    .expect("plan with the purge");

    let estimate = with_purge
        .orphaned_version_histories()
        .node_record_bytes_estimate;
    assert!(estimate > 0, "the orphans hold node records");
    assert_eq!(
        without_purge
            .predicted_copy_output_bytes()
            .expect("the plain plan predicts a copy"),
        with_purge
            .predicted_copy_output_bytes()
            .expect("the purging plan predicts a copy")
            + estimate,
        "the purge makes the predicted copy cheaper by exactly its estimate"
    );
}

/// Convergence-gate condition: a selected purge forces the copy. The same
/// store that gates an ordinary full run must not gate one that omits
/// content, or the purge would silently never happen.
#[test]
fn a_selected_purge_forces_the_copy_through_the_gate() {
    let directory = TestDirectory::new("purge-through-gate");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    compact(
        &directory.path,
        CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("compact without the purge");

    let gated = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the plain repeat");
    assert!(
        gated.already_fully_compacted(),
        "without a purge the compacted store gates the copy"
    );
    assert_eq!(gated.effective_compaction_kind(), None);

    let purging = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge(),
    )
    .expect("plan the purging repeat");
    assert!(
        !purging.already_fully_compacted(),
        "a selected purge means the run has content work"
    );
    assert_eq!(
        purging.effective_compaction_kind(),
        Some(CompactionKind::Full)
    );
    assert!(
        purging.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories { .. }
        )),
        "the purge is in the plan: {:?}",
        purging.actions()
    );
}

/// A purge whose selection is empty must change nothing about the plan:
/// action for action, it is the plan the same options produce without the
/// purge flag.
#[test]
fn an_empty_selection_leaves_the_plan_identical() {
    let directory = TestDirectory::new("empty-selection");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let two_centuries = std::time::Duration::from_secs(200 * 365 * 24 * 60 * 60);
    let without_purge = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan without the purge");
    let empty_purge = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge()
            .with_purged_history_minimum_age(two_centuries),
    )
    .expect("plan the age-emptied purge");

    assert_eq!(
        without_purge.actions(),
        empty_purge.actions(),
        "an empty selection is the plain plan, byte for byte"
    );
    assert_eq!(
        without_purge.predicted_copy_output_bytes(),
        empty_purge.predicted_copy_output_bytes(),
        "nothing is omitted, so nothing is subtracted"
    );
}

/// With a checkpoint retained, both the purge action and the report say
/// so: the operator reading either sees that released bulk may stay
/// pinned until the checkpoint expires.
#[test]
fn a_retained_checkpoint_is_reported_on_the_purge_and_the_report() {
    let directory = TestDirectory::new("checkpoint-caveat");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: true,
        },
    );

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge(),
    )
    .expect("plan the scoped purge");
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories {
                retained_checkpoints: 1,
                ..
            }
        )),
        "the action names the retained checkpoint: {:?}",
        plan.actions()
    );
    assert_eq!(
        plan.orphaned_version_histories().retained_checkpoints,
        1,
        "the report carries the same caveat"
    );
}

/// Bulk the purge releases is real: an orphan holding a large inline
/// binary reports released bulk, and the store shrinks by at least that
/// much once the purge lands.
#[test]
fn released_bulk_is_reported_and_actually_freed() {
    let directory = TestDirectory::new("released-bulk");
    let store = WritableRepository::open(&directory.path).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    // Large enough that the binary cannot live inline in a data segment
    // and must spill into bulk blocks.
    let large_binary = writer
        .write_binary_content(&vec![0xAB_u8; 600 * 1024])
        .expect("write the large binary");
    let file = writer
        .write_node(
            Some("nt:file"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(large_binary),
            }],
        )
        .expect("write the file node");
    let orphan_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: ORPHAN_VERSIONABLE,
            history_identifier: "bbbbbbbb-2222-4222-8222-999999999999",
            version_identifier: ORPHAN_VERSION,
            frozen_primary_type: "nt:unstructured",
            frozen_child: Some(("file", file)),
        },
    );
    let (orphan_name, orphan_wrapped) = wrap_in_intermediates(
        &mut writer,
        "ba",
        "bb",
        "bc",
        ORPHAN_VERSIONABLE,
        orphan_history,
    );
    publish_version_storage(
        &store,
        writer,
        vec![(orphan_name, orphan_wrapped)],
        Vec::new(),
    );
    store.close().expect("close the store");

    let directory_bytes = |path: &std::path::Path| -> u64 {
        std::fs::read_dir(path)
            .expect("list the store")
            .map(|entry| {
                entry
                    .expect("read an entry")
                    .metadata()
                    .expect("stat")
                    .len()
            })
            .sum()
    };
    let before = directory_bytes(&directory.path);

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    let plan = plan_compaction(&directory.path, &options).expect("plan the purge");
    let report = plan.orphaned_version_histories();
    assert!(
        report.released_bulk_segments > 0,
        "a 600 KiB binary must occupy bulk segments"
    );
    assert!(report.released_bulk_bytes > 0);
    assert_eq!(
        report.retained_checkpoints, 0,
        "no checkpoints, so nothing can pin the released blocks"
    );

    compact(&directory.path, options).expect("apply the purge");
    let after = directory_bytes(&directory.path);
    // The reported figure is an upper bound in general (a record shared
    // into kept content retains its blocks), but this fixture shares
    // nothing across histories, so here the ceiling is attained and the
    // store must shrink by at least that much.
    assert!(
        before.saturating_sub(after) >= report.released_bulk_bytes,
        "the store must shrink by at least the released bulk: before {before}, after {after}, released {}",
        report.released_bulk_bytes
    );
}

/// External binaries referenced only inside version storage are part of
/// the head's footprint. The census must count them even though the
/// version-storage walk is the only walk that certifies those records —
/// a blob held solely by old versions is exactly the blob-store bloat the
/// footprint exists to explain.
#[test]
fn the_external_footprint_covers_version_storage() {
    let directory = TestDirectory::new("external-in-history");
    let store = WritableRepository::open(&directory.path).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    let history_only_blob = writer
        .write_external_binary_identifier(
            "cafe84c92565b98a45f1bb0a9fef2eff804239ba1b96e4ae4e29e0e4222829ab#2048",
        )
        .expect("write the history-only reference");
    let file = writer
        .write_node(
            Some("nt:file"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(history_only_blob),
            }],
        )
        .expect("write the frozen file");
    let orphan_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: ORPHAN_VERSIONABLE,
            history_identifier: "bbbbbbbb-2222-4222-8222-999999999999",
            version_identifier: ORPHAN_VERSION,
            frozen_primary_type: "nt:unstructured",
            frozen_child: Some(("file", file)),
        },
    );
    let (orphan_name, orphan_wrapped) = wrap_in_intermediates(
        &mut writer,
        "ba",
        "bb",
        "bc",
        ORPHAN_VERSIONABLE,
        orphan_history,
    );
    let live_blob = writer
        .write_external_binary_identifier(
            "00b6d84c92565b98a45f1bb0a9fef2eff804239ba1b96e4ae4e29e0e4222829a#1000",
        )
        .expect("write the live reference");
    let live_file = writer
        .write_node(
            Some("nt:file"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(live_blob),
            }],
        )
        .expect("write the live file");
    publish_version_storage(
        &store,
        writer,
        vec![(orphan_name, orphan_wrapped)],
        vec![("attachment".to_owned(), live_file)],
    );
    store.close().expect("close the store");

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the store");
    let footprint = plan.external_binary_footprint();
    assert_eq!(
        footprint.distinct_references, 2,
        "one live blob and one referenced only inside the history"
    );
    assert_eq!(footprint.measured_bytes, 3048);
    assert_eq!(footprint.unmeasured_references, 0);
    assert_eq!(
        plan.orphaned_version_histories().external_references,
        1,
        "the orphan report still attributes the history's own reference"
    );
}
