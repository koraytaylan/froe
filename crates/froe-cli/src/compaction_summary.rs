//! What `froe compact` prints once the confirmed plan has been applied:
//! the completion summary, and the deletions the run could not perform.
//!
//! Every line is built to be read in its own context: a cleanup that was
//! not part of the run says why it was not, counts agree with their nouns,
//! and a zero is omitted rather than reported.

use std::fmt::Write as _;

use froe::FileDeletionFailure;

use crate::compaction::SkipReason;
use crate::output::count_noun;
use crate::progress::format_count;

/// The purge the confirmed plan carries, restated by the summary after the
/// progress output has scrolled the plan away.
#[derive(Clone, Copy)]
pub(crate) struct PurgeFacts {
    pub(crate) histories: u64,
    pub(crate) nodes: u64,
    pub(crate) retained_checkpoints: u64,
}

/// What the summary needs from the moment of confirmation: read off the
/// authoritative plan and the resolved questions before the plan is
/// consumed.
pub(crate) struct CompletionContext {
    /// Segments the plan identified as reclaimable and then kept.
    /// Restated so the run's last word on a store full of retained
    /// garbage is never a bare "0 bytes".
    pub(crate) retained_reclaimable_segments: usize,
    pub(crate) retained_reclaimable_bytes: u64,
    pub(crate) purge: Option<PurgeFacts>,
    pub(crate) full_copy_planned: bool,
    pub(crate) backups_skipped: Option<SkipReason>,
}

/// The completion line: the head movement, and what left the store with it.
fn completion_line(
    complete: bool,
    head_movement: &str,
    removed_checkpoints: u64,
    removed_journal_lines: u64,
) -> String {
    let status = if complete {
        "compaction complete"
    } else {
        "compaction partially completed"
    };
    let mut removed = Vec::new();
    if removed_checkpoints != 0 {
        removed.push(count_noun(removed_checkpoints, "checkpoint", "checkpoints"));
    }
    if removed_journal_lines != 0 {
        removed.push(count_noun(
            removed_journal_lines,
            "journal line",
            "journal lines",
        ));
    }
    let mut line = format!("{status}: head {head_movement}");
    if !removed.is_empty() {
        let _ = write!(line, "; {} removed", removed.join(" and "));
    }
    line
}

/// The archive movement line: only the verbs that happened, then the byte
/// figures either way.
fn archives_line(
    rewritten: u64,
    reclaimed: u64,
    stale_removed: u64,
    orphan_segments: u64,
    bytes_before: u64,
    bytes_after: u64,
) -> String {
    let mut changes = Vec::new();
    if rewritten != 0 {
        changes.push(format!("{} rewritten", format_count(rewritten)));
    }
    if reclaimed != 0 {
        changes.push(format!("{} reclaimed", format_count(reclaimed)));
    }
    if stale_removed != 0 {
        changes.push(format!("{} stale removed", format_count(stale_removed)));
    }
    let mut line = String::from("archives: ");
    if !changes.is_empty() {
        line.push_str(&changes.join(", "));
        line.push_str("; ");
    }
    if orphan_segments != 0 {
        let _ = write!(
            line,
            "{} removed; ",
            count_noun(orphan_segments, "orphan segment", "orphan segments")
        );
    }
    let _ = write!(
        line,
        "{} -> {}",
        froe::format_byte_size(bytes_before),
        froe::format_byte_size(bytes_after),
    );
    line
}

