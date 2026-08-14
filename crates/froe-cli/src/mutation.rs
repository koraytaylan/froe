//! The mutating commands: compaction, `cleanup`, backup, restore, journal
//! recovery, and checkpoint management.
//!
//! Every one of these takes the exclusive repository lock, so it can never
//! run against a live AEM instance. Because they change the store on disk,
//! each requires explicit confirmation — either an interactive `yes` or
//! the `--yes` flag — before proceeding.

use std::io::Write;
use std::path::Path;

use froe::writer::commit::{
    create_checkpoint, release_checkpoint, remove_all_checkpoints, remove_unreferenced_checkpoints,
};
use froe::writer::compaction::CompactionKind;
use froe::writer::store_writer::WritableRepository;
use froe::{
    CleanupAction, CleanupDeletionFailure, CleanupOptions, CleanupPlan, PreparedCleanup,
    backup_with_progress, compact_with_progress, plan_cleanup_with_progress,
    recover_journal_with_progress, restore_with_progress,
};

use crate::progress::Reporter;

/// Asks for confirmation before a mutating operation, unless `assume_yes`.
///
/// The prompt is written with the reporter suspended, so a live progress
/// line is erased first and nothing is drawn over the question while the
/// operator is answering it. `--silent` never hides this prompt: it is a
/// question about a destructive change, not a progress report.
fn confirm(action: &str, assume_yes: bool, reporter: &Reporter) -> bool {
    if assume_yes {
        return true;
    }
    reporter.while_suspended(|| {
        let _ = std::io::stdout().flush();
        eprint!("froe: {action} — this modifies the repository. Continue? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
    })
}

/// `froe compact`: offline full or tail compaction.
pub(crate) fn run_compact(
    repository: &Path,
    tail: bool,
    assume_yes: bool,
    reporter: &Reporter,
) -> froe::Result<bool> {
    let kind = if tail {
        CompactionKind::Tail
    } else {
        CompactionKind::Full
    };
    reporter.status(
        "note: post-compaction TAR rewrites require same-directory hard-link and directory-fsync support; an unsupported filesystem fails safely with source archives retained",
    );
    if !confirm(
        &format!(
            "about to run {} compaction on {}",
            kind_name(kind),
            crate::output::sanitize_terminal_path(repository)
        ),
        assume_yes,
        reporter,
    ) {
        eprintln!("froe: compaction cancelled");
        return Ok(false);
    }
    let mut store = WritableRepository::open_with_progress(repository, &mut reporter.clone())?;
    let outcome = compact_with_progress(&mut store, kind, &mut reporter.clone())?;
    store.close()?;
    reporter.finish();
    println!(
        "compacted {} nodes; {} bytes -> {} bytes ({} reclaimed)",
        outcome.compacted_nodes,
        outcome.size_before,
        outcome.size_after,
        outcome.size_before.saturating_sub(outcome.size_after),
    );
    Ok(true)
}

fn kind_name(kind: CompactionKind) -> &'static str {
    match kind {
        CompactionKind::Full => "full",
        CompactionKind::Tail => "tail",
    }
}

/// Introduces the lock-protected plan, attributing why it differs.
///
/// When repairs were selected the plan changed because cleanup itself
/// rebuilt the indexes under the lock — that is the task working, not an
/// outside writer, and saying otherwise sends the operator hunting for a
/// process that does not exist.
fn announce_authoritative_plan(plan: &froe::CleanupPlan, repaired: usize) {
    if repaired == 0 {
        eprintln!(
            "froe: repository state changed before the lock was acquired; authoritative plan:"
        );
    } else {
        eprintln!(
            "froe: rebuilt the index of {repaired} archive(s) under the repository lock, \
             retaining the originals under .bak names; everything below could only be planned \
             once that was done:"
        );
    }
    print_cleanup_plan(plan);
}

