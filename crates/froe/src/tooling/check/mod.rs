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
//!
//! Paths checked at one revision share one `PackedRecordSet` of completed
//! subtrees, so a checkpoint that shares the live root is not walked twice.
//! The set is dropped between journal revisions: keeping it would accumulate
//! historical nodes toward the size of the store.

use std::collections::HashSet;

use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::journal::read_journal;
use crate::packed_records::PackedRecordSet;
use crate::progress::{DiscardedProgress, ProgressObserver, Step, StrideCounter, WorkUnit};
use crate::segment::record::RecordIdentifier;
use crate::store::{ArchiveSet, open_all_archives_with_progress};

mod path;
mod subtree;
#[cfg(test)]
mod test_support;

pub(crate) use path::*;
pub use subtree::*;

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
pub(crate) enum PathRoot {
    /// The head content tree (the super-root's `root` child).
    Head,
    /// A checkpoint's root snapshot
    /// (`checkpoints/<name>/root` under the super-root).
    Checkpoint(String),
}

/// One path still being checked, with its resolution root.
pub(crate) struct PathToCheck {
    pub(crate) root: PathRoot,
    pub(crate) path: String,
    pub(crate) verdict: PathVerdict,
    /// Sub-paths (relative to the resolved path node) found corrupt at
    /// newer revisions. Java re-probes these *first* at every older
    /// revision: each must exist and pass a shallow check before the
    /// full traversal runs, so a revision predating a later-corrupted
    /// node can never be reported good for this path.
    pub(crate) corrupt_paths: Vec<String>,
}

/// How thoroughly a consistency check verifies inline binary values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryCheck {
    /// Resolve each binary value's record without reading its blocks.
    RecordsOnly,
    /// Read every block of every inline binary value, like Java's `--bin`.
    EveryBlock,
}

/// Checks the store at `directory`, verifying the given content paths at
/// each journal revision (newest first) against the head and against
/// every checkpoint of the current head. `binary_check` decides whether
/// inline binary values are read in full or only resolved. At most
/// `revision_limit` revisions are examined.
pub fn check_consistency(
    directory: &std::path::Path,
    filter_paths: &[String],
    binary_check: BinaryCheck,
    revision_limit: usize,
) -> Result<ConsistencyReport> {
    check_consistency_with_progress(
        directory,
        filter_paths,
        binary_check,
        revision_limit,
        &mut DiscardedProgress,
    )
}

/// Checks exactly like [`check_consistency`], reporting the archive scan
/// and the revision walk to `observer`.
pub fn check_consistency_with_progress(
    directory: &std::path::Path,
    filter_paths: &[String],
    binary_check: BinaryCheck,
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
        // Dropped at the end of this revision so historical unique nodes
        // cannot accumulate. Peak is the union of paths checked here, which
        // for `/` plus checkpoints is the super-root — the same ceiling
        // compact already pays.
        let mut verified_this_revision = PackedRecordSet::new();
        for path_to_check in &mut paths_to_check {
            if path_to_check.verdict.latest_good_revision.is_some() {
                continue;
            }
            match check_one_path(
                &provider,
                &super_root,
                path_to_check,
                binary_check,
                &mut verified_this_revision,
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
pub(crate) fn examinable_revisions(journal_length: usize, revision_limit: usize) -> usize {
    if revision_limit == 0 {
        journal_length
    } else {
        journal_length.min(revision_limit)
    }
}

#[cfg(test)]
pub(crate) mod examinable_revision_tests {
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
pub(crate) fn build_paths_to_check(
    base_paths: &[String],
    checkpoints: &[String],
) -> Vec<PathToCheck> {
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
pub(crate) fn provider_contains(provider: &ArchiveSet, head: RecordIdentifier) -> bool {
    provider.segment(head.segment).is_ok()
}

/// The checkpoint names of the current head — the newest revision whose
/// segment resolves, matching the read-only store's head binding. Java
/// resolves the literal `"all"` checkpoint set against this head and any
/// exception fails the whole command; a store with no resolvable head
/// simply has no checkpoints to expand.
pub(crate) fn current_head_checkpoint_names(
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

#[cfg(test)]
mod tests {
    use super::{BinaryCheck, check_consistency};
    use crate::tooling::check::test_support::{TestDirectory, write_content_revision};
    use crate::writer::store_writer::WritableRepository;

    #[test]
    fn pins_each_path_to_the_newest_consistent_revision() {
        let directory = TestDirectory::new("consistent");
        write_content_revision(&directory.path, "first");
        write_content_revision(&directory.path, "second");

        let report = check_consistency(
            &directory.path,
            &["/content".to_owned()],
            BinaryCheck::EveryBlock,
            100,
        )
        .expect("check");
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
        let report = check_consistency(
            &directory.path,
            &["/nonexistent".to_owned()],
            BinaryCheck::RecordsOnly,
            100,
        )
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
            BinaryCheck::RecordsOnly,
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
        let report =
            check_consistency(&directory.path, &[], BinaryCheck::EveryBlock, 100).expect("check");
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
