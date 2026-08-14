//! Offline compaction: rewriting the repository into a fresh generation.
//!
//! Compaction deep-copies every record reachable from the current head —
//! the content root and every checkpoint — into new segments stamped with
//! an advanced garbage collection generation, then swaps the head to the
//! rewritten super-root and reclaims the now-unreferenced old generations.
//! An exact source-record-keyed memo preserves the sharing of the content
//! graph: a checkpoint whose `root` shares records with the live root stays
//! shared after compaction, and each distinct node is copied exactly once,
//! so the compacted output never exceeds the source through duplication.
//! The walk terminates on a corrupt self-referential graph by refusing the
//! record that closes the cycle.
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

use std::io::Write;

use crate::content::node::{NodeState, PropertyState, PropertyValues};
use crate::content::property::{PropertyType, PropertyValue};
use crate::content::provider::SegmentProvider;
use crate::content::value::BinaryValue;
use crate::error::{Error, Result};
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::identifier::SegmentIdentifier;
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
/// rewriting every reachable record exactly once, so the content DAG's
/// sharing is preserved exactly: a subtree the live root and a checkpoint
/// both reference is copied once and referenced twice. Returns the rewritten
/// root and the number of nodes copied, which equals the number of distinct
/// node records reachable from `source_root`. Used by compaction, backup,
/// and restore.
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
        segments: SegmentInterner::new(),
        rewritten_nodes: RewrittenNodes::new(),
        nodes_on_path: std::collections::HashSet::new(),
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

/// How deep this walk can descend within the stack it has. It is a stack
/// bound and nothing else: a cycle in a corrupt store is refused exactly,
/// by `nodes_on_path`, at the record that closes it. The value matches
/// [`crate::tooling::MAXIMUM_CHECK_DEPTH`], which the post-write health
/// traversal applies to the same tree — exceeding it here refuses before
/// any work, where exceeding it there refuses after the head is durable.
const MAXIMUM_COMPACTION_DEPTH: usize = 4000;

/// Maps each segment identifier met during one compaction to a small index,
/// so the memo can hold four bytes where a `SegmentIdentifier` holds sixteen.
///
/// The cost is per *segment*, not per node — a 25 GB store names on the order
/// of 250k of them — which is why this is affordable where storing the UUID
/// in every entry is not. Index 0 is never issued, so a packed key of zero is
/// unambiguously an empty slot.
struct SegmentInterner {
    indices: std::collections::HashMap<SegmentIdentifier, u32>,
    identifiers: Vec<SegmentIdentifier>,
}

impl SegmentInterner {
    fn new() -> Self {
        Self {
            indices: std::collections::HashMap::new(),
            // Index 0 is the never-issued sentinel; this placeholder keeps
            // `identifiers[index]` addressable without an offset everywhere.
            identifiers: vec![SegmentIdentifier {
                most_significant_bits: 0,
                least_significant_bits: 0,
            }],
        }
    }

    fn index_of(&mut self, segment: SegmentIdentifier) -> u32 {
        if let Some(index) = self.indices.get(&segment) {
            return *index;
        }
        let index = u32::try_from(self.identifiers.len()).expect("segments per compaction fit u32");
        self.identifiers.push(segment);
        self.indices.insert(segment, index);
        index
    }

    fn identifier(&self, index: u32) -> SegmentIdentifier {
        self.identifiers[index as usize]
    }

    /// Packs an interned record into the eight bytes the memo stores.
    fn pack(&mut self, record: RecordIdentifier) -> u64 {
        u64::from(self.index_of(record.segment)) << 32 | u64::from(record.record_number)
    }

    fn unpack(&self, packed: u64) -> RecordIdentifier {
        RecordIdentifier {
            segment: self.identifier((packed >> 32) as u32),
            record_number: packed as u32,
        }
    }
}

/// Source node to its rewritten copy, exactly and without eviction.
///
/// Copying each distinct node once is an invariant, not an optimization: a
/// miss does not cost one extra copy, it re-walks the whole subtree, and
/// misses nest. So this is an open-addressed table over two `Vec<u64>` —
/// no per-entry overhead, no eviction queue, and sixteen bytes a slot
/// against the ~110 a `HashMap<RecordIdentifier, RecordIdentifier>` measures.
/// A packed key of zero marks an empty slot, which [`SegmentInterner`]
/// guarantees no real record can collide with.
struct RewrittenNodes {
    keys: Vec<u64>,
    values: Vec<u64>,
    len: usize,
}

