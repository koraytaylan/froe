//! Node history: how one node changed across journal revisions.
//!
//! `node_history` walks the journal newest first and, for each revision,
//! navigates to a path and captures the node's record identifier. Callers
//! see how a node's state moved over time — and, because unchanged
//! subtrees share records, repeated identifiers mark revisions where the
//! node did not change.

use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::error::Result;
use crate::journal::read_journal;
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::record::RecordIdentifier;
use crate::store::{ArchiveSet, open_all_archives_with_progress};

/// One revision's view of a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHistoryEntry {
    /// The revision string.
    pub revision: String,
    /// The timestamp in milliseconds, or -1 when unknown.
    pub timestamp_milliseconds: i64,
    /// The node's record identifier at this revision, or `None` when the
    /// path did not exist.
    pub record: Option<RecordIdentifier>,
}

/// Returns the history of the node at `path` across journal revisions,
/// newest first. `path` is relative to the content root; a leading
/// `checkpoints` segment addresses the super-root's checkpoints.
pub fn node_history(directory: &std::path::Path, path: &str) -> Result<Vec<NodeHistoryEntry>> {
    node_history_with_progress(directory, path, &mut DiscardedProgress)
}

/// Traces exactly like [`node_history`], reporting the archive scan and
/// the revision walk to `observer`.
pub fn node_history_with_progress(
    directory: &std::path::Path,
    path: &str,
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<NodeHistoryEntry>> {
    let archives = open_all_archives_with_progress(directory, observer)?;
    let provider = ArchiveSet::new(archives);
    // An unreadable journal is a loud failure, not an empty history.
    let journal_entries = read_journal(&directory.join("journal.log"))?;

    let mut history = Vec::new();
    observer.step_began(
        &Step::new("tracing revisions", WorkUnit::Revisions)
            .with_total(crate::progress::count(journal_entries.len())),
    );
    for (traced, entry) in journal_entries.iter().enumerate() {
        observer.step_advanced(crate::progress::count(traced));
        let Some(head) = entry.record_identifier() else {
            continue;
        };
        if provider.segment(head.segment).is_err() {
            continue;
        }
        let record = match navigate(&provider, head, path) {
            Ok(node) => node.map(|node| node.record_identifier()),
            Err(error) => {
                observer.step_ended();
                return Err(error);
            }
        };
        history.push(NodeHistoryEntry {
            revision: entry.revision_text.clone(),
            timestamp_milliseconds: entry.timestamp_milliseconds,
            record,
        });
    }
    observer.step_advanced(crate::progress::count(journal_entries.len()));
    observer.step_ended();
    Ok(history)
}

/// Navigates from a super-root to `path` under the content root.
fn navigate<'provider>(
    provider: &'provider ArchiveSet,
    head: RecordIdentifier,
    path: &str,
) -> Result<Option<NodeState<'provider>>> {
    let super_root = NodeState::new(provider, head);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    // A path starting with "checkpoints" walks from the super-root, whose
    // children are `root` and `checkpoints` — the first segment names the
    // checkpoints container itself, so nothing is skipped.
    let (mut current, rest) = if segments.first() == Some(&"checkpoints") {
        (super_root, &segments[..])
    } else {
        match super_root.child_node("root")? {
            Some(root) => (root, &segments[..]),
            None => return Ok(None),
        }
    };
    for name in rest {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

#[cfg(test)]
mod tests {
    use super::node_history;
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-history-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_revision(directory: &std::path::Path, title: &str) {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let value = writer.write_string(title).expect("value");
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "title".to_owned(),
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
        store.close().expect("close");
    }

    #[test]
    fn tracks_a_node_across_revisions() {
        let directory = TestDirectory::new("track");
        write_revision(&directory.path, "first");
        write_revision(&directory.path, "second");
        write_revision(&directory.path, "third");

        let history = node_history(&directory.path, "/content").expect("history");
        // Three content revisions plus the empty bootstrap head (which has
        // no /content).
        assert!(history.len() >= 3);
        assert!(
            history[0].record.is_some(),
            "the newest revision has the content node"
        );
        // The content record changes across the content revisions.
        let distinct: std::collections::HashSet<_> =
            history.iter().filter_map(|entry| entry.record).collect();
        assert!(
            distinct.len() >= 3,
            "each revision has a distinct content record"
        );
    }

    #[test]
    fn missing_paths_are_recorded_as_absent() {
        let directory = TestDirectory::new("absent");
        write_revision(&directory.path, "only");
        let history = node_history(&directory.path, "/does-not-exist").expect("history");
        assert!(history.iter().all(|entry| entry.record.is_none()));
    }
}
