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
use froe::writer::store_writer::WritableRepository;
use froe::{
    CompactionAction, CompactionOptions, CompactionPlan, FileDeletionFailure, PreparedCompaction,
    backup_with_progress, plan_compaction_with_progress, recover_journal_with_progress,
    restore_with_progress,
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
/// Introduces the lock-protected plan, attributing why it differs.
///
/// When repairs were selected the plan changed because cleanup itself
/// rebuilt the indexes under the lock — that is the task working, not an
/// outside writer, and saying otherwise sends the operator hunting for a
/// process that does not exist.
fn announce_authoritative_plan(plan: &froe::CompactionPlan, repaired: usize) {
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
/// `froe compact`: the one maintenance command.
///
/// Plans read-only first and prints the plan, because the operator's evidence
/// for authorizing a destructive run has to exist before the run does. With
/// `--dry-run` that is the whole command. Otherwise the plan is confirmed, the
/// exclusive lock taken, the plan rebuilt from disk under it, and — if the
/// authoritative plan differs from what was confirmed — shown and confirmed
/// again before anything is touched.
pub(crate) fn run_compact(
    repository: &Path,
    options: CompactionOptions,
    dry_run: bool,
    assume_yes: bool,
    reporter: &Reporter,
) -> froe::Result<bool> {
    reporter.status(
        "note: archive rewrites require same-directory hard-link and directory-fsync support; an unsupported filesystem fails safely with source archives retained",
    );
    let preview = plan_compaction_with_progress(repository, &options, &mut reporter.clone())?;
    // The plan is the operator's evidence for a destructive decision: end
    // every report before a single line of it is written.
    reporter.finish();
    print_cleanup_plan(&preview);
    if dry_run {
        println!("dry-run: repository was not modified");
        return Ok(true);
    }
    if preview.is_empty() {
        println!("no maintenance mutations are needed; review any warnings above");
        return Ok(true);
    }
    if !confirm(
        &format!(
            "about to apply this compaction plan to {}",
            crate::output::sanitize_terminal_path(preview.directory())
        ),
        assume_yes,
        reporter,
    ) {
        eprintln!("froe: compaction cancelled");
        return Ok(false);
    }

    let prepared = PreparedCompaction::prepare_with_progress(
        preview.directory(),
        options,
        &mut reporter.clone(),
    )?;
    if prepared.plan() != &preview {
        reporter.finish();
        let repaired = prepared.repaired_archives();
        announce_authoritative_plan(prepared.plan(), repaired);
        if !confirm(
            "about to apply the changed authoritative compaction plan",
            assume_yes,
            reporter,
        ) {
            eprintln!("froe: compaction cancelled");
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
    fn of(plan: &froe::CompactionPlan) -> Self {
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
    outcome: &froe::CompactionOutcome,
    retention: RetentionSummary,
    complete: bool,
) {
    let status = if complete {
        "compaction complete"
    } else {
        "compaction partially completed"
    };
    println!(
        "{status}: head {} -> {}; {} checkpoints and {} journal lines removed",
        outcome.head_before,
        outcome.head_after,
        outcome.removed_checkpoints,
        outcome.removed_journal_lines,
    );
    println!(
        "archives: {} rewritten, {} reclaimed, {} stale removed; {} orphan segments removed; {} -> {}",
        outcome.rewritten_archives,
        outcome.removed_reclaimable_archives,
        outcome.removed_stale_archives,
        crate::progress::format_count(outcome.removed_segments() as u64),
        froe::format_byte_size(outcome.archive_bytes_before),
        froe::format_byte_size(outcome.archive_bytes_after),
    );
    if retention.segments != 0 {
        println!(
            "identified but retained: {} segments / {} of reclaimable garbage were left in archives that cannot be rewritten; see the warnings above",
            crate::progress::format_count(retention.segments as u64),
            froe::format_byte_size(retention.bytes),
        );
    }
    if retention.history_segments != 0 {
        println!(
            "journal history still protects {} data segments the head does not reach; retiring it would let this sweep free a further {} segments ({})",
            crate::progress::format_count(retention.history_segments as u64),
            crate::progress::format_count(retention.history_reclaimable_segments as u64),
            froe::format_byte_size(retention.history_reclaimable_bytes),
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
            "recovery backups on disk: {} (outside the archive figures above; retire with --backup-minimum-age-days and --backup-keep-latest)",
            froe::format_byte_size(outcome.retained_recovery_backup_bytes)
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

fn cleanup_deletion_warning(failure: &FileDeletionFailure) -> String {
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

#[allow(
    clippy::too_many_lines,
    reason = "the complete deterministic confirmation listing is clearest as one display pass"
)]
fn print_cleanup_plan(plan: &CompactionPlan) {
    println!(
        "compaction plan for {} (verified head {}):",
        crate::output::sanitize_terminal_path(plan.directory()),
        plan.current_head()
    );
    if plan.actions().is_empty() {
        println!("  no mutations");
    }
    for action in plan.actions() {
        match action {
            CompactionAction::RepairArchiveIndex {
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
                println!(
                    "    {} retained across the retired originals",
                    froe::format_byte_size(*bytes)
                );
            }
            CompactionAction::PruneJournal {
                lines,
                parser_ignored,
                missing_segments,
                unreadable_revisions,
                beyond_retention,
            } => println!(
                "  prune {lines} journal lines ({parser_ignored} parser-ignored, {missing_segments} missing-segment, {unreadable_revisions} unreadable historical, {beyond_retention} beyond retention)"
            ),
            CompactionAction::UpgradeManifest => {
                println!("  atomically upgrade manifest to store.version=2");
            }
            CompactionAction::RemoveCheckpoints {
                names,
                expired,
                unreferenced,
            } => {
                println!(
                    "  omit {} checkpoints from the copy ({expired} expired, {unreferenced} unreferenced):",
                    names.len()
                );
                for name in names {
                    println!(
                        "    checkpoint {}",
                        crate::output::quote_terminal_text(name)
                    );
                }
            }
            CompactionAction::RemoveReclaimableArchive {
                file_name,
                segments,
                bytes,
            } => println!(
                "  remove {file_name}: {segments} orphan segments, {}",
                froe::format_byte_size(*bytes)
            ),
            CompactionAction::RewriteArchive {
                file_name,
                replacement_name,
                segments,
                eligible_bytes,
            } => println!(
                "  rewrite {file_name} as {replacement_name}: omit {segments} orphan segments ({} of entries)",
                froe::format_byte_size(*eligible_bytes)
            ),
            CompactionAction::RemoveStaleArchive {
                file_name,
                reason,
                bytes,
            } => println!(
                "  remove stale archive {file_name} ({reason}; {})",
                froe::format_byte_size(*bytes)
            ),
            CompactionAction::RemoveTemporary { file_name, bytes } => {
                println!(
                    "  remove redundant temporary {file_name} ({})",
                    froe::format_byte_size(*bytes)
                );
            }
            CompactionAction::RetireJournalHistory { revisions } => {
                println!(
                    "  retire all {} journal lines, keeping only the compacted head",
                    crate::progress::format_count(*revisions as u64)
                );
                println!("    journal.log is copied to a numbered .bak first");
                println!("    the removed history is not recoverable from the store afterwards");
            }
            CompactionAction::RetireInterruptedCompactionResidue { segments } => {
                println!(
                    "  retire {} segments of interrupted-compaction residue",
                    crate::progress::format_count(*segments as u64)
                );
            }
            CompactionAction::CopyHeadIntoFreshGeneration {
                head_nodes,
                target_generation,
                kind,
            } => {
                println!(
                    "  {} compaction: copy the head into generation ({},{},compacted)",
                    match kind {
                        froe::CompactionKind::Full => "full",
                        froe::CompactionKind::Tail => "tail",
                    },
                    target_generation.generation,
                    target_generation.full_generation
                );
                println!(
                    "    the head reaches {} nodes; the copy rewrites those it still retains",
                    crate::progress::format_count(*head_nodes)
                );
            }
            CompactionAction::RemoveRecoveryBackup { file_name, bytes } => {
                println!(
                    "  remove old recovery backup {file_name} ({})",
                    froe::format_byte_size(*bytes)
                );
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
            CompactionAction::RepairArchiveIndex { bytes, .. } => Some(*bytes),
            _ => None,
        })
        .sum();
    if repair_bytes != 0 {
        let repair_size = froe::format_byte_size(repair_bytes);
        println!(
            "index rebuilds need {repair_size} of transient space and leave {repair_size} of .bak files: the repository grows until those are retired"
        );
    }
    println!(
        "estimated reclaimable: {}",
        froe::format_byte_size(plan.estimated_reclaimable_bytes())
    );
    // A zero estimate has two very different meanings — "no garbage" and
    // "garbage this run declined to move" — and the run used to print the
    // same line for both. These two say which one it is.
    if plan.retained_reclaimable_segments() != 0 {
        println!(
            "identified but retained: {} segments / {} of reclaimable garbage, left in archives this run cannot rewrite (see the warnings above)",
            crate::progress::format_count(plan.retained_reclaimable_segments() as u64),
            froe::format_byte_size(plan.retained_reclaimable_bytes()),
        );
    }
    let (history_reclaimable_segments, history_reclaimable_bytes) =
        plan.history_protected_reclaimable();
    if plan.history_protected_segments() != 0 {
        println!(
            "journal history protects {} data segments the current head does not reach; retiring that history would let this same sweep free {} segments ({}), binary content included",
            crate::progress::format_count(plan.history_protected_segments() as u64),
            crate::progress::format_count(history_reclaimable_segments as u64),
            froe::format_byte_size(history_reclaimable_bytes),
        );
        if history_reclaimable_segments != 0 {
            println!(
                "  to retire it: run `froe compact` on a stopped repository, or bound the journal with --retain-journal-revisions"
            );
        }
    }
    if plan.estimated_archive_rewrite_source_bytes() != 0 {
        println!(
            "archive rewrite working-space proxy: {} of source archives (additional headroom may be required)",
            froe::format_byte_size(plan.estimated_archive_rewrite_source_bytes())
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
