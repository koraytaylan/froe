//! Consistency checking: finding each path's newest traversable revision.
//!
//! `check` follows Oak's `ConsistencyChecker`: the checkpoint set is read
//! from the current head, then the journal is walked newest first. Each
//! requested path — under the head's content root and under every
//! checkpoint's root snapshot — is pinned to the first (newest) revision
//! where its whole subtree reads without a missing segment or malformed
//! record, and is never re-checked. The overall revision is the one at
//! which the last outstanding path verified; the check as a whole
//! succeeds when *any* path found a good revision (Java's default,
//! fail-fast off).

use std::collections::HashSet;

use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::journal::read_journal;
use crate::segment::record::RecordIdentifier;
use crate::store::{ArchiveSet, open_all_archives};

/// The maximum content tree depth checked before assuming a cycle; set
/// well below the stack-overflow threshold. Real trees are far shallower.
const MAXIMUM_CHECK_DEPTH: usize = 4000;

/// One checked path's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathVerdict {
    /// The content path (relative to the head's content root or the
    /// checkpoint's root snapshot).
    pub path: String,
    /// The newest revision at which the path verified, when one exists.
    pub latest_good_revision: Option<String>,
    /// The journal timestamp of that revision, when one exists.
    pub latest_good_timestamp_milliseconds: Option<i64>,
    /// The failure at the newest examined revision, for diagnostics;
    /// `None` when the path verified at the newest revision it was
    /// checked at.
    pub newest_failure: Option<String>,
}

/// The overall consistency report, mirroring Oak's check output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyReport {
    /// How many journal revisions were examined.
    pub checked_revisions: usize,
    /// The checkpoint names read from the current head, in stored order.
    pub checkpoints: Vec<String>,
    /// Per requested path: the verdict against the head content tree.
    pub head_paths: Vec<PathVerdict>,
    /// Per checkpoint, per requested path: the verdict against that
    /// checkpoint's root snapshot.
    pub checkpoint_paths: Vec<(String, Vec<PathVerdict>)>,
    /// The revision at which the last outstanding path verified — set
    /// only when every path found a good revision.
    pub overall_revision: Option<String>,
}

impl ConsistencyReport {
    /// Java's exit-code predicate with fail-fast off: the check succeeds
    /// when *any* head or checkpoint path found a good revision.
    #[must_use]
    pub fn has_good_revision(&self) -> bool {
        self.head_paths
            .iter()
            .chain(
                self.checkpoint_paths
                    .iter()
                    .flat_map(|(_, verdicts)| verdicts.iter()),
            )
            .any(|verdict| verdict.latest_good_revision.is_some())
    }
}

/// Where a checked path is rooted.
enum PathRoot {
    /// The head content tree (the super-root's `root` child).
    Head,
    /// A checkpoint's root snapshot
    /// (`checkpoints/<name>/root` under the super-root).
    Checkpoint(String),
}

/// One path still being checked, with its resolution root.
struct PathToCheck {
    root: PathRoot,
    path: String,
    verdict: PathVerdict,
    /// Sub-paths (relative to the resolved path node) found corrupt at
    /// newer revisions. Java re-probes these *first* at every older
    /// revision: each must exist and pass a shallow check before the
    /// full traversal runs, so a revision predating a later-corrupted
    /// node can never be reported good for this path.
    corrupt_paths: Vec<String>,
}