/// The recovery-backups-on-disk line, when any bytes remain: how much, and
/// exactly why they are still there. The archive byte figures count active
/// archive names only, so without this a run that grew the directory
/// reports its size as unchanged.
fn recovery_backups_line(
    retained_bytes: u64,
    written_by_this_run: u64,
    backups_skipped: Option<SkipReason>,
) -> Option<String> {
    if retained_bytes == 0 {
        return None;
    }
    let size = froe::format_byte_size(retained_bytes);
    Some(match backups_skipped {
        None => {
            // Removal ran; what remains is this run's own journal backup
            // plus whatever the age/count retention window protected.
            let kept_by_policy = retained_bytes.saturating_sub(written_by_this_run);
            if kept_by_policy == 0 {
                format!("recovery backups on disk: {size}, written by this run")
            } else {
                format!(
                    "recovery backups on disk: {size} ({} kept by the age/count retention \
                     window; the rest written by this run)",
                    froe::format_byte_size(kept_by_policy)
                )
            }
        }
        Some(SkipReason::RepairRunsFirst) => format!(
            "recovery backups on disk: {size}; kept this run because the index repair writes \
             its own — the next run removes the old ones once the store verifies"
        ),
        Some(SkipReason::Flag) => format!(
            "recovery backups on disk: {size} (kept, as --skip-removing-recovery-backups \
             requests)"
        ),
        Some(SkipReason::Declined) => {
            format!(
                "recovery backups on disk: {size} (kept: their removal was declined at the prompt)"
            )
        }
        Some(SkipReason::Unconfirmed) => format!(
            "recovery backups on disk: {size} (kept: their removal had no confirmation; rerun \
             with --yes to remove them)"
        ),
        Some(SkipReason::TailCompaction) => {
            format!("recovery backups on disk: {size}")
        }
    })
}

/// The purge restatement, and — whenever retained checkpoints pin the
/// purged histories — the explanation for the one figure that otherwise
/// reads as impossible: a copy that wrote *more* node records than the
/// pre-purge head reached.
fn purge_lines(purge: &PurgeFacts) -> Vec<String> {
    let mut lines = vec![format!(
        "purged: {} ({} omitted from the copy)",
        count_noun(
            purge.histories,
            "orphaned version history",
            "orphaned version histories"
        ),
        count_noun(purge.nodes, "node", "nodes"),
    )];
    if purge.retained_checkpoints != 0 {
        lines.push(format!(
            "  {} of the purged histories until they expire; the copy therefore wrote the \
             head's rewritten version storage beside those snapshots, which is why it can \
             report more node records than the head reached before the purge — that overhead \
             returns when the checkpoints expire",
            count_noun(
                purge.retained_checkpoints,
                "retained checkpoint keeps its own snapshot",
                "retained checkpoints keep their own snapshots"
            ),
        ));
    }
    lines
}

pub(crate) fn print_cleanup_summary(
    outcome: &froe::CompactionOutcome,
    context: &CompletionContext,
) {
    println!(
        "{}",
        completion_line(
            outcome.is_complete(),
            &format!("{} -> {}", outcome.head_before, outcome.head_after),
            outcome.removed_checkpoints,
            outcome.removed_journal_lines as u64,
        )
    );
    println!(
        "{}",
        archives_line(
            outcome.rewritten_archives as u64,
            outcome.removed_reclaimable_archives as u64,
            outcome.removed_stale_archives as u64,
            outcome.removed_segments() as u64,
            outcome.archive_bytes_before,
            outcome.archive_bytes_after,
        )
    );
    if context.retained_reclaimable_segments != 0 {
        println!(
            "identified but retained: {} segments / {} of reclaimable garbage were left in archives that cannot be rewritten; see the warnings above",
            format_count(context.retained_reclaimable_segments as u64),
            froe::format_byte_size(context.retained_reclaimable_bytes),
        );
    }
    if outcome.repaired_archives != 0 {
        if outcome.repaired_archives == 1 {
            println!("archive indexes rebuilt: 1 (the original is retained under a .bak name)");
        } else {
            println!(
                "archive indexes rebuilt: {} (the originals are retained under .bak names)",
                format_count(outcome.repaired_archives as u64)
            );
        }
    }
    let journal_backup_bytes = outcome
        .journal_backup_path()
        .and_then(|path| std::fs::metadata(path).ok())
        .map_or(0, |metadata| metadata.len());
    if let Some(line) = recovery_backups_line(
        outcome.retained_recovery_backup_bytes,
        journal_backup_bytes,
        context.backups_skipped,
    ) {
        println!("{line}");
    }
    let mut removed_files = Vec::new();
    if outcome.removed_temporaries != 0 {
        removed_files.push(count_noun(
            outcome.removed_temporaries as u64,
            "stale temporary file",
            "stale temporary files",
        ));
    }
    if outcome.removed_recovery_backups != 0 {
        removed_files.push(count_noun(
            outcome.removed_recovery_backups as u64,
            "recovery backup",
            "recovery backups",
        ));
    }
    if !removed_files.is_empty() {
        println!("files removed: {}", removed_files.join(" and "));
    }
    if let Some(backup_path) = outcome.journal_backup_path() {
        println!(
            "journal recovery backup: {}",
            crate::output::sanitize_terminal_path(backup_path)
        );
    }
    if outcome.is_complete()
        && outcome.compacted.is_some()
        && let Some(purge) = &context.purge
    {
        for line in purge_lines(purge) {
            println!("{line}");
        }
    }
    if outcome.is_complete() && context.full_copy_planned && outcome.compacted.is_some() {
        // The promise must survive the repeat run it makes. Recovery
        // backups — this run's own journal backup and repair originals
        // included — are exactly what a repeat run's default removal would
        // still touch, so a store still holding any gets the narrower
        // promise.
        if outcome.retained_recovery_backup_bytes == 0 {
            println!("the store is now fully compacted; a repeat run will report nothing to do");
        } else {
            println!(
                "the store is now fully compacted; a repeat run has only recovery backups left to remove"
            );
        }
    }
}

