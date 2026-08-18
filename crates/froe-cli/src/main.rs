//! Command-line interface for inspecting, exporting from, and maintaining
//! Apache Jackrabbit Oak `segment-tar` (`TarMK`) repositories.
//!
//! Inspection and export commands are read-only: the repository lock
//! is never taken, so they are safe against a live repository (archives
//! are memory-mapped under the store's never-modify-in-place file
//! protocol, the same reliance a running Oak instance has). The
//! mutating maintenance commands — `compact`, `backup`, `restore`,
//! `recover-journal`, and checkpoint mutation — take the exclusive repository
//! lock, so they must only be run against a *stopped* repository and ask for
//! confirmation first. `compact --dry-run` is strictly read-only.

mod content_display;
mod inspection;
mod mutation;
mod output;
mod progress;
mod tooling_display;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use froe::progress::ProgressObserver as _;
use froe::store::Repository;
use froe::tooling::BinaryCheck;

use crate::mutation::{CheckpointRemoval, CompactionRun, Confirmation};
use crate::progress::{ProgressWhen, Reporter};

mod command_line;
mod export;

use command_line::{
    ArchiveRewritePolicyArgument, CheckpointAction, Command, CommandLine, ExportFormat,
    parse_value_predicate,
};
use export::run_export_command;

pub(crate) fn main() -> ExitCode {
    // Rust ignores SIGPIPE by default, turning a closed pipe (for example
    // `froe node … | head`) into a write panic. Restore the conventional
    // terminate-quietly behavior before doing anything else.
    #[cfg(unix)]
    // SAFETY: resetting a signal disposition before any other work and
    // without a custom handler is the documented safe use of `signal`.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let command_line = match CommandLine::try_parse() {
        Ok(command_line) => command_line,
        Err(error) => {
            let exit_code =
                u8::try_from(error.exit_code()).map_or(ExitCode::FAILURE, ExitCode::from);
            let diagnostic = output::sanitize_terminal_diagnostic(&error.to_string());
            if error.use_stderr() {
                eprint!("{diagnostic}");
            } else {
                print!("{diagnostic}");
            }
            return exit_code;
        }
    };
    let reporter = Reporter::new(command_line.progress, command_line.silent);
    let outcome = run(command_line.command, &reporter);
    // Close any step the command left open and clear the live line, so an
    // error message never lands on top of a progress bar.
    reporter.finish();
    match outcome {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!(
                "froe: {}",
                output::sanitize_terminal_text(&error.to_string())
            );
            ExitCode::FAILURE
        }
    }
}

/// Opens a repository read-only, reporting the archive scan.
pub(crate) fn open_repository(directory: &Path, reporter: &Reporter) -> froe::Result<Repository> {
    Repository::open_with_progress(directory, &mut reporter.clone())
}

pub(crate) fn run(command: Command, reporter: &Reporter) -> froe::Result<ExitCode> {
    run_inspection_command(command, reporter)
}

