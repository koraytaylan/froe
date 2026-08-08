//! Consistency checking: finding the newest fully traversable revision.
//!
//! `check` walks the journal from the newest revision backwards. For each
//! revision it resolves the head record, then verifies that every
//! requested content path — and the whole subtree beneath it — can be
//! read without a missing segment or malformed record, optionally
//! materializing binary content. The newest revision that passes for a
//! path is that path's good revision; the newest revision good for every
//! path is the overall good revision, the one a repository should be
//! rewound to.

use std::collections::HashSet;

use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::error::Result;
use crate::journal::read_journal;
use crate::segment::record::RecordIdentifier;
use crate::store::{ArchiveSet, open_all_archives};

/// The result of checking one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionCheck {
    /// The revision string from the journal.
    pub revision: String,
    /// The paths that were fully consistent at this revision.
    pub consistent_paths: Vec<String>,
    /// The paths that were inconsistent, with a short reason.
    pub inconsistent_paths: Vec<(String, String)>,
}

impl RevisionCheck {
    /// Whether every checked path was consistent at this revision.
    #[must_use]
    pub fn is_fully_consistent(&self) -> bool {
        self.inconsistent_paths.is_empty()
    }
}

/// The overall consistency report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyReport {
    /// Per-revision results, newest first, up to the first fully
    /// consistent revision.
    pub revisions: Vec<RevisionCheck>,
    /// The newest revision good for every requested path, when one
    /// exists.
    pub good_revision: Option<String>,
}

/// Checks the store at `directory`, verifying the given content paths at
/// each journal revision (newest first). `check_binaries` materializes
/// binary values instead of only resolving their records. At most
/// `revision_limit` revisions are examined.
pub fn check_consistency(
    directory: &std::path::Path,
    filter_paths: &[String],
    check_binaries: bool,
    revision_limit: usize,
) -> Result<ConsistencyReport> {
    let archives = open_all_archives(directory)?;
    let provider = ArchiveSet::new(archives);
    let journal_entries = read_journal(&directory.join("journal.log")).unwrap_or_default();

    let paths: Vec<String> = if filter_paths.is_empty() {
        vec!["/".to_owned()]
    } else {
        filter_paths.to_vec()
    };

    let mut revisions = Vec::new();
    let mut good_revision = None;
    for entry in journal_entries.iter().take(revision_limit) {
        let Some(head) = entry.record_identifier() else {
            continue;
        };
        if !provider_contains(&provider, head) {
            continue;
        }
        let check = check_revision(
            &provider,
            &entry.revision_text,
            head,
            &paths,
            check_binaries,
        );
        let fully_consistent = check.is_fully_consistent();
        revisions.push(check);
        if fully_consistent {
            good_revision = Some(entry.revision_text.clone());
            break;
        }
    }

    Ok(ConsistencyReport {
        revisions,
        good_revision,
    })
}

/// Whether the provider can resolve the head's segment.
fn provider_contains(provider: &ArchiveSet, head: RecordIdentifier) -> bool {
    provider.segment(head.segment).is_ok()
}

/// Checks every path at one revision.
fn check_revision(
    provider: &dyn SegmentProvider,
    revision: &str,
    head: RecordIdentifier,
    paths: &[String],
    check_binaries: bool,
) -> RevisionCheck {
    let super_root = NodeState::new(provider, head);
    let content_root = super_root.child_node("root").ok().flatten();

    let mut consistent_paths = Vec::new();
    let mut inconsistent_paths = Vec::new();
    for path in paths {
        match resolve_path(&super_root, content_root.as_ref(), path) {
            Ok(Some(node)) => {
                match verify_subtree(provider, node.record_identifier(), check_binaries) {
                    Ok(()) => consistent_paths.push(path.clone()),
                    Err(reason) => inconsistent_paths.push((path.clone(), reason)),
                }
            }
            Ok(None) => {
                inconsistent_paths.push((path.clone(), "path does not exist".to_owned()));
            }
            Err(error) => inconsistent_paths.push((path.clone(), error.to_string())),
        }
    }
    RevisionCheck {
        revision: revision.to_owned(),
        consistent_paths,
        inconsistent_paths,
    }
}

