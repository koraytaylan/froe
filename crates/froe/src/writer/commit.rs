//! The commit path: editing the content tree and managing checkpoints.
//!
//! Every commit rewrites the *super-root* — the node the journal points
//! at, whose children are `root` (the content tree) and `checkpoints` —
//! and advances the head with a compare-and-set, exactly like Oak's
//! `SegmentNodeStore`. Untouched subtrees, property values, and child
//! maps are preserved by record identifier, so a commit writes only the
//! spine of records from the changed nodes up to the super-root.
//!
//! Checkpoints follow `LockBasedScheduler.CPCreator` faithfully: creation
//! first purges expired or corrupt checkpoints, then records `timestamp`
//! (the expiry, clamped at the 64-bit maximum), `created`, a `properties`
//! child with the caller's string metadata, and a `root` child that
//! *shares* the current content root's record — an immutable snapshot at
//! zero cost. Releasing a checkpoint is plain child removal.

use std::collections::{BTreeMap, BTreeSet};

use crate::content::provider::SegmentProvider;
use crate::content::template::ChildNodeArity;
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;
use crate::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use crate::writer::store_writer::WritableRepository;

/// A named edit to one node's children: `Some` inserts or replaces the
/// child with the given node record, `None` removes it.
pub type ChildEdits = BTreeMap<String, Option<RecordIdentifier>>;

/// The reusable parts of an existing node.
struct NodeParts {
    primary_type: Option<String>,
    mixin_types: Vec<String>,
    properties: Vec<PropertyToWrite>,
    children: ExistingChildren,
}

/// The child structure of an existing node.
enum ExistingChildren {
    Zero,
    One {
        name: String,
        node: RecordIdentifier,
    },
    Many {
        map: RecordIdentifier,
    },
}

/// Reads the parts of an existing node needed to rewrite it while
/// preserving untouched records by identifier.
fn read_node_parts(provider: &dyn SegmentProvider, node: RecordIdentifier) -> Result<NodeParts> {
    let view = provider.segment(node.segment)?;
    let template_identifier = view.read_record_identifier(node.record_number, 0, 1)?;
    let template = provider.template(template_identifier)?;

    let children = match &template.child_arity {
        ChildNodeArity::Zero => ExistingChildren::Zero,
        ChildNodeArity::One { child_name } => ExistingChildren::One {
            name: child_name.clone(),
            node: view.read_record_identifier(node.record_number, 0, 2)?,
        },
        ChildNodeArity::Many => ExistingChildren::Many {
            map: view.read_record_identifier(node.record_number, 0, 2)?,
        },
    };

    let mut properties = Vec::with_capacity(template.properties.len());
    if !template.properties.is_empty() {
        let slot = if matches!(template.child_arity, ChildNodeArity::Zero) {
            2
        } else {
            3
        };
        let list_identifier = view.read_record_identifier(node.record_number, 0, slot)?;
        for (index, property) in template.properties.iter().enumerate() {
            let value_slot = crate::content::list::uncounted_list_entry(
                provider,
                list_identifier,
                template.properties.len() as u64,
                index as u64,
            )?;
            properties.push(PropertyToWrite {
                name: property.name.clone(),
                property_type: property.property_type,
                values: PropertyValuesToWrite::PreservedSlot {
                    value_slot,
                    is_multiple: property.is_multiple,
                },
            });
        }
    }

    Ok(NodeParts {
        primary_type: template.primary_type.clone(),
        mixin_types: template.mixin_types.clone(),
        properties,
        children,
    })
}

