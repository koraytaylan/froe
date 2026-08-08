//! Offline compaction: rewriting the repository into a fresh generation.
//!
//! Compaction deep-copies every record reachable from the current head —
//! the content root and every checkpoint — into new segments stamped with
//! an advanced garbage collection generation, then swaps the head to the
//! rewritten super-root and reclaims the now-unreferenced old generations.
//! A source-record-keyed cache preserves the sharing of the content
//! graph: a checkpoint whose `root` shares records with the live root
//! stays shared after compaction, and the walk terminates over the DAG.
//!
//! This is the *classic* deep-copy compaction — the checkpoint-aware and
//! parallel compactors in Oak are throughput optimizations that produce
//! an equivalent result. Full compaction advances both the generation
//! and the full generation; tail compaction advances only the
//! generation, keeping the full generation so a later full compaction
//! can still reclaim the tail. Offline compaction retains a single
//! generation, so every pre-compaction segment becomes reclaimable.
//!
//! After compaction the journal is rewritten to a single line naming the
//! compacted head — matching Oak's offline `compact` tool — so a
//! subsequent AEM start resolves the compacted state directly.

use std::collections::HashMap;
use std::io::Write;

use crate::content::node::{NodeState, PropertyState, PropertyValues};
use crate::content::property::{PropertyType, PropertyValue};
use crate::content::provider::SegmentProvider;
use crate::content::value::BinaryValue;
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;
use crate::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
    sort_properties_for_template,
};
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::WritableRepository;

/// The kind of compaction to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionKind {
    /// Advances both generation and full generation; reclaims everything.
    Full,
    /// Advances only the generation, keeping the full generation.
    Tail,
}

/// The outcome of a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionOutcome {
    /// Bytes occupied by archives before compaction.
    pub size_before: u64,
    /// Bytes occupied by archives after compaction and cleanup.
    pub size_after: u64,
    /// The number of nodes rewritten.
    pub compacted_nodes: u64,
}

/// Deep-copies a node tree from a source provider into a record writer,
/// rewriting every reachable record and sharing results through a
/// source-record cache so the content DAG's sharing is preserved and the
/// walk terminates. Returns the rewritten root and the number of nodes
/// copied. Used by compaction, backup, and restore.
pub fn deep_copy_tree<Sink: SegmentSink>(
    source: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    source_root: RecordIdentifier,
) -> Result<(RecordIdentifier, u64)> {
    let mut copier = Compactor {
        source,
        writer,
        rewritten_nodes: HashMap::new(),
        compacted_nodes: 0,
    };
    let root = copier.compact_node(source_root)?;
    Ok((root, copier.compacted_nodes))
}

/// Deep-copies nodes into a fresh generation, sharing rewritten records
/// through a source-record cache.
struct Compactor<'writer, Sink: SegmentSink> {
    source: &'writer dyn SegmentProvider,
    writer: &'writer mut RecordWriter<Sink>,
    rewritten_nodes: HashMap<RecordIdentifier, RecordIdentifier>,
    compacted_nodes: u64,
}

impl<Sink: SegmentSink> Compactor<'_, Sink> {
    fn compact_node(&mut self, source_node: RecordIdentifier) -> Result<RecordIdentifier> {
        if let Some(&rewritten) = self.rewritten_nodes.get(&source_node) {
            return Ok(rewritten);
        }
        let node = NodeState::new(self.source, source_node);
        let template = node.template()?;

        // Rewrite children first so the node record can reference them.
        let mut child_entries = Vec::new();
        for (name, child) in node.child_node_entries()? {
            child_entries.push((name, self.compact_node(child.record_identifier())?));
        }
        let children = match child_entries.len() {
            0 => ChildNodesToWrite::Zero,
            1 => {
                let (name, node) = child_entries.into_iter().next().expect("one child");
                ChildNodesToWrite::One { name, node }
            }
            _ => ChildNodesToWrite::Many(child_entries),
        };

        // Rewrite property values into fresh records.
        let mut properties = Vec::new();
        for property in node.properties()? {
            // jcr:primaryType and jcr:mixinTypes are synthesized from the
            // template head, not stored as properties.
            if property.name == "jcr:primaryType" || property.name == "jcr:mixinTypes" {
                continue;
            }
            properties.push(self.rewrite_property(&property)?);
        }
        sort_properties_for_template(&mut properties);

        let rewritten = self.writer.write_node(
            template.primary_type.as_deref(),
            &template.mixin_types,
            &children,
            &properties,
        )?;
        self.rewritten_nodes.insert(source_node, rewritten);
        self.compacted_nodes += 1;
        Ok(rewritten)
    }

    /// Rewrites one property's values into fresh value records.
    fn rewrite_property(&mut self, property: &PropertyState) -> Result<PropertyToWrite> {
        let values = match &property.values {
            PropertyValues::Single(value) => {
                PropertyValuesToWrite::Single(self.rewrite_value(property.property_type, value)?)
            }
            PropertyValues::Multiple(values) => {
                let mut rewritten = Vec::with_capacity(values.len());
                for value in values {
                    rewritten.push(self.rewrite_value(property.property_type, value)?);
                }
                PropertyValuesToWrite::Multiple(rewritten)
            }
        };
        Ok(PropertyToWrite {
            name: property.name.clone(),
            property_type: property.property_type,
            values,
        })
    }

    /// Writes a fresh value record for one decoded property value.
    fn rewrite_value(
        &mut self,
        property_type: PropertyType,
        value: &PropertyValue,
    ) -> Result<RecordIdentifier> {
        if property_type == PropertyType::Binary {
            return match value {
                PropertyValue::Binary(BinaryValue::External { blob_identifier }) => self
                    .writer
                    .write_external_binary_identifier(blob_identifier),
                PropertyValue::Binary(BinaryValue::Inline {
                    record_identifier, ..
                }) => {
                    let content = crate::content::value::read_binary_content(
                        self.source,
                        *record_identifier,
                    )?;
                    self.writer.write_binary_content(&content)
                }
                _ => Err(Error::InvalidFormat {
                    details: "binary property did not decode to a binary value".to_owned(),
                }),
            };
        }
        // Every non-binary value is stored as its string form.
        let text = value.as_text().ok_or_else(|| Error::InvalidFormat {
            details: format!("property value {value:?} has no string form"),
        })?;
        self.writer.write_string(&text)
    }
}