/// `froe cleanup`: read-only preview, lock-protected replan, confirmation,
/// application, and fresh final verification.
pub(crate) fn run_cleanup(
    repository: &Path,
    options: CleanupOptions,
    dry_run: bool,
    assume_yes: bool,
    reporter: &Reporter,
) -> froe::Result<bool> {
    let preview = plan_cleanup_with_progress(repository, &options, &mut reporter.clone())?;
    // The plan is the operator's evidence for a destructive decision: end
    // every report before a single line of it is written.
    reporter.finish();
    print_cleanup_plan(&preview);
    if dry_run {
        println!("dry-run: repository was not modified");
        return Ok(true);
    }
    if preview.is_empty() {
        println!("no cleanup mutations are needed; review any warnings above");
        return Ok(true);
    }
    if !confirm(
        &format!(
            "about to apply this cleanup plan to {}",
            crate::output::sanitize_terminal_path(preview.directory())
        ),
        assume_yes,
        reporter,
    ) {
        eprintln!("froe: cleanup cancelled");
        return Ok(false);
    }

    let prepared = PreparedCleanup::prepare_with_progress(
        preview.directory(),
        options,
        &mut reporter.clone(),
    )?;
    if prepared.plan() != &preview {
        reporter.finish();
        let repaired = prepared.repaired_archives();
        announce_authoritative_plan(prepared.plan(), repaired);
        if !confirm(
            "about to apply the changed authoritative cleanup plan",
            assume_yes,
            reporter,
        ) {
            eprintln!("froe: cleanup cancelled");
            if repaired != 0 {
                // The repair is already durable. Saying "cancelled" alone
                // would imply the store is untouched, and it is not.
                eprintln!(
                    "froe: note: the {repaired} archive index rebuild(s) above were already \
                     applied and are not undone by cancelling; the originals remain under \
                     .bak names"
                );
            }
            return Ok(false);
        }
    }
    // The preview has served its only purpose — the comparison above — and
    // holds a second copy of every store-wide set the plan carries. Release
    // it before the apply rather than pinning both plans through the phase
    // that needs the memory.
    drop(preview);
    // Captured from the authoritative plan before it is consumed: the
    // preview's figures can be stale, and the operator reads the summary
    // after minutes of progress output has scrolled the warnings away.
    let retention = RetentionSummary::of(prepared.plan());
    let outcome = prepared.apply_with_progress(&mut reporter.clone())?;
    reporter.finish();
    let complete = outcome.is_complete();
    print_cleanup_summary(&outcome, retention, complete);
    for failure in outcome.deletion_failures() {
        eprintln!("{}", cleanup_deletion_warning(failure));
    }
    if !complete {
        eprintln!("{}", cleanup_partial_summary(outcome.deletion_failures()));
    }
    Ok(complete)
}

/// What the applied plan identified as reclaimable and then kept.
///
/// Read off the authoritative plan before it is consumed, so the summary can
/// restate it once the progress output has scrolled the plan's own warnings
/// out of view. Without this the run's last word on a store full of retained
/// garbage is a bare "0 bytes".
#[derive(Clone, Copy)]
struct RetentionSummary {
    segments: usize,
    bytes: u64,
    history_segments: usize,
    history_reclaimable_segments: usize,
    history_reclaimable_bytes: u64,
}

impl RetentionSummary {
    fn of(plan: &froe::CleanupPlan) -> Self {
        let (history_reclaimable_segments, history_reclaimable_bytes) =
            plan.history_protected_reclaimable();
        Self {
            segments: plan.retained_reclaimable_segments(),
            bytes: plan.retained_reclaimable_bytes(),
            history_segments: plan.history_protected_segments(),
            history_reclaimable_segments,
            history_reclaimable_bytes,
        }
    }
}

