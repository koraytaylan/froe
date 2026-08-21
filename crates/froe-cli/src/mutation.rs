//! The mutating commands other than compaction — backup, restore, journal
//! recovery, and checkpoint management — and the question-asking
//! primitives every mutating command shares.
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
use froe::{backup_with_progress, recover_journal_with_progress, restore_with_progress};

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

/// What the operator answered at a yes/no question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptAnswer {
    /// An affirmative answer.
    Yes,
    /// Anything else typed — the conventional default.
    No,
    /// No answer was available at all: standard input is closed or
    /// unreadable, which is what a scripted run without `--yes` looks
    /// like. Callers distinguish this from an explicit no so they can
    /// point at `--yes` instead of implying the operator declined.
    NoAnswer,
}

/// Asks a yes/no question on standard error and reads the answer from
/// standard input.
///
/// The question is written with the reporter suspended, so a live progress
/// line is erased first and nothing is drawn over it while the operator is
/// answering. `--silent` never hides a question: it is about a change to
/// the repository, not a progress report.
pub(crate) fn ask(question: &str, reporter: &Reporter) -> PromptAnswer {
    reporter.while_suspended(|| {
        let _ = std::io::stdout().flush();
        eprint!("froe: {question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        match std::io::stdin().read_line(&mut answer) {
            Err(_) | Ok(0) => PromptAnswer::NoAnswer,
            Ok(_) => {
                if matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
                    PromptAnswer::Yes
                } else {
                    PromptAnswer::No
                }
            }
        }
    })
}

/// Asks for confirmation before a mutating operation, unless `--yes`
/// already answered it.
pub(crate) fn confirm(
    action: &str,
    confirmation: Confirmation,
    reporter: &Reporter,
) -> PromptAnswer {
    if confirmation == Confirmation::AssumeYes {
        return PromptAnswer::Yes;
    }
    ask(
        &format!("{action} — this modifies the repository. Continue?"),
        reporter,
    )
}

/// Reports a declined or unanswerable confirmation. An explicit no is
/// simply stated; an absent answer names the flag that supplies one, so a
/// scripted run learns what it was missing rather than what it never did.
pub(crate) fn report_cancelled(operation: &str, answer: PromptAnswer) {
    eprintln!("froe: {operation} cancelled");
    if answer == PromptAnswer::NoAnswer {
        eprintln!(
            "froe: no confirmation was available on standard input; rerun with --yes to proceed \
             without one"
        );
    }
}

/// `froe backup`: copy the source repository's head into a target.
pub(crate) fn run_backup(
    source: &Path,
    target: &Path,
    confirmation: Confirmation,
    reporter: &Reporter,
) -> froe::Result<bool> {
    let answer = confirm(
        &format!(
            "about to back up {} into {}",
            crate::output::sanitize_terminal_path(source),
            crate::output::sanitize_terminal_path(target)
        ),
        confirmation,
        reporter,
    );
    if answer != PromptAnswer::Yes {
        report_cancelled("backup", answer);
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
    let answer = confirm(
        &format!(
            "about to restore {} into {} (overwriting its head)",
            crate::output::sanitize_terminal_path(backup_directory),
            crate::output::sanitize_terminal_path(target)
        ),
        confirmation,
        reporter,
    );
    if answer != PromptAnswer::Yes {
        report_cancelled("restore", answer);
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
    let answer = confirm(
        &format!(
            "about to rebuild the journal of {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        confirmation,
        reporter,
    );
    if answer != PromptAnswer::Yes {
        report_cancelled("recovery", answer);
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
    let answer = confirm(
        &format!(
            "about to create a checkpoint in {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        confirmation,
        reporter,
    );
    if answer != PromptAnswer::Yes {
        report_cancelled("checkpoint creation", answer);
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
    let answer = confirm(
        &format!(
            "about to remove {target_description} from {}",
            crate::output::sanitize_terminal_path(repository)
        ),
        confirmation,
        reporter,
    );
    if answer != PromptAnswer::Yes {
        report_cancelled("checkpoint removal", answer);
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