/// Rewrites a node with edits to its children, preserving its primary
/// type, mixin types, and every property slot. `base` of `None` starts
/// from an empty node.
#[allow(
    clippy::missing_panics_doc,
    reason = "the single-entry expect is guarded by the match arm's length"
)]
pub fn rewrite_node_with_child_edits<Sink: SegmentSink>(
    provider: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    base: Option<RecordIdentifier>,
    edits: &ChildEdits,
) -> Result<RecordIdentifier> {
    let parts = match base {
        Some(node) => read_node_parts(provider, node)?,
        None => NodeParts {
            primary_type: None,
            mixin_types: Vec::new(),
            properties: Vec::new(),
            children: ExistingChildren::Zero,
        },
    };

    // Materialize the resulting child set. Unchanged many-child maps are
    // preserved wholesale; any edit rebuilds the map from its entries
    // (which themselves preserve the child records by identifier).
    let children = if edits.is_empty() {
        match parts.children {
            ExistingChildren::Zero => ChildNodesToWrite::Zero,
            ExistingChildren::One { name, node } => ChildNodesToWrite::One { name, node },
            ExistingChildren::Many { map } => ChildNodesToWrite::ManyExistingMap(map),
        }
    } else {
        let mut entries: BTreeMap<String, RecordIdentifier> = match parts.children {
            ExistingChildren::Zero => BTreeMap::new(),
            ExistingChildren::One { name, node } => {
                let mut entries = BTreeMap::new();
                entries.insert(name, node);
                entries
            }
            ExistingChildren::Many { map } => crate::content::map::map_entries(provider, map)?
                .into_iter()
                .map(|entry| (entry.name, entry.value))
                .collect(),
        };
        for (name, edit) in edits {
            match edit {
                Some(node) => {
                    entries.insert(name.clone(), *node);
                }
                None => {
                    entries.remove(name);
                }
            }
        }
        match entries.len() {
            0 => ChildNodesToWrite::Zero,
            1 => {
                let (name, node) = entries.into_iter().next().expect("one entry");
                ChildNodesToWrite::One { name, node }
            }
            _ => ChildNodesToWrite::Many(entries.into_iter().collect()),
        }
    };

    writer.write_node(
        parts.primary_type.as_deref(),
        &parts.mixin_types,
        &children,
        &parts.properties,
    )
}

/// The checkpoint metadata AEM reads back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointDescription {
    /// The checkpoint's name (a random UUID string).
    pub name: String,
    /// Creation time in milliseconds since the epoch, when present.
    pub created_milliseconds: Option<i64>,
    /// Expiry time in milliseconds since the epoch, when present.
    pub expires_milliseconds: Option<i64>,
}

/// Lists the checkpoints of the store's current head.
pub fn list_checkpoints(store: &WritableRepository) -> Result<Vec<CheckpointDescription>> {
    let head = store.head_node();
    let Some(checkpoints) = head.child_node("checkpoints")? else {
        return Ok(Vec::new());
    };
    let mut descriptions = Vec::new();
    for (name, checkpoint) in checkpoints.child_node_entries()? {
        descriptions.push(CheckpointDescription {
            created_milliseconds: read_long_property(&checkpoint, "created")?,
            expires_milliseconds: read_long_property(&checkpoint, "timestamp")?,
            name,
        });
    }
    Ok(descriptions)
}

fn read_long_property(
    node: &crate::content::node::NodeState<'_>,
    name: &str,
) -> Result<Option<i64>> {
    use crate::content::node::PropertyValues;
    use crate::content::property::PropertyValue;
    Ok(node
        .property(name)?
        .and_then(|property| match property.values {
            PropertyValues::Single(PropertyValue::Long(value)) => Some(value),
            _ => None,
        }))
}

