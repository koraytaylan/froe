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

mod gc_log;
mod memo;
#[cfg(test)]
mod test_support;
mod walk;

pub(crate) use gc_log::*;
pub(crate) use memo::*;
#[cfg(test)]
pub(crate) use test_support::*;
pub(crate) use walk::*;

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
    deep_copy_super_root_omitting_subtrees(
        source,
        writer,
        super_root,
        omitted_checkpoints,
        &SubtreeOmissions {
            omitted_subtree_records: &std::collections::HashSet::new(),
            context_dependent_records: &std::collections::HashSet::new(),
        },
        bulk_sharing,
        observer,
    )
}

/// The subtree omissions a purging copy applies outside checkpoint
/// snapshots: the roots it declines to enter, and the ancestors whose
/// rewritten form therefore depends on the scope.
pub struct SubtreeOmissions<'omissions> {
    /// The records the copy never enters outside checkpoint snapshots.
    pub omitted_subtree_records: &'omissions std::collections::HashSet<RecordIdentifier>,
    /// The ancestors on the path from the content root down to each omitted
    /// record, memoized per scope because the head's copy and a checkpoint
    /// snapshot's copy of them differ.
    pub context_dependent_records: &'omissions std::collections::HashSet<RecordIdentifier>,
}

/// Deep-copies a super-root, additionally omitting whole subtrees by their
/// root records — outside checkpoint snapshots, which keep everything they
/// froze. This is the confirmed version-history purge's entry point: the
/// omitted subtrees simply never enter the fresh generation, exactly the
/// mechanism checkpoint retirement uses, and the reclaim pass that follows
/// the copy is what turns the omission into reclaimed space.
///
/// # Panics
///
/// Panics on the same copy-once violations as [`deep_copy_tree_with_progress`].
pub fn deep_copy_super_root_omitting_subtrees<Sink: SegmentSink>(
    source: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    super_root: RecordIdentifier,
    omitted_checkpoints: &std::collections::BTreeSet<String>,
    omissions: &SubtreeOmissions<'_>,
    bulk_sharing: BulkBlockSharing,
    observer: &mut dyn ProgressObserver,
) -> Result<(RecordIdentifier, u64)> {
    let source_root = super_root;
    let mut copier = Compactor {
        source,
        writer,
        omitted_checkpoints,
        omitted_subtree_records: omissions.omitted_subtree_records,
        context_dependent_records: omissions.context_dependent_records,
        scoped_rewrites: [
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        ],
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
    let scoped: usize = copier
        .scoped_rewrites
        .iter()
        .map(std::collections::HashMap::len)
        .sum();
    assert_eq!(
        copier.compacted_nodes,
        (memoized + scoped) as u64,
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

    if !store.compare_and_set_head(head, new_head) {
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
                    kind,
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

/// Rewrites `journal.log` to a single line naming `head`, matching the
/// offline compact tool. The store's own journal handle is bypassed so
/// the truncation is atomic from the reader's perspective (write to a
/// temporary file, then rename over the original).
#[cfg(test)]
pub(crate) fn rewrite_journal_to_head(
    store: &WritableRepository,
    head: RecordIdentifier,
) -> Result<()> {
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

/// Appends one line to `gc.log`:
/// `repoSize,reclaimedSize,timestamp,generation,fullGeneration,nodes,root`.
#[cfg(test)]
pub(crate) fn append_gc_log(
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

#[cfg(test)]
mod tests;
