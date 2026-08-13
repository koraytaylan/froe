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
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
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
    deep_copy_tree_with_progress(source, writer, source_root, &mut DiscardedProgress)
}

/// Deep-copies exactly like [`deep_copy_tree`], reporting the number of
/// nodes rewritten so far to `observer`.
pub fn deep_copy_tree_with_progress<Sink: SegmentSink>(
    source: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    source_root: RecordIdentifier,
    observer: &mut dyn ProgressObserver,
) -> Result<(RecordIdentifier, u64)> {
    let mut copier = Compactor {
        source,
        writer,
        rewritten_nodes: HashMap::new(),
        compacted_nodes: 0,
        reported_nodes: 0,
        observer,
    };
    let root = copier.compact_node(source_root, 0)?;
    // The stride suppressed the last partial batch; report the exact
    // total so the copy does not end short of what it wrote.
    copier.observer.step_advanced(copier.compacted_nodes);
    Ok((root, copier.compacted_nodes))
}

/// How many nodes a deep copy rewrites between progress reports.
const COPIED_NODE_REPORT_STRIDE: u64 = 512;

/// A content tree is never this deep — JCR paths in even the largest AEM
/// repositories stay under a few hundred levels. A greater depth means the
/// source node records form a cycle in a corrupt store. The bound is set
/// well below the point where recursion would overflow the stack.
const MAXIMUM_COMPACTION_DEPTH: usize = 4000;

/// Deep-copies nodes into a fresh generation, sharing rewritten records
/// through a source-record cache.
struct Compactor<'writer, Sink: SegmentSink> {
    source: &'writer dyn SegmentProvider,
    writer: &'writer mut RecordWriter<Sink>,
    rewritten_nodes: HashMap<RecordIdentifier, RecordIdentifier>,
    compacted_nodes: u64,
    /// The count at the last progress report, so the observer is called
    /// once per stride rather than once per node.
    reported_nodes: u64,
    observer: &'writer mut dyn ProgressObserver,
}

impl<Sink: SegmentSink> Compactor<'_, Sink> {
    fn compact_node(
        &mut self,
        source_node: RecordIdentifier,
        depth: usize,
    ) -> Result<RecordIdentifier> {
        if let Some(&rewritten) = self.rewritten_nodes.get(&source_node) {
            return Ok(rewritten);
        }
        // The cache is only populated once a node is fully written, so a
        // cycle in a corrupt source would otherwise recurse forever before
        // any entry exists; the depth bound stops it.
        if depth > MAXIMUM_COMPACTION_DEPTH {
            return Err(Error::InvalidFormat {
                details: format!(
                    "node tree exceeds depth {MAXIMUM_COMPACTION_DEPTH}; \
                     the source records probably form a cycle"
                ),
            });
        }
        let node = NodeState::new(self.source, source_node);
        let template = node.template()?;
        let stable_identifier = node.stable_identifier_bytes()?;

        // Rewrite children first so the node record can reference them.
        let mut child_entries = Vec::new();
        for (name, child) in node.child_node_entries()? {
            child_entries.push((
                name,
                self.compact_node(child.record_identifier(), depth + 1)?,
            ));
        }
        let children = match child_entries.len() {
            0 => ChildNodesToWrite::Zero,
            1 => {
                let (name, node) = child_entries.into_iter().next().expect("one child");
                ChildNodesToWrite::One { name, node }
            }
            _ => ChildNodesToWrite::Many(child_entries),
        };

        // Rewrite the *stored* property values into fresh records — never
        // the synthesized jcr:primaryType/jcr:mixinTypes, and never a
        // name filter (which would drop an ordinary property of one of
        // those names). The head types come from the template.
        let mut properties = Vec::new();
        for property in node.stored_properties()? {
            properties.push(self.rewrite_property(&property)?);
        }
        sort_properties_for_template(&mut properties);

        let rewritten = self.writer.write_node_with_stable_identifier(
            template.primary_type.as_deref(),
            &template.mixin_types,
            &children,
            &properties,
            Some(stable_identifier),
        )?;
        self.rewritten_nodes.insert(source_node, rewritten);
        self.compacted_nodes += 1;
        if self.compacted_nodes - self.reported_nodes >= COPIED_NODE_REPORT_STRIDE {
            self.reported_nodes = self.compacted_nodes;
            self.observer.step_advanced(self.compacted_nodes);
        }
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
                    // Copy the binary streaming, block by block, so a
                    // multi-gigabyte inline binary never has to fit in
                    // memory at once.
                    self.writer
                        .copy_binary_value(self.source, *record_identifier)
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
    compact_with_progress(store, kind, &mut DiscardedProgress)
}