fn print_cleanup_summary(
    outcome: &froe::CleanupOutcome,
    retention: RetentionSummary,
    complete: bool,
) {
    let status = if complete {
        "cleanup complete"
    } else {
        "cleanup partially completed"
    };
    println!(
        "{status}: head {} -> {}; {} checkpoints and {} journal lines removed",
        outcome.head_before,
        outcome.head_after,
        outcome.removed_checkpoints,
        outcome.removed_journal_lines,
    );
    println!(
        "archives: {} rewritten, {} reclaimed, {} stale removed; {} orphan segments removed; {} -> {} bytes",
        outcome.rewritten_archives,
        outcome.removed_reclaimable_archives,
        outcome.removed_stale_archives,
        outcome.removed_segments(),
        outcome.archive_bytes_before,
        outcome.archive_bytes_after,
    );
    if retention.segments != 0 {
        println!(
            "identified but retained: {} segments / {} bytes of reclaimable garbage were left in place; rewriting their archives does not repay the rewrite",
            retention.segments, retention.bytes,
        );
    }
    if retention.history_segments != 0 {
        println!(
            "journal history still protects {} data segments the head does not reach; retiring it would let this sweep free a further {} segments ({} bytes)",
            retention.history_segments,
            retention.history_reclaimable_segments,
            retention.history_reclaimable_bytes,
        );
    }
    if outcome.repaired_archives != 0 {
        println!(
            "archive indexes rebuilt: {} (originals retained under .bak names; a later run with --task recovery-backups can retire them once the store is verified)",
            outcome.repaired_archives
        );
    }
    // The archive byte figures above count active archive names only, so a
    // run that retires an original to a `.bak` reports no change while the
    // directory grew. State the held bytes rather than let the summary imply
    // the store stayed the same size.
    if outcome.retained_recovery_backup_bytes != 0 {
        println!(
            "recovery backups on disk: {} bytes (outside the archive figures above; retire with --task recovery-backups)",
            outcome.retained_recovery_backup_bytes
        );
    }
    if outcome.removed_temporaries != 0 || outcome.removed_recovery_backups != 0 {
        println!(
            "files: {} stale temporaries and {} recovery backups removed",
            outcome.removed_temporaries, outcome.removed_recovery_backups
        );
    }
    if let Some(backup_path) = outcome.journal_backup_path() {
        println!(
            "journal recovery backup: {}",
            crate::output::sanitize_terminal_path(backup_path)
        );
    }
}

fn cleanup_deletion_warning(failure: &CleanupDeletionFailure) -> String {
    cleanup_deletion_warning_fields(
        failure.file_name(),
        failure.error(),
        failure.target_was_already_absent(),
    )
}

fn cleanup_deletion_warning_fields(
    file_name: &str,
    detail: &str,
    target_was_already_absent: bool,
) -> String {
    let file_name = crate::output::sanitize_terminal_text(file_name);
    let detail = crate::output::sanitize_terminal_text(detail);
    if target_was_already_absent {
        format!(
            "froe: warning: deletion of {file_name} was already satisfied outside this cleanup ({detail}); no deletion retry is needed"
        )
    } else {
        format!(
            "froe: warning: could not delete {file_name} ({detail}); the target was retained and a later cleanup can retry"
        )
    }
}