/// The planned deletions the applied run could not perform or confirm
/// itself, and the partial-result summary they add up to.
pub(crate) fn print_deletion_failures(outcome: &froe::CompactionOutcome) {
    for failure in outcome.deletion_failures() {
        eprintln!("{}", cleanup_deletion_warning(failure));
    }
    if !outcome.is_complete() {
        eprintln!("{}", cleanup_partial_summary(outcome.deletion_failures()));
    }
}

fn cleanup_deletion_warning(failure: &FileDeletionFailure) -> String {
    cleanup_deletion_warning_fields(
        failure.file_name(),
        failure.error(),
        if failure.target_was_already_absent() {
            DeletionTarget::AlreadyAbsent
        } else {
            DeletionTarget::Retained
        },
    )
}

/// What a failed planned deletion left behind.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeletionTarget {
    /// Something else had already removed it, so the plan's intent holds.
    AlreadyAbsent,
    /// It is still on disk, and a later cleanup can retry.
    Retained,
}

fn cleanup_deletion_warning_fields(
    file_name: &str,
    detail: &str,
    target: DeletionTarget,
) -> String {
    let file_name = crate::output::sanitize_terminal_text(file_name);
    let detail = crate::output::sanitize_terminal_text(detail);
    if target == DeletionTarget::AlreadyAbsent {
        format!(
            "froe: warning: deletion of {file_name} was already satisfied outside this cleanup ({detail}); no deletion retry is needed"
        )
    } else {
        format!(
            "froe: warning: could not delete {file_name} ({detail}); the target was retained and a later cleanup can retry"
        )
    }
}

fn cleanup_partial_summary(failures: &[FileDeletionFailure]) -> String {
    let already_absent = failures
        .iter()
        .filter(|failure| failure.target_was_already_absent())
        .count();
    let retained = failures.len() - already_absent;
    cleanup_partial_summary_counts(retained, already_absent)
}