/// Compacts exactly like [`compact`], reporting the deep copy, the
/// reclamation sweep, and the journal rewrite to `observer`.
pub fn compact_with_progress(
    store: &mut WritableRepository,
    kind: CompactionKind,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionOutcome> {
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

    // Refuse damaged base payloads or incomplete graph/BRF trailers before
    // allocating the compacted copy. Reclamation certifies them again at its
    // mutation boundary, but doing the first pass here prevents every retry
    // against a pre-existing defect from durably appending another full copy.
    store.preflight_reclaim_sources_with_progress(observer)?;

    let mut writer = store.record_writer_with_identifier(target_generation, "c");
    let (new_head, compacted_nodes) = crate::progress::observe(
        observer,
        &Step::new("copying nodes into a fresh generation", WorkUnit::Nodes),
        |observer| deep_copy_tree_with_progress(store, &mut writer, head, observer),
    )?;
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
    crate::progress::observe(
        observer,
        &Step::new("reclaiming old generations", WorkUnit::Archives),
        |_observer| store.reclaim_old_generations(target_generation, kind == CompactionKind::Full),
    )?;
    rewrite_journal_to_head(store, new_head)?;

    let size_after = store.archive_size_on_disk()?;
    // Append the gc.log line Oak's cleanup writes, so a later Oak tail
    // compaction against this store finds its previous-compaction record.
    append_gc_log(
        store,
        size_after,
        size_before.saturating_sub(size_after),
        target_generation,
        compacted_nodes,
        new_head,
    )?;

    Ok(CompactionOutcome {
        size_before,
        size_after,
        compacted_nodes,
    })
}

/// Appends one line to `gc.log`:
/// `repoSize,reclaimedSize,timestamp,generation,fullGeneration,nodes,root`.
fn append_gc_log(
    store: &WritableRepository,
    repository_size: u64,
    reclaimed_size: u64,
    generation: GarbageCollectionGeneration,
    compacted_nodes: u64,
    root: RecordIdentifier,
) -> Result<()> {
    use std::io::Write;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let line = format!(
        "{repository_size},{reclaimed_size},{timestamp},{},{},{compacted_nodes},{}:{}\n",
        generation.generation, generation.full_generation, root.segment, root.record_number as i32,
    );
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(store.directory().join("gc.log"))?;
    file.write_all(line.as_bytes())?;
    file.sync_data()?;
    Ok(())
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
    // fsync the directory so the rename (and the deletion of the old
    // archives during the preceding reclaim) is durable before the caller
    // considers compaction complete.
    fsync_directory(store.directory());
    store.reset_persisted_head(head)?;
    Ok(())
}

/// Forces a directory's metadata to disk, so renames and deletions within
/// it survive a power failure. A no-op on platforms where a directory
/// cannot be opened as a file.
pub(crate) fn fsync_directory(directory: &std::path::Path) {
    if let Ok(handle) = std::fs::File::open(directory) {
        // Directories cannot be data-synced on every filesystem; ignore an
        // error from sync while still opening the handle where possible.
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::{CompactionKind, compact};
    use crate::content::node::PropertyValues;
    use crate::content::property::PropertyValue;
    use crate::content::provider::SegmentProvider;
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

    fn corrupt_graph_checksum(path: &std::path::Path) {
        let mut bytes = std::fs::read(path).expect("read archive");
        let mut offset = 0usize;
        while offset + 512 <= bytes.len() {
            let header = &bytes[offset..offset + 512];
            if header.iter().all(|byte| *byte == 0) {
                break;
            }
            let name_end = header[..100]
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(100);
            let name = std::str::from_utf8(&header[..name_end]).expect("UTF-8 TAR entry name");
            let size_text = std::str::from_utf8(&header[124..136])
                .expect("ASCII TAR size")
                .trim_matches(['\0', ' ']);
            let size = usize::from_str_radix(size_text, 8).expect("octal TAR size");
            if std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("gph"))
            {
                let payload_end = offset + 512 + size;
                assert!(size >= 16, "graph payload includes its footer");
                bytes[payload_end - 16] ^= 0x01;
                std::fs::write(path, bytes).expect("corrupt graph checksum");
                return;
            }
            offset += 512 + size.div_ceil(512) * 512;
        }
        panic!("graph trailer not found in {}", path.display());
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
        // A gc.log line was appended.
        let gc_log = std::fs::read_to_string(directory.path.join("gc.log")).expect("gc.log");
        assert_eq!(gc_log.lines().count(), 1);
        assert_eq!(gc_log.split(',').count(), 7, "seven gc.log fields");
    }

    #[test]
    fn compaction_preserves_stable_identifiers() {
        let directory = TestDirectory::new("stable-ids");
        build_populated_store(&directory);

        // Record the content node's stable identifier before compaction.
        let before = {
            let repository = Repository::open(&directory.path).expect("reader");
            repository
                .node_at_path("/content")
                .expect("resolve")
                .expect("present")
                .stable_identifier()
                .expect("stable id")
        };
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            compact(&mut store, CompactionKind::Full).expect("compact");
            store.close().expect("close");
        }
        let after = {
            let repository = Repository::open(&directory.path).expect("reader");
            repository
                .node_at_path("/content")
                .expect("resolve")
                .expect("present")
                .stable_identifier()
                .expect("stable id")
        };
        assert_eq!(
            before, after,
            "the stable identifier survives compaction so Oak's fast path keeps matching"
        );
    }

    #[test]
    fn compaction_preserves_infinite_doubles_and_type_named_properties() {
        let directory = TestDirectory::new("edge-values");
        {
            let store = WritableRepository::open(&directory.path).expect("open");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            // A DOUBLE property holding positive infinity, and a STRING
            // property literally named jcr:primaryType (a non-name-typed
            // reserved name, stored as an ordinary property by Oak).
            let infinity_value = writer.write_string("Infinity").expect("value");
            let odd_name_value = writer.write_string("literal").expect("value");
            // No synthesized (Name-typed) primary type, so the String
            // property literally named jcr:primaryType is the only carrier
            // of that name — exactly the shape Oak stores as an ordinary
            // property and that a name filter would drop.
            let content = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[
                        PropertyToWrite {
                            name: "ratio".to_owned(),
                            property_type: crate::content::property::PropertyType::Double,
                            values: PropertyValuesToWrite::Single(infinity_value),
                        },
                        PropertyToWrite {
                            name: "jcr:primaryType".to_owned(),
                            property_type: crate::content::property::PropertyType::String,
                            values: PropertyValuesToWrite::Single(odd_name_value),
                        },
                    ],
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
            store.close().expect("close");
        }
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            compact(&mut store, CompactionKind::Full).expect("compact");
            store.close().expect("close");
        }
        let repository = Repository::open(&directory.path).expect("reader");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        // The infinite double survives with a value AEM can parse.
        let ratio = content.property("ratio").expect("read").expect("present");
        assert_eq!(
            ratio.values,
            PropertyValues::Single(PropertyValue::Double(f64::INFINITY))
        );
        // The oddly-typed jcr:primaryType survives as a String property,
        // not silently dropped.
        let odd = content
            .property("jcr:primaryType")
            .expect("read")
            .expect("present");
        assert_eq!(
            odd.property_type,
            crate::content::property::PropertyType::String
        );
        assert_eq!(
            odd.values,
            PropertyValues::Single(PropertyValue::String("literal".to_owned()))
        );
    }

    #[test]
    fn compaction_streams_long_binaries_through_bulk_segments() {
        let directory = TestDirectory::new("long-binary");
        // A binary spanning multiple 4 KiB blocks plus a full 256 KiB bulk
        // run, so the streaming copy path (not the inline materialization)
        // is exercised.
        let content: Vec<u8> = (0..300 * 1024).map(|index| (index % 251) as u8).collect();
        {
            let store = WritableRepository::open(&directory.path).expect("open");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let binary_value = writer.write_binary_content(&content).expect("binary");
            let content_node = writer
                .write_node(
                    Some("nt:file"),
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "data".to_owned(),
                        property_type: crate::content::property::PropertyType::Binary,
                        values: PropertyValuesToWrite::Single(binary_value),
                    }],
                )
                .expect("content");
            let root = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "content".to_owned(),
                        node: content_node,
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
            store.close().expect("close");
        }
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            compact(&mut store, CompactionKind::Full).expect("compact");
            store.close().expect("close");
        }
        // The binary content survives compaction byte for byte.
        let repository = Repository::open(&directory.path).expect("reader");
        let content_node = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        let data = content_node
            .property("data")
            .expect("read")
            .expect("present");
        let record = match &data.values {
            PropertyValues::Single(PropertyValue::Binary(
                crate::content::value::BinaryValue::Inline {
                    record_identifier, ..
                },
            )) => *record_identifier,
            other => panic!("expected an inline binary, got {other:?}"),
        };
        let read_back =
            crate::content::value::read_binary_content(&repository, record).expect("content");
        assert_eq!(
            read_back, content,
            "the long binary round-trips through compaction"
        );
    }

    #[test]
    fn committing_after_compaction_in_one_session_persists_the_journal() {
        let directory = TestDirectory::new("commit-after-compact");
        build_populated_store(&directory);
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            compact(&mut store, CompactionKind::Full).expect("compact");
            // A checkpoint create moves the head; its journal line must
            // reach the live journal, not the orphaned pre-rewrite inode.
            create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
            store.close().expect("close");
        }
        // The reader resolves the post-compaction checkpoint head.
        let repository = Repository::open(&directory.path).expect("reader");
        assert_eq!(
            repository.checkpoints().expect("checkpoints").len(),
            2,
            "the checkpoint created after compaction is visible in the journal"
        );
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

    #[test]
    fn compaction_certifies_base_archives_before_writing_a_retry_copy() {
        let directory = TestDirectory::new("preflight-base-certificate");
        build_populated_store(&directory);
        let repository = Repository::open(&directory.path).expect("open healthy repository");
        let archive_name = repository.archives()[0].file_name().to_owned();
        drop(repository);
        corrupt_graph_checksum(&directory.path.join(&archive_name));

        let journal_before =
            std::fs::read(directory.path.join("journal.log")).expect("read journal before");
        let archives_before =
            crate::store::list_archive_file_names(&directory.path).expect("list archives before");
        let bytes_before: Vec<_> = archives_before
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    std::fs::read(directory.path.join(name)).expect("read archive before"),
                )
            })
            .collect();

        for attempt in 1..=2 {
            let mut store = WritableRepository::open(&directory.path)
                .expect("ordinary read path tolerates an invalid optional graph");
            let error = compact(&mut store, CompactionKind::Full)
                .expect_err("strict reclaim source preflight must refuse the graph");
            assert!(error.to_string().contains("segment graph"), "{error}");
            drop(store);
            assert_eq!(
                crate::store::list_archive_file_names(&directory.path)
                    .expect("list archives after refused attempt"),
                archives_before,
                "refused retry {attempt} must not allocate another compacted TAR"
            );
        }

        assert_eq!(
            crate::store::list_archive_file_names(&directory.path).expect("list archives after"),
            archives_before,
            "preflight refusal must not allocate a compacted TAR"
        );
        for (name, expected) in bytes_before {
            assert_eq!(
                std::fs::read(directory.path.join(name)).expect("read archive after"),
                expected
            );
        }
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("read journal after"),
            journal_before,
            "preflight refusal must not publish another head"
        );
    }

    /// Every data segment's referenced segments must resolve — the sweep
    /// must never delete a bulk segment a kept data segment points at.
    fn assert_no_dangling_segment_references(directory: &TestDirectory) {
        let repository = Repository::open(&directory.path).expect("reader opens");
        for segment_identifier in repository.segment_identifiers() {
            if segment_identifier.is_bulk_segment() {
                continue;
            }
            let view = repository
                .segment(segment_identifier)
                .expect("data segment readable");
            for referenced in &view.structure.referenced_segments {
                assert!(
                    repository.contains_segment(*referenced),
                    "kept data segment {segment_identifier} references missing segment \
                     {referenced}"
                );
            }
        }
    }

    #[test]
    fn tail_compaction_keeps_bulk_segments_referenced_by_retained_data_segments() {
        let directory = TestDirectory::new("tail-bulk-mark");
        build_populated_store(&directory);

        // A value long enough to force a full 256 KiB block run, stored
        // as a bulk segment referenced by the data segment holding the
        // value's block list.
        {
            let store = WritableRepository::open(&directory.path).expect("open");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let large = writer
                .write_string(&"bulk-backed-value ".repeat(20_000))
                .expect("large value");
            let content = writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "data".to_owned(),
                        property_type: crate::content::property::PropertyType::String,
                        values: PropertyValuesToWrite::Single(large),
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
            store.close().expect("close");
        }

        // Full compaction rewrites everything into compacted segments —
        // including fresh bulk segments at (0, 0, false), the triple the
        // format mandates for bulk.
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            compact(&mut store, CompactionKind::Full).expect("full compact");
            store.close().expect("close");
        }
        assert_no_dangling_segment_references(&directory);

        // Tail compaction *retains* the full-compacted data segments
        // (same full generation, compacted) — the mark phase must then
        // keep the generation-(0,0,false) bulk segments they reference,
        // which the generation predicate alone would reclaim.
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            compact(&mut store, CompactionKind::Tail).expect("tail compact");
            store.close().expect("close");
        }
        assert_no_dangling_segment_references(&directory);

        // The large value itself is still fully readable.
        let repository = Repository::open(&directory.path).expect("reader opens");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        let data = content.property("data").expect("read").expect("present");
        assert_eq!(
            data.values,
            PropertyValues::Single(PropertyValue::String("bulk-backed-value ".repeat(20_000)))
        );
    }
}
