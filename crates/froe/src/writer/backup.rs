//! Backup, restore, and journal recovery.
//!
//! * **Backup** deep-copies the source repository's head — the super-root,
//!   so the content root *and* every checkpoint — into a target store,
//!   read-only on the source (no lock) and read-write on the target.
//! * **Restore** does the same with the roles swapped: the backup is the
//!   read-only source, and its head is copied into an existing store and
//!   becomes the new head.
//! * **Recover-journal** rebuilds `journal.log` when it is missing or
//!   unusable by scanning every data segment for candidate super-roots
//!   (nodes with both a `root` and a `checkpoints` child), verifying the
//!   newest one is fully traversable, and rewriting the journal with the
//!   surviving candidates oldest first — ordered by the timestamp in
//!   each segment's info record — so the consistent head is the last
//!   line.
//!
//! All three run under the target's repository lock, so they can never
//! race a running AEM instance, and all leave the store in a state a
//! subsequent AEM start consumes cleanly.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use crate::cache::BoundedCache;
use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::writer::compaction::deep_copy_tree_across_stores_with_progress;
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::WritableRepository;

/// Copies the source repository's head into `target_directory`,
/// overwriting the target's head with the copied super-root. The target
/// is created when absent.
pub fn backup(source_directory: &Path, target_directory: &Path) -> Result<()> {
    backup_with_progress(source_directory, target_directory, &mut DiscardedProgress)
}

/// Backs up exactly like [`backup`], reporting the source scan and the
/// node copy to `observer`.
pub fn backup_with_progress(
    source_directory: &Path,
    target_directory: &Path,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let source = Repository::open_with_progress(source_directory, observer)?;
    let target = WritableRepository::open_with_progress(target_directory, observer)?;
    copy_head_between(
        &source,
        source.head_record_identifier(),
        &target,
        "b",
        observer,
    )?;
    target.close()
}

/// Restores a backup into an existing store: the backup's head is copied
/// into `target_directory` and becomes the new head. The target's earlier
/// revisions remain in the journal until later compaction reclaims them.
pub fn restore(backup_directory: &Path, target_directory: &Path) -> Result<()> {
    restore_with_progress(backup_directory, target_directory, &mut DiscardedProgress)
}

/// Restores exactly like [`restore`], reporting the backup scan and the
/// node copy to `observer`.
pub fn restore_with_progress(
    backup_directory: &Path,
    target_directory: &Path,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let backup = Repository::open_with_progress(backup_directory, observer)?;
    let target = WritableRepository::open_with_progress(target_directory, observer)?;
    copy_head_between(
        &backup,
        backup.head_record_identifier(),
        &target,
        "r",
        observer,
    )?;
    target.close()
}

/// Deep-copies `source_head` from `source` into `target`, advances the
/// target head, and flushes. Copied segments carry the source head's
/// exact garbage collection generation triple, never the target's — Oak
/// stamps `sourceHead.getSegmentId().getGcGeneration()` verbatim, and a
/// later Java cleanup pass reclaims by generation distance, so an
/// invented triple could make it delete segments the head still needs.
///
/// Both callers cross a store boundary, so the copy must carry every
/// binary block rather than re-linking bulk segments where they lie. That
/// re-linking is correct only within one store: it is what keeps a bulk
/// segment reachable across a compaction, and it is meaningless when the
/// target is a different directory, where the reference resolves to
/// nothing. Getting it wrong is silent — the target opens, serves its
/// whole content tree, and holds no binary content.
fn copy_head_between(
    source: &Repository,
    source_head: RecordIdentifier,
    target: &WritableRepository,
    writer_identifier: &str,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let generation = source_head_generation(source, source_head)?;
    let mut writer = target.record_writer_with_identifier(generation, writer_identifier);
    let (new_head, _) = crate::progress::observe(
        observer,
        &Step::new("copying nodes", WorkUnit::Nodes),
        |observer| {
            deep_copy_tree_across_stores_with_progress(source, &mut writer, source_head, observer)
        },
    )?;
    writer.finish()?;
    target.replace_head(new_head);
    target.flush()
}