/// The read-only commands that print what the store already holds.
pub(crate) fn run_inspection_command(
    command: Command,
    reporter: &Reporter,
) -> froe::Result<ExitCode> {
    match command {
        Command::Summary { repository } => {
            inspection::print_summary(&open_repository(&repository, reporter)?)?;
        }
        Command::Journal { repository, limit } => {
            inspection::print_journal(&open_repository(&repository, reporter)?, limit);
        }
        Command::Archives { repository } => {
            inspection::print_archives(&open_repository(&repository, reporter)?);
        }
        Command::Segments { repository } => {
            inspection::print_segments(&open_repository(&repository, reporter)?);
        }
        Command::Segment {
            repository,
            identifier,
            hex,
        } => {
            let segment_identifier = identifier.to_lowercase().parse()?;
            let repository = open_repository(&repository, reporter)?;
            if hex {
                tooling_display::print_segment_dump(&repository, segment_identifier)?;
            } else {
                inspection::print_segment(&repository, segment_identifier)?;
            }
        }
        Command::Debug {
            repository,
            archives,
        } => {
            tooling_display::print_archive_debug(
                &open_repository(&repository, reporter)?,
                &archives,
            )?;
        }
        Command::Node { repository, path } => {
            if !content_display::print_node(&open_repository(&repository, reporter)?, &path)? {
                eprintln!("froe: no node at {}", output::sanitize_terminal_text(&path));
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Tree {
            repository,
            path,
            depth,
        } => {
            if !content_display::print_tree(&open_repository(&repository, reporter)?, &path, depth)?
            {
                eprintln!("froe: no node at {}", output::sanitize_terminal_text(&path));
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Checkpoints { repository } => {
            inspection::print_checkpoints(&open_repository(&repository, reporter)?)?;
        }
        other => return run_export_command(other, reporter),
    }
    Ok(ExitCode::SUCCESS)
}

/// The read-only commands that verify, digest, or compare revisions.
pub(crate) fn run_diagnostic_command(
    command: Command,
    reporter: &Reporter,
) -> froe::Result<ExitCode> {
    match command {
        Command::Check {
            repository,
            paths,
            binaries,
            revisions,
        } => {
            // Absent limit = examine every revision, oak-run's default.
            let revision_limit = revisions.unwrap_or(usize::MAX);
            let binary_check = if binaries {
                BinaryCheck::EveryBlock
            } else {
                BinaryCheck::RecordsOnly
            };
            if !tooling_display::print_check(
                &repository,
                &paths,
                binary_check,
                revision_limit,
                reporter,
            )? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Digest {
            repository,
            output,
            baseline,
        } => {
            if !tooling_display::print_digest(&repository, output.as_deref(), baseline.as_deref())?
            {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Difference {
            repository,
            before,
            after,
            path,
        } => {
            tooling_display::print_difference(&repository, &before, &after, &path, reporter)?;
        }
        Command::History { repository, path } => {
            tooling_display::print_history(&repository, &path, reporter)?;
        }
        Command::SearchNodes {
            repository,
            has_properties,
            has_children,
            values,
            limit,
        } => {
            let mut property_values = Vec::with_capacity(values.len());
            for value in &values {
                match parse_value_predicate(value) {
                    Ok(pair) => property_values.push(pair),
                    Err(message) => {
                        eprintln!("froe: {message}");
                        return Ok(ExitCode::FAILURE);
                    }
                }
            }
            tooling_display::print_search(
                &repository,
                &has_properties,
                &has_children,
                &property_values,
                limit,
                reporter,
            )?;
        }
        other => return run_compaction_command(other, reporter),
    }
    Ok(ExitCode::SUCCESS)
}

/// `froe compact`, the one maintenance command.
pub(crate) fn run_compaction_command(
    command: Command,
    reporter: &Reporter,
) -> froe::Result<ExitCode> {
    match command {
        Command::Compact {
            repository,
            tail,
            dry_run,
            yes,
            repair_archive_indexes,
            keep_expired_checkpoints,
            remove_unreferenced_checkpoints,
            backup_minimum_age_days,
            backup_keep_latest,
            archive_rewrite_policy,
        } => {
            let kind = if tail {
                froe::CompactionKind::Tail
            } else {
                froe::CompactionKind::Full
            };
            let mut options = froe::CompactionOptions::default().with_compaction(kind);
            if archive_rewrite_policy == ArchiveRewritePolicyArgument::OakSavingsGate {
                options = options.with_oak_savings_gate();
            }
            if keep_expired_checkpoints {
                options = options.keeping_expired_checkpoints();
            }
            if remove_unreferenced_checkpoints {
                options = options.with_unreferenced_checkpoint_removal();
            }
            if repair_archive_indexes {
                options = options.with_archive_index_repair();
            }
            match (backup_minimum_age_days, backup_keep_latest) {
                (Some(days), Some(keep_latest)) => {
                    let Some(seconds) = days.checked_mul(24 * 60 * 60) else {
                        eprintln!("froe: --backup-minimum-age-days is too large");
                        return Ok(ExitCode::FAILURE);
                    };
                    options = options.with_recovery_backup_policy(froe::RecoveryBackupPolicy::new(
                        std::time::Duration::from_secs(seconds),
                        keep_latest,
                    ));
                }
                (None, None) => {}
                _ => {
                    eprintln!(
                        "froe: --backup-minimum-age-days and --backup-keep-latest must be supplied together"
                    );
                    return Ok(ExitCode::FAILURE);
                }
            }
            if !mutation::run_compact(
                &repository,
                options,
                CompactionRun::from_dry_run_flag(dry_run),
                Confirmation::from_assume_yes_flag(yes),
                reporter,
            )? {
                return Ok(ExitCode::FAILURE);
            }
        }
        other => return run_mutating_command(other, reporter),
    }
    Ok(ExitCode::SUCCESS)
}

/// The remaining commands that take the lock and change the store.
pub(crate) fn run_mutating_command(
    command: Command,
    reporter: &Reporter,
) -> froe::Result<ExitCode> {
    match command {
        Command::Backup {
            source,
            target,
            yes,
        } => {
            if !mutation::run_backup(
                &source,
                &target,
                Confirmation::from_assume_yes_flag(yes),
                reporter,
            )? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Restore {
            backup,
            target,
            yes,
        } => {
            if !mutation::run_restore(
                &backup,
                &target,
                Confirmation::from_assume_yes_flag(yes),
                reporter,
            )? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::RecoverJournal { repository, yes } => {
            if !mutation::run_recover_journal(
                &repository,
                Confirmation::from_assume_yes_flag(yes),
                reporter,
            )? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Checkpoint { action } => {
            if !run_checkpoint(action, reporter)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        // Unreachable: every dispatcher above delegates here only after
        // excluding the commands it handles itself.
        other => unreachable!("{other:?} is not a mutating command"),
    }
    Ok(ExitCode::SUCCESS)
}

/// Dispatches a checkpoint subcommand. Returns whether it succeeded.
pub(crate) fn run_checkpoint(action: CheckpointAction, reporter: &Reporter) -> froe::Result<bool> {
    match action {
        CheckpointAction::List { repository } => {
            // Read-only, exactly like `froe checkpoints`: listing must
            // never take the lock or touch the manifest.
            inspection::print_checkpoints(&open_repository(&repository, reporter)?)?;
            Ok(true)
        }
        CheckpointAction::Create {
            repository,
            lifetime_milliseconds,
            yes,
        } => mutation::run_checkpoint_create(
            &repository,
            lifetime_milliseconds,
            Confirmation::from_assume_yes_flag(yes),
            reporter,
        ),
        CheckpointAction::Remove {
            repository,
            name,
            yes,
        } => mutation::run_checkpoint_remove(
            &repository,
            &CheckpointRemoval::Named(name),
            Confirmation::from_assume_yes_flag(yes),
            reporter,
        ),
        CheckpointAction::RemoveAll { repository, yes } => mutation::run_checkpoint_remove(
            &repository,
            &CheckpointRemoval::All,
            Confirmation::from_assume_yes_flag(yes),
            reporter,
        ),
        CheckpointAction::RemoveUnreferenced { repository, yes } => {
            mutation::run_checkpoint_remove(
                &repository,
                &CheckpointRemoval::Unreferenced,
                Confirmation::from_assume_yes_flag(yes),
                reporter,
            )
        }
    }
}
