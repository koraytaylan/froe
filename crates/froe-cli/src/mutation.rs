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
    CleanupAction, CleanupDeletionFailure, CleanupOptions, CleanupPlan, PreparedCleanup, backup,
    compact, plan_cleanup, recover_journal, restore,
};

/// Asks for confirmation before a mutating operation, unless `assume_yes`.
fn confirm(action: &str, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    let _ = std::io::stdout().flush();
    eprint!("froe: {action} — this modifies the repository. Continue? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
}

/// `froe compact`: offline full or tail compaction.
pub(crate) fn run_compact(repository: &Path, tail: bool, assume_yes: bool) -> froe::Result<bool> {
    let kind = if tail {
        CompactionKind::Tail
    } else {
        CompactionKind::Full
    };
    eprintln!(
        "froe: note: post-compaction TAR rewrites require same-directory hard-link and directory-fsync support; an unsupported filesystem fails safely with source archives retained"
    );
    if !confirm(
        &format!(
            "about to run {} compaction on {}",
            kind_name(kind),
            crate::output::sanitize_terminal_path(repository)
        ),
        assume_yes,
    ) {
        eprintln!("froe: compaction cancelled");
        return Ok(false);
    }
    let mut store = WritableRepository::open(repository)?;
    let outcome = compact(&mut store, kind)?;
    store.close()?;
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

/// `froe cleanup`: read-only preview, lock-protected replan, confirmation,
/// application, and fresh final verification.
pub(crate) fn run_cleanup(
    repository: &Path,
    options: CleanupOptions,
    dry_run: bool,
    assume_yes: bool,
) -> froe::Result<bool> {
    let preview = plan_cleanup(repository, &options)?;
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
    ) {
        eprintln!("froe: cleanup cancelled");
        return Ok(false);
    }

    let prepared = PreparedCleanup::prepare(preview.directory(), options)?;
    if prepared.plan() != &preview {
        eprintln!(
            "froe: repository state changed before the lock was acquired; authoritative plan:"
        );
        print_cleanup_plan(prepared.plan());
        if !confirm(
            "about to apply the changed authoritative cleanup plan",
            assume_yes,
        ) {
            eprintln!("froe: cleanup cancelled");
            return Ok(false);
        }
    }
    let outcome = prepared.apply()?;
    let complete = outcome.is_complete();
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
    for failure in outcome.deletion_failures() {
        eprintln!("{}", cleanup_deletion_warning(failure));
    }
    if !complete {
        eprintln!("{}", cleanup_partial_summary(outcome.deletion_failures()));
    }
    Ok(complete)
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
            CleanupAction::PruneJournal {
                lines,
                parser_ignored,
                missing_segments,
                unreadable_revisions,
            } => println!(
                "  prune {lines} journal lines ({parser_ignored} parser-ignored, {missing_segments} missing-segment, {unreadable_revisions} unreadable historical)"
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
    println!(
        "estimated reclaimable bytes: {}",
        plan.estimated_reclaimable_bytes()
    );
    if plan.estimated_archive_rewrite_source_bytes() != 0 {
        println!(
            "archive rewrite working-space proxy: {} source bytes (additional headroom may be required)",
            plan.estimated_archive_rewrite_source_bytes()
        );
    }
}

/// `froe backup`: copy the source repository's head into a target.
pub(crate) fn run_backup(source: &Path, target: &Path, assume_yes: bool) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to back up {} into {}",
            crate::output::sanitize_terminal_path(source),
            crate::output::sanitize_terminal_path(target)
        ),
        assume_yes,
    ) {
        eprintln!("froe: backup cancelled");
        return Ok(false);
    }
    backup(source, target)?;
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
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to restore {} into {} (overwriting its head)",
            crate::output::sanitize_terminal_path(backup_directory),
            crate::output::sanitize_terminal_path(target)
        ),
        assume_yes,
    ) {
        eprintln!("froe: restore cancelled");
        return Ok(false);
    }
    restore(backup_directory, target)?;
    println!(
        "restore complete: {} -> {}",
        crate::output::sanitize_terminal_path(backup_directory),
        crate::output::sanitize_terminal_path(target)
    );
    Ok(true)
}

/// `froe recover-journal`: rebuild journal.log from the segments.
pub(crate) fn run_recover_journal(repository: &Path, assume_yes: bool) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to rebuild the journal of {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        assume_yes,
    ) {
        eprintln!("froe: recovery cancelled");
        return Ok(false);
    }
    let outcome = recover_journal(repository)?;
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
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to create a checkpoint in {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        assume_yes,
    ) {
        eprintln!("froe: checkpoint creation cancelled");
        return Ok(false);
    }
    let store = WritableRepository::open(repository)?;
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
    ) {
        eprintln!("froe: checkpoint removal cancelled");
        return Ok(false);
    }
    let store = WritableRepository::open(repository)?;
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