/// The garbage collection generation triple of the source head's segment,
/// from the archive index or — for a recovered archive without index
/// metadata — the segment header itself.
fn source_head_generation(
    source: &Repository,
    source_head: RecordIdentifier,
) -> Result<GarbageCollectionGeneration> {
    for archive in source.archives() {
        if let Some(entry) = archive.index_entry(source_head.segment) {
            return Ok(GarbageCollectionGeneration {
                generation: entry.generation,
                full_generation: entry.full_generation,
                is_compacted: entry.is_compacted,
            });
        }
    }
    let view = source.segment(source_head.segment)?;
    Ok(GarbageCollectionGeneration {
        generation: view.structure.generation,
        full_generation: view.structure.full_generation,
        is_compacted: view.structure.is_compacted,
    })
}

/// The outcome of a journal recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryOutcome {
    /// The record identifier the journal was recovered to.
    pub recovered_head: RecordIdentifier,
    /// How many candidate super-roots were found.
    pub candidates_examined: usize,
    /// The path the previous journal was backed up to, when one existed.
    pub previous_journal_backup: Option<std::path::PathBuf>,
}

/// A candidate head discovered during recovery.
struct Candidate {
    record: RecordIdentifier,
    timestamp_milliseconds: i64,
}

/// Rebuilds `journal.log` from the segments on disk. Scans every data
/// segment for super-root candidates, verifies the newest one is fully
/// traversable, and rewrites the journal with the surviving candidates
/// oldest first, so the verified head is the last (winning) line. Backs
/// up any existing journal to `journal.log.bak.NNN`.
///
/// Deliberate deviation from Java, which refuses to recover unless the
/// existing journal already has a resolvable line: this recovery also
/// works when `journal.log` is missing or fully unresolvable — the
/// candidates come from the segments, not the journal, and the newest
/// one must still pass the full consistency gate before anything is
/// written.
pub fn recover_journal(directory: &Path) -> Result<RecoveryOutcome> {
    recover_journal_with_progress(directory, &mut DiscardedProgress)
}