fn cleanup_partial_summary(failures: &[CleanupDeletionFailure]) -> String {
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
            "froe: cleanup is partial because {absent} planned deletion targets were already absent and could not be confirmed as this cleanup's work"
        ),
        (retained, 0) => format!(
            "froe: cleanup is partial because {retained} planned file deletion targets remain"
        ),
        (retained, absent) => format!(
            "froe: cleanup is partial because {retained} planned file deletion targets remain and {absent} were already absent"
        ),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the complete deterministic confirmation listing is clearest as one display pass"
)]
fn print_cleanup_plan(plan: &CleanupPlan) {
    println!(
        "cleanup plan for {} (verified head {}):",
        crate::output::sanitize_terminal_path(plan.directory()),
        plan.current_head()
    );
    if plan.tasks().is_empty() {
        println!("  selected tasks: none (health verification only)");
    } else {
        let selected = plan
            .tasks()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        println!("  selected tasks: {selected}");
    }
    if plan.actions().is_empty() {
        println!("  no mutations");
    }
    for action in plan.actions() {
        match action {
            CleanupAction::RepairArchiveIndex {
                file_name,
                retired_file_names,
                reason,
                bytes,
            } => {
                println!(
                    "  repair the index of {} ({}; original retained as {}.bak)",
                    crate::output::sanitize_terminal_text(file_name),
                    crate::output::sanitize_terminal_text(reason),
                    crate::output::sanitize_terminal_text(file_name),
                );
                if !retired_file_names.is_empty() {
                    // These leave the archive namespace, so they are named
                    // rather than counted: the confirmation covers the files
                    // the plan printed.
                    let names: Vec<_> = retired_file_names
                        .iter()
                        .map(|name| crate::output::sanitize_terminal_text(name))
                        .collect();
                    println!(
                        "    merging and retiring {} to .bak names",
                        names.join(", ")
                    );
                }
                println!("    {bytes} bytes retained across the retired originals");
            }
            CleanupAction::PruneJournal {
                lines,
                parser_ignored,
                missing_segments,
                unreadable_revisions,
                beyond_retention,
            } => println!(
                "  prune {lines} journal lines ({parser_ignored} parser-ignored, {missing_segments} missing-segment, {unreadable_revisions} unreadable historical, {beyond_retention} beyond retention)"
            ),
            CleanupAction::UpgradeManifest => {
                println!("  atomically upgrade manifest to store.version=2");
            }
            CleanupAction::RemoveCheckpoints {
                names,
                expired,
                unreferenced,
            } => {
                println!(
                    "  remove {} checkpoints ({expired} expired, {unreferenced} unreferenced):",
                    names.len()
                );
                for name in names {
                    println!(
                        "    checkpoint {}",
                        crate::output::quote_terminal_text(name)
                    );
                }
            }
            CleanupAction::RemoveReclaimableArchive {
                file_name,
                segments,
                bytes,
            } => println!("  remove {file_name}: {segments} orphan segments, {bytes} bytes"),
            CleanupAction::RewriteArchive {
                file_name,
                replacement_name,
                segments,
                eligible_bytes,
            } => println!(
                "  rewrite {file_name} as {replacement_name}: omit {segments} orphan segments ({eligible_bytes} entry bytes)"
            ),
            CleanupAction::RemoveStaleArchive {
                file_name,
                reason,
                bytes,
            } => println!("  remove stale archive {file_name} ({reason}; {bytes} bytes)"),
            CleanupAction::RemoveTemporary { file_name, bytes } => {
                println!("  remove redundant temporary {file_name} ({bytes} bytes)");
            }
            CleanupAction::RemoveRecoveryBackup { file_name, bytes } => {
                println!("  remove old recovery backup {file_name} ({bytes} bytes)");
            }
            _ => println!("  apply an action added by this froe version"),
        }
    }
    for removal in plan.journal_line_removals() {
        let preview = crate::output::escape_terminal_bytes(removal.preview_bytes());
        let truncated = if removal.preview_truncated() {
            "…"
        } else {
            ""
        };
        if let Some(identifier) = removal.record_identifier() {
            println!(
                "    journal line {}: {} ({identifier}); b\"{preview}\"{truncated}",
                removal.line_number(),
                removal.reason(),
            );
        } else {
            println!(
                "    journal line {}: {}; b\"{preview}\"{truncated}",
                removal.line_number(),
                removal.reason(),
            );
        }
    }
    for warning in plan.warnings() {
        eprintln!(
            "froe: warning: {}",
            crate::output::sanitize_terminal_text(warning)
        );
    }
    // Every other estimate here describes space the run gives back. Repair is
    // the one action that takes space and keeps it, so it is stated
    // separately rather than netted against a reclaim figure it would
    // silently contradict.
    let repair_bytes: u64 = plan
        .actions()
        .iter()
        .filter_map(|action| match action {
            CleanupAction::RepairArchiveIndex { bytes, .. } => Some(*bytes),
            _ => None,
        })
        .sum();
    if repair_bytes != 0 {
        println!(
            "index rebuilds need {repair_bytes} bytes of transient space and leave {repair_bytes} bytes of .bak files: the repository grows until those are retired"
        );
    }
    println!(
        "estimated reclaimable bytes: {}",
        plan.estimated_reclaimable_bytes()
    );
    // A zero estimate has two very different meanings — "no garbage" and
    // "garbage this run declined to move" — and the run used to print the
    // same line for both. These two say which one it is.
    if plan.retained_reclaimable_segments() != 0 {
        println!(
            "identified but retained: {} segments / {} bytes of reclaimable garbage, left in place because rewriting their archives does not repay the rewrite (see the warnings above)",
            plan.retained_reclaimable_segments(),
            plan.retained_reclaimable_bytes(),
        );
    }
    let (history_reclaimable_segments, history_reclaimable_bytes) =
        plan.history_protected_reclaimable();
    if plan.history_protected_segments() != 0 {
        println!(
            "journal history protects {} data segments the current head does not reach; retiring that history would let this same sweep free {} segments ({} bytes), binary content included",
            plan.history_protected_segments(),
            history_reclaimable_segments,
            history_reclaimable_bytes,
        );
        if history_reclaimable_segments != 0 {
            println!(
                "  to retire it: run `froe compact` on a stopped repository, or bound the journal with --retain-journal-revisions"
            );
        }
    }
    if plan.estimated_archive_rewrite_source_bytes() != 0 {
        println!(
            "archive rewrite working-space proxy: {} source bytes (additional headroom may be required)",
            plan.estimated_archive_rewrite_source_bytes()
        );
    }
}