/// Compacts the repository in place: deep-copies the head into a fresh
/// generation, swaps the head, reclaims the old generations, and
/// rewrites the journal to a single line.
pub fn compact(store: &mut WritableRepository, kind: CompactionKind) -> Result<CompactionOutcome> {
    let size_before = store.archive_size_on_disk()?;

    let head = store.head();
    let base_generation = store
        .segment_generation(head.segment)
        .ok_or(Error::SegmentNotFound {
            segment_identifier: head.segment,
        })?;
    let target_generation = match kind {
        CompactionKind::Full => GarbageCollectionGeneration {
            generation: base_generation.generation.wrapping_add(1),
            full_generation: base_generation.full_generation.wrapping_add(1),
            is_compacted: true,
        },
        CompactionKind::Tail => GarbageCollectionGeneration {
            generation: base_generation.generation.wrapping_add(1),
            full_generation: base_generation.full_generation,
            is_compacted: true,
        },
    };

    let mut writer = store.record_writer_with_identifier(target_generation, "c");
    let (new_head, compacted_nodes) = deep_copy_tree(store, &mut writer, head)?;
    writer.finish()?;

    if !store.set_head(head, new_head) {
        return Err(Error::InvalidFormat {
            details: "the head moved during compaction".to_owned(),
        });
    }
    store.flush()?;

    // Reclaim generations older than the target. Full compaction keeps
    // only the new full generation; tail compaction keeps the shared full
    // generation, so it reclaims by generation alone.
    store.reclaim_old_generations(target_generation, kind == CompactionKind::Full)?;
    rewrite_journal_to_head(store, new_head)?;

    let size_after = store.archive_size_on_disk()?;
    Ok(CompactionOutcome {
        size_before,
        size_after,
        compacted_nodes,
    })
}