/// Recovers exactly like [`recover_journal`], reporting the candidate
/// scan and the consistency probe to `observer`.
pub fn recover_journal_with_progress(
    directory: &Path,
    observer: &mut dyn ProgressObserver,
) -> Result<RecoveryOutcome> {
    // Deliberate deviation from Java, which recovers locklessly: hold the
    // exclusive repository lock for the whole recovery, so a running AEM
    // instance can never append to a journal this function is about to
    // replace. Strictly safer — the tool fails fast instead of losing the
    // instance's subsequent commits.
    let _repository_lock = RepositoryLock::acquire(directory)?;

    // A read-only view of every archive, opened without needing a
    // resolvable journal.
    let archives = crate::store::open_all_archives_with_progress(directory, observer)?;

    let mut candidates: Vec<Candidate> = Vec::new();
    let provider = crate::store::ArchiveSet::new(archives);

    // Every statement in this loop is infallible — an unreadable segment
    // is skipped, not raised — so the step is closed by the single
    // `step_ended` after it.
    observer.step_began(
        &Step::new("scanning segments for super-roots", WorkUnit::Segments)
            .with_total(crate::progress::count(provider.segment_identifier_count())),
    );
    let mut scanned_segments = 0usize;
    // Distinct segments, not every archive occurrence. This scan used to
    // dedupe with a `HashSet<RecordIdentifier>` over every node record in
    // every data segment — live and garbage alike, the largest set in the
    // codebase. It was never needed at that grain: a segment's record table
    // is rejected unless it is strictly ascending by record number, so one
    // segment cannot yield a duplicate record. Only a segment served by two
    // archives could, and that is settled here for free.
    for (scanned, segment_identifier) in provider.distinct_segment_identifiers().enumerate() {
        observer.step_advanced(crate::progress::count(scanned));
        scanned_segments = scanned + 1;
        if segment_identifier.is_bulk_segment() {
            continue;
        }
        let Ok(view) = provider.segment(segment_identifier) else {
            continue;
        };
        // A segment without a parseable info timestamp is skipped whole,
        // as in Java ("No timestamp found in segment ..."); Java aborts
        // the entire run on malformed info JSON, which is folded into the
        // same skip here — strictly safer, recovery proceeds on the rest.
        let Some(timestamp) = read_segment_info_timestamp(&provider, segment_identifier) else {
            continue;
        };
        // Every NODE record of the segment is a potential super-root.
        let node_records: Vec<u32> = view
            .structure
            .record_table()
            .iter()
            .filter(|entry| entry.record_type() == Some(crate::segment::record::RecordType::Node))
            .map(|entry| entry.record_number)
            .collect();
        for record_number in node_records {
            let record = RecordIdentifier::new(segment_identifier, record_number);
            if is_super_root(&provider, record) {
                candidates.push(Candidate {
                    record,
                    timestamp_milliseconds: timestamp,
                });
            }
        }
    }
    observer.step_advanced(crate::progress::count(scanned_segments));
    observer.step_ended();
    let candidates_examined = candidates.len();

    // Order candidates newest first — the exact reverse of Oak's
    // ascending sort (timestamp, then segment UUID compared as signed
    // halves like Java's UUID.compareTo, then record number), which Oak
    // then iterates backwards from the newest.
    candidates.sort_by(|first, second| {
        second
            .timestamp_milliseconds
            .cmp(&first.timestamp_milliseconds)
            .then_with(|| signed_uuid_key(second.record).cmp(&signed_uuid_key(first.record)))
            .then_with(|| second.record.record_number.cmp(&first.record.record_number))
    });

    // Take the newest fully consistent candidate. Consistency is checked
    // lazily in newest-first order — with Oak's shared corrupt-path
    // memory: a path found corrupt at a newer candidate is re-probed at
    // every older one and must exist and pass a shallow check there, so
    // a candidate that merely *predates* the corrupted node is rejected
    // exactly as Java rejects it. Matching Oak, only the inconsistent
    // newest suffix is dropped: the surviving older candidates are
    // written below the verified head, unverified, as the fallback lines
    // head resolution skips to when a segment of the last line later
    // goes missing.
    let mut corrupt_memory: Vec<String> = Vec::new();
    observer.step_began(
        &Step::new("probing candidates for consistency", WorkUnit::Revisions)
            .with_total(crate::progress::count(candidates_examined)),
    );
    let consistent_position = candidates
        .iter()
        .enumerate()
        .position(|(probed, candidate)| {
            observer.step_advanced(crate::progress::count(probed));
            is_fully_consistent(&provider, candidate.record, &mut corrupt_memory)
        });
    // Every candidate up to and including the accepted one was probed;
    // without a match, all of them were.
    observer.step_advanced(crate::progress::count(
        consistent_position.map_or(candidates_examined, |position| position + 1),
    ));
    observer.step_ended();
    let consistent_position = consistent_position.ok_or_else(|| Error::InvalidFormat {
        details: format!(
            "no consistent super-root found among {candidates_examined} candidates in {}",
            directory.display()
        ),
    })?;
    let survivors = &candidates[consistent_position..];
    let recovered_head = survivors[0].record;

    let previous_journal_backup = back_up_existing_journal(directory)?;
    // The backup is a copy and the new journal arrives by atomic rename,
    // so `journal.log` exists — old or new — at every moment. On failure,
    // the copy is removed only when the recovered journal was *not*
    // installed (the temporary file still exists); after a successful
    // rename the copy is the sole pre-recovery journal and must survive.
    if let Err(error) = write_recovered_journal(directory, survivors) {
        let temporary_path = directory.join("journal.log.recovered");
        if temporary_path.exists() {
            let _ = std::fs::remove_file(&temporary_path);
            if let Some(backup_path) = &previous_journal_backup {
                let _ = std::fs::remove_file(backup_path);
            }
        }
        return Err(error);
    }

    Ok(RecoveryOutcome {
        recovered_head,
        candidates_examined,
        previous_journal_backup,
    })
}

/// Whether a node looks like a super-root: Oak's recovery keeps a
/// candidate iff it has *both* a `root` and a `checkpoints` child —
/// requiring only `root` would let ordinary content nodes (every page
/// with a child named `root`) flood the candidate list and even become
/// the recovered head.
fn is_super_root(provider: &dyn SegmentProvider, record: RecordIdentifier) -> bool {
    let node = NodeState::new(provider, record);
    matches!(node.child_node("root"), Ok(Some(_)))
        && matches!(node.child_node("checkpoints"), Ok(Some(_)))
}

/// The segment UUID as Java's `UUID.compareTo` orders it: most then least
/// significant half, compared as *signed* 64-bit values.
fn signed_uuid_key(record: RecordIdentifier) -> (i64, i64) {
    (
        record.segment.most_significant_bits as i64,
        record.segment.least_significant_bits as i64,
    )
}