/// `froe backup`: copy the source repository's head into a target.
pub(crate) fn run_backup(
    source: &Path,
    target: &Path,
    assume_yes: bool,
    reporter: &Reporter,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to back up {} into {}",
            crate::output::sanitize_terminal_path(source),
            crate::output::sanitize_terminal_path(target)
        ),
        assume_yes,
        reporter,
    ) {
        eprintln!("froe: backup cancelled");
        return Ok(false);
    }
    backup_with_progress(source, target, &mut reporter.clone())?;
    reporter.finish();
    println!(
        "backup complete: {} -> {}",
        crate::output::sanitize_terminal_path(source),
        crate::output::sanitize_terminal_path(target)
    );
    Ok(true)
}

/// `froe restore`: copy a backup's head into an existing store.
pub(crate) fn run_restore(
    backup_directory: &Path,
    target: &Path,
    assume_yes: bool,
    reporter: &Reporter,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to restore {} into {} (overwriting its head)",
            crate::output::sanitize_terminal_path(backup_directory),
            crate::output::sanitize_terminal_path(target)
        ),
        assume_yes,
        reporter,
    ) {
        eprintln!("froe: restore cancelled");
        return Ok(false);
    }
    restore_with_progress(backup_directory, target, &mut reporter.clone())?;
    reporter.finish();
    println!(
        "restore complete: {} -> {}",
        crate::output::sanitize_terminal_path(backup_directory),
        crate::output::sanitize_terminal_path(target)
    );
    Ok(true)
}

/// `froe recover-journal`: rebuild journal.log from the segments.
pub(crate) fn run_recover_journal(
    repository: &Path,
    assume_yes: bool,
    reporter: &Reporter,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to rebuild the journal of {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        assume_yes,
        reporter,
    ) {
        eprintln!("froe: recovery cancelled");
        return Ok(false);
    }
    let outcome = recover_journal_with_progress(repository, &mut reporter.clone())?;
    reporter.finish();
    println!(
        "recovered head {} from {} candidates",
        outcome.recovered_head, outcome.candidates_examined
    );
    if let Some(backup_path) = outcome.previous_journal_backup {
        println!(
            "previous journal backed up at {}",
            crate::output::sanitize_terminal_path(&backup_path)
        );
    }
    Ok(true)
}

