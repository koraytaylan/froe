//! Deciding whether a candidate revision is whole: a bounded walk of its
//! tree that remembers the corrupt paths it has already proven, so a
//! shared subtree is not re-verified per candidate.

use super::{BoundedCache, HashSet, NodeState, RecordIdentifier, Result, SegmentProvider};

/// Byte budget for the recovery walk's shared visited memo.
///
/// One candidate probe walks the head and every checkpoint; the memo exists
/// so a subtree those trees share is verified once. A miss re-walks, which
/// is time rather than a different answer: entries go in only once a subtree
/// has verified, and nothing else consults them.
pub(crate) const RECOVERY_VISITED_BUDGET_BYTES: usize = 128 * 1024 * 1024;

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
pub(crate) fn is_fully_consistent(
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
    if !every_corrupt_path_verifies(provider, &content_root, corrupt_memory) {
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
        if !every_corrupt_path_verifies(provider, &snapshot_root, corrupt_memory) {
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
pub(crate) fn every_corrupt_path_verifies(
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
pub(crate) fn resolve_descendant<'provider>(
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
pub(crate) fn remember_corrupt(corrupt_memory: &mut Vec<String>, corrupt_path: String) {
    if !corrupt_memory.contains(&corrupt_path) {
        corrupt_memory.push(corrupt_path);
    }
}

/// Checks one node without recursing: every property is decoded and
/// every inline binary read — the recovery gate always verifies
/// binaries.
pub(crate) fn check_node_shallow(
    provider: &dyn SegmentProvider,
    record: RecordIdentifier,
) -> Result<()> {
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
pub(crate) fn verify_tree(
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
pub(crate) fn verify_inline_binary(
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

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::writer::backup::recover::recover_journal;
    use crate::writer::backup::test_support::{TestDirectory, write_revision_with_children};

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
}