/// Byte budget for the recovery walk's shared visited memo.
///
/// One candidate probe walks the head and every checkpoint; the memo exists
/// so a subtree those trees share is verified once. A miss re-walks, which
/// is time rather than a different answer: entries go in only once a subtree
/// has verified, and nothing else consults them.
const RECOVERY_VISITED_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// Whether a candidate super-root passes Oak's consistency gate: the
/// tree under its `root` child and the tree under every checkpoint's
/// `root` snapshot must traverse without a missing segment or malformed
/// record, and a checkpoint *without* a root snapshot fails the revision
/// (Java's null `retrieve`). Checkpoint metadata is not verified — Java
/// never reads it here, and being stricter could reject a head a real
/// AEM instance serves fine. Inline binaries have every block resolved
/// and read (without being materialized) so a head whose binary bulk
/// segments are missing fails the gate.
///
/// `corrupt_memory` is Java's shared `corruptedPaths` set — flat and
/// unkeyed: every remembered path must exist and pass a shallow check
/// under the head's content root *and* under every checkpoint root this
/// candidate has (a checkpoint the candidate lacks is simply never
/// probed), or the candidate is rejected without a full traversal.
///
/// Deliberate deviation from Java: errors while *enumerating* a
/// candidate's checkpoints reject only that candidate, where Java aborts
/// the whole run with the journal untouched. Nothing is lost — the
/// original journal survives as the `.bak` copy — and recovery still
/// finds the newest candidate whose trees all verify.
fn is_fully_consistent(
    provider: &dyn SegmentProvider,
    record: RecordIdentifier,
    corrupt_memory: &mut Vec<String>,
) -> bool {
    let super_root = NodeState::new(provider, record);
    // Bounded rather than exact: this memo only skips re-walking a subtree
    // two trees share. Cycle detection is the separate `ancestors` set, so
    // an eviction costs a re-walk and never a wrong verdict. Unbounded it
    // held every node reachable from the candidate.
    let mut visited = BoundedCache::new(RECOVERY_VISITED_BUDGET_BYTES);

    // Java's per-tree interleave, order included: the head is probed and
    // walked first — recording its corrupt path before any checkpoint is
    // even resolved — then each checkpoint in stored order is resolved
    // (a missing root snapshot rejects, Java's null retrieve), probed,
    // and walked. The interleave matters: a rejection must not skip the
    // recording a preceding tree's walk would have made, because older
    // candidates consume that memory.
    let Ok(Some(content_root)) = super_root.child_node("root") else {
        return false;
    };
    if !probe_corrupt_paths(provider, &content_root, corrupt_memory) {
        return false;
    }
    if let Err(corrupt_path) = verify_tree(provider, content_root.record_identifier(), &mut visited)
    {
        remember_corrupt(corrupt_memory, corrupt_path);
        return false;
    }

    let Ok(Some(checkpoints)) = super_root.child_node("checkpoints") else {
        return false;
    };
    let Ok(checkpoint_entries) = checkpoints.child_node_entries() else {
        return false;
    };
    for (_, checkpoint) in checkpoint_entries {
        let Ok(Some(snapshot_root)) = checkpoint.child_node("root") else {
            return false;
        };
        if !probe_corrupt_paths(provider, &snapshot_root, corrupt_memory) {
            return false;
        }
        if let Err(corrupt_path) =
            verify_tree(provider, snapshot_root.record_identifier(), &mut visited)
        {
            remember_corrupt(corrupt_memory, corrupt_path);
            return false;
        }
    }
    true
}

/// Re-probes the remembered corrupt paths under one tree root, in
/// insertion order: each must exist and pass a shallow check — a missing
/// path counts as still corrupt (this candidate may simply predate the
/// corrupted node), exactly like Java.
fn probe_corrupt_paths(
    provider: &dyn SegmentProvider,
    tree_root: &NodeState<'_>,
    corrupt_memory: &[String],
) -> bool {
    for corrupt_path in corrupt_memory {
        let Ok(Some(corrupt_node)) = resolve_descendant(tree_root, corrupt_path) else {
            return false;
        };
        if check_node_shallow(provider, corrupt_node.record_identifier()).is_err() {
            return false;
        }
    }
    true
}