/// Slots in a fresh table. Grown geometrically, so this only sets the floor.
const INITIAL_MEMO_SLOTS: usize = 1024;

impl RewrittenNodes {
    fn new() -> Self {
        Self {
            keys: vec![0; INITIAL_MEMO_SLOTS],
            values: vec![0; INITIAL_MEMO_SLOTS],
            len: 0,
        }
    }

    /// Fibonacci hashing over the packed key, which is dense in the low bits
    /// (record numbers count up) and in the high bits (segment indices count
    /// up), so the multiply is what spreads both across the probe sequence.
    fn slot_of(&self, key: u64) -> usize {
        let mixed = key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> 32) as usize & (self.keys.len() - 1)
    }

    fn get(&self, key: u64) -> Option<u64> {
        let mut slot = self.slot_of(key);
        loop {
            match self.keys[slot] {
                0 => return None,
                found if found == key => return Some(self.values[slot]),
                _ => slot = (slot + 1) & (self.keys.len() - 1),
            }
        }
    }

    fn insert(&mut self, key: u64, value: u64) {
        // Grow at ~70% load, before probe sequences get long.
        if (self.len + 1) * 10 >= self.keys.len() * 7 {
            self.grow();
        }
        self.insert_without_growing(key, value);
        self.len += 1;
    }

    fn insert_without_growing(&mut self, key: u64, value: u64) {
        let mut slot = self.slot_of(key);
        while self.keys[slot] != 0 {
            slot = (slot + 1) & (self.keys.len() - 1);
        }
        self.keys[slot] = key;
        self.values[slot] = value;
    }

    fn grow(&mut self) {
        let occupied: Vec<(u64, u64)> = self
            .keys
            .iter()
            .zip(&self.values)
            .filter(|(key, _)| **key != 0)
            .map(|(key, value)| (*key, *value))
            .collect();
        self.keys = vec![0; self.keys.len() * 2];
        self.values = vec![0; self.values.len() * 2];
        for (key, value) in occupied {
            self.insert_without_growing(key, value);
        }
    }
}

