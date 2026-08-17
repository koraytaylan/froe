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
//! The walk carries its own stack on the heap and imposes no depth limit —
//! tree depth is a property of the repository, not something this code may
//! choose — and terminates on a corrupt self-referential graph by refusing
//! the record that closes the cycle.
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

use crate::content::node::{NodeState, PropertyState, PropertyValues};
use crate::content::property::{PropertyType, PropertyValue};
use crate::content::provider::SegmentProvider;
use crate::content::value::BinaryValue;
use crate::error::{Error, Result};
use crate::packed_records::SegmentInterner;
use crate::progress::{DiscardedProgress, ProgressObserver};
#[cfg(test)]
use crate::progress::{Step, WorkUnit};
use crate::segment::record::RecordIdentifier;
use crate::writer::record_writer::{
    BulkBlockSharing, ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter,
    SegmentSink, sort_properties_for_template,
};
use crate::writer::segment_builder::GarbageCollectionGeneration;
#[cfg(test)]
use crate::writer::store_writer::{
    ArchiveRewritePolicy, GenerationReclaimRequest, RETAINED_GENERATIONS, ReclaimRule,
    WritableRepository,
};

/// The kind of compaction to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionKind {
    /// Advances both generation and full generation; reclaims everything.
    Full,
    /// Advances only the generation, keeping the full generation.
    Tail,
}

/// The outcome of the test-only compaction primitive.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactionOutcome {
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
///
/// # Panics
///
/// Panics if the copy-once invariant is violated — if the number of nodes
/// copied disagrees with the number memoized, or if a source record is
/// memoized twice. Neither is reachable from any input, valid or corrupt:
/// they mean a logic error in the walk, and failing loudly beats writing a
/// store whose node count cannot be trusted.
pub fn deep_copy_tree<Sink: SegmentSink>(
    source: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    source_root: RecordIdentifier,
) -> Result<(RecordIdentifier, u64)> {
    deep_copy_tree_with_progress(source, writer, source_root, &mut DiscardedProgress)
}

/// Deep-copies exactly like [`deep_copy_tree`], reporting the number of
/// nodes rewritten so far to `observer`.
///
/// # Panics
///
/// Panics if the copy-once invariant is violated — if the number of nodes
/// copied disagrees with the number memoized, or if a source record is
/// memoized twice. Neither is reachable from any input, valid or corrupt:
/// they mean a logic error in the walk, and failing loudly beats writing a
/// store whose node count cannot be trusted.
pub fn deep_copy_tree_with_progress<Sink: SegmentSink>(
    source: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    source_root: RecordIdentifier,
    observer: &mut dyn ProgressObserver,
) -> Result<(RecordIdentifier, u64)> {
    deep_copy_super_root_with_progress(
        source,
        writer,
        source_root,
        &std::collections::BTreeSet::new(),
        observer,
    )
}

/// Deep-copies a tree from one store into a **different** one, copying
/// every binary block rather than referencing bulk segments in place.
///
/// This is what backup and restore need. Using the same-store copy for
/// them produces a target that opens, serves its whole content tree, and
/// passes a consistency check that does not read binaries — while the
/// binaries themselves stayed behind in the source.
///
/// # Panics
///
/// Panics on the same copy-once violations as [`deep_copy_tree_with_progress`].
pub fn deep_copy_tree_across_stores_with_progress<Sink: SegmentSink>(
    source: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    source_root: RecordIdentifier,
    observer: &mut dyn ProgressObserver,
) -> Result<(RecordIdentifier, u64)> {
    deep_copy_super_root_sharing(
        source,
        writer,
        source_root,
        &std::collections::BTreeSet::new(),
        BulkBlockSharing::AcrossStores,
        observer,
    )
}