/// Checks the store at `directory`, verifying the given content paths at
/// each journal revision (newest first) against the head and against
/// every checkpoint of the current head. `check_binaries` reads every
/// block of inline binary values instead of only resolving their records.
/// At most `revision_limit` revisions are examined.
pub fn check_consistency(
    directory: &std::path::Path,
    filter_paths: &[String],
    check_binaries: bool,
    revision_limit: usize,
) -> Result<ConsistencyReport> {
    let archives = open_all_archives(directory)?;
    let provider = ArchiveSet::new(archives);
    // An unreadable journal is a loud failure, exactly like Java's check.
    let journal_entries = read_journal(&directory.join("journal.log"))?;

    let base_paths: Vec<String> = if filter_paths.is_empty() {
        vec!["/".to_owned()]
    } else {
        filter_paths.to_vec()
    };

    // Java resolves the "all" checkpoint set against the current head
    // before checking anything; an unreadable checkpoint container fails
    // the whole command there, and does here too.
    let checkpoints = current_head_checkpoint_names(&provider, &journal_entries)?;

    let mut paths_to_check = build_paths_to_check(&base_paths, &checkpoints);

    // Walk the journal newest first, pinning each path to the first
    // revision where it verifies; a pinned path is never re-checked.
    // Java counts every *attempted* entry — including unresolvable ones —
    // and tests the limit only at the end of a successful iteration, so
    // skipped entries never consume the budget's stopping point and a
    // limit of zero means unlimited.
    let mut checked_revisions = 0usize;
    let mut overall_revision = None;
    for entry in &journal_entries {
        checked_revisions += 1;
        let Some(head) = entry.record_identifier() else {
            continue;
        };
        if !provider_contains(&provider, head) {
            continue;
        }
        let super_root = NodeState::new(&provider, head);
        let mut all_pinned = true;
        for path_to_check in &mut paths_to_check {
            if path_to_check.verdict.latest_good_revision.is_some() {
                continue;
            }
            match check_one_path(&provider, &super_root, path_to_check, check_binaries) {
                Ok(()) => {
                    path_to_check.verdict.latest_good_revision = Some(entry.revision_text.clone());
                    path_to_check.verdict.latest_good_timestamp_milliseconds =
                        Some(entry.timestamp_milliseconds);
                }
                Err(reason) => {
                    all_pinned = false;
                    if path_to_check.verdict.newest_failure.is_none() {
                        path_to_check.verdict.newest_failure = Some(reason);
                    }
                }
            }
        }
        if all_pinned {
            // The revision at which the last outstanding path verified.
            overall_revision = Some(entry.revision_text.clone());
            break;
        }
        if checked_revisions == revision_limit {
            break;
        }
    }

    let mut head_paths = Vec::new();
    let mut checkpoint_paths: Vec<(String, Vec<PathVerdict>)> = checkpoints
        .iter()
        .map(|name| (name.clone(), Vec::new()))
        .collect();
    for path_to_check in paths_to_check {
        match &path_to_check.root {
            PathRoot::Head => head_paths.push(path_to_check.verdict),
            PathRoot::Checkpoint(name) => {
                if let Some((_, verdicts)) = checkpoint_paths
                    .iter_mut()
                    .find(|(checkpoint, _)| checkpoint == name)
                {
                    verdicts.push(path_to_check.verdict);
                }
            }
        }
    }

    Ok(ConsistencyReport {
        checked_revisions,
        checkpoints,
        head_paths,
        checkpoint_paths,
        overall_revision,
    })
}

/// The paths to check: every filter path against the head, then against
/// each checkpoint's root snapshot.
fn build_paths_to_check(base_paths: &[String], checkpoints: &[String]) -> Vec<PathToCheck> {
    let unchecked_verdict = |path: &String| PathVerdict {
        path: path.clone(),
        latest_good_revision: None,
        latest_good_timestamp_milliseconds: None,
        newest_failure: None,
    };
    let mut paths_to_check: Vec<PathToCheck> = Vec::new();
    for path in base_paths {
        paths_to_check.push(PathToCheck {
            root: PathRoot::Head,
            path: path.clone(),
            verdict: unchecked_verdict(path),
            corrupt_paths: Vec::new(),
        });
    }
    for checkpoint in checkpoints {
        for path in base_paths {
            paths_to_check.push(PathToCheck {
                root: PathRoot::Checkpoint(checkpoint.clone()),
                path: path.clone(),
                verdict: unchecked_verdict(path),
                corrupt_paths: Vec::new(),
            });
        }
    }
    paths_to_check
}

