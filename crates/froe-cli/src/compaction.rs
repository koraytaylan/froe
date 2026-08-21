//! `froe compact`: the one maintenance command — offline full or tail
//! compaction, planned once under the exclusive lock, confirmed, applied,
//! and proven by a fresh final verification.
//!
//! A full run is the default: orphaned version histories are purged, a
//! missing archive index is rebuilt, and recovery backups from earlier
//! runs are removed. Each of those three is settled as a yes/no question —
//! answered by its `--skip-*` flag, by `--yes`, or interactively at a
//! prompt — before the store is opened for planning, so the one plan the
//! operator confirms already reflects every answer.

use std::fmt::Write as _;
use std::path::Path;

use froe::{CompactionAction, PreparedCompaction, plan_compaction_with_progress};

use crate::compaction_report::print_cleanup_plan;
use crate::compaction_summary::{
    CompletionContext, PurgeFacts, print_cleanup_summary, print_deletion_failures,
};
use crate::mutation::{Confirmation, PromptAnswer, ask, confirm, report_cancelled};
use crate::output::count_noun;
use crate::progress::Reporter;

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

/// The parsed `froe compact` command line, one field per flag, with the
/// two hidden compatibility spellings already folded into what they mean.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one field per command-line flag; the flags are genuinely independent switches"
)]
pub(crate) struct CompactionCommandLine {
    pub(crate) tail: bool,
    pub(crate) always_copy: bool,
    pub(crate) dry_run: bool,
    pub(crate) assume_yes: bool,
    pub(crate) skip_purging_orphaned_version_histories: bool,
    /// `--purge-orphaned-version-histories`, the spelling from when the
    /// purge was opt-in: selects the purge without asking.
    pub(crate) purge_preauthorized: bool,
    pub(crate) purged_history_minimum_age_days: Option<u64>,
    pub(crate) skip_repairing_archive_indexes: bool,
    /// `--repair-archive-indexes`, the spelling from when the repair was
    /// opt-in: authorizes the rebuild without asking.
    pub(crate) repair_preauthorized: bool,
    pub(crate) skip_removing_recovery_backups: bool,
    pub(crate) backup_minimum_age_days: Option<u64>,
    pub(crate) backup_keep_latest: Option<usize>,
    pub(crate) keep_expired_checkpoints: bool,
    pub(crate) remove_unreferenced_checkpoints: bool,
    pub(crate) oak_savings_gate: bool,
}

/// Why an otherwise-default cleanup is not part of this run. Carried into
/// the plan report and the completion summary, so both say what actually
/// held a cleanup back instead of advising a flag that is already the
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// The matching `--skip-*` flag was given.
    Flag,
    /// The operator answered no at the prompt.
    Declined,
    /// No answer was available on standard input and `--yes` was not
    /// passed.
    Unconfirmed,
    /// This run repairs archive indexes, and the repair writes the very
    /// `.bak` files a removal could otherwise delete in the same run —
    /// recovery-backup removal therefore waits for the next run.
    RepairRunsFirst,
    /// A tail compaction retains the shared full generation a purge must
    /// reclaim, so only a full compaction purges.
    TailCompaction,
}

/// The questions of one run, resolved: the options to plan with, and why
/// any default cleanup was left out.
struct ResolvedRun {
    options: froe::CompactionOptions,
    purge_skipped: Option<SkipReason>,
    backups_skipped: Option<SkipReason>,
}

/// Settles the repair question. The survey is read-only and answers in
/// seconds what planning would discover minutes in: whether any active
/// archive lacks a usable index. When none does, there is nothing to ask.
/// When authorization is refused — by flag, by answer, or by absence of
/// one — the run proceeds without the repair task and planning refuses
/// with the full census of the damage, naming what authorization would
/// have rebuilt.
fn resolve_archive_index_repair(
    repository: &Path,
    command: &CompactionCommandLine,
    reporter: &Reporter,
) -> froe::Result<bool> {
    let survey = froe::survey_archive_indexes(repository)?;
    if !survey.any_archive_lacks_an_index() {
        return Ok(false);
    }
    if command.skip_repairing_archive_indexes {
        return Ok(false);
    }
    // An unrepairable archive dooms the run whatever is answered, so no
    // question is asked: selecting the task yields the refusal that names
    // exactly those files, raised before anything is rewritten.
    if !survey.unrepairable.is_empty() {
        return Ok(true);
    }
    if command.repair_preauthorized || command.assume_yes || command.dry_run {
        return Ok(true);
    }
    let question = if let [only] = survey.repairable.as_slice() {
        format!(
            "active archive {only} has no usable index — what a killed writer leaves behind. \
             Rebuild its index from the archive's own entries, keeping the original under a \
             .bak name?"
        )
    } else {
        const NAMES_SHOWN: usize = 3;
        let mut named = survey
            .repairable
            .iter()
            .take(NAMES_SHOWN)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if survey.repairable.len() > NAMES_SHOWN {
            let _ = write!(
                named,
                ", and {} more",
                survey.repairable.len() - NAMES_SHOWN
            );
        }
        format!(
            "{} active archives have no usable index ({named}) — what a killed writer leaves \
             behind. Rebuild the missing indexes from the archives' own entries, keeping the \
             originals under .bak names?",
            crate::progress::format_count(survey.repairable.len() as u64),
        )
    };
    Ok(ask(&question, reporter) == PromptAnswer::Yes)
}