/// Deep-copies nodes into a fresh generation, sharing rewritten records
/// through an exact source-record memo.
struct Compactor<'writer, Sink: SegmentSink> {
    source: &'writer dyn SegmentProvider,
    writer: &'writer mut RecordWriter<Sink>,
    /// Interns the segments both the memo and the path set name.
    segments: SegmentInterner,
    /// Source record to its rewritten copy — exact, so each distinct node is
    /// copied once and `compacted_nodes` equals the distinct reachable count.
    rewritten_nodes: RewrittenNodes,
    /// The records currently being expanded — the ancestor path, packed the
    /// same way the memo packs. Exact and unbounded: it carries termination,
    /// so it is never budgeted. One entry per live level, not per node.
    nodes_on_path: std::collections::HashSet<u64>,
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
        let packed_source = self.segments.pack(source_node);
        if let Some(rewritten) = self.rewritten_nodes.get(packed_source) {
            return Ok(self.segments.unpack(rewritten));
        }
        // A node reachable from itself is corruption — valid records only
        // reference already-written records — and is refused exactly, at the
        // record that closes the cycle. The memo cannot mask it: a memo hit
        // returns above, so a memoized node is never on the path.
        if !self.nodes_on_path.insert(packed_source) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "node record {source_node} is contained in its own subtree; \
                     the source records form a cycle"
                ),
            });
        }
        // Depth is a stack bound and nothing else: cycles are refused above.
        if depth > MAXIMUM_COMPACTION_DEPTH {
            return Err(Error::InvalidFormat {
                details: format!(
                    "node tree exceeds depth {MAXIMUM_COMPACTION_DEPTH}, \
                     which this walk cannot descend within its stack"
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
        self.nodes_on_path.remove(&packed_source);
        let packed_rewritten = self.segments.pack(rewritten);
        self.rewritten_nodes.insert(packed_source, packed_rewritten);
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
///
/// The memo maps each source node to its rewritten copy and is exact, so a
/// subtree the live root and a checkpoint both reference is copied once and
/// `compacted_nodes` equals the number of distinct node records reachable
/// from the head.
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
    use super::{CompactionKind, compact, deep_copy_tree_with_progress};
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
    fn a_deep_copy_copies_each_distinct_node_exactly_once() {
        let directory = TestDirectory::new("memo-exact");
        build_populated_store(&directory);

        // The memo is exact, so the copy visits the shared subtree behind the
        // checkpoint and the live root once. `copied` is therefore the
        // distinct reachable node count, not merely at least it.
        let (copied, distinct) = {
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (root, copied) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                head,
                &mut crate::progress::DiscardedProgress,
            )
            .expect("deep copy");
            let distinct = distinct_reachable_nodes(&store, head);
            writer.finish().expect("finish");
            assert!(store.set_head(head, root));
            store.close().expect("close");
            (copied, distinct)
        };
        assert_eq!(
            copied as usize, distinct,
            "every distinct node is copied exactly once"
        );

        assert_content_intact(&directory);
    }

    /// Builds `levels` diamonds under the super-root: every level references
    /// the *same* next-level node twice, so distinct nodes grow linearly
    /// while distinct root-to-leaf paths grow as 2^levels.
    ///
    /// `ballast` fresh nodes sit between the two references. They are what
    /// decides whether the memo survives from the first reference to the
    /// second: with `ballast` below the budget the second lookup hits, and
    /// with it above, every level re-copies its whole subtree.
    fn build_diamond_chain(directory: &TestDirectory, levels: usize, ballast: usize) {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);

        let mut node = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("leaf");
        for level in 0..levels {
            let mut children = vec![("a_left".to_owned(), node)];
            for index in 0..ballast {
                let value = writer
                    .write_string(&format!("{level}-{index}"))
                    .expect("filler value");
                let filler = writer
                    .write_node(
                        Some("nt:unstructured"),
                        &[],
                        &ChildNodesToWrite::Zero,
                        &[PropertyToWrite {
                            name: "n".to_owned(),
                            property_type: crate::content::property::PropertyType::String,
                            values: PropertyValuesToWrite::Single(value),
                        }],
                    )
                    .expect("filler");
                children.push((format!("b_fill{index:04}"), filler));
            }
            children.push(("c_right".to_owned(), node));
            node = writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &ChildNodesToWrite::Many(children),
                    &[],
                )
                .expect("diamond");
        }
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        store.close().expect("close");
    }

    /// The exact number of distinct node records reachable from `root` — the
    /// figure `compacted_nodes` is supposed to equal.
    fn distinct_reachable_nodes(
        provider: &dyn SegmentProvider,
        root: crate::segment::record::RecordIdentifier,
    ) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut pending = vec![root];
        while let Some(record) = pending.pop() {
            if !seen.insert(record) {
                continue;
            }
            let node = crate::content::node::NodeState::new(provider, record);
            for (_, child) in node.child_node_entries().expect("children") {
                pending.push(child.record_identifier());
            }
        }
        seen.len()
    }

    #[test]
    fn a_shared_subtree_is_copied_once_however_deep_the_sharing_nests() {
        // Every level references the same next-level node twice, so the
        // distinct root-to-leaf paths grow as 2^levels while the distinct
        // nodes grow linearly. A memo that can be starved turns those paths
        // into copies: at 14 levels this shape measured 557,024 copies
        // against 464 distinct nodes. An exact memo cannot, at any depth.
        for (levels, ballast) in [(4usize, 0usize), (14, 0), (14, 32), (24, 4)] {
            let directory = TestDirectory::new(&format!("diamond-{levels}-{ballast}"));
            build_diamond_chain(&directory, levels, ballast);
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let distinct = distinct_reachable_nodes(&store, head);
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (_root, copied) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                head,
                &mut crate::progress::DiscardedProgress,
            )
            .expect("deep copy");
            writer.finish().expect("finish");
            store.close().expect("close");
            assert_eq!(
                copied as usize, distinct,
                "levels={levels} ballast={ballast}: copied must equal the distinct node count"
            );
        }
    }

    /// A wide, shallow tree of roughly `fanout * fanout` leaves.
    fn build_wide_store(directory: &TestDirectory, fanout: usize) {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);

        let mut branches = Vec::with_capacity(fanout);
        for branch in 0..fanout {
            let mut leaves = Vec::with_capacity(fanout);
            for leaf in 0..fanout {
                let value = writer
                    .write_string(&format!("{branch}-{leaf}"))
                    .expect("leaf value");
                let node = writer
                    .write_node(
                        Some("nt:unstructured"),
                        &[],
                        &ChildNodesToWrite::Zero,
                        &[PropertyToWrite {
                            name: "n".to_owned(),
                            property_type: crate::content::property::PropertyType::String,
                            values: PropertyValuesToWrite::Single(value),
                        }],
                    )
                    .expect("leaf");
                leaves.push((format!("leaf{leaf:05}"), node));
            }
            let node = writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &ChildNodesToWrite::Many(leaves),
                    &[],
                )
                .expect("branch");
            branches.push((format!("branch{branch:05}"), node));
        }
        let root = writer
            .write_node(None, &[], &ChildNodesToWrite::Many(branches), &[])
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

    #[test]
    fn the_exact_memo_costs_a_bounded_number_of_bytes_a_node() {
        use super::{RewrittenNodes, SegmentInterner};
        use crate::segment::identifier::SegmentIdentifier;
        use crate::segment::record::RecordIdentifier;

        // Exactness is only affordable because an entry is two packed u64s
        // rather than two `RecordIdentifier`s: the same map keyed on the
        // 24-byte identifier measures ~110 bytes a node, which is the figure
        // that made an exact memo look impossible. One segment holds many
        // records, so the interner stays small while the memo grows.
        for count in [1_000_000usize, 4_000_000] {
            let mut interner = SegmentInterner::new();
            let mut memo = RewrittenNodes::new();
            for index in 0..count {
                let record = RecordIdentifier {
                    segment: SegmentIdentifier {
                        most_significant_bits: (index / 8192) as u64,
                        least_significant_bits: 0x5eed,
                    },
                    record_number: index as u32,
                };
                let packed = interner.pack(record);
                memo.insert(packed, packed);
                assert_eq!(interner.unpack(packed), record, "packing round-trips");
            }
            // Resident bytes vary with allocator reuse, so the pinned figure
            // is the table's own occupancy: two `u64` vectors over its slots.
            // Resident size tracked it within a few bytes a node when measured
            // in isolation (44 at a million entries, 35 at four million).
            let bytes_per_node = memo.keys.len() * 2 * std::mem::size_of::<u64>() / count;
            assert_eq!(memo.len, count);
            assert!(
                bytes_per_node <= 48,
                "{count} entries cost {bytes_per_node} bytes a node; the packed \
                 table must stay far below the ~110 an identifier-keyed map costs"
            );
            assert!(
                memo.len * 10 <= memo.keys.len() * 7,
                "the table stays under its load factor"
            );
        }
    }

    #[test]
    fn the_exact_memo_holds_only_what_the_tree_reaches() {
        for fanout in [100usize, 320] {
            let directory = TestDirectory::new(&format!("footprint-{fanout}"));
            build_wide_store(&directory, fanout);
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let distinct = distinct_reachable_nodes(&store, head);
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (_root, copied) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                head,
                &mut crate::progress::DiscardedProgress,
            )
            .expect("deep copy");
            writer.finish().expect("finish");
            store.close().expect("close");
            assert_eq!(copied as usize, distinct, "the copy is exact at {fanout}");
        }
    }

    #[test]
    fn a_cyclic_source_is_refused_at_the_record_that_closes_the_cycle() {
        use crate::content::provider::tests::MemorySegmentProvider;
        use crate::error::Error;
        use crate::writer::segment_builder::SegmentBufferBuilder;

        let directory = TestDirectory::new("compaction-cycle");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let original_child = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("original child");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "loop".to_owned(),
                    node: original_child,
                },
                &[],
            )
            .expect("root");
        writer.finish().expect("finish segment");

        // Point the root's only child slot back at the root itself.
        let view = store.segment(root.segment).expect("root segment");
        let root_position = view
            .record_position(root.record_number)
            .expect("root position");
        let mut cyclic_bytes = view.bytes.to_vec();
        let child_slot: &mut [u8; 6] = (&mut cyclic_bytes[root_position + 12..root_position + 18])
            .try_into()
            .expect("one child identifier slot");
        SegmentBufferBuilder::write_record_identifier_bytes(0, root.record_number, child_slot);
        let mut memory = MemorySegmentProvider::default();
        memory.insert(root.segment, cyclic_bytes);

        let mut sink_writer = store.record_writer(generation);
        let error = deep_copy_tree_with_progress(
            &memory,
            &mut sink_writer,
            root,
            &mut crate::progress::DiscardedProgress,
        )
        .expect_err("a cyclic source is refused");
        let Error::InvalidFormat { details } = &error else {
            panic!("a cycle is a format error, got {error:?}");
        };
        // Exactly, at the closing record — not "probably a cycle" after 4000
        // wasted levels, and naming the record so the store can be repaired.
        assert!(
            details.contains("contained in its own subtree"),
            "unexpected detail: {details}"
        );
        assert!(
            details.contains(&root.to_string()),
            "the error names the offending record: {details}"
        );
        store.close().expect("close");
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