/// Creates a checkpoint with the given lifetime and string metadata,
/// returning its generated name. Expired checkpoints are purged first,
/// exactly like Oak's checkpoint creator.
pub fn create_checkpoint(
    store: &WritableRepository,
    lifetime_milliseconds: i64,
    properties: &[(String, String)],
) -> Result<String> {
    if lifetime_milliseconds <= 0 {
        return Err(Error::InvalidFormat {
            details: "checkpoint lifetime must be positive".to_owned(),
        });
    }
    let now = current_time_milliseconds();
    let name = random_checkpoint_name();
    let expiry = if i64::MAX - now > lifetime_milliseconds {
        now + lifetime_milliseconds
    } else {
        i64::MAX
    };

    let head = store.head();
    let head_node = store.head_node();
    let checkpoints_container = head_node.child_node("checkpoints")?;
    let content_root = head_node
        .child_node("root")?
        .ok_or_else(|| Error::InvalidFormat {
            details: "the super-root has no \"root\" child node".to_owned(),
        })?;

    let generation = store.writing_generation()?;
    let mut writer = store.record_writer(generation);

    // Purge expired or corrupt checkpoints while assembling the edits.
    let mut edits: ChildEdits = ChildEdits::new();
    if let Some(container) = &checkpoints_container {
        for (existing_name, checkpoint) in container.child_node_entries()? {
            let expires = read_long_property(&checkpoint, "timestamp")?;
            if expires.is_none_or(|expires| now > expires) {
                edits.insert(existing_name, None);
            }
        }
    }

    // The properties child holds the caller's string metadata.
    let mut property_writes = Vec::with_capacity(properties.len());
    for (key, value) in properties {
        let value_identifier = writer.write_string(value)?;
        property_writes.push(PropertyToWrite {
            name: key.clone(),
            property_type: crate::content::property::PropertyType::String,
            values: PropertyValuesToWrite::Single(value_identifier),
        });
    }
    crate::writer::record_writer::sort_properties_for_template(&mut property_writes);
    let properties_node =
        writer.write_node(None, &[], &ChildNodesToWrite::Zero, &property_writes)?;

    // The checkpoint node: timestamp, created, properties, root snapshot.
    let timestamp_value = writer.write_string(&expiry.to_string())?;
    let created_value = writer.write_string(&now.to_string())?;
    let mut checkpoint_properties = vec![
        PropertyToWrite {
            name: "timestamp".to_owned(),
            property_type: crate::content::property::PropertyType::Long,
            values: PropertyValuesToWrite::Single(timestamp_value),
        },
        PropertyToWrite {
            name: "created".to_owned(),
            property_type: crate::content::property::PropertyType::Long,
            values: PropertyValuesToWrite::Single(created_value),
        },
    ];
    crate::writer::record_writer::sort_properties_for_template(&mut checkpoint_properties);
    let checkpoint_node = writer.write_node(
        None,
        &[],
        &ChildNodesToWrite::Many(vec![
            ("properties".to_owned(), properties_node),
            ("root".to_owned(), content_root.record_identifier()),
        ]),
        &checkpoint_properties,
    )?;
    edits.insert(name.clone(), Some(checkpoint_node));

    // Rebuild the checkpoints container and the super-root.
    let container_base = checkpoints_container.map(|container| container.record_identifier());
    let new_container = rewrite_node_with_child_edits(store, &mut writer, container_base, &edits)?;
    let mut super_root_edits = ChildEdits::new();
    super_root_edits.insert("checkpoints".to_owned(), Some(new_container));
    let new_head =
        rewrite_node_with_child_edits(store, &mut writer, Some(head), &super_root_edits)?;
    writer.finish()?;

    if !store.set_head(head, new_head) {
        return Err(Error::InvalidFormat {
            details: "the head moved while creating the checkpoint".to_owned(),
        });
    }
    store.flush()?;
    Ok(name)
}

/// Releases (removes) a checkpoint by name. Returns whether it existed.
pub fn release_checkpoint(store: &WritableRepository, name: &str) -> Result<bool> {
    Ok(remove_checkpoints_with_head_move_error(
        store,
        &[name.to_owned()],
        "the head moved while releasing the checkpoint",
    )? != 0)
}

/// Removes the named checkpoints in one atomic head update and returns
/// how many existed.
///
/// Duplicate names count once and names absent from the captured head are
/// ignored. When none of the supplied names exist, this is a true no-op:
/// no records are written, the head is not advanced, and the journal is
/// not flushed. Otherwise all removals share one rewritten checkpoint
/// container, one compare-and-set of the head, and one flush.
pub fn remove_checkpoints(store: &WritableRepository, names: &[String]) -> Result<u64> {
    remove_checkpoints_with_head_move_error(
        store,
        names,
        "the head moved while removing checkpoints",
    )
}

