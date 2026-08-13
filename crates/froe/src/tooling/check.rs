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

use std::collections::{HashMap, HashSet};

use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::journal::read_journal;
use crate::progress::{DiscardedProgress, ProgressObserver, Step, StrideCounter, WorkUnit};
use crate::segment::record::RecordIdentifier;
use crate::store::{ArchiveSet, open_all_archives_with_progress};

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
    check_consistency_with_progress(
        directory,
        filter_paths,
        check_binaries,
        revision_limit,
        &mut DiscardedProgress,
    )
}

/// Checks exactly like [`check_consistency`], reporting the archive scan
/// and the revision walk to `observer`.
pub fn check_consistency_with_progress(
    directory: &std::path::Path,
    filter_paths: &[String],
    check_binaries: bool,
    revision_limit: usize,
    observer: &mut dyn ProgressObserver,
) -> Result<ConsistencyReport> {
    let archives = open_all_archives_with_progress(directory, observer)?;
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
    // One step per revision, counting the nodes that revision's walk
    // resolves. A single step spanning the whole loop could only count
    // revisions, and a healthy store pins every path at the first one —
    // so the report would sit at 0 of N for the entire run, which is the
    // silence this reporting exists to remove. The revision's position is
    // in the description instead, where a count of one unit can carry it.
    //
    // Every statement in this loop is infallible — a path that fails to
    // verify becomes a verdict, not an error — so each step is closed by
    // the `step_ended` at the end of its iteration.
    let examinable = examinable_revisions(journal_entries.len(), revision_limit);
    for entry in &journal_entries {
        checked_revisions += 1;
        // Java counts every *attempted* entry and tests the limit only at
        // the end of an iteration that did not skip, so an unresolvable
        // line can carry the count one past the limit. That rule is Oak's
        // and stays; the label must not advertise the overshoot, so the
        // numerator is clamped to the bound the denominator declares.
        let position = checked_revisions.min(examinable);
        let description = format!("checking revision {position} of {examinable}");
        observer.step_began(&Step::new(&description, WorkUnit::Nodes));
        let mut nodes = VerifiedNodeCount::new(observer);
        let Some(head) = entry.record_identifier() else {
            nodes.finish();
            observer.step_ended();
            continue;
        };
        if !provider_contains(&provider, head) {
            nodes.finish();
            observer.step_ended();
            continue;
        }
        let super_root = NodeState::new(&provider, head);
        let mut all_pinned = true;
        for path_to_check in &mut paths_to_check {
            if path_to_check.verdict.latest_good_revision.is_some() {
                continue;
            }
            match check_one_path(
                &provider,
                &super_root,
                path_to_check,
                check_binaries,
                &mut nodes,
            ) {
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
        nodes.finish();
        observer.step_ended();
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

/// How many revisions a run may examine: the journal's length, bounded by
/// an explicit limit. Java's limit of zero means unlimited, and so does
/// froe's, so a zero limit yields the whole journal. Reporting more than
/// this would declare a total the run is forbidden to reach.
fn examinable_revisions(journal_length: usize, revision_limit: usize) -> usize {
    if revision_limit == 0 {
        journal_length
    } else {
        journal_length.min(revision_limit)
    }
}

#[cfg(test)]
mod examinable_revision_tests {
    use super::examinable_revisions;

    #[test]
    fn a_limit_bounds_the_declared_total() {
        assert_eq!(examinable_revisions(5_000, 2), 2);
        assert_eq!(examinable_revisions(2, 5_000), 2);
        assert_eq!(
            examinable_revisions(5_000, 0),
            5_000,
            "a zero limit means unlimited, as it does in Java"
        );
        assert_eq!(examinable_revisions(5_000, usize::MAX), 5_000);
        assert_eq!(examinable_revisions(0, 3), 0);
    }
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
    progress: &mut VerifiedNodeCount<'_>,
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
    match verify_subtree(
        provider,
        node.record_identifier(),
        SubtreeChecks {
            binaries: check_binaries,
            stable_identifiers: false,
        },
        progress,
    ) {
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

/// Verifies one complete node subtree, including every stable-identifier
/// record and inline binary.
///
/// `root` is the exact node record to check; the provider may be a full
/// repository or any other segment source. External binaries are verified
/// only as references because their content is outside the segment store.
/// A corrupt root or descendant is reported as [`Error::InvalidFormat`]
/// whose details include its path relative to `root` (`/` denotes `root`
/// itself).
/// This deliberately classifies every traversal failure as invalid repository
/// data so path context is retained; the original error text is preserved in
/// the details even when the underlying failure was I/O or a missing segment.
pub fn verify_node_tree(provider: &dyn SegmentProvider, root: RecordIdentifier) -> Result<()> {
    NodeTreeVerifier::new(provider).verify(root)
}

/// A provider-bound verifier which reuses certificates for fully verified
/// immutable node subtrees across multiple roots.
///
/// Segment providers expose immutable record bytes, so a node that completed
/// every shallow, stable-identifier, inline-binary, and descendant check can
/// be safely reused while this verifier remains bound to the same provider.
/// Failed and cyclic subtrees are never cached. Cached subtree height is also
/// retained: a reuse that could cross the traversal depth limit falls back to
/// a real walk, preserving both the limit and the original path diagnostic.
pub struct NodeTreeVerifier<'provider> {
    provider: &'provider dyn SegmentProvider,
    verified_subtree_heights: HashMap<RecordIdentifier, usize>,
    /// Nodes resolved across every `verify_with_progress` call, so a
    /// caller verifying several roots inside one reported step sees one
    /// running total rather than a count that restarts per root.
    verified_nodes: u64,
}

impl<'provider> NodeTreeVerifier<'provider> {
    /// Binds a reusable verifier to one immutable segment provider.
    #[must_use]
    pub fn new(provider: &'provider dyn SegmentProvider) -> Self {
        Self {
            provider,
            verified_subtree_heights: HashMap::new(),
            verified_nodes: 0,
        }
    }

    /// Verifies `root`, reusing only subtrees which a previous call completed
    /// successfully against this same provider.
    pub fn verify(&mut self, root: RecordIdentifier) -> Result<()> {
        self.verify_with_progress(root, &mut DiscardedProgress)
    }

    /// Verifies exactly like [`NodeTreeVerifier::verify`], reporting the
    /// number of nodes resolved so far to `observer`. Nodes served from
    /// the verified-subtree cache are not counted again.
    pub fn verify_with_progress(
        &mut self,
        root: RecordIdentifier,
        observer: &mut dyn ProgressObserver,
    ) -> Result<()> {
        let mut progress = VerifiedNodeCount::resuming(observer, self.verified_nodes);
        let verified = verify_subtree_with_cache(
            self.provider,
            root,
            SubtreeChecks {
                binaries: true,
                stable_identifiers: true,
            },
            &mut self.verified_subtree_heights,
            &mut progress,
        );
        progress.finish();
        self.verified_nodes = progress.completed();
        verified
            .map(|_| ())
            .map_err(|corrupt| node_tree_error(&corrupt))
    }
}

fn node_tree_error(corrupt: &CorruptLocation) -> Error {
    Error::InvalidFormat {
        details: format!(
            "node tree verification failed at {}: {}",
            display_relative(&corrupt.path),
            corrupt.reason
        ),
    }
}

#[derive(Clone, Copy)]
struct SubtreeChecks {
    binaries: bool,
    stable_identifiers: bool,
}

/// Traverses a subtree, resolving every node, property, and — when asked
/// — binary content and stable identifier. Returns the first corrupt
/// location, which the caller remembers and re-probes at older revisions.
fn verify_subtree(
    provider: &dyn SegmentProvider,
    root: RecordIdentifier,
    checks: SubtreeChecks,
    progress: &mut VerifiedNodeCount<'_>,
) -> std::result::Result<(), CorruptLocation> {
    verify_subtree_with_cache(provider, root, checks, &mut HashMap::new(), progress).map(|_| ())
}

/// Counts verified nodes for a [`ProgressObserver`], reporting on a stride
/// so a million-node tree does not become a million observer calls.
struct VerifiedNodeCount<'observer> {
    observer: &'observer mut dyn ProgressObserver,
    counter: StrideCounter,
}

impl<'observer> VerifiedNodeCount<'observer> {
    fn new(observer: &'observer mut dyn ProgressObserver) -> Self {
        Self::resuming(observer, 0)
    }

    /// A counter continuing from `already`, so a step that verifies
    /// several roots keeps one running total instead of restarting — and
    /// the second root, which the subtree cache makes cheaper than the
    /// first, cannot report a smaller number than the first did.
    fn resuming(observer: &'observer mut dyn ProgressObserver, already: u64) -> Self {
        Self {
            observer,
            counter: StrideCounter::resuming(VERIFIED_NODE_REPORT_STRIDE, already),
        }
    }

    /// How many nodes this counter has seen, including the ones it
    /// resumed from.
    fn completed(&self) -> u64 {
        self.counter.completed()
    }

    fn advance(&mut self) {
        self.counter.advance(self.observer);
    }

    /// Reports the exact number of nodes resolved, including the last
    /// partial stride.
    fn finish(&mut self) {
        self.counter.finish(self.observer);
    }
}

/// How many nodes a verification walk resolves between progress reports.
const VERIFIED_NODE_REPORT_STRIDE: u64 = 512;

/// Returns the verified subtree height. A record enters `verified` only
/// after every descendant completed successfully.
fn verify_subtree_with_cache(
    provider: &dyn SegmentProvider,
    root: RecordIdentifier,
    checks: SubtreeChecks,
    verified: &mut HashMap<RecordIdentifier, usize>,
    progress: &mut VerifiedNodeCount<'_>,
) -> std::result::Result<usize, CorruptLocation> {
    #[allow(
        clippy::too_many_arguments,
        reason = "the recursive walk threads its provider, checks, caches, path, depth, and progress; bundling them would only rename the same state"
    )]
    fn walk(
        provider: &dyn SegmentProvider,
        record: RecordIdentifier,
        checks: SubtreeChecks,
        verified: &mut HashMap<RecordIdentifier, usize>,
        ancestors: &mut HashSet<RecordIdentifier>,
        depth: usize,
        path: &mut String,
        progress: &mut VerifiedNodeCount<'_>,
    ) -> std::result::Result<usize, CorruptLocation> {
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
        if let Some(&subtree_height) = verified.get(&record)
            && depth
                .checked_add(subtree_height)
                .is_some_and(|deepest| deepest <= MAXIMUM_CHECK_DEPTH)
        {
            return Ok(subtree_height);
        }
        ancestors.insert(record);
        progress.advance();
        // Not `map_err(corrupt_here)`: different clippy versions disagree
        // about the borrow there, and an explicit struct keeps both quiet.
        if let Err(reason) = check_node_shallow(provider, record, checks.binaries) {
            return Err(CorruptLocation {
                path: path.clone(),
                reason,
            });
        }
        let node = NodeState::new(provider, record);
        if checks.stable_identifiers
            && let Err(error) = node.stable_identifier_bytes()
        {
            return Err(CorruptLocation {
                path: path.clone(),
                reason: error.to_string(),
            });
        }
        let children = node
            .child_node_entries()
            .map_err(|error| corrupt_here(error.to_string()))?;
        let mut subtree_height = 0usize;
        for (name, child) in children {
            let parent_length = path.len();
            path.push('/');
            path.push_str(&name);
            let child_height = walk(
                provider,
                child.record_identifier(),
                checks,
                verified,
                ancestors,
                depth + 1,
                path,
                progress,
            )?;
            path.truncate(parent_length);
            subtree_height = subtree_height.max(child_height + 1);
        }
        ancestors.remove(&record);
        verified.insert(record, subtree_height);
        Ok(subtree_height)
    }

    let mut ancestors = HashSet::new();
    let mut path = String::new();
    walk(
        provider,
        root,
        checks,
        verified,
        &mut ancestors,
        0,
        &mut path,
        progress,
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
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{NodeTreeVerifier, check_consistency, verify_node_tree};
    use crate::content::provider::SegmentProvider;
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::content::template::{Template, read_template};
    use crate::content::value::read_string;
    use crate::error::{Error, Result};
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::record::RecordIdentifier;
    use crate::segment::view::SegmentView;
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::segment_builder::SegmentBufferBuilder;
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

    /// A provider that makes selected, otherwise-valid segments disappear.
    /// Its string/template readers deliberately route back through `self`,
    /// so their record accesses cannot bypass the hiding behavior by
    /// delegating directly to the wrapped repository.
    struct HidingProvider<'store> {
        store: &'store WritableRepository,
        exact: Option<SegmentIdentifier>,
        bulk: bool,
    }

    impl SegmentProvider for HidingProvider<'_> {
        fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
            if self.exact == Some(identifier) || self.bulk && identifier.is_bulk_segment() {
                return Err(Error::SegmentNotFound {
                    segment_identifier: identifier,
                });
            }
            self.store.segment(identifier)
        }

        fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
            read_string(self, identifier).map(Arc::from)
        }

        fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
            read_template(self, identifier).map(Arc::new)
        }
    }

    /// Counts every segment resolution and can make one segment unavailable.
    /// String/template reads route through `self`, so all record access is
    /// observable by the counter.
    struct CountingProvider<'provider> {
        inner: &'provider dyn SegmentProvider,
        hidden: Option<SegmentIdentifier>,
        reads: RefCell<HashMap<SegmentIdentifier, usize>>,
    }

    impl<'provider> CountingProvider<'provider> {
        fn new(inner: &'provider dyn SegmentProvider) -> Self {
            Self {
                inner,
                hidden: None,
                reads: RefCell::new(HashMap::new()),
            }
        }

        fn hiding(inner: &'provider dyn SegmentProvider, hidden: SegmentIdentifier) -> Self {
            Self {
                inner,
                hidden: Some(hidden),
                reads: RefCell::new(HashMap::new()),
            }
        }

        fn reads_of(&self, segment: SegmentIdentifier) -> usize {
            self.reads.borrow().get(&segment).copied().unwrap_or(0)
        }
    }

    impl SegmentProvider for CountingProvider<'_> {
        fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
            *self.reads.borrow_mut().entry(identifier).or_default() += 1;
            if self.hidden == Some(identifier) {
                return Err(Error::SegmentNotFound {
                    segment_identifier: identifier,
                });
            }
            self.inner.segment(identifier)
        }

        fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
            read_string(self, identifier).map(Arc::from)
        }

        fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
            read_template(self, identifier).map(Arc::new)
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

    #[test]
    fn node_tree_verifier_reports_the_corrupt_descendant_path() {
        let directory = TestDirectory::new("verify-corrupt-path");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        // Finish the child first so it occupies a different segment from
        // the parent. The wrapped provider can then make only that child
        // unavailable while leaving the parent perfectly readable.
        let mut child_writer = store.record_writer(generation);
        let child = child_writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("child");
        child_writer.finish().expect("finish child");
        let mut root_writer = store.record_writer(generation);
        let root = root_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "broken".to_owned(),
                    node: child,
                },
                &[],
            )
            .expect("root");
        root_writer.finish().expect("finish root");

        verify_node_tree(&store, root).expect("the complete tree verifies");
        let provider = HidingProvider {
            store: &store,
            exact: Some(child.segment),
            bulk: false,
        };
        let error = verify_node_tree(&provider, root).expect_err("hidden child must fail");
        let Error::InvalidFormat { details } = error else {
            panic!("verification must return a structured format error");
        };
        assert!(
            details.contains("at /broken:"),
            "the error identifies the corrupt relative path: {details}"
        );
        assert!(
            details.contains(&child.segment.to_string()),
            "the underlying failure remains useful: {details}"
        );

        let provider = HidingProvider {
            store: &store,
            exact: Some(root.segment),
            bulk: false,
        };
        let error = verify_node_tree(&provider, root).expect_err("hidden root must fail");
        let Error::InvalidFormat { details } = error else {
            panic!("verification must return a structured format error");
        };
        assert!(
            details.contains("at /:"),
            "root corruption uses the documented root path: {details}"
        );
        store.close().expect("close");
    }

    #[test]
    fn reusable_node_tree_verifier_reuses_fully_verified_shared_descendants() {
        let directory = TestDirectory::new("verify-shared-cache");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        let mut child_writer = store.record_writer(generation);
        let shared = child_writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("shared child");
        child_writer.finish().expect("finish shared child");

        let mut first_writer = store.record_writer(generation);
        let first_root = first_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "shared".to_owned(),
                    node: shared,
                },
                &[],
            )
            .expect("first root");
        first_writer.finish().expect("finish first root");

        let mut second_writer = store.record_writer(generation);
        let second_root = second_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "shared".to_owned(),
                    node: shared,
                },
                &[],
            )
            .expect("second root");
        second_writer.finish().expect("finish second root");

        let provider = CountingProvider::new(&store);
        let mut verifier = NodeTreeVerifier::new(&provider);
        verifier.verify(first_root).expect("first tree verifies");
        let shared_reads = provider.reads_of(shared.segment);
        assert!(shared_reads > 0, "the first root traverses the shared node");
        assert!(
            verifier.verified_subtree_heights.contains_key(&shared),
            "only a completed shared subtree receives a certificate"
        );

        verifier.verify(second_root).expect("second tree verifies");
        assert_eq!(
            provider.reads_of(shared.segment),
            shared_reads,
            "the second root reuses the provider-bound subtree certificate"
        );
        store.close().expect("close");
    }

    #[test]
    fn reusable_node_tree_verifier_never_caches_a_failed_subtree() {
        let directory = TestDirectory::new("verify-failed-not-cached");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        let mut child_writer = store.record_writer(generation);
        let child = child_writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("child");
        child_writer.finish().expect("finish child");
        let mut root_writer = store.record_writer(generation);
        let root = root_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "broken".to_owned(),
                    node: child,
                },
                &[],
            )
            .expect("root");
        root_writer.finish().expect("finish root");

        let provider = CountingProvider::hiding(&store, child.segment);
        let mut verifier = NodeTreeVerifier::new(&provider);
        let first = verifier.verify(root).expect_err("hidden child fails");
        let first_reads = provider.reads_of(child.segment);
        assert!(first_reads > 0);
        assert!(
            verifier.verified_subtree_heights.is_empty(),
            "neither the corrupt child nor its incomplete ancestor is cached"
        );
        let second = verifier
            .verify(root)
            .expect_err("the same hidden child must be re-read and fail again");
        assert!(provider.reads_of(child.segment) > first_reads);
        assert_eq!(first.to_string(), second.to_string());
        assert!(verifier.verified_subtree_heights.is_empty());
        store.close().expect("close");
    }

    #[test]
    fn reusable_node_tree_verifier_never_caches_a_cyclic_subtree() {
        let directory = TestDirectory::new("verify-cycle-not-cached");
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

        let provider = CountingProvider::new(&memory);
        let mut verifier = NodeTreeVerifier::new(&provider);
        let first = verifier.verify(root).expect_err("self-cycle fails");
        let first_reads = provider.reads_of(root.segment);
        let Error::InvalidFormat { details } = &first else {
            panic!("cycle verification returns a format error");
        };
        assert!(details.contains("at /loop:"));
        assert!(details.contains("contained in its own subtree"));
        assert!(verifier.verified_subtree_heights.is_empty());

        let second = verifier.verify(root).expect_err("cycle is never cached");
        assert!(provider.reads_of(root.segment) > first_reads);
        assert_eq!(first.to_string(), second.to_string());
        assert!(verifier.verified_subtree_heights.is_empty());
        store.close().expect("close");
    }

    #[test]
    fn node_tree_verifier_materializes_long_inline_binary_blocks() {
        let directory = TestDirectory::new("verify-inline-binary");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");
        let content: Vec<u8> = (0..300 * 1024).map(|index| (index % 251) as u8).collect();
        let mut writer = store.record_writer(generation);
        let binary = writer.write_binary_content(&content).expect("binary");
        let payload = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "data".to_owned(),
                    property_type: crate::content::property::PropertyType::Binary,
                    values: PropertyValuesToWrite::Single(binary),
                }],
            )
            .expect("payload");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "payload".to_owned(),
                    node: payload,
                },
                &[],
            )
            .expect("root");
        writer.finish().expect("finish");

        verify_node_tree(&store, root).expect("complete binary verifies");
        let provider = HidingProvider {
            store: &store,
            exact: None,
            bulk: true,
        };
        let error = verify_node_tree(&provider, root).expect_err("missing block must fail");
        let Error::InvalidFormat { details } = error else {
            panic!("verification must return a structured format error");
        };
        assert!(
            details.contains("at /payload:"),
            "binary corruption is attributed to its containing node: {details}"
        );
        assert!(
            details.contains("not found in any archive"),
            "the missing block reason is retained: {details}"
        );
        store.close().expect("close");
    }

    #[test]
    fn node_tree_verifier_resolves_preserved_stable_identifiers() {
        let directory = TestDirectory::new("verify-stable-identifier");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        let mut child_writer = store.record_writer(generation);
        let child = child_writer
            .write_node_with_stable_identifier(
                None,
                &[],
                &ChildNodesToWrite::Zero,
                &[],
                Some([0x5a; 20]),
            )
            .expect("child");
        child_writer.finish().expect("finish child");
        let mut root_writer = store.record_writer(generation);
        let root = root_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "stable".to_owned(),
                    node: child,
                },
                &[],
            )
            .expect("root");
        root_writer.finish().expect("finish root");
        verify_node_tree(&store, root).expect("valid stable identifier verifies");

        // Keep both node-shaped segments intact except for the child's slot
        // zero. Point that slot at a nonexistent record in its own segment:
        // templates, properties, and children still decode, so only a
        // verifier that resolves stable identifiers detects this corruption.
        let child_view = store.segment(child.segment).expect("child segment");
        let mut child_bytes = child_view.bytes.to_vec();
        let child_position = child_view
            .record_position(child.record_number)
            .expect("child position");
        child_bytes[child_position..child_position + 2].copy_from_slice(&0u16.to_be_bytes());
        child_bytes[child_position + 2..child_position + 6]
            .copy_from_slice(&u32::MAX.to_be_bytes());
        let root_view = store.segment(root.segment).expect("root segment");
        let mut provider = MemorySegmentProvider::default();
        provider.insert(child.segment, child_bytes);
        provider.insert(root.segment, root_view.bytes.to_vec());

        let error = verify_node_tree(&provider, root).expect_err("invalid stable id must fail");
        let Error::InvalidFormat { details } = error else {
            panic!("verification must return a structured format error");
        };
        assert!(
            details.contains("at /stable:"),
            "stable-id corruption is attributed to its node: {details}"
        );
        assert!(
            details.contains("record 4294967295 does not exist"),
            "the stable-id failure reason is retained: {details}"
        );
        store.close().expect("close");
    }
}