/// Settles the recovery-backup question. A run that repairs never removes
/// backups — the library refuses the combination outright — and a store
/// holding none has nothing to ask about.
fn resolve_recovery_backup_removal(
    command: &CompactionCommandLine,
    repair_selected: bool,
    backups_present: froe::RecoveryBackupSurvey,
    reporter: &Reporter,
) -> Option<SkipReason> {
    if repair_selected {
        return Some(SkipReason::RepairRunsFirst);
    }
    if command.skip_removing_recovery_backups {
        return Some(SkipReason::Flag);
    }
    if command.assume_yes || command.dry_run || backups_present.files == 0 {
        return None;
    }
    let question = format!(
        "remove the {} ({}) earlier runs left behind? The plan lists each file before the \
         final confirmation.",
        count_noun(backups_present.files, "recovery backup", "recovery backups"),
        froe::format_byte_size(backups_present.bytes),
    );
    match ask(&question, reporter) {
        PromptAnswer::Yes => None,
        PromptAnswer::No => Some(SkipReason::Declined),
        PromptAnswer::NoAnswer => Some(SkipReason::Unconfirmed),
    }
}

/// Settles the purge question. The orphan count is not known until the
/// planning walk runs, so the interactive question is asked up front and
/// the plan then states the exact count — and the final confirmation still
/// covers it — before anything is applied.
fn resolve_orphaned_version_history_purge(
    command: &CompactionCommandLine,
    reporter: &Reporter,
) -> Option<SkipReason> {
    if command.tail {
        return Some(SkipReason::TailCompaction);
    }
    if command.skip_purging_orphaned_version_histories {
        return Some(SkipReason::Flag);
    }
    if command.purge_preauthorized
        || command.purged_history_minimum_age_days.is_some()
        || command.assume_yes
        || command.dry_run
    {
        return None;
    }
    match ask(
        "purge orphaned version histories, if any are found? Their versionables no longer \
         exist, removal is permanent, and the plan states the exact count before the final \
         confirmation.",
        reporter,
    ) {
        PromptAnswer::Yes => None,
        PromptAnswer::No => Some(SkipReason::Declined),
        PromptAnswer::NoAnswer => Some(SkipReason::Unconfirmed),
    }
}

/// Converts a day count from the command line into a duration, refusing
/// the absurd value that would overflow instead of wrapping it.
fn days_as_duration(days: u64, flag: &str) -> Option<std::time::Duration> {
    let Some(seconds) = days.checked_mul(24 * 60 * 60) else {
        eprintln!("froe: {flag} is too large");
        return None;
    };
    Some(std::time::Duration::from_secs(seconds))
}

/// Resolves every question of the run and assembles the library options.
/// `None` means a refusal that has already been reported.
fn resolve_run(
    repository: &Path,
    command: &CompactionCommandLine,
    reporter: &Reporter,
) -> froe::Result<Option<ResolvedRun>> {
    let repair_selected = resolve_archive_index_repair(repository, command, reporter)?;
    let backups_present = froe::survey_recovery_backups(repository)?;
    let purge_skipped = resolve_orphaned_version_history_purge(command, reporter);
    let backups_skipped =
        resolve_recovery_backup_removal(command, repair_selected, backups_present, reporter);
    if repair_selected && backups_present.files != 0 {
        reporter.status(&format!(
            "note: this run keeps the {} already on disk: the index rebuild writes new .bak \
             files, and removing backups in the same run could discard the only copy of what \
             the rebuild could not read; the next run removes them",
            count_noun(backups_present.files, "recovery backup", "recovery backups"),
        ));
    }

    let kind = if command.tail {
        froe::CompactionKind::Tail
    } else {
        froe::CompactionKind::Full
    };
    let mut options = froe::CompactionOptions::default().with_compaction(kind);
    if command.always_copy {
        options = options.with_copy_when_already_compacted();
    }
    if purge_skipped.is_none() {
        options = options.with_orphaned_version_history_purge();
        if let Some(days) = command.purged_history_minimum_age_days {
            let Some(minimum_age) = days_as_duration(days, "--purged-history-minimum-age-days")
            else {
                return Ok(None);
            };
            options = options.with_purged_history_minimum_age(minimum_age);
        }
    }
    if command.oak_savings_gate {
        options = options.with_oak_savings_gate();
    }
    if command.keep_expired_checkpoints {
        options = options.keeping_expired_checkpoints();
    }
    if command.remove_unreferenced_checkpoints {
        options = options.with_unreferenced_checkpoint_removal();
    }
    if repair_selected {
        options = options.with_archive_index_repair();
    }
    if backups_skipped.is_none() {
        let days = command.backup_minimum_age_days.unwrap_or(0);
        let Some(minimum_age) = days_as_duration(days, "--backup-minimum-age-days") else {
            return Ok(None);
        };
        options = options.with_recovery_backup_policy(froe::RecoveryBackupPolicy::new(
            minimum_age,
            command.backup_keep_latest.unwrap_or(0),
        ));
    }
    Ok(Some(ResolvedRun {
        options,
        purge_skipped,
        backups_skipped,
    }))
}