/// Resolves a content path. `/` and paths under it are relative to the
/// content root; the special prefix `/checkpoints` addresses the
/// super-root's checkpoints instead.
fn resolve_path<'provider>(
    super_root: &NodeState<'provider>,
    content_root: Option<&NodeState<'provider>>,
    path: &str,
) -> Result<Option<NodeState<'provider>>> {
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    // A path starting with "checkpoints" walks from the super-root.
    let (mut current, rest) = if segments.first() == Some(&"checkpoints") {
        (*super_root, &segments[1..])
    } else {
        match content_root {
            Some(root) => (*root, &segments[..]),
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

/// Traverses a subtree, resolving every node, property, and — when asked
/// — binary content. Returns a short reason string on the first failure.
fn verify_subtree(
    provider: &dyn SegmentProvider,
    root: RecordIdentifier,
    check_binaries: bool,
) -> std::result::Result<(), String> {
    fn walk(
        provider: &dyn SegmentProvider,
        record: RecordIdentifier,
        check_binaries: bool,
        visited: &mut HashSet<RecordIdentifier>,
        depth: usize,
    ) -> Result<()> {
        if depth > 100_000 || !visited.insert(record) {
            return Ok(());
        }
        let node = NodeState::new(provider, record);
        for property in node.properties()? {
            if !check_binaries {
                continue;
            }
            if let crate::content::node::PropertyValues::Single(value) = &property.values {
                materialize_binary(provider, value)?;
            } else if let crate::content::node::PropertyValues::Multiple(values) = &property.values
            {
                for value in values {
                    materialize_binary(provider, value)?;
                }
            }
        }
        for (_, child) in node.child_node_entries()? {
            walk(
                provider,
                child.record_identifier(),
                check_binaries,
                visited,
                depth + 1,
            )?;
        }
        Ok(())
    }

    let mut visited = HashSet::new();
    walk(provider, root, check_binaries, &mut visited, 0).map_err(|error| error.to_string())
}

/// Reads inline binary content to force its blocks to resolve; external
/// binaries have no local content to check.
fn materialize_binary(
    provider: &dyn SegmentProvider,
    value: &crate::content::property::PropertyValue,
) -> Result<()> {
    use crate::content::property::PropertyValue;
    use crate::content::value::BinaryValue;
    if let PropertyValue::Binary(BinaryValue::Inline {
        record_identifier, ..
    }) = value
    {
        crate::content::value::read_binary_content(provider, *record_identifier)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_consistency;
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-check-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_content_revision(directory: &std::path::Path, title: &str) {
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
    fn reports_the_newest_consistent_revision() {
        let directory = TestDirectory::new("consistent");
        write_content_revision(&directory.path, "first");
        write_content_revision(&directory.path, "second");

        let report =
            check_consistency(&directory.path, &["/content".to_owned()], true, 100).expect("check");
        assert!(
            report.good_revision.is_some(),
            "a consistent revision exists"
        );
        assert!(report.revisions[0].is_fully_consistent());
        assert!(
            report.revisions[0]
                .consistent_paths
                .contains(&"/content".to_owned())
        );
    }

    #[test]
    fn a_missing_path_is_reported_inconsistent() {
        let directory = TestDirectory::new("missing-path");
        write_content_revision(&directory.path, "only");
        let report = check_consistency(&directory.path, &["/nonexistent".to_owned()], false, 100)
            .expect("check");
        assert!(report.good_revision.is_none());
        assert!(!report.revisions.is_empty());
        assert!(!report.revisions[0].is_fully_consistent());
    }

    #[test]
    fn the_root_path_checks_the_whole_content_tree() {
        let directory = TestDirectory::new("root-path");
        write_content_revision(&directory.path, "content");
        let report = check_consistency(&directory.path, &[], true, 100).expect("check");
        assert!(report.good_revision.is_some());
        assert!(
            report.revisions[0]
                .consistent_paths
                .contains(&"/".to_owned())
        );
    }
}