fn remove_checkpoints_with_head_move_error(
    store: &WritableRepository,
    names: &[String],
    head_move_error: &str,
) -> Result<u64> {
    if names.is_empty() {
        return Ok(0);
    }

    let requested: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    let head = store.head();
    // Bind all reads to the head used by the compare-and-set. This keeps
    // the planned edits internally consistent even if another caller
    // advances the in-memory head before our CAS (the CAS will then fail).
    let head_node = crate::content::node::NodeState::new(store, head);
    let Some(container) = head_node.child_node("checkpoints")? else {
        return Ok(0);
    };

    let mut existing_names = Vec::with_capacity(requested.len());
    for name in requested {
        if container.child_node(name)?.is_some() {
            existing_names.push(name.to_owned());
        }
    }
    if existing_names.is_empty() {
        return Ok(0);
    }

    let generation = store.writing_generation()?;
    let mut writer = store.record_writer(generation);
    let mut edits = ChildEdits::new();
    for name in &existing_names {
        edits.insert(name.clone(), None);
    }
    let new_container = rewrite_node_with_child_edits(
        store,
        &mut writer,
        Some(container.record_identifier()),
        &edits,
    )?;
    let mut super_root_edits = ChildEdits::new();
    super_root_edits.insert("checkpoints".to_owned(), Some(new_container));
    let new_head =
        rewrite_node_with_child_edits(store, &mut writer, Some(head), &super_root_edits)?;
    writer.finish()?;

    if !store.set_head(head, new_head) {
        return Err(Error::InvalidFormat {
            details: head_move_error.to_owned(),
        });
    }
    store.flush()?;
    Ok(existing_names.len() as u64)
}

/// Removes every checkpoint. Returns how many were removed.
pub fn remove_all_checkpoints(store: &WritableRepository) -> Result<u64> {
    let names: Vec<String> = list_checkpoints(store)?
        .into_iter()
        .map(|checkpoint| checkpoint.name)
        .collect();
    remove_checkpoints(store, &names)
}

/// Removes every checkpoint not referenced from the asynchronous indexer
/// state at `/:async`. Conservative: any string property value (or
/// member of a multi-valued string property) of that node counts as a
/// reference.
pub fn remove_unreferenced_checkpoints(store: &WritableRepository) -> Result<u64> {
    use crate::content::node::PropertyValues;
    use crate::content::property::PropertyValue;

    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let head_node = store.head_node();
    if let Some(content_root) = head_node.child_node("root")?
        && let Some(async_state) = content_root.child_node(":async")?
    {
        for property in async_state.properties()? {
            match property.values {
                PropertyValues::Single(PropertyValue::String(value)) => {
                    referenced.insert(value);
                }
                PropertyValues::Multiple(values) => {
                    for value in values {
                        if let PropertyValue::String(value) = value {
                            referenced.insert(value);
                        }
                    }
                }
                PropertyValues::Single(_) => {}
            }
        }
    }

    let names: Vec<String> = list_checkpoints(store)?
        .into_iter()
        .map(|checkpoint| checkpoint.name)
        .filter(|name| !referenced.contains(name))
        .collect();
    remove_checkpoints(store, &names)
}

/// Replaces the content root (`/root` of the super-root) with an already
/// written node record and advances the head. The restore operation's
/// final step.
pub fn replace_content_root(store: &WritableRepository, new_root: RecordIdentifier) -> Result<()> {
    let head = store.head();
    let generation = store.writing_generation()?;
    let mut writer = store.record_writer(generation);
    let mut edits = ChildEdits::new();
    edits.insert("root".to_owned(), Some(new_root));
    let new_head = rewrite_node_with_child_edits(store, &mut writer, Some(head), &edits)?;
    writer.finish()?;
    if !store.set_head(head, new_head) {
        return Err(Error::InvalidFormat {
            details: "the head moved while replacing the content root".to_owned(),
        });
    }
    store.flush()?;
    Ok(())
}

/// The current wall clock in milliseconds since the Unix epoch.
fn current_time_milliseconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

