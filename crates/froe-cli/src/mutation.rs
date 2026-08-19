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

/// Whether a mutating command asks the operator before it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confirmation {
    /// Prompt on the terminal, and abort unless the answer is yes.
    Ask,
    /// Proceed without prompting, as `--yes` requests.
    AssumeYes,
}

impl Confirmation {
    /// Maps the parsed `--yes` flag, the one place a bare flag becomes a
    /// confirmation, so no command below this boundary takes one.
    pub(crate) fn from_assume_yes_flag(assume_yes: bool) -> Self {
        if assume_yes {
            Self::AssumeYes
        } else {
            Self::Ask
        }
    }
}

/// Whether `froe compact` stops after reporting its plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionRun {
    /// Report the plan and leave the repository untouched, as `--dry-run`
    /// requests.
    PlanOnly,
    /// Apply the plan once the operator has confirmed it.
    Apply,
}

impl CompactionRun {
    /// Maps the parsed `--dry-run` flag.
    pub(crate) fn from_dry_run_flag(dry_run: bool) -> Self {
        if dry_run { Self::PlanOnly } else { Self::Apply }
    }
}

/// Asks for confirmation before a mutating operation.
///
/// The prompt is written with the reporter suspended, so a live progress
/// line is erased first and nothing is drawn over the question while the
/// operator is answering it. `--silent` never hides this prompt: it is a
/// question about a destructive change, not a progress report.
fn confirm(action: &str, confirmation: Confirmation, reporter: &Reporter) -> bool {
    if confirmation == Confirmation::AssumeYes {
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

/// `froe compact`: the one maintenance command — offline full or tail
/// compaction, planned once under the exclusive lock, confirmed, applied,
/// and proven by a fresh final verification.
///
/// With `--dry-run` the command plans read-only, prints the plan, and takes
/// no lock. Otherwise the lock is acquired *first* and the one plan is built
/// under it, so the plan the operator confirms is byte-for-byte the plan
/// that applies: nothing can change the store between the evidence and the
/// decision, and an accidentally started Oak cannot open the store while
/// the operator is reading. Index repairs selected with
/// `--repair-archive-indexes` run under the same lock before planning, so
/// even a repairing run shows exactly one plan. The store is offline by
/// precondition, which is what makes holding the lock through the prompt a
/// strengthening rather than a discourtesy.
pub(crate) fn run_compact(
    repository: &Path,
    options: CompactionOptions,
    run_mode: CompactionRun,
    confirmation: Confirmation,
    reporter: &Reporter,
) -> froe::Result<bool> {
    reporter.status(
        "note: archive rewrites require same-directory hard-link and directory-fsync support; an unsupported filesystem fails safely with source archives retained",
    );
    if run_mode == CompactionRun::PlanOnly {
        let preview = plan_compaction_with_progress(repository, &options, &mut reporter.clone())?;
        // The plan is the operator's evidence: end every report before a
        // single line of it is written.
        reporter.finish();
        print_cleanup_plan(&preview);
        println!("dry-run: repository was not modified");
        return Ok(true);
    }

    let prepared =
        PreparedCompaction::prepare_with_progress(repository, options, &mut reporter.clone())?;
    reporter.finish();
    print_cleanup_plan(prepared.plan());
    if prepared.plan().is_empty() {
        if prepared.plan().already_fully_compacted() {
            println!("the store is already fully compacted; nothing to do");
        } else {
            println!("no maintenance mutations are needed; review any warnings above");
        }
        let repaired = prepared.repaired_archives();
        if repaired != 0 {
            // "Nothing to do" must not read as "nothing happened": index
            // rebuilds run before planning and are already durable.
            println!(
                "note: {repaired} archive index rebuild(s) above were already applied; \
                 the originals remain under .bak names"
            );
        }
        return Ok(true);
    }
    if !confirm(
        &format!(
            "about to apply this compaction plan to {}",
            crate::output::sanitize_terminal_path(prepared.plan().directory())
        ),
        confirmation,
        reporter,
    ) {
        eprintln!("froe: compaction cancelled");
        let repaired = prepared.repaired_archives();
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
    // Captured before the plan is consumed: the operator reads the summary
    // after minutes of progress output has scrolled the warnings away.
    let retention = RetentionSummary::of(prepared.plan());
    let full_copy_planned =
        prepared.plan().effective_compaction_kind() == Some(froe::CompactionKind::Full);
    let purge_summary = purge_summary_of(prepared.plan());
    let outcome = prepared.apply_with_progress(&mut reporter.clone())?;
    reporter.finish();
    let complete = outcome.is_complete();
    print_cleanup_summary(&outcome, retention);
    if complete
        && let Some((histories, purged_nodes, head_nodes_before)) = purge_summary
        && let Some(compacted) = outcome.compacted
    {
        println!(
            "nodes: {} -> {} (removed {} orphaned version histories holding {} nodes)",
            crate::progress::format_count(head_nodes_before),
            crate::progress::format_count(compacted.nodes),
            crate::progress::format_count(histories),
            crate::progress::format_count(purged_nodes),
        );
    }
    if complete && full_copy_planned && outcome.compacted.is_some() {
        println!("the store is now fully compacted; a repeat run will report nothing to do");
    }
    for failure in outcome.deletion_failures() {
        eprintln!("{}", cleanup_deletion_warning(failure));
    }
    if !complete {
        eprintln!("{}", cleanup_partial_summary(outcome.deletion_failures()));
    }
    Ok(complete)
}

/// The purge the confirmed plan carries, as `(histories, nodes, head
/// nodes before)`, so the summary can state the content delta after the
/// progress output has scrolled the plan away.
fn purge_summary_of(plan: &froe::CompactionPlan) -> Option<(u64, u64, u64)> {
    let mut purge = None;
    let mut head_nodes_before = 0;
    for action in plan.actions() {
        match action {
            CompactionAction::PurgeOrphanedVersionHistories {
                histories, nodes, ..
            } => {
                purge = Some((*histories, *nodes));
            }
            CompactionAction::CopyHeadIntoFreshGeneration { head_nodes, .. } => {
                head_nodes_before = *head_nodes;
            }
            _ => {}
        }
    }
    purge.map(|(histories, nodes)| (histories, nodes, head_nodes_before))
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
}

impl RetentionSummary {
    fn of(plan: &froe::CompactionPlan) -> Self {
        Self {
            segments: plan.retained_reclaimable_segments(),
            bytes: plan.retained_reclaimable_bytes(),
        }
    }
}

fn print_cleanup_summary(outcome: &froe::CompactionOutcome, retention: RetentionSummary) {
    let status = if outcome.is_complete() {
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
    if outcome.repaired_archives != 0 {
        println!(
            "archive indexes rebuilt: {} (originals retained under .bak names; a later run with --backup-minimum-age-days and --backup-keep-latest can retire them once the store is verified)",
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

/// One line per planned action, in the order the plan lists them.
fn print_planned_action(action: &CompactionAction) {
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
        other => print_store_action(other),
    }
}

/// Actions on the journal, checkpoints, manifest, and the files an
/// interrupted run left behind.
fn print_store_action(action: &CompactionAction) {
    match action {
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
        CompactionAction::RemoveTemporary { file_name, bytes } => {
            println!(
                "  remove redundant temporary {file_name} ({})",
                froe::format_byte_size(*bytes)
            );
        }
        CompactionAction::PurgeOrphanedVersionHistories {
            histories,
            nodes,
            retained_checkpoints,
        } => {
            println!(
                "  purge {} orphaned version histories ({} nodes) by omitting them from the copy",
                crate::progress::format_count(*histories),
                crate::progress::format_count(*nodes),
            );
            println!("    their versionables no longer exist; removal is permanent");
            println!(
                "    a versionable recreated with its old identifier will no longer re-attach a purged history"
            );
            if *retained_checkpoints != 0 {
                println!(
                    "    {} retained checkpoints keep their own snapshots of these histories; that storage returns when the checkpoints expire",
                    crate::progress::format_count(*retained_checkpoints)
                );
            }
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
        CompactionAction::RemoveRecoveryBackup { file_name, bytes } => {
            println!(
                "  remove old recovery backup {file_name} ({})",
                froe::format_byte_size(*bytes)
            );
        }
        _ => println!("  apply an action added by this froe version"),
    }
}

/// Where the content's external binaries live, so blob-store expectations
/// land on blob-store garbage collection rather than on this run. Silent
/// when the store references none: a line about an absent blob store would
/// only raise the question it exists to answer.
fn print_external_binary_footprint(footprint: froe::ExternalBinaryFootprint) {
    if footprint.distinct_references == 0 {
        return;
    }
    let count = crate::progress::format_count(footprint.distinct_references);
    let size = match (footprint.measured_bytes, footprint.unmeasured_references) {
        (0, _) => String::new(),
        (bytes, 0) => format!(" (about {})", froe::format_byte_size(bytes)),
        (bytes, unmeasured) => format!(
            " (about {}; length unknown for {})",
            froe::format_byte_size(bytes),
            crate::progress::format_count(unmeasured)
        ),
    };
    println!(
        "content references {count} external binaries{size} in the blob store; compaction never affects those bytes, which return only through blob-store garbage collection after content deletion"
    );
}

/// The always-on orphaned-version-history report: what the semantically
/// dead histories hold, and how to purge them. Printed whenever there is
/// something to say, so the flag that removes them gets discovered from
/// the very plan that surfaces them.
fn print_orphaned_version_history_report(report: froe::OrphanedVersionHistoryReport) {
    if report.orphaned_histories == 0 && report.malformed_identifiers == 0 {
        return;
    }
    println!(
        "orphaned version histories: {} (their versionables no longer exist)",
        crate::progress::format_count(report.orphaned_histories)
    );
    if report.orphaned_histories != 0 {
        println!(
            "  holding {} nodes, {} of inline binary content, and {} external binary references",
            crate::progress::format_count(report.orphaned_nodes),
            froe::format_byte_size(report.inline_binary_bytes),
            crate::progress::format_count(report.external_references),
        );
        // A ceiling, not a promise: a record shared between a purged
        // history and one the store keeps retains its blocks (Oak's writer
        // dedups identical frozen subtrees into shared records), and a
        // retained checkpoint's snapshot can pin blocks until it expires.
        let checkpoint_caveat = if report.retained_checkpoints == 0 {
            String::new()
        } else {
            format!(
                "; {} retained checkpoints may pin some until they expire",
                crate::progress::format_count(report.retained_checkpoints)
            )
        };
        println!(
            "  a purge releases up to {} bulk segments ({}{checkpoint_caveat}) and about {} of node records (realized by the copy)",
            crate::progress::format_count(report.released_bulk_segments),
            froe::format_byte_size(report.released_bulk_bytes),
            froe::format_byte_size(report.node_record_bytes_estimate),
        );
        println!(
            "  purge with --purge-orphaned-version-histories; removal is permanent and is listed above when selected"
        );
    }
    if report.malformed_identifiers != 0 {
        println!(
            "  {} histories carry versionable identifiers that do not parse and were not classified",
            crate::progress::format_count(report.malformed_identifiers)
        );
    }
}

/// The plan's totals: what it would reclaim, what it deliberately keeps,
/// and what the rewrites cost while they run.
fn print_plan_totals(plan: &CompactionPlan) {
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
    // A run that copies has two sides, and an estimate that states only the
    // reclaimed one over-promises by the size of the generation it writes:
    // a swap that removes one generation and writes an equal one nets
    // nothing. The net line says which way the store actually moves.
    if let Some(copy_output) = plan.predicted_copy_output_bytes() {
        let reclaimable = plan.estimated_reclaimable_bytes();
        let target_generation = plan.actions().iter().find_map(|action| match action {
            CompactionAction::CopyHeadIntoFreshGeneration {
                target_generation, ..
            } => Some(*target_generation),
            _ => None,
        });
        match target_generation {
            Some(generation) => println!(
                "the copy writes about {} into generation ({},{},compacted)",
                froe::format_byte_size(copy_output),
                generation.generation,
                generation.full_generation,
            ),
            None => println!(
                "the copy writes about {} into the fresh generation",
                froe::format_byte_size(copy_output)
            ),
        }
        println!(
            "the sweep reclaims about {} of archives and entries",
            froe::format_byte_size(reclaimable)
        );
        if reclaimable >= copy_output {
            println!(
                "estimated net change: about {} reclaimed",
                froe::format_byte_size(reclaimable - copy_output)
            );
        } else {
            println!(
                "estimated net change: about {} of growth (the fresh generation exceeds what the sweep removes)",
                froe::format_byte_size(copy_output - reclaimable)
            );
        }
    } else {
        println!(
            "estimated reclaimable: {}",
            froe::format_byte_size(plan.estimated_reclaimable_bytes())
        );
    }
    print_external_binary_footprint(plan.external_binary_footprint());
    print_orphaned_version_history_report(plan.orphaned_version_histories());
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
    if plan.estimated_archive_rewrite_source_bytes() != 0 {
        println!(
            "archive rewrite working-space proxy: {} of source archives (additional headroom may be required)",
            froe::format_byte_size(plan.estimated_archive_rewrite_source_bytes())
        );
    }
}

fn print_cleanup_plan(plan: &CompactionPlan) {
    println!(
        "compaction plan for {} (verified head {}):",
        crate::output::sanitize_terminal_path(plan.directory()),
        plan.current_head()
    );
    // The convergence verdict comes before the actions, because it explains
    // an absence: an operator who selected a compaction and sees no copy
    // line must learn it was proven unnecessary, not lost.
    if plan.already_fully_compacted() {
        if plan.effective_compaction_kind().is_some() {
            println!(
                "  the head is already fully compacted; --always-copy forces the copy regardless"
            );
        } else {
            println!(
                "  the head is already fully compacted; the selected copy would replace this generation with an identical one and was dropped from the plan"
            );
        }
    }
    if plan.actions().is_empty() {
        println!("  no mutations");
    }
    for action in plan.actions() {
        print_planned_action(action);
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
    print_plan_totals(plan);
}

/// `froe backup`: copy the source repository's head into a target.
pub(crate) fn run_backup(
    source: &Path,
    target: &Path,
    confirmation: Confirmation,
    reporter: &Reporter,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to back up {} into {}",
            crate::output::sanitize_terminal_path(source),
            crate::output::sanitize_terminal_path(target)
        ),
        confirmation,
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
    confirmation: Confirmation,
    reporter: &Reporter,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to restore {} into {} (overwriting its head)",
            crate::output::sanitize_terminal_path(backup_directory),
            crate::output::sanitize_terminal_path(target)
        ),
        confirmation,
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
    confirmation: Confirmation,
    reporter: &Reporter,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to rebuild the journal of {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        confirmation,
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
    confirmation: Confirmation,
    reporter: &Reporter,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to create a checkpoint in {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        confirmation,
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
    confirmation: Confirmation,
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
        confirmation,
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
    use super::{DeletionTarget, cleanup_deletion_warning_fields, cleanup_partial_summary_counts};

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
}
