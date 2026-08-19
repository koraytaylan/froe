//! The shared fixture: hand-built version storage in every shape the
//! suite needs, and the independent oracle the detection is held to.

use froe::writer::commit::create_checkpoint;
use froe::writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, WritableRepository};
use froe::{NodeState, PropertyType, Repository};

/// A scratch repository directory, removed when the test drops it.
pub(crate) struct TestDirectory {
    pub(crate) path: std::path::PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
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

pub(crate) const LIVE_VERSIONABLE: &str = "aaaaaaaa-1111-4111-8111-111111111111";
pub(crate) const ORPHAN_VERSIONABLE: &str = "bbbbbbbb-2222-4222-8222-222222222222";
pub(crate) const CONFIGURATION_VERSIONABLE: &str = "cccccccc-3333-4333-8333-333333333333";
pub(crate) const ORPHAN_VERSION: &str = "bbbbbbbb-2222-4222-8222-aaaaaaaaaaaa";

/// What the fixture varies per scenario.
pub(crate) struct Fixture {
    /// A live REFERENCE property naming the orphan's version, which the
    /// advisory pass must then treat as a demotion.
    pub(crate) reference_into_orphan: bool,
    /// A checkpoint sharing the pre-deletion state, to prove scoping.
    pub(crate) checkpoint: bool,
}

/// One version history subtree: the history node, its root version, and
/// the frozen node.
pub(crate) struct HistoryToWrite<'fixture> {
    pub(crate) versionable: &'fixture str,
    pub(crate) history_identifier: &'fixture str,
    pub(crate) version_identifier: &'fixture str,
    pub(crate) frozen_primary_type: &'fixture str,
    /// A record the frozen node also holds — the shape that once made the
    /// planner mistake a live history for an orphan, when the shared
    /// record was certified inside version storage first.
    pub(crate) frozen_child: Option<(&'fixture str, froe::RecordIdentifier)>,
}

pub(crate) fn string_property(
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

pub(crate) fn write_history(
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
pub(crate) fn wrap_in_intermediates(
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
pub(crate) fn write_store(directory: &std::path::Path, fixture: &Fixture) {
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
pub(crate) fn oracle_orphans(directory: &std::path::Path) -> Vec<String> {
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

/// Joins prepared version-storage children under `jcr:system`, adds any
/// further root children, and publishes the head — the finalization every
/// hand-built fixture shares. The caller closes the store, whose borrow
/// the writer holds until this returns.
pub(crate) fn publish_version_storage(
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