/// Resolves a relative path (empty = the node itself) under a node.
fn resolve_descendant<'provider>(
    node: &NodeState<'provider>,
    relative_path: &str,
) -> Result<Option<NodeState<'provider>>> {
    let mut current = *node;
    for name in relative_path
        .split('/')
        .filter(|segment| !segment.is_empty())
    {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// Records a newly found corrupt path once.
fn remember_corrupt(corrupt_memory: &mut Vec<String>, corrupt_path: String) {
    if !corrupt_memory.contains(&corrupt_path) {
        corrupt_memory.push(corrupt_path);
    }
}

/// Checks one node without recursing: every property is decoded and
/// every inline binary read — the recovery gate always verifies
/// binaries.
fn check_node_shallow(provider: &dyn SegmentProvider, record: RecordIdentifier) -> Result<()> {
    let node = NodeState::new(provider, record);
    for property in node.properties()? {
        match &property.values {
            crate::content::node::PropertyValues::Single(value) => {
                verify_inline_binary(provider, value)?;
            }
            crate::content::node::PropertyValues::Multiple(values) => {
                for value in values {
                    verify_inline_binary(provider, value)?;
                }
            }
        }
    }
    Ok(())
}

/// Fully traverses one tree, sharing the visited set across trees so
/// records the checkpoints share with the live content verify once. On
/// failure returns the relative path of the corrupt node (empty for the
/// tree root itself).
///
/// The walk carries its own stack on the heap and imposes no depth limit:
/// depth is a property of the candidate store, not something this code may
/// choose. Termination on a self-referential record graph is the exact
/// `ancestors` set.
fn verify_tree(
    provider: &dyn SegmentProvider,
    record: RecordIdentifier,
    visited: &mut BoundedCache<RecordIdentifier, ()>,
) -> std::result::Result<(), String> {
    /// One suspended node: children still to descend into, and the path
    /// length to restore when it completes.
    struct Frame {
        record: RecordIdentifier,
        pending_children: Vec<(String, RecordIdentifier)>,
        parent_path_length: usize,
    }

    /// Checks one node and enumerates its children. `None` means the memo
    /// already holds it, so the subtree needs no walking.
    fn open(
        provider: &dyn SegmentProvider,
        record: RecordIdentifier,
        visited: &BoundedCache<RecordIdentifier, ()>,
        ancestors: &mut HashSet<RecordIdentifier>,
        path: &str,
    ) -> std::result::Result<Option<Vec<(String, RecordIdentifier)>>, String> {
        // A node inside its own subtree is corruption and fails the gate;
        // meeting an already-completed node again is ordinary shared-
        // subtree deduplication and verifies for free. Tested before the
        // memo so a cycle can never be served from it.
        if ancestors.contains(&record) {
            return Err(path.to_owned());
        }
        if visited.get(&record).is_some() {
            return Ok(None);
        }
        ancestors.insert(record);
        check_node_shallow(provider, record).map_err(|_| path.to_owned())?;
        let node = NodeState::new(provider, record);
        let children = node.child_node_entries().map_err(|_| path.to_owned())?;
        let mut children: Vec<(String, RecordIdentifier)> = children
            .into_iter()
            .map(|(name, child)| (name, child.record_identifier()))
            .collect();
        // Reversed so `pop` yields enumeration order.
        children.reverse();
        Ok(Some(children))
    }

    let mut ancestors = HashSet::new();
    let mut path = String::new();
    let Some(children) = open(provider, record, visited, &mut ancestors, &path)? else {
        return Ok(());
    };
    let mut stack = vec![Frame {
        record,
        pending_children: children,
        parent_path_length: 0,
    }];

    loop {
        let next = stack
            .last_mut()
            .expect("the loop returns before the stack empties")
            .pending_children
            .pop();
        if let Some((name, child)) = next {
            let parent_path_length = path.len();
            path.push('/');
            path.push_str(&name);
            match open(provider, child, visited, &mut ancestors, &path)? {
                Some(children) => stack.push(Frame {
                    record: child,
                    pending_children: children,
                    parent_path_length,
                }),
                None => path.truncate(parent_path_length),
            }
            continue;
        }
        let finished = stack.pop().expect("a frame was just inspected");
        ancestors.remove(&finished.record);
        // On completion, never on entry. Entering a node into the memo before
        // its subtree is verified caches failed and cyclic subtrees, and makes
        // a shared root the *oldest* entry of its own subtree, so any subtree
        // larger than the budget guarantees the sibling misses.
        visited.insert(finished.record, ());
        if stack.is_empty() {
            return Ok(());
        }
        path.truncate(finished.parent_path_length);
    }
}

/// Resolves and reads every block of an inline binary without holding the
/// content in memory; external binaries have no local content.
fn verify_inline_binary(
    provider: &dyn SegmentProvider,
    value: &crate::content::property::PropertyValue,
) -> Result<()> {
    use crate::content::property::PropertyValue;
    use crate::content::value::BinaryValue;
    if let PropertyValue::Binary(BinaryValue::Inline {
        record_identifier, ..
    }) = value
    {
        crate::content::value::verify_binary_content(provider, *record_identifier)?;
    }
    Ok(())
}

/// Reads the `"t"` timestamp from a segment's info record (record 0).
fn read_segment_info_timestamp(
    provider: &dyn SegmentProvider,
    segment: SegmentIdentifier,
) -> Option<i64> {
    let view = provider.segment(segment).ok()?;
    let first_record = view.structure.record_table().first()?.record_number;
    let info =
        crate::content::value::read_string(provider, RecordIdentifier::new(segment, first_record))
            .ok()?;
    parse_info_timestamp(&info)
}

/// Extracts the `"t":<number>` value from a segment-info JSON string.
fn parse_info_timestamp(info: &str) -> Option<i64> {
    let marker = "\"t\":";
    let start = info.find(marker)? + marker.len();
    let rest = &info[start..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Backs up an existing `journal.log` to the first free
/// `journal.log.bak.NNN` (000–999). Deliberate deviation from Java's
/// plain rename: the backup is a *copy*, so `journal.log` never
/// disappears — the recovered journal later replaces it atomically.
fn back_up_existing_journal(directory: &Path) -> Result<Option<std::path::PathBuf>> {
    let journal_path = directory.join("journal.log");
    if !journal_path.exists() {
        return Ok(None);
    }
    for counter in 0..1000 {
        let backup = directory.join(format!("journal.log.bak.{counter:03}"));
        if !backup.exists() {
            std::fs::copy(&journal_path, &backup)?;
            std::fs::File::open(&backup)?.sync_all()?;
            return Ok(Some(backup));
        }
    }
    Err(Error::InvalidFormat {
        details: "all journal backup names (000-999) are taken".to_owned(),
    })
}

/// Writes the recovered journal: the surviving candidates oldest first,
/// each line `<uuid>:<record-number> root <segment-info-timestamp>`,
/// exactly as Oak's recovery writes them. The file is assembled beside
/// the journal, fsynced, renamed into place atomically, and the
/// directory entry is fsynced — Java fsyncs nothing here; a crash can
/// therefore never leave the store without a journal.
fn write_recovered_journal(directory: &Path, survivors: &[Candidate]) -> Result<()> {
    let temporary_path = directory.join("journal.log.recovered");
    {
        let mut file = std::fs::File::create(&temporary_path)?;
        for candidate in survivors.iter().rev() {
            let line = format!(
                "{}:{} root {}\n",
                candidate.record.segment,
                candidate.record.record_number as i32,
                candidate.timestamp_milliseconds
            );
            file.write_all(line.as_bytes())?;
        }
        file.sync_all()?;
    }
    std::fs::rename(&temporary_path, directory.join("journal.log"))?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{backup, parse_info_timestamp, recover_journal, restore};
    use crate::content::node::PropertyValues;
    use crate::content::property::PropertyValue;
    use crate::store::Repository;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-backup-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Populates a store with `/content` (a title property, two children)
    /// and one checkpoint.
    fn populate(directory: &std::path::Path) {
        let store = WritableRepository::open(directory).expect("bootstrap");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let alpha = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("alpha");
        let beta = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("beta");
        let title = writer.write_string("Backup Source").expect("value");
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("alpha".to_owned(), alpha),
                    ("beta".to_owned(), beta),
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
        create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
        store.close().expect("close");
    }

    fn assert_content(directory: &std::path::Path, expected_title: &str) {
        let repository = Repository::open(directory).expect("reader");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        assert_eq!(content.child_node_count().expect("count"), 2);
        assert_eq!(
            content
                .property("title")
                .expect("read")
                .expect("present")
                .values,
            PropertyValues::Single(PropertyValue::String(expected_title.to_owned()))
        );
    }

    /// Writes one `/content` revision whose child count is `child_count`,
    /// then a checkpoint so it is a full super-root candidate.
    fn write_revision_with_children(directory: &std::path::Path, child_count: usize) {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let mut children = Vec::new();
        for index in 0..child_count {
            let child = writer
                .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
                .expect("child");
            children.push((format!("child-{index}"), child));
        }
        let child_structure = if children.is_empty() {
            ChildNodesToWrite::Zero
        } else {
            ChildNodesToWrite::Many(children)
        };
        let content = writer
            .write_node(Some("nt:unstructured"), &[], &child_structure, &[])
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
        create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
        store.close().expect("close");
    }

    #[test]
    fn recovery_picks_the_newest_revision_even_when_it_has_fewer_nodes() {
        // A recovery must return the newest committed state, never an older
        // one that happens to reach more content — otherwise a commit that
        // deleted content would be silently reverted.
        let directory = TestDirectory::new("recover-newest");
        write_revision_with_children(&directory.path, 8); // older, larger
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_revision_with_children(&directory.path, 2); // newer, smaller

        let newest_head = {
            let repository = Repository::open(&directory.path).expect("reader");
            repository.head_record_identifier()
        };

        std::fs::remove_file(directory.path.join("journal.log")).expect("remove journal");
        let outcome = recover_journal(&directory.path).expect("recover");
        assert_eq!(
            outcome.recovered_head, newest_head,
            "recovery returns the newest revision, not the larger older one"
        );
        // The recovered content has the newer, smaller child set.
        let repository = Repository::open(&directory.path).expect("reader");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        assert_eq!(content.child_node_count().expect("count"), 2);
    }

    #[test]
    fn backup_copies_content_and_checkpoints() {
        let source = TestDirectory::new("backup-source");
        let target = TestDirectory::new("backup-target");
        populate(&source.path);

        backup(&source.path, &target.path).expect("backup");

        assert_content(&target.path, "Backup Source");
        let repository = Repository::open(&target.path).expect("reader");
        assert_eq!(
            repository.checkpoints().expect("checkpoints").len(),
            1,
            "checkpoints are copied with the head"
        );
        // The source is untouched and still readable.
        assert_content(&source.path, "Backup Source");
    }

    /// A binary long enough that its blocks land in a bulk segment, which
    /// is the only shape that distinguishes copying from referencing.
    ///
    /// Blocks are 4 KiB and a full 256 KiB run becomes a bulk segment, so
    /// this is comfortably over that threshold.
    const BULK_BINARY_BYTES: usize = 1024 * 1024;

    fn bulk_binary_content() -> Vec<u8> {
        (0..BULK_BINARY_BYTES)
            .map(|index| (index % 251) as u8)
            .collect()
    }

    /// Writes a store whose head carries one binary big enough to occupy a
    /// bulk segment.
    fn populate_with_bulk_binary(directory: &std::path::Path) -> Vec<u8> {
        let content = bulk_binary_content();
        let store = WritableRepository::open(directory).expect("open the source");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let binary = writer
            .write_binary_content(&content)
            .expect("write the binary");
        let resource = writer
            .write_node(
                Some("nt:resource"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "jcr:data".to_owned(),
                    property_type: crate::content::property::PropertyType::Binary,
                    values: PropertyValuesToWrite::Single(binary),
                }],
            )
            .expect("write the resource");
        let root = writer
            .write_node(
                Some("rep:root"),
                &[],
                &ChildNodesToWrite::One {
                    name: "file".to_owned(),
                    node: resource,
                },
                &[],
            )
            .expect("write the root");
        let checkpoints = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("write the checkpoints container");
        let super_root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("root".to_owned(), root),
                    ("checkpoints".to_owned(), checkpoints),
                ]),
                &[],
            )
            .expect("write the super-root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, super_root), "advance the head");
        store.close().expect("close");
        content
    }

    #[test]
    fn a_backup_carries_binary_content_that_lived_in_a_bulk_segment() {
        // Compaction shares bulk-segment blocks by reference rather than
        // copying them, because within one store a reference from the new
        // generation is exactly what keeps a bulk segment alive. Backup
        // used the same copy, and a reference into the *source's* bulk
        // segments resolves to nothing in the target — so the backup came
        // out holding the whole content tree and none of the binary.
        //
        // Nothing caught it, because the damage is invisible to everything
        // short of reading the bytes back: the target opens, its node and
        // property structure is complete, and a consistency check that
        // only resolves binary records rather than reading them passes.
        let source = TestDirectory::new("backup-bulk-source");
        let target = TestDirectory::new("backup-bulk-target");
        let content = populate_with_bulk_binary(&source.path);

        backup(&source.path, &target.path).expect("backup");

        // Read the binary back out of the target *alone*. Opening the
        // source anywhere in this assertion would let the missing blocks
        // resolve through it and hide the defect.
        let repository = Repository::open(&target.path).expect("open the backup");
        let resource = repository
            .content_root()
            .expect("content root")
            .child_node("file")
            .expect("read")
            .expect("the file node is present");
        let data = resource
            .property("jcr:data")
            .expect("read")
            .expect("jcr:data is present");
        let PropertyValues::Single(PropertyValue::Binary(binary)) = &data.values else {
            panic!("jcr:data did not decode as a single binary: {data:?}");
        };
        let crate::content::value::BinaryValue::Inline {
            record_identifier, ..
        } = binary
        else {
            panic!("expected an inline binary, got {binary:?}");
        };
        let copied = crate::content::value::read_binary_content(&repository, *record_identifier)
            .expect("the backup holds every block of the binary");
        assert_eq!(
            copied.len(),
            content.len(),
            "the backup holds the whole binary, not a prefix of it"
        );
        assert!(
            copied == content,
            "the binary in the backup is byte-identical to the source's"
        );
    }

    #[test]
    fn restore_overwrites_the_target_head() {
        let backup_store = TestDirectory::new("restore-backup");
        let target = TestDirectory::new("restore-target");
        populate(&backup_store.path);

        // The target starts as a different, bootstrapped store.
        {
            let store = WritableRepository::open(&target.path).expect("bootstrap target");
            store.close().expect("close");
        }
        restore(&backup_store.path, &target.path).expect("restore");
        assert_content(&target.path, "Backup Source");
    }

    #[test]
    fn recover_journal_rebuilds_a_deleted_journal() {
        let directory = TestDirectory::new("recover");
        populate(&directory.path);

        // Delete the journal, then recover it from the segments.
        std::fs::remove_file(directory.path.join("journal.log")).expect("remove journal");
        let outcome = recover_journal(&directory.path).expect("recover");
        assert!(outcome.candidates_examined >= 1);

        // The store reads back with content intact.
        assert_content(&directory.path, "Backup Source");
    }

    #[test]
    fn recover_journal_backs_up_a_corrupt_journal() {
        let directory = TestDirectory::new("recover-backup");
        populate(&directory.path);

        std::fs::write(
            directory.path.join("journal.log"),
            "garbage-with-no-space\n",
        )
        .expect("corrupt journal");
        let outcome = recover_journal(&directory.path).expect("recover");
        assert!(
            outcome.previous_journal_backup.is_some(),
            "the corrupt journal is backed up"
        );
        assert!(directory.path.join("journal.log.bak.000").exists());
        assert_content(&directory.path, "Backup Source");
    }

    #[test]
    fn recover_journal_requires_the_repository_lock() {
        let directory = TestDirectory::new("recover-locked");
        populate(&directory.path);
        let held_lock =
            crate::writer::repository_lock::RepositoryLock::acquire(&directory.path).expect("lock");
        assert!(
            recover_journal(&directory.path).is_err(),
            "recovery must refuse to run while another process holds repo.lock"
        );
        drop(held_lock);
        recover_journal(&directory.path).expect("recovery succeeds once the lock is free");
    }

    #[test]
    fn backup_stamps_the_source_head_generation_verbatim() {
        let source = TestDirectory::new("backup-generation-source");
        let target = TestDirectory::new("backup-generation-target");
        populate(&source.path);
        // Advance the source's generation so the stamp is distinguishable
        // from a fresh target's (0, 0, false).
        {
            let mut store = WritableRepository::open(&source.path).expect("open");
            crate::writer::compaction::compact(
                &mut store,
                crate::writer::compaction::CompactionKind::Full,
            )
            .expect("compact");
            store.close().expect("close");
        }
        let source_head_generation = {
            let repository = Repository::open(&source.path).expect("reader");
            let head_segment = repository.head_record_identifier().segment;
            repository
                .archives()
                .iter()
                .find_map(|archive| archive.index_entry(head_segment))
                .map(|entry| (entry.generation, entry.full_generation, entry.is_compacted))
                .expect("head segment is indexed")
        };
        assert_ne!(
            source_head_generation.0, 0,
            "compaction advanced the source generation"
        );

        backup(&source.path, &target.path).expect("backup");

        let repository = Repository::open(&target.path).expect("target reader");
        let head_segment = repository.head_record_identifier().segment;
        let stamped = repository
            .archives()
            .iter()
            .find_map(|archive| archive.index_entry(head_segment))
            .map(|entry| (entry.generation, entry.full_generation, entry.is_compacted))
            .expect("target head segment is indexed");
        assert_eq!(
            stamped, source_head_generation,
            "the source head's generation triple is stamped verbatim"
        );
    }

    #[test]
    fn parses_segment_info_timestamps() {
        assert_eq!(
            parse_info_timestamp("{\"wid\":\"froe\",\"sno\":3,\"t\":1700000000000}"),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_info_timestamp("{\"wid\":\"x\"}"), None);
    }
}