/// Deep-copies a super-root, omitting the named checkpoints.
///
/// A checkpoint a maintenance run retires is never entered, so neither its
/// snapshot root nor any record only it reaches is copied. This is how
/// expiry happens: not by rewriting the live head first — which would move
/// the head twice, append a second journal line, and strand records at the
/// old generation inside an archive the reclaim pass never sweeps — but
/// simply by declining to carry them into the fresh generation. A subtree a
/// retired checkpoint shares with the content root, or with a checkpoint
/// that stays, is still copied through those.
///
/// `omitted_checkpoints` names children of the super-root's `checkpoints`
/// container. Any other name in the set is silently absent from the tree and
/// therefore has no effect.
///
/// # Panics
///
/// Panics on the same copy-once violations as [`deep_copy_tree_with_progress`].
pub fn deep_copy_super_root_with_progress<Sink: SegmentSink>(
    source: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    super_root: RecordIdentifier,
    omitted_checkpoints: &std::collections::BTreeSet<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<(RecordIdentifier, u64)> {
    deep_copy_super_root_sharing(
        source,
        writer,
        super_root,
        omitted_checkpoints,
        BulkBlockSharing::WithinOneStore,
        observer,
    )
}

/// Deep-copies a super-root with an explicit bulk-block sharing mode.
///
/// Every copy that crosses a store boundary must pass
/// [`BulkBlockSharing::AcrossStores`], or the result references bulk
/// segments that exist only in the source.
///
/// # Panics
///
/// Panics on the same copy-once violations as [`deep_copy_tree_with_progress`].
pub fn deep_copy_super_root_sharing<Sink: SegmentSink>(
    source: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    super_root: RecordIdentifier,
    omitted_checkpoints: &std::collections::BTreeSet<String>,
    bulk_sharing: BulkBlockSharing,
    observer: &mut dyn ProgressObserver,
) -> Result<(RecordIdentifier, u64)> {
    let source_root = super_root;
    let mut copier = Compactor {
        source,
        writer,
        omitted_checkpoints,
        bulk_sharing,
        segments: SegmentInterner::new(),
        rewritten_nodes: RewrittenNodes::new(),
        nodes_on_path: std::collections::HashSet::new(),
        compacted_nodes: 0,
        reported_nodes: 0,
        observer,
    };
    let root = copier.compact_tree(source_root)?;
    // The copy-once invariant as a postcondition rather than an argument
    // about the code. Occupancy is recounted from the table rather than read
    // from `len`: the two are incremented together, so comparing against
    // `len` would be comparing a counter with itself and could not see a
    // growth that lost entries. One pass over the slots at the end of a copy
    // that took minutes.
    let memoized = copier.rewritten_nodes.occupied_slots();
    assert_eq!(
        copier.compacted_nodes, memoized as u64,
        "copied node count diverged from the number of memoized nodes"
    );
    assert_eq!(
        copier.rewritten_nodes.len, memoized,
        "the memo's entry count diverged from its occupancy"
    );
    // The stride suppressed the last partial batch; report the exact
    // total so the copy does not end short of what it wrote.
    copier.observer.step_advanced(copier.compacted_nodes);
    Ok((root, copier.compacted_nodes))
}

/// How many nodes a deep copy rewrites between progress reports.
const COPIED_NODE_REPORT_STRIDE: u64 = 512;

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
/// Probing masks with `slots - 1`, so a non-power-of-two would silently make
/// part of the table unreachable; nothing else holds that property.
const INITIAL_MEMO_SLOTS: usize = 1024;
const _: () = assert!(INITIAL_MEMO_SLOTS.is_power_of_two());

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

    /// Counts the slots actually holding an entry, independently of `len`.
    fn occupied_slots(&self) -> usize {
        self.keys.iter().filter(|key| **key != 0).count()
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

    /// Places a key known to be absent. Open addressing has no natural
    /// duplicate check — a second insert of the same key would occupy a
    /// second slot, leaving `len` counting one node twice while `get` still
    /// answered correctly, so the drift would be silent. The walk cannot
    /// produce one (it probes before inserting, and only inserts on the
    /// single path out of a miss), which is exactly why a violation here
    /// means a logic error rather than bad input, and must be loud.
    fn insert_without_growing(&mut self, key: u64, value: u64) {
        let mut slot = self.slot_of(key);
        while self.keys[slot] != 0 {
            assert_ne!(
                self.keys[slot], key,
                "a source record was memoized twice; the memo probe or the \
                 path set is broken"
            );
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
    /// Whether bulk blocks may be referenced in place or must be copied.
    bulk_sharing: BulkBlockSharing,
    /// Children of the super-root's `checkpoints` container this copy never
    /// enters. Empty for every copy that is not a maintenance run's.
    omitted_checkpoints: &'writer std::collections::BTreeSet<String>,
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

/// The outcome of resolving one node reference.
enum Entered {
    /// Not yet copied; descend into this frame.
    Fresh(CompactionFrame),
    /// Already copied; the memo holds the rewritten record.
    Memoized(RecordIdentifier),
}

/// One suspended node in the deep copy: its children have to be rewritten
/// before it can be written itself, so a node is visited twice — once to
/// enumerate its children, once to emit it after they are all rewritten.
struct CompactionFrame {
    source: RecordIdentifier,
    packed: u64,
    /// The name this node has in its parent, so the rewritten record can be
    /// attached when the frame pops. `None` for the root.
    name_in_parent: Option<String>,
    /// Remaining children to descend into, in reverse order so the next one
    /// is a `pop`.
    pending_children: Vec<(String, RecordIdentifier)>,
    /// Children already rewritten, in enumeration order.
    rewritten_children: Vec<(String, RecordIdentifier)>,
}

impl<Sink: SegmentSink> Compactor<'_, Sink> {
    /// Rewrites `source_root` and everything it reaches.
    ///
    /// The walk carries its own stack on the heap, so how deep a tree it can
    /// copy is bounded by memory rather than by the thread it happens to run
    /// on. There is no depth limit: depth is a property of the repository,
    /// not something this code can choose, and a bound on it would refuse
    /// valid stores. Termination on a corrupt self-referential graph is
    /// `nodes_on_path`, which decides it exactly.
    fn compact_tree(&mut self, source_root: RecordIdentifier) -> Result<RecordIdentifier> {
        let mut stack = match self.enter(source_root)? {
            Entered::Fresh(root) => vec![root],
            Entered::Memoized(rewritten) => return Ok(rewritten),
        };

        loop {
            let next = stack
                .last_mut()
                .expect("the loop returns before the stack empties")
                .pending_children
                .pop();
            if let Some((name, child)) = next {
                match self.enter(child)? {
                    Entered::Fresh(mut frame) => {
                        // The `checkpoints` container directly under the
                        // super-root is the one node whose child *names* this
                        // copy reads. A retired checkpoint is dropped here,
                        // before the frame is pushed, so the walk never enters
                        // it. A memo hit cannot bypass this: the container is
                        // reached exactly once per copy, and `Memoized` means
                        // the node was already emitted — which for the
                        // container can only happen after this filter ran.
                        if stack.len() == 1
                            && name == "checkpoints"
                            && !self.omitted_checkpoints.is_empty()
                        {
                            frame.pending_children.retain(|(checkpoint, _)| {
                                !self.omitted_checkpoints.contains(checkpoint)
                            });
                        }
                        frame.name_in_parent = Some(name);
                        stack.push(frame);
                    }
                    Entered::Memoized(rewritten) => stack
                        .last_mut()
                        .expect("the parent frame is still on the stack")
                        .rewritten_children
                        .push((name, rewritten)),
                }
                continue;
            }
            let finished = stack.pop().expect("a frame was just inspected");
            let rewritten = self.emit(
                finished.source,
                finished.packed,
                finished.rewritten_children,
            )?;
            match stack.last_mut() {
                Some(parent) => parent.rewritten_children.push((
                    finished
                        .name_in_parent
                        .expect("only the root frame has no name"),
                    rewritten,
                )),
                None => return Ok(rewritten),
            }
        }
    }

    /// Resolves one node: either the memo already holds its rewritten copy,
    /// or a frame to descend into.
    fn enter(&mut self, source_node: RecordIdentifier) -> Result<Entered> {
        let packed = self.segments.pack(source_node);
        if let Some(rewritten) = self.rewritten_nodes.get(packed) {
            return Ok(Entered::Memoized(self.segments.unpack(rewritten)));
        }
        // A node reachable from itself is corruption — valid records only
        // reference already-written records — and is refused exactly, at the
        // record that closes the cycle. The memo cannot mask it: a memo hit
        // returns above, so a memoized node is never on the path.
        if !self.nodes_on_path.insert(packed) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "node record {source_node} is contained in its own subtree; \
                     the source records form a cycle"
                ),
            });
        }
        let node = NodeState::new(self.source, source_node);
        let mut pending_children: Vec<(String, RecordIdentifier)> = node
            .child_node_entries()?
            .into_iter()
            .map(|(name, child)| (name, child.record_identifier()))
            .collect();
        // Reversed so `pop` yields enumeration order.
        pending_children.reverse();
        Ok(Entered::Fresh(CompactionFrame {
            source: source_node,
            packed,
            name_in_parent: None,
            pending_children,
            rewritten_children: Vec::new(),
        }))
    }

    /// Writes one node whose children have all been rewritten.
    fn emit(
        &mut self,
        source_node: RecordIdentifier,
        packed_source: u64,
        mut child_entries: Vec<(String, RecordIdentifier)>,
    ) -> Result<RecordIdentifier> {
        let node = NodeState::new(self.source, source_node);
        let template = node.template()?;
        let stable_identifier = node.stable_identifier_bytes()?;

        let children = match child_entries.len() {
            0 => ChildNodesToWrite::Zero,
            1 => {
                let (name, node) = child_entries.pop().expect("one child");
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
                    self.writer.copy_binary_value_with_sharing(
                        self.source,
                        *record_identifier,
                        self.bulk_sharing,
                    )
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

/// Compacts an open session in place: deep-copies the head into a fresh
/// generation, swaps the head, reclaims the old generations, and rewrites the
/// journal to a single line.
///
/// Not the shipped entry point — `froe compact` plans, confirms and applies
/// under one lock through `writer::maintenance`, and this performs no
/// planning, takes no lock and asks nothing. It survives as the focused
/// primitive the copy-and-reclaim unit tests drive directly, so a failure in
/// the deep copy is diagnosed where it happens rather than through a whole
/// maintenance run.
#[cfg(test)]
pub(crate) fn compact(
    store: &mut WritableRepository,
    kind: CompactionKind,
) -> Result<CompactionOutcome> {
    compact_with_progress(store, kind, &mut DiscardedProgress)
}

/// Compacts exactly like [`compact`], reporting the deep copy, the
/// reclamation sweep, and the journal rewrite to `observer`.
///
/// Test-only for the same reason as [`compact`].
///
/// The memo maps each source node to its rewritten copy and is exact, so a
/// subtree the live root and a checkpoint both reference is copied once and
/// `compacted_nodes` equals the number of distinct node records reachable
/// from the head.
#[cfg(test)]
pub(crate) fn compact_with_progress(
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
    // allocating the compacted copy: without this pass, every retry against a
    // pre-existing defect durably appends another full copy before failing.
    //
    // The proof travels to reclamation, which would otherwise re-derive the
    // identical certificate over the identical bytes. Nothing between here and
    // there writes to a base archive — the deep copy only appends new ones —
    // and each source is certified again through a fresh no-follow descriptor
    // immediately before it is mutated, which is the certificate that actually
    // guards the sweep.
    let certified_sources = store.preflight_reclaim_sources_with_progress(observer)?;

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
        |_observer| {
            store.reclaim_old_generations_with(GenerationReclaimRequest {
                rule: ReclaimRule {
                    reference: target_generation,
                    full: kind == CompactionKind::Full,
                    retained_generations: RETAINED_GENERATIONS,
                },
                rewrite_policy: ArchiveRewritePolicy::EveryReclaimableArchive,
                certified_sources: Some(&certified_sources),
                expected: None,
            })
        },
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
#[cfg(test)]
fn append_gc_log(
    store: &WritableRepository,
    repository_size: u64,
    reclaimed_size: u64,
    generation: GarbageCollectionGeneration,
    compacted_nodes: u64,
    root: RecordIdentifier,
) -> Result<()> {
    let line = garbage_collection_log_entry(
        repository_size,
        reclaimed_size,
        generation,
        compacted_nodes,
        root,
    );
    append_garbage_collection_log_entry(store.directory(), &line)
}

/// Oak's seven-field `gc.log` line for one completed compaction cycle:
/// `repoSize,reclaimedSize,timestamp,generation,fullGeneration,nodes,root`.
///
/// Built separately from the append so a caller can hold the exact bytes it
/// wrote and prove afterwards that the file grew by those and nothing else.
/// The timestamp makes the line unreproducible, which is why proving it after
/// the fact means remembering it rather than recomputing it.
pub(crate) fn garbage_collection_log_entry(
    repository_size: u64,
    reclaimed_size: u64,
    generation: GarbageCollectionGeneration,
    compacted_nodes: u64,
    root: RecordIdentifier,
) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    format!(
        "{repository_size},{reclaimed_size},{timestamp},{},{},{compacted_nodes},{}:{}\n",
        generation.generation, generation.full_generation, root.segment, root.record_number as i32,
    )
}

/// Appends one already-built entry to `gc.log`, durably.
pub(crate) fn append_garbage_collection_log_entry(
    directory: &std::path::Path,
    line: &str,
) -> Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("gc.log"))?;
    file.write_all(line.as_bytes())?;
    file.sync_data()?;
    Ok(())
}

/// Rewrites `journal.log` to a single line naming `head`, matching the
/// offline compact tool. The store's own journal handle is bypassed so
/// the truncation is atomic from the reader's perspective (write to a
/// temporary file, then rename over the original).
#[cfg(test)]
fn rewrite_journal_to_head(store: &WritableRepository, head: RecordIdentifier) -> Result<()> {
    use std::io::Write as _;
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
    use super::{
        CompactionKind, compact, deep_copy_super_root_with_progress, deep_copy_tree_with_progress,
    };
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

    /// Builds a store whose super-root carries a content root and three
    /// checkpoints, and returns the checkpoint names in creation order.
    fn build_store_with_checkpoints(directory: &TestDirectory) -> Vec<String> {
        build_populated_store(directory);
        let store = WritableRepository::open(&directory.path).expect("open for checkpoints");
        let mut names = Vec::new();
        for _ in 0..3 {
            names.push(
                create_checkpoint(&store, 60 * 60 * 1000, &[]).expect("create the checkpoint"),
            );
        }
        store.close().expect("close after checkpoints");
        names
    }

    /// Every child name a node has, in stored order.
    fn child_names(node: &crate::content::node::NodeState<'_>) -> Vec<String> {
        node.child_node_entries()
            .expect("enumerate the children")
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Copies a repository directory, so two mechanisms can be applied to the
    /// same store. Checkpoint names are randomly generated, so a fixture built
    /// twice is not the same fixture and the two results cannot be compared.
    fn copy_repository(source: &std::path::Path, target: &std::path::Path) {
        std::fs::create_dir_all(target).expect("create the copy directory");
        for entry in std::fs::read_dir(source).expect("list the source store") {
            let entry = entry.expect("read the directory entry");
            if entry.file_type().expect("file type").is_file() {
                std::fs::copy(entry.path(), target.join(entry.file_name()))
                    .expect("copy a repository file");
            }
        }
        // `repo.lock` is a live artefact, never part of the copy.
        let _ = std::fs::remove_file(target.join("repo.lock"));
    }

    /// A copy that omits a checkpoint must land on the same tree as removing
    /// that checkpoint from the head first and then copying — two independent
    /// mechanisms, one of which (`remove_checkpoints`) is the commit path Oak's
    /// own checkpoint removal mirrors. Both run against copies of one store,
    /// because checkpoint names are random and a fixture built twice carries
    /// different ones.
    #[test]
    fn a_copy_that_omits_a_checkpoint_reproduces_every_other_child_exactly() {
        let source = TestDirectory::new("omit-checkpoint-source");
        let names = build_store_with_checkpoints(&source);
        let dropped = names[1].clone();

        let omitting = TestDirectory::new("omit-checkpoint-copy");
        copy_repository(&source.path, &omitting.path);
        let removing = TestDirectory::new("omit-checkpoint-removal");
        copy_repository(&source.path, &removing.path);

        let (omitted_head, omitted_nodes) = {
            let store = WritableRepository::open(&omitting.path).expect("open the omitting store");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (head, nodes) = deep_copy_super_root_with_progress(
                &store,
                &mut writer,
                store.head(),
                &std::collections::BTreeSet::from([dropped.clone()]),
                &mut crate::progress::DiscardedProgress,
            )
            .expect("copy while omitting one checkpoint");
            writer.finish().expect("finish");
            assert!(store.set_head(store.head(), head));
            store.flush().expect("flush");
            store.close().expect("close the omitting store");
            (head, nodes)
        };

        let (removed_head, removed_nodes) = {
            let store = WritableRepository::open(&removing.path).expect("open the removing store");
            crate::writer::commit::remove_checkpoints(&store, std::slice::from_ref(&dropped))
                .expect("remove the checkpoint from the live head");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (head, nodes) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                store.head(),
                &mut crate::progress::DiscardedProgress,
            )
            .expect("copy the already-reduced head");
            writer.finish().expect("finish");
            assert!(store.set_head(store.head(), head));
            store.flush().expect("flush");
            store.close().expect("close the removing store");
            (head, nodes)
        };

        let omitted = Repository::open(&omitting.path).expect("reopen the omitting store");
        let removed = Repository::open(&removing.path).expect("reopen the removing store");
        let omitted_root = omitted.node(omitted_head);
        let removed_root = removed.node(removed_head);
        assert_eq!(
            child_names(&omitted_root),
            child_names(&removed_root),
            "the super-root carries the same children either way"
        );

        let omitted_checkpoints = omitted_root
            .child_node("checkpoints")
            .expect("read")
            .expect("present");
        let removed_checkpoints = removed_root
            .child_node("checkpoints")
            .expect("read")
            .expect("present");
        let mut surviving = child_names(&omitted_checkpoints);
        let mut expected = child_names(&removed_checkpoints);
        surviving.sort();
        expected.sort();
        assert_eq!(
            surviving, expected,
            "exactly the same checkpoints survive both mechanisms"
        );
        assert!(
            !surviving.contains(&dropped),
            "the retired checkpoint is gone: {surviving:?}"
        );
        // Record addresses differ between two stores by construction, so the
        // equivalence that means something is the shape: both mechanisms copy
        // the same number of distinct nodes, and every surviving checkpoint
        // still resolves its snapshot.
        assert_eq!(
            omitted_nodes, removed_nodes,
            "both mechanisms copy the same tree, so the same number of distinct nodes"
        );
        for name in &surviving {
            for (label, container) in [
                ("omitted", &omitted_checkpoints),
                ("removed", &removed_checkpoints),
            ] {
                let checkpoint = container
                    .child_node(name)
                    .expect("read the checkpoint")
                    .unwrap_or_else(|| panic!("{label} store keeps checkpoint {name}"));
                assert!(
                    checkpoint
                        .child_node("root")
                        .expect("read the snapshot")
                        .is_some(),
                    "{label} store resolves the snapshot of checkpoint {name}"
                );
            }
        }
    }

    /// A checkpoint whose subtree nothing else reaches costs exactly its own
    /// distinct records, and none of them reaches the output.
    #[test]
    fn an_omitted_checkpoint_leaves_its_exclusive_records_uncopied() {
        let directory = TestDirectory::new("omit-exclusive-records");
        let names = build_store_with_checkpoints(&directory);
        let store = WritableRepository::open(&directory.path).expect("open");

        let generation = store.writing_generation().expect("generation");
        let mut whole_writer = store.record_writer(generation);
        let (_, whole) = deep_copy_super_root_with_progress(
            &store,
            &mut whole_writer,
            store.head(),
            &std::collections::BTreeSet::new(),
            &mut crate::progress::DiscardedProgress,
        )
        .expect("copy everything");
        whole_writer.finish().expect("finish");

        let mut reduced_writer = store.record_writer(generation);
        let (_, reduced) = deep_copy_super_root_with_progress(
            &store,
            &mut reduced_writer,
            store.head(),
            &std::collections::BTreeSet::from([names[0].clone()]),
            &mut crate::progress::DiscardedProgress,
        )
        .expect("copy while omitting one checkpoint");
        reduced_writer.finish().expect("finish");

        assert!(
            reduced < whole,
            "omitting a checkpoint copies strictly fewer nodes: {reduced} against {whole}"
        );
        store.close().expect("close");
    }

    /// A subtree the retired checkpoint shares with the live content root is
    /// still copied — through the root. Omission drops a name, not a subtree.
    #[test]
    fn omitting_a_shared_subtree_still_copies_it_through_the_live_root() {
        let directory = TestDirectory::new("omit-shared-subtree");
        let names = build_store_with_checkpoints(&directory);
        let store = WritableRepository::open(&directory.path).expect("open");

        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let (head, _) = deep_copy_super_root_with_progress(
            &store,
            &mut writer,
            store.head(),
            &std::collections::BTreeSet::from([names[0].clone()]),
            &mut crate::progress::DiscardedProgress,
        )
        .expect("copy while omitting one checkpoint");
        writer.finish().expect("finish");
        assert!(store.set_head(store.head(), head));
        store.flush().expect("flush");
        store.close().expect("close");

        // The checkpoints snapshot the same content root, so the content the
        // retired one pointed at is still fully readable through `root`.
        let repository = Repository::open(&directory.path).expect("reopen");
        let content = repository
            .node_at_path("/content")
            .expect("resolve the content")
            .expect("the shared content survives the omission");
        assert!(
            content.child_node_count().expect("count") > 0,
            "the shared subtree is intact"
        );
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
            // `len` is a counter, so it would still be right if a growth
            // dropped entries. Retrieval is what actually pins the invariant:
            // the table crosses many growths at these sizes, and losing one
            // entry means re-copying that node's whole subtree.
            for index in 0..count {
                let record = RecordIdentifier {
                    segment: SegmentIdentifier {
                        most_significant_bits: (index / 8192) as u64,
                        least_significant_bits: 0x5eed,
                    },
                    record_number: index as u32,
                };
                let packed = interner.pack(record);
                assert_eq!(
                    memo.get(packed),
                    Some(packed),
                    "entry {index} of {count} survives every growth"
                );
            }
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

    /// Throughput of the exact copy, for extrapolating to a field-scale head.
    /// Ignored by default: it writes about a million nodes.
    #[test]
    fn a_tree_deeper_than_any_call_stack_copies_whole() {
        // 100k levels on the 2 MiB stack a spawned thread gets by default.
        // The recursive walk aborted the process at 2900 levels here; there
        // is no depth this can refuse, because depth is the repository's
        // property and not this code's to bound.
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let directory = TestDirectory::new("deep-chain");
                build_diamond_chain(&directory, 100_000, 0);
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
                .expect("a deep tree copies rather than aborting");
                writer.finish().expect("finish");
                assert_eq!(copied as usize, distinct);
                assert!(distinct > 100_000, "the chain really is that deep");
                // The post-write health traversal walks the same tree, so a
                // depth limit there would only move the failure.
                crate::tooling::verify_node_tree(&store, head)
                    .expect("the verifier has no depth limit either");
                store.close().expect("close");
            })
            .expect("spawn");
        handle.join().expect("the walk stays off the call stack");
    }

    fn resident_bytes() -> usize {
        let statm = std::fs::read_to_string("/proc/self/statm").expect("statm");
        let pages: usize = statm
            .split_whitespace()
            .nth(1)
            .expect("resident field")
            .parse()
            .expect("page count");
        pages * 4096
    }

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn measure_deep_chain_walk_footprint() {
        for levels in [100_000usize, 400_000] {
            let directory = TestDirectory::new(&format!("deep-footprint-{levels}"));
            build_diamond_chain(&directory, levels, 0);
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let before = resident_bytes();
            crate::tooling::verify_node_tree(&store, head).expect("verifies");
            let after_verify = resident_bytes();
            store.close().expect("close");
            println!(
                "levels={levels:>7} verify_rss_delta={:>6} MiB = {:>4} B/level",
                after_verify.saturating_sub(before) / 1024 / 1024,
                after_verify.saturating_sub(before) / levels,
            );
        }
    }

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn measure_copy_throughput() {
        let directory = TestDirectory::new("throughput");
        let build_started = std::time::Instant::now();
        build_wide_store(&directory, 1000);
        let built = build_started.elapsed();
        let store = WritableRepository::open(&directory.path).expect("open");
        let head = store.head();
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let started = std::time::Instant::now();
        let (_root, copied) = deep_copy_tree_with_progress(
            &store,
            &mut writer,
            head,
            &mut crate::progress::DiscardedProgress,
        )
        .expect("deep copy");
        let elapsed = started.elapsed();
        writer.finish().expect("finish");
        store.close().expect("close");
        let per_second =
            u32::try_from(copied).map_or(f64::INFINITY, f64::from) / elapsed.as_secs_f64();
        println!(
            "built {copied} nodes in {:.1}s; copied in {:.2}s = {per_second:.0} nodes/s; \
             18.8M nodes extrapolates to {:.1} min",
            built.as_secs_f64(),
            elapsed.as_secs_f64(),
            18_800_000.0 / per_second / 60.0,
        );
    }

    /// A deterministic generator, so a failure names a seed that reproduces
    /// it. Nothing in the crate needs randomness, so this stays local.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            if bound == 0 {
                return 0;
            }
            (self.next() % bound as u64) as usize
        }
    }

    /// Builds a random acyclic content graph. Every node draws its children
    /// from the nodes already written, which is what the segment format
    /// guarantees anyway (a record only references earlier records), so the
    /// result is a legal DAG with arbitrary sharing.
    fn build_random_dag(directory: &TestDirectory, seed: u64) {
        let mut rng = Rng(seed | 1);
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);

        // Most shapes stay small so many of them run; every twentieth is big
        // enough that `RewrittenNodes` crosses at least one growth (its first
        // is at 717 entries), so rehashing is exercised end to end and not
        // only by the table's own test.
        let node_count = if seed.is_multiple_of(20) {
            800 + rng.below(1700)
        } else {
            8 + rng.below(60)
        };
        let mut written = Vec::with_capacity(node_count);
        for index in 0..node_count {
            let child_count = if written.is_empty() { 0 } else { rng.below(5) };
            let mut children = Vec::with_capacity(child_count);
            for child in 0..child_count {
                // Draw with replacement, so the same record can be referenced
                // several times from one parent and from many parents.
                let picked = written[rng.below(written.len())];
                children.push((format!("c{child:03}"), picked));
            }
            let value = writer
                .write_string(&format!("seed{seed}-node{index}"))
                .expect("value");
            let node = writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &match children.len() {
                        0 => ChildNodesToWrite::Zero,
                        1 => {
                            let (name, node) = children.into_iter().next().expect("one child");
                            ChildNodesToWrite::One { name, node }
                        }
                        _ => ChildNodesToWrite::Many(children),
                    },
                    &[PropertyToWrite {
                        name: "n".to_owned(),
                        property_type: crate::content::property::PropertyType::String,
                        values: PropertyValuesToWrite::Single(value),
                    }],
                )
                .expect("node");
            written.push(node);
        }
        let root = *written.last().expect("at least one node");
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
    fn every_random_shape_copies_each_distinct_node_exactly_once() {
        // The count is cross-checked against `distinct_reachable_nodes`, which
        // walks with a `HashSet<RecordIdentifier>` and shares no code with the
        // interner or the memo — so agreement is two independent answers
        // matching, not one implementation agreeing with itself.
        for seed in 1..=200u64 {
            let directory = TestDirectory::new(&format!("random-dag-{seed}"));
            build_random_dag(&directory, seed);
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
                "seed {seed}: copied {copied} but {distinct} distinct nodes are reachable"
            );
        }
    }

    #[test]
    fn the_memo_and_the_interner_hold_their_own_invariants() {
        use super::{RewrittenNodes, SegmentInterner};
        use crate::segment::identifier::SegmentIdentifier;
        use crate::segment::record::RecordIdentifier;

        let mut rng = Rng(0x5EED_1234_9ABC_DEF1);
        let mut interner = SegmentInterner::new();
        let mut memo = RewrittenNodes::new();
        let mut expected = std::collections::HashMap::new();
        let mut packed_seen = std::collections::HashMap::new();

        for _ in 0..60_000 {
            let record = RecordIdentifier {
                segment: SegmentIdentifier {
                    most_significant_bits: rng.next() % 400,
                    least_significant_bits: rng.next() % 7,
                },
                record_number: (rng.next() % 5000) as u32,
            };
            let packed = interner.pack(record);

            // The sentinel is never a real key, so an occupied slot is never
            // mistaken for an empty one.
            assert_ne!(packed, 0, "no real record packs to the empty-slot key");
            // Packing is injective: two distinct records never share a key,
            // and a key always unpacks to the record it came from.
            assert_eq!(interner.unpack(packed), record, "packing round-trips");
            if let Some(previous) = packed_seen.insert(packed, record) {
                assert_eq!(previous, record, "two distinct records packed alike");
            }

            if let std::collections::hash_map::Entry::Vacant(slot) = expected.entry(packed) {
                let value = interner.pack(RecordIdentifier {
                    segment: record.segment,
                    record_number: record.record_number ^ 0x00FF_00FF,
                });
                slot.insert(value);
                memo.insert(packed, value);
            }

            // Everything ever inserted is still retrievable, across every
            // growth the table has performed by now.
            assert_eq!(memo.len, expected.len());
            for (key, value) in &expected {
                assert_eq!(memo.get(*key), Some(*value));
            }
            if expected.len() > 40 {
                expected.clear();
                memo = RewrittenNodes::new();
            }
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