fn cleanup_partial_summary_counts(retained: usize, already_absent: usize) -> String {
    match (retained, already_absent) {
        (0, absent) => format!(
            "froe: compaction is partial because {absent} planned deletion targets were already absent and could not be confirmed as this run's work"
        ),
        (retained, 0) => format!(
            "froe: compaction is partial because {retained} planned file deletion targets remain"
        ),
        (retained, absent) => format!(
            "froe: compaction is partial because {retained} planned file deletion targets remain and {absent} were already absent"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_cleanup_diagnostics_distinguish_absent_targets_from_retained_ones() {
        let absent = cleanup_deletion_warning_fields(
            "journal.log.compacting",
            "file was already absent when deletion was attempted",
            DeletionTarget::AlreadyAbsent,
        );
        assert!(absent.contains("was already satisfied outside this cleanup"));
        assert!(absent.contains("no deletion retry is needed"));
        assert!(!absent.contains("target was retained"));

        let retained = cleanup_deletion_warning_fields(
            "journal.log.compacting",
            "permission denied",
            DeletionTarget::Retained,
        );
        assert!(retained.contains("could not delete journal.log.compacting"));
        assert!(retained.contains("target was retained"));
        assert!(retained.contains("a later cleanup can retry"));

        let absent_summary = cleanup_partial_summary_counts(0, 1);
        assert!(absent_summary.contains("were already absent"));
        assert!(!absent_summary.contains("deletion targets remain"));
        assert!(cleanup_partial_summary_counts(1, 0).contains("deletion targets remain"));
    }

    /// The one figure that reads as impossible — a copy reporting more
    /// node records than the pre-purge head — is explained exactly when it
    /// can happen: retained checkpoints keep their own snapshots of the
    /// purged histories, so the head's rewritten version storage stops
    /// sharing records with them.
    #[test]
    fn the_purge_summary_explains_the_node_count_only_when_checkpoints_pin_it() {
        let pinned = purge_lines(&PurgeFacts {
            histories: 31_473,
            nodes: 524_086,
            retained_checkpoints: 2,
        });
        assert_eq!(
            pinned[0],
            "purged: 31,473 orphaned version histories (524,086 nodes omitted from the copy)"
        );
        assert!(pinned[1].starts_with("  2 retained checkpoints keep their own snapshots"));
        assert!(
            pinned[1].contains("more node records than the head reached before the purge"),
            "the impossible-looking figure is named and resolved: {}",
            pinned[1]
        );

        let unpinned = purge_lines(&PurgeFacts {
            histories: 1,
            nodes: 2,
            retained_checkpoints: 0,
        });
        assert_eq!(
            unpinned,
            ["purged: 1 orphaned version history (2 nodes omitted from the copy)"],
            "without retained checkpoints there is nothing to explain"
        );
    }

    /// Counts agree with their nouns and zeros are omitted rather than
    /// reported: `1 journal line`, never `1 journal lines`, and no
    /// `0 checkpoints` filler.
    #[test]
    fn completion_lines_pluralize_and_omit_zero_counts() {
        let line = completion_line(true, "a -> b", 0, 1);
        assert_eq!(
            line,
            "compaction complete: head a -> b; 1 journal line removed"
        );

        let line = completion_line(true, "a -> b", 2, 4_366);
        assert_eq!(
            line,
            "compaction complete: head a -> b; 2 checkpoints and 4,366 journal lines removed"
        );

        let line = completion_line(true, "a -> b", 0, 0);
        assert_eq!(line, "compaction complete: head a -> b");

        let line = completion_line(false, "a -> b", 1, 0);
        assert_eq!(
            line,
            "compaction partially completed: head a -> b; 1 checkpoint removed"
        );
    }

    #[test]
    fn the_archives_line_reports_only_what_happened() {
        const GIBIBYTE: u64 = 1024 * 1024 * 1024;
        let line = archives_line(0, 26, 0, 25_806, 21 * GIBIBYTE, 20 * GIBIBYTE);
        assert_eq!(
            line,
            "archives: 26 reclaimed; 25,806 orphan segments removed; 21.0 GiB -> 20.0 GiB"
        );

        let line = archives_line(0, 0, 0, 0, 21 * GIBIBYTE, 21 * GIBIBYTE);
        assert_eq!(line, "archives: 21.0 GiB -> 21.0 GiB");

        let line = archives_line(2, 1, 3, 1, GIBIBYTE, GIBIBYTE);
        assert_eq!(
            line,
            "archives: 2 rewritten, 1 reclaimed, 3 stale removed; 1 orphan segment removed; 1.0 GiB -> 1.0 GiB"
        );
    }

    /// The backups-on-disk line states why the bytes are still there, and
    /// after a removal it distinguishes this run's own journal backup from
    /// what the retention window protected.
    #[test]
    fn the_recovery_backup_line_names_the_reason_the_bytes_remain() {
        let deferred =
            recovery_backups_line(121, 0, Some(SkipReason::RepairRunsFirst)).expect("bytes remain");
        assert!(deferred.contains("kept this run because the index repair writes its own"));
        assert!(deferred.contains("the next run removes the old ones"));

        let skipped = recovery_backups_line(121, 0, Some(SkipReason::Flag)).expect("bytes remain");
        assert!(skipped.contains("--skip-removing-recovery-backups"));

        let own_only = recovery_backups_line(121, 121, None).expect("bytes remain");
        assert_eq!(
            own_only,
            "recovery backups on disk: 121 bytes, written by this run"
        );

        let with_policy = recovery_backups_line(800, 121, None).expect("bytes remain");
        assert!(
            with_policy.contains("679 bytes kept by the age/count retention window"),
            "{with_policy}"
        );

        assert_eq!(recovery_backups_line(0, 0, None), None);
    }
}