/// Whether the provider can resolve the head's segment.
fn provider_contains(provider: &ArchiveSet, head: RecordIdentifier) -> bool {
    provider.segment(head.segment).is_ok()
}

/// The checkpoint names of the current head — the newest revision whose
/// segment resolves, matching the read-only store's head binding. Java
/// resolves the literal `"all"` checkpoint set against this head and any
/// exception fails the whole command; a store with no resolvable head
/// simply has no checkpoints to expand.
fn current_head_checkpoint_names(
    provider: &ArchiveSet,
    journal_entries: &[crate::journal::JournalEntry],
) -> Result<Vec<String>> {
    let Some(head) = journal_entries
        .iter()
        .filter_map(crate::journal::JournalEntry::record_identifier)
        .find(|identifier| provider_contains(provider, *identifier))
    else {
        return Ok(Vec::new());
    };
    let super_root = NodeState::new(provider, head);
    match super_root.child_node("checkpoints")? {
        None => Ok(Vec::new()),
        Some(checkpoints) => Ok(checkpoints
            .child_node_entries()?
            .into_iter()
            .map(|(name, _)| name)
            .collect()),
    }
}

/// Checks one path at one revision: resolves it under its root,
/// re-probes the sub-paths found corrupt at newer revisions (each must
/// exist and pass a shallow check, or the path stays inconsistent and
/// the full traversal is skipped — Java's `findFirstCorruptedPathInSet`),
/// then verifies the whole subtree. Returns a short reason on failure.
fn check_one_path(
    provider: &dyn SegmentProvider,
    super_root: &NodeState<'_>,
    path_to_check: &mut PathToCheck,
    check_binaries: bool,
) -> std::result::Result<(), String> {
    let node = match resolve_path(super_root, &path_to_check.root, &path_to_check.path) {
        Ok(Some(node)) => node,
        Ok(None) => return Err("path does not exist".to_owned()),
        Err(error) => return Err(error.to_string()),
    };
    for corrupt_path in &path_to_check.corrupt_paths {
        match resolve_relative(&node, corrupt_path) {
            Ok(Some(corrupt_node)) => {
                if let Err(reason) =
                    check_node_shallow(provider, corrupt_node.record_identifier(), check_binaries)
                {
                    return Err(format!(
                        "previously corrupt path {}: {reason}",
                        display_relative(corrupt_path)
                    ));
                }
            }
            Ok(None) => {
                return Err(format!(
                    "previously corrupt path {} does not exist",
                    display_relative(corrupt_path)
                ));
            }
            Err(error) => {
                return Err(format!(
                    "previously corrupt path {}: {error}",
                    display_relative(corrupt_path)
                ));
            }
        }
    }
    match verify_subtree(provider, node.record_identifier(), check_binaries) {
        Ok(()) => Ok(()),
        Err(corrupt) => {
            if !path_to_check.corrupt_paths.contains(&corrupt.path) {
                path_to_check.corrupt_paths.push(corrupt.path.clone());
            }
            Err(format!(
                "{} at {}",
                corrupt.reason,
                display_relative(&corrupt.path)
            ))
        }
    }
}

/// Resolves a relative path (empty = the node itself) under a node.
fn resolve_relative<'provider>(
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

/// Renders a relative corrupt path for messages: the checked node itself
/// is `/`.
fn display_relative(relative_path: &str) -> &str {
    if relative_path.is_empty() {
        "/"
    } else {
        relative_path
    }
}