/// A random version 4 UUID string, the checkpoint naming scheme.
fn random_checkpoint_name() -> String {
    // Reuse the segment identifier entropy stream; a checkpoint name is
    // an ordinary v4 UUID without the segment kind marker.
    let identifier = crate::writer::identifier_generator::new_data_segment_identifier();
    let most = identifier.most_significant_bits;
    // Restore a proper random variant nibble (10xx) in place of the data
    // segment marker.
    let least = (identifier.least_significant_bits & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        most >> 32,
        (most >> 16) & 0xFFFF,
        most & 0xFFFF,
        least >> 48,
        least & 0xFFFF_FFFF_FFFF,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        create_checkpoint, list_checkpoints, release_checkpoint, remove_all_checkpoints,
        remove_checkpoints, remove_unreferenced_checkpoints, replace_content_root,
    };
    use crate::store::Repository;
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-commit-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn journal_line_count(directory: &TestDirectory) -> usize {
        std::fs::read_to_string(directory.path.join("journal.log"))
            .expect("read journal")
            .lines()
            .count()
    }

    #[test]
    fn checkpoints_are_created_shared_and_released() {
        let directory = TestDirectory::new("lifecycle");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            let name = create_checkpoint(
                &store,
                1_000_000,
                &[("creator".to_owned(), "froe-test".to_owned())],
            )
            .expect("create");

            let listed = list_checkpoints(&store).expect("list");
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].name, name);
            assert!(listed[0].created_milliseconds.is_some());
            assert!(listed[0].expires_milliseconds.is_some());
            store.close().expect("close");

            // The reader sees the checkpoint with a shared root snapshot.
            let repository = Repository::open(&directory.path).expect("reader");
            let checkpoints = repository.checkpoints().expect("checkpoints");
            assert_eq!(checkpoints.len(), 1);
            let (_, checkpoint) = &checkpoints[0];
            let snapshot = checkpoint
                .child_node("root")
                .expect("read")
                .expect("snapshot present");
            let live_root = repository.content_root().expect("content root");
            assert_eq!(
                snapshot.record_identifier(),
                live_root.record_identifier(),
                "the snapshot shares the live root's record"
            );
            let properties = checkpoint
                .child_node("properties")
                .expect("read")
                .expect("properties present");
            let creator = properties
                .property("creator")
                .expect("read")
                .expect("present");
            assert_eq!(
                creator.values,
                crate::content::node::PropertyValues::Single(
                    crate::content::property::PropertyValue::String("froe-test".to_owned())
                )
            );
        }
        {
            let store = WritableRepository::open(&directory.path).expect("reopen");
            let listed = list_checkpoints(&store).expect("list");
            assert_eq!(listed.len(), 1);
            assert!(
                release_checkpoint(&store, &listed[0].name).expect("release"),
                "the checkpoint existed"
            );
            assert!(list_checkpoints(&store).expect("list").is_empty());
            assert!(
                !release_checkpoint(&store, "missing").expect("release"),
                "absent checkpoints report false"
            );
            store.close().expect("close");
        }
    }

    #[test]
    fn expired_checkpoints_are_purged_on_create() {
        let directory = TestDirectory::new("expiry");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        // A one-millisecond lifetime expires immediately.
        let expired = create_checkpoint(&store, 1, &[]).expect("create short lived");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let fresh = create_checkpoint(&store, 1_000_000, &[]).expect("create fresh");

        let names: Vec<String> = list_checkpoints(&store)
            .expect("list")
            .into_iter()
            .map(|checkpoint| checkpoint.name)
            .collect();
        assert!(
            !names.contains(&expired),
            "the expired checkpoint is purged"
        );
        assert!(names.contains(&fresh));
        store.close().expect("close");
    }

    #[test]
    fn remove_all_reports_the_count() {
        let directory = TestDirectory::new("remove-all");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        create_checkpoint(&store, 1_000_000, &[]).expect("first");
        create_checkpoint(&store, 1_000_000, &[]).expect("second");
        let lines_before = journal_line_count(&directory);
        assert_eq!(remove_all_checkpoints(&store).expect("remove"), 2);
        assert_eq!(
            journal_line_count(&directory),
            lines_before + 1,
            "all checkpoint removals share one persisted head"
        );
        assert!(list_checkpoints(&store).expect("list").is_empty());
        store.close().expect("close");
    }

    #[test]
    fn a_checkpoint_set_is_removed_with_one_head_and_noop_sets_write_nothing() {
        let directory = TestDirectory::new("remove-set");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        let first = create_checkpoint(&store, 1_000_000, &[]).expect("first");
        let retained = create_checkpoint(&store, 1_000_000, &[]).expect("retained");
        let third = create_checkpoint(&store, 1_000_000, &[]).expect("third");

        let head_before = store.head();
        let lines_before = journal_line_count(&directory);
        let requested = vec![first.clone(), "missing".to_owned(), third.clone(), first];
        assert_eq!(remove_checkpoints(&store, &requested).expect("remove"), 2);
        assert_ne!(
            store.head(),
            head_before,
            "a non-empty removal advances head"
        );
        assert_eq!(
            journal_line_count(&directory),
            lines_before + 1,
            "duplicates and multiple names still produce one journal revision"
        );
        assert_eq!(
            list_checkpoints(&store)
                .expect("list")
                .into_iter()
                .map(|checkpoint| checkpoint.name)
                .collect::<Vec<_>>(),
            vec![retained.clone()]
        );

        let head_after = store.head();
        let lines_after = journal_line_count(&directory);
        assert_eq!(
            remove_checkpoints(&store, &["missing".to_owned()]).expect("missing no-op"),
            0
        );
        assert_eq!(remove_checkpoints(&store, &[]).expect("empty no-op"), 0);
        assert_eq!(store.head(), head_after, "no-op removals preserve head");
        assert_eq!(
            journal_line_count(&directory),
            lines_after,
            "no-op removals do not append journal revisions"
        );
        store.close().expect("close");

        let repository = Repository::open(&directory.path).expect("reopen read-only");
        assert_eq!(
            repository
                .checkpoints()
                .expect("persisted checkpoints")
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec![retained],
            "the finalized archive retains exactly the unremoved checkpoint"
        );
    }

    #[test]
    fn unreferenced_checkpoint_removal_is_batched() {
        let directory = TestDirectory::new("remove-unreferenced");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        create_checkpoint(&store, 1_000_000, &[]).expect("first");
        create_checkpoint(&store, 1_000_000, &[]).expect("second");
        create_checkpoint(&store, 1_000_000, &[]).expect("third");
        let lines_before = journal_line_count(&directory);

        // With no /:async state, the established API regards every
        // checkpoint as unreferenced; the removals must still be atomic.
        assert_eq!(
            remove_unreferenced_checkpoints(&store).expect("remove unreferenced"),
            3
        );
        assert_eq!(
            journal_line_count(&directory),
            lines_before + 1,
            "the unreferenced set shares one persisted head"
        );
        assert!(list_checkpoints(&store).expect("list").is_empty());
        store.close().expect("close");

        let repository = Repository::open(&directory.path).expect("reopen read-only");
        assert!(
            repository
                .checkpoints()
                .expect("persisted checkpoints")
                .is_empty(),
            "the finalized archive persists the batched removal"
        );
    }

    #[test]
    fn unreferenced_checkpoint_removal_preserves_async_references() {
        let directory = TestDirectory::new("remove-unreferenced-referenced");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        let retained = create_checkpoint(&store, 1_000_000, &[]).expect("retained");
        create_checkpoint(&store, 1_000_000, &[]).expect("unreferenced one");
        create_checkpoint(&store, 1_000_000, &[]).expect("unreferenced two");

        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let checkpoint_value = writer.write_string(&retained).expect("checkpoint value");
        let async_state = writer
            .write_node(
                None,
                &[],
                &crate::writer::record_writer::ChildNodesToWrite::Zero,
                &[crate::writer::record_writer::PropertyToWrite {
                    name: "async".to_owned(),
                    property_type: crate::content::property::PropertyType::String,
                    values: crate::writer::record_writer::PropertyValuesToWrite::Single(
                        checkpoint_value,
                    ),
                }],
            )
            .expect("async state");
        let content_root = writer
            .write_node(
                None,
                &[],
                &crate::writer::record_writer::ChildNodesToWrite::One {
                    name: ":async".to_owned(),
                    node: async_state,
                },
                &[],
            )
            .expect("content root");
        writer.finish().expect("finish content root");
        replace_content_root(&store, content_root).expect("install content root");

        let lines_before = journal_line_count(&directory);
        assert_eq!(
            remove_unreferenced_checkpoints(&store).expect("remove unreferenced"),
            2
        );
        assert_eq!(journal_line_count(&directory), lines_before + 1);
        assert_eq!(
            list_checkpoints(&store)
                .expect("list")
                .into_iter()
                .map(|checkpoint| checkpoint.name)
                .collect::<Vec<_>>(),
            vec![retained.clone()]
        );
        store.close().expect("close");

        let repository = Repository::open(&directory.path).expect("reopen read-only");
        assert_eq!(
            repository
                .checkpoints()
                .expect("persisted checkpoints")
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec![retained],
            "the finalized archive preserves the referenced checkpoint"
        );
    }
}