/// `froe checkpoint create`: create a checkpoint.
pub(crate) fn run_checkpoint_create(
    repository: &Path,
    lifetime_milliseconds: i64,
    assume_yes: bool,
    reporter: &Reporter,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to create a checkpoint in {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        assume_yes,
        reporter,
    ) {
        eprintln!("froe: checkpoint creation cancelled");
        return Ok(false);
    }
    let store = WritableRepository::open_with_progress(repository, &mut reporter.clone())?;
    let name = create_checkpoint(&store, lifetime_milliseconds, &[])?;
    store.close()?;
    println!(
        "created checkpoint {}",
        crate::output::sanitize_terminal_text(&name)
    );
    Ok(true)
}

/// `froe checkpoint remove`: remove a checkpoint by name, all, or
/// unreferenced ones.
pub(crate) fn run_checkpoint_remove(
    repository: &Path,
    target: &CheckpointRemoval,
    assume_yes: bool,
    reporter: &Reporter,
) -> froe::Result<bool> {
    let target_description = match target {
        CheckpointRemoval::Named(name) => {
            format!("checkpoint {}", crate::output::quote_terminal_text(name))
        }
        CheckpointRemoval::All => "every checkpoint".to_owned(),
        CheckpointRemoval::Unreferenced => "unreferenced checkpoints".to_owned(),
    };
    if !confirm(
        &format!(
            "about to remove {target_description} from {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        assume_yes,
        reporter,
    ) {
        eprintln!("froe: checkpoint removal cancelled");
        return Ok(false);
    }
    let store = WritableRepository::open_with_progress(repository, &mut reporter.clone())?;
    match target {
        CheckpointRemoval::Named(name) => {
            if release_checkpoint(&store, name)? {
                println!(
                    "removed checkpoint {}",
                    crate::output::quote_terminal_text(name)
                );
            } else {
                println!(
                    "no checkpoint named {}",
                    crate::output::quote_terminal_text(name)
                );
            }
        }
        CheckpointRemoval::All => {
            let removed = remove_all_checkpoints(&store)?;
            println!("removed {removed} checkpoints");
        }
        CheckpointRemoval::Unreferenced => {
            let removed = remove_unreferenced_checkpoints(&store)?;
            println!("removed {removed} unreferenced checkpoints");
        }
    }
    store.close()?;
    Ok(true)
}

/// Which checkpoints a removal targets.
pub(crate) enum CheckpointRemoval {
    /// One checkpoint by name.
    Named(String),
    /// Every checkpoint.
    All,
    /// Checkpoints not referenced by the asynchronous indexer.
    Unreferenced,
}

#[cfg(test)]
mod tests {
    use super::{cleanup_deletion_warning_fields, cleanup_partial_summary_counts};

    #[test]
    fn partial_cleanup_diagnostics_distinguish_absent_targets_from_retained_ones() {
        let absent = cleanup_deletion_warning_fields(
            "journal.log.compacting",
            "file was already absent when deletion was attempted",
            true,
        );
        assert!(absent.contains("was already satisfied outside this cleanup"));
        assert!(absent.contains("no deletion retry is needed"));
        assert!(!absent.contains("target was retained"));

        let retained =
            cleanup_deletion_warning_fields("journal.log.compacting", "permission denied", false);
        assert!(retained.contains("could not delete journal.log.compacting"));
        assert!(retained.contains("target was retained"));
        assert!(retained.contains("a later cleanup can retry"));

        let absent_summary = cleanup_partial_summary_counts(0, 1);
        assert!(absent_summary.contains("were already absent"));
        assert!(!absent_summary.contains("deletion targets remain"));
        assert!(cleanup_partial_summary_counts(1, 0).contains("deletion targets remain"));
    }
}