/// Checks one node without recursing: every property is decoded, and —
/// when asked — every inline binary is read, exactly Java's `checkNode`.
fn check_node_shallow(
    provider: &dyn SegmentProvider,
    record: RecordIdentifier,
    check_binaries: bool,
) -> std::result::Result<(), String> {
    let node = NodeState::new(provider, record);
    let properties = node.properties().map_err(|error| error.to_string())?;
    if check_binaries {
        for property in &properties {
            match &property.values {
                crate::content::node::PropertyValues::Single(value) => {
                    materialize_binary(provider, value).map_err(|error| error.to_string())?;
                }
                crate::content::node::PropertyValues::Multiple(values) => {
                    for value in values {
                        materialize_binary(provider, value).map_err(|error| error.to_string())?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Resolves a content path under its root: the head's content root, or a
/// checkpoint's root snapshot. User paths are always content paths — a
/// content node literally named `checkpoints` is reachable, unlike Java's
/// path-hijacking alternatives.
fn resolve_path<'provider>(
    super_root: &NodeState<'provider>,
    root: &PathRoot,
    path: &str,
) -> Result<Option<NodeState<'provider>>> {
    let mut current = match root {
        PathRoot::Head => match super_root.child_node("root")? {
            Some(content_root) => content_root,
            None => {
                return Err(Error::InvalidFormat {
                    details: "the super-root has no \"root\" child node".to_owned(),
                });
            }
        },
        PathRoot::Checkpoint(name) => {
            let Some(checkpoints) = super_root.child_node("checkpoints")? else {
                return Ok(None);
            };
            let Some(checkpoint) = checkpoints.child_node(name)? else {
                return Ok(None);
            };
            match checkpoint.child_node("root")? {
                Some(snapshot_root) => snapshot_root,
                None => return Ok(None),
            }
        }
    };
    for name in path.split('/').filter(|segment| !segment.is_empty()) {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// A corrupt location inside a checked subtree: the relative path of the
/// node where verification failed (empty for the checked node itself)
/// and the reason.
struct CorruptLocation {
    path: String,
    reason: String,
}

/// Traverses a subtree, resolving every node, property, and — when asked
/// — binary content. Returns the first corrupt location, which the
/// caller remembers and re-probes at older revisions.
fn verify_subtree(
    provider: &dyn SegmentProvider,
    root: RecordIdentifier,
    check_binaries: bool,
) -> std::result::Result<(), CorruptLocation> {
    fn walk(
        provider: &dyn SegmentProvider,
        record: RecordIdentifier,
        check_binaries: bool,
        visited: &mut HashSet<RecordIdentifier>,
        ancestors: &mut HashSet<RecordIdentifier>,
        depth: usize,
        path: &mut String,
    ) -> std::result::Result<(), CorruptLocation> {
        let corrupt_here = |reason: String| CorruptLocation {
            path: path.clone(),
            reason,
        };
        // A subtree deeper than the cap cannot be verified within bounded
        // stack; calling it consistent would bless corruption below the
        // cap, so it fails the check instead.
        if depth > MAXIMUM_CHECK_DEPTH {
            return Err(corrupt_here(format!(
                "tree exceeds depth {MAXIMUM_CHECK_DEPTH}"
            )));
        }
        // A node reachable from itself is corruption (valid records only
        // reference already-written records), and must fail the check —
        // whereas meeting an already-*completed* node again is ordinary
        // shared-subtree deduplication and verifies for free.
        if ancestors.contains(&record) {
            return Err(corrupt_here(format!(
                "node record {record} is contained in its own subtree"
            )));
        }
        if !visited.insert(record) {
            return Ok(());
        }
        ancestors.insert(record);
        // Not `map_err(corrupt_here)`: different clippy versions disagree
        // about the borrow there, and an explicit struct keeps both quiet.
        if let Err(reason) = check_node_shallow(provider, record, check_binaries) {
            return Err(CorruptLocation {
                path: path.clone(),
                reason,
            });
        }
        let node = NodeState::new(provider, record);
        let children = node
            .child_node_entries()
            .map_err(|error| corrupt_here(error.to_string()))?;
        for (name, child) in children {
            let parent_length = path.len();
            path.push('/');
            path.push_str(&name);
            walk(
                provider,
                child.record_identifier(),
                check_binaries,
                visited,
                ancestors,
                depth + 1,
                path,
            )?;
            path.truncate(parent_length);
        }
        ancestors.remove(&record);
        Ok(())
    }

    let mut visited = HashSet::new();
    let mut ancestors = HashSet::new();
    let mut path = String::new();
    walk(
        provider,
        root,
        check_binaries,
        &mut visited,
        &mut ancestors,
        0,
        &mut path,
    )
}

/// Resolves and reads every block of an inline binary without holding the
/// content in memory (a multi-gigabyte binary must not have to fit);
/// external binaries have no local content to check.
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
        crate::content::value::verify_binary_content(provider, *record_identifier)?;
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
    fn pins_each_path_to_the_newest_consistent_revision() {
        let directory = TestDirectory::new("consistent");
        write_content_revision(&directory.path, "first");
        write_content_revision(&directory.path, "second");

        let report =
            check_consistency(&directory.path, &["/content".to_owned()], true, 100).expect("check");
        assert!(report.has_good_revision(), "a consistent revision exists");
        assert_eq!(report.head_paths.len(), 1);
        let verdict = &report.head_paths[0];
        assert_eq!(verdict.path, "/content");
        assert!(verdict.latest_good_revision.is_some());
        assert!(verdict.newest_failure.is_none());
        assert_eq!(
            report.overall_revision, verdict.latest_good_revision,
            "with one path, overall is that path's revision"
        );
        assert!(report.checked_revisions >= 1);
    }

    #[test]
    fn a_missing_path_is_reported_without_a_good_revision() {
        let directory = TestDirectory::new("missing-path");
        write_content_revision(&directory.path, "only");
        let report = check_consistency(&directory.path, &["/nonexistent".to_owned()], false, 100)
            .expect("check");
        assert!(!report.has_good_revision());
        assert!(report.overall_revision.is_none());
        let verdict = &report.head_paths[0];
        assert!(verdict.latest_good_revision.is_none());
        assert_eq!(
            verdict.newest_failure.as_deref(),
            Some("path does not exist")
        );
    }

    #[test]
    fn a_good_path_succeeds_even_when_another_never_verifies() {
        // Java's default (fail-fast off) succeeds when *any* path finds a
        // good revision; a permanently missing sibling must not veto it.
        let directory = TestDirectory::new("partial-good");
        write_content_revision(&directory.path, "content");
        let report = check_consistency(
            &directory.path,
            &["/content".to_owned(), "/nonexistent".to_owned()],
            false,
            100,
        )
        .expect("check");
        assert!(report.has_good_revision());
        assert!(
            report.overall_revision.is_none(),
            "overall requires every path to verify"
        );
        assert!(report.head_paths[0].latest_good_revision.is_some());
        assert!(report.head_paths[1].latest_good_revision.is_none());
    }

    #[test]
    fn the_root_path_checks_the_whole_content_tree_and_checkpoints() {
        let directory = TestDirectory::new("root-path");
        write_content_revision(&directory.path, "content");
        {
            let store = WritableRepository::open(&directory.path).expect("open");
            crate::writer::commit::create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
            store.close().expect("close");
        }
        let report = check_consistency(&directory.path, &[], true, 100).expect("check");
        assert!(report.has_good_revision());
        assert_eq!(report.checkpoints.len(), 1, "the checkpoint is expanded");
        assert_eq!(report.head_paths[0].path, "/");
        assert!(report.head_paths[0].latest_good_revision.is_some());
        let (_, checkpoint_verdicts) = &report.checkpoint_paths[0];
        assert!(
            checkpoint_verdicts[0].latest_good_revision.is_some(),
            "the checkpoint's root snapshot verifies"
        );
        assert!(
            report.overall_revision.is_some(),
            "every path verified, so an overall revision exists"
        );
    }
}