/// Reports index rebuilds that are already durable, on the paths where
/// "nothing to do" or "cancelled" would otherwise read as "nothing
/// happened".
fn report_durable_rebuilds(repaired: usize, to_standard_output: bool) {
    if repaired == 0 {
        return;
    }
    let rebuilds = count_noun(
        repaired as u64,
        "archive index rebuild",
        "archive index rebuilds",
    );
    let message = format!(
        "note: the {rebuilds} above {} already applied and durable; the originals remain \
         under .bak names",
        if repaired == 1 { "is" } else { "are" },
    );
    if to_standard_output {
        println!("{message}");
    } else {
        eprintln!("froe: {message}");
    }
}

/// Runs `froe compact` end to end.
///
/// With `--dry-run` the command plans read-only, prints the plan, and takes
/// no lock. Otherwise the lock is acquired *first* and the one plan is built
/// under it, so the plan the operator confirms is byte-for-byte the plan
/// that applies: nothing can change the store between the evidence and the
/// decision, and an accidentally started Oak cannot open the store while
/// the operator is reading. An authorized index repair runs under the same
/// lock before planning, so even a repairing run shows exactly one plan.
/// The store is offline by precondition, which is what makes holding the
/// lock through the prompt a strengthening rather than a discourtesy.
pub(crate) fn run_compact(
    repository: &Path,
    command: &CompactionCommandLine,
    reporter: &Reporter,
) -> froe::Result<bool> {
    reporter.status(
        "note: archive rewrites require same-directory hard-link and directory-fsync support; an unsupported filesystem fails safely with source archives retained",
    );
    let confirmation = Confirmation::from_assume_yes_flag(command.assume_yes);
    let Some(resolved) = resolve_run(repository, command, reporter)? else {
        return Ok(false);
    };
    if CompactionRun::from_dry_run_flag(command.dry_run) == CompactionRun::PlanOnly {
        let preview =
            plan_compaction_with_progress(repository, &resolved.options, &mut reporter.clone())?;
        // The plan is the operator's evidence: end every report before a
        // single line of it is written.
        reporter.finish();
        print_cleanup_plan(&preview, resolved.purge_skipped);
        println!("dry-run: repository was not modified");
        return Ok(true);
    }

    let prepared = PreparedCompaction::prepare_with_progress(
        repository,
        resolved.options,
        &mut reporter.clone(),
    )?;
    reporter.finish();
    print_cleanup_plan(prepared.plan(), resolved.purge_skipped);
    if prepared.plan().is_empty() {
        if prepared.plan().already_fully_compacted() {
            println!("the store is already fully compacted; nothing to do");
        } else {
            println!("no maintenance mutations are needed; review any warnings above");
        }
        // "Nothing to do" must not read as "nothing happened": index
        // rebuilds run before planning and are already durable.
        report_durable_rebuilds(prepared.repaired_archives(), true);
        return Ok(true);
    }
    let answer = confirm(
        &format!(
            "about to apply this compaction plan to {}",
            crate::output::sanitize_terminal_path(prepared.plan().directory())
        ),
        confirmation,
        reporter,
    );
    if answer != PromptAnswer::Yes {
        report_cancelled("compaction", answer);
        // The repair is already durable. Saying "cancelled" alone would
        // imply the store is untouched, and it is not.
        report_durable_rebuilds(prepared.repaired_archives(), false);
        return Ok(false);
    }
    // Captured before the plan is consumed: the operator reads the summary
    // after minutes of progress output has scrolled the plan away.
    let context = CompletionContext {
        retained_reclaimable_segments: prepared.plan().retained_reclaimable_segments(),
        retained_reclaimable_bytes: prepared.plan().retained_reclaimable_bytes(),
        purge: purge_facts_of(prepared.plan()),
        full_copy_planned: prepared.plan().effective_compaction_kind()
            == Some(froe::CompactionKind::Full),
        backups_skipped: resolved.backups_skipped,
    };
    let outcome = prepared.apply_with_progress(&mut reporter.clone())?;
    reporter.finish();
    print_cleanup_summary(&outcome, &context);
    print_deletion_failures(&outcome);
    Ok(outcome.is_complete())
}

/// The purge the confirmed plan carries, read off the authoritative plan
/// before it is consumed, so the summary can restate the content delta
/// after the progress output has scrolled the plan away.
fn purge_facts_of(plan: &froe::CompactionPlan) -> Option<PurgeFacts> {
    plan.actions().iter().find_map(|action| match action {
        CompactionAction::PurgeOrphanedVersionHistories {
            histories,
            nodes,
            retained_checkpoints,
        } => Some(PurgeFacts {
            histories: *histories,
            nodes: *nodes,
            retained_checkpoints: *retained_checkpoints,
        }),
        _ => None,
    })
}