/// Rewrites `journal.log` to a single line naming `head`, matching the
/// offline compact tool. The store's own journal handle is bypassed so
/// the truncation is atomic from the reader's perspective (write to a
/// temporary file, then rename over the original).
fn rewrite_journal_to_head(store: &WritableRepository, head: RecordIdentifier) -> Result<()> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let line = format!(
        "{}:{} root {timestamp}\n",
        head.segment, head.record_number as i32
    );
    let journal_path = store.directory().join("journal.log");
    let temporary_path = store.directory().join("journal.log.compacting");
    {
        let mut file = std::fs::File::create(&temporary_path)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary_path, &journal_path)?;
    store.reset_persisted_head(head);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CompactionKind, compact};
    use crate::content::node::PropertyValues;
    use crate::content::property::PropertyValue;
    use crate::store::Repository;
    use crate::writer::commit::{create_checkpoint, list_checkpoints};
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-compaction-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Builds a store with a `/content` node carrying properties and two
    /// children, plus one checkpoint sharing the root.
    fn build_populated_store(directory: &TestDirectory) {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);

        let first_child = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("child");
        let second_child = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("child");
        let title = writer.write_string("Compaction Test").expect("value");
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("alpha".to_owned(), first_child),
                    ("beta".to_owned(), second_child),
                ]),
                &[PropertyToWrite {
                    name: "title".to_owned(),
                    property_type: crate::content::property::PropertyType::String,
                    values: PropertyValuesToWrite::Single(title),
                }],
            )
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content,
                },
                &[],
            )
            .expect("root");
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
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        create_checkpoint(
            &store,
            10_000_000,
            &[("purpose".to_owned(), "test".to_owned())],
        )
        .expect("checkpoint");
        store.close().expect("close");
    }

    fn assert_content_intact(directory: &TestDirectory) {
        let repository = Repository::open(&directory.path).expect("reader opens");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        assert_eq!(content.child_node_count().expect("count"), 2);
        assert!(content.child_node("alpha").expect("read").is_some());
        assert!(content.child_node("beta").expect("read").is_some());
        let title = content.property("title").expect("read").expect("present");
        assert_eq!(
            title.values,
            PropertyValues::Single(PropertyValue::String("Compaction Test".to_owned()))
        );
        let checkpoints = repository.checkpoints().expect("checkpoints");
        assert_eq!(checkpoints.len(), 1, "the checkpoint survives compaction");
        let (_, checkpoint) = &checkpoints[0];
        let snapshot = checkpoint
            .child_node("root")
            .expect("read")
            .expect("snapshot");
        assert!(snapshot.child_node("content").expect("read").is_some());
    }

    #[test]
    fn full_compaction_preserves_content_and_checkpoints() {
        let directory = TestDirectory::new("full");
        build_populated_store(&directory);

        let outcome = {
            let mut store = WritableRepository::open(&directory.path).expect("open for compaction");
            let before_generation = store
                .segment_generation(store.head().segment)
                .expect("generation");
            let outcome = compact(&mut store, CompactionKind::Full).expect("compact");
            let after_generation = store
                .segment_generation(store.head().segment)
                .expect("generation");
            assert_eq!(
                after_generation.generation,
                before_generation.generation + 1
            );
            assert_eq!(
                after_generation.full_generation,
                before_generation.full_generation + 1
            );
            assert!(after_generation.is_compacted);
            store.close().expect("close");
            outcome
        };
        assert!(outcome.compacted_nodes > 0);

        assert_content_intact(&directory);

        // The journal is a single line and the reader opens cleanly.
        let journal = std::fs::read_to_string(directory.path.join("journal.log")).expect("journal");
        assert_eq!(journal.lines().count(), 1, "journal rewritten to one line");
    }

    #[test]
    fn tail_compaction_keeps_the_full_generation() {
        let directory = TestDirectory::new("tail");
        build_populated_store(&directory);
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            let before = store
                .segment_generation(store.head().segment)
                .expect("generation");
            compact(&mut store, CompactionKind::Tail).expect("compact");
            let after = store
                .segment_generation(store.head().segment)
                .expect("generation");
            assert_eq!(after.generation, before.generation + 1);
            assert_eq!(
                after.full_generation, before.full_generation,
                "tail compaction keeps the full generation"
            );
            store.close().expect("close");
        }
        assert_content_intact(&directory);
    }

    #[test]
    fn compaction_reclaims_disk_space_from_garbage() {
        let directory = TestDirectory::new("reclaim");
        // Write many revisions that leave garbage behind.
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            for revision in 0..30 {
                let generation = store.writing_generation().expect("generation");
                let mut writer = store.record_writer(generation);
                let value = writer
                    .write_string(&format!("revision-{revision}").repeat(2000))
                    .expect("value");
                let content = writer
                    .write_node(
                        Some("nt:unstructured"),
                        &[],
                        &ChildNodesToWrite::Zero,
                        &[PropertyToWrite {
                            name: "data".to_owned(),
                            property_type: crate::content::property::PropertyType::String,
                            values: PropertyValuesToWrite::Single(value),
                        }],
                    )
                    .expect("content");
                let root = writer
                    .write_node(
                        None,
                        &[],
                        &ChildNodesToWrite::One {
                            name: "content".to_owned(),
                            node: content,
                        },
                        &[],
                    )
                    .expect("root");
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
                    .expect("super root");
                writer.finish().expect("finish");
                let previous = store.head();
                assert!(store.set_head(previous, head));
                store.flush().expect("flush");
            }
            store.close().expect("close");
        }

        let mut store = WritableRepository::open(&directory.path).expect("open");
        let outcome = compact(&mut store, CompactionKind::Full).expect("compact");
        store.close().expect("close");
        assert!(
            outcome.size_after < outcome.size_before,
            "compaction reclaims garbage: {} -> {}",
            outcome.size_before,
            outcome.size_after
        );

        // Only the newest content survives; the reader opens cleanly.
        let repository = Repository::open(&directory.path).expect("reader");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        let data = content.property("data").expect("read").expect("present");
        assert_eq!(
            data.values,
            PropertyValues::Single(PropertyValue::String("revision-29".repeat(2000)))
        );
    }

    #[test]
    fn compacted_stores_survive_a_second_compaction() {
        let directory = TestDirectory::new("twice");
        build_populated_store(&directory);
        for _ in 0..2 {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            compact(&mut store, CompactionKind::Full).expect("compact");
            store.close().expect("close");
            assert_content_intact(&directory);
        }
        let store = WritableRepository::open(&directory.path).expect("open");
        assert_eq!(list_checkpoints(&store).expect("list").len(), 1);
        store.close().expect("close");
    }
}
