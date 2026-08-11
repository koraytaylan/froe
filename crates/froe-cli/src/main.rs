//! Command-line interface for inspecting, exporting from, and maintaining
//! Apache Jackrabbit Oak `segment-tar` (`TarMK`) repositories.
//!
//! Inspection and export commands are read-only: the repository lock
//! is never taken, so they are safe against a live repository (archives
//! are memory-mapped under the store's never-modify-in-place file
//! protocol, the same reliance a running Oak instance has). The
//! mutating maintenance commands — `compact`, `cleanup` apply, `backup`,
//! `restore`, `recover-journal`, and checkpoint mutation — take the exclusive
//! repository lock, so they must only be run against a *stopped* repository
//! and ask for confirmation first. `cleanup --dry-run` is strictly read-only.

mod content_display;
mod inspection;
mod mutation;
mod output;
mod progress;
mod tooling_display;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use froe::store::Repository;

use crate::mutation::CheckpointRemoval;

#[derive(Parser)]
#[command(
    name = "froe",
    version,
    about = "Tooling for Apache Jackrabbit Oak segment-tar (TarMK) repositories",
    long_about = "Tooling for Apache Jackrabbit Oak segment-tar (TarMK) repositories, the storage \
                  format of Apache Jackrabbit Oak and Adobe Experience Manager. Inspection and \
                  export commands are read-only and safe against a live repository (archives \
                  are memory-mapped under the store's never-modify-in-place file protocol, the \
                  same reliance a running Oak instance has); the compact, backup, restore, \
                  cleanup, recover-journal, and checkpoint commands modify the store and must be run \
                  against a stopped repository. If repo.lock is absent, every mutating command \
                  requires same-directory hard-link and durable directory-fsync support to publish \
                  that lock safely."
)]
struct CommandLine {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show a repository overview: archives, segments, journal, and head.
    Summary {
        /// The segment store directory (contains journal.log and data*.tar).
        repository: PathBuf,
    },
    /// List the journal revisions, newest first.
    Journal {
        /// The segment store directory.
        repository: PathBuf,
        /// Print at most this many revisions.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List the archives with their sizes and segment counts.
    Archives {
        /// The segment store directory.
        repository: PathBuf,
    },
    /// List every segment across all archives.
    Segments {
        /// The segment store directory.
        repository: PathBuf,
    },
    /// Show one segment's header, references, and record statistics.
    Segment {
        /// The segment store directory.
        repository: PathBuf,
        /// The segment UUID, for example f81378fb-92b1-4b52-a5c8-e0a67152ed2c.
        identifier: String,
    },
    /// Show one node: record identifiers, properties, and children.
    Node {
        /// The segment store directory.
        repository: PathBuf,
        /// The content path, for example /content/dam.
        path: String,
    },
    /// Show the content tree under a path.
    Tree {
        /// The segment store directory.
        repository: PathBuf,
        /// The content path to start from.
        #[arg(default_value = "/")]
        path: String,
        /// How many levels below the starting node to show.
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
    /// List the repository's checkpoints.
    Checkpoints {
        /// The segment store directory.
        repository: PathBuf,
    },
    /// Export node data as JSON lines, Parquet tables, or a SQLite
    /// database.
    // `extract` shipped in v0.1.0 as the JSON lines exporter and maps
    // exactly onto `export --format json-lines`, so it lives on as a
    // hidden compatibility alias; `export` is the documented spelling.
    #[command(alias = "extract")]
    #[allow(
        clippy::doc_markdown,
        reason = "SQLite is a proper noun; this doc comment doubles as the --help text"
    )]
    Export {
        /// The segment store directory.
        repository: PathBuf,
        /// The content path to export from.
        #[arg(long, default_value = "/")]
        path: String,
        /// Bound the export depth; omit to export the whole subtree.
        #[arg(long)]
        depth: Option<usize>,
        /// The output format.
        #[arg(long, value_enum, default_value = "json-lines")]
        format: ExportFormat,
        /// Where the export goes. For json-lines a file (standard output
        /// when omitted); for parquet the directory holding
        /// nodes.parquet and properties.parquet — an existing export
        /// there is refreshed in place, decoding only what changed; for
        /// sqlite the database file (required). The json-lines and
        /// sqlite formats never overwrite an existing file.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Rebuild a Parquet export from scratch instead of refreshing
        /// the existing one in place.
        #[arg(long)]
        full: bool,
        /// Suppress the progress reports on standard error.
        #[arg(long)]
        quiet: bool,
    },
    /// Find each path's newest consistent revision (read-only). Exits 0
    /// when ANY checked path found a good revision — oak-run's contract
    /// with fail-fast off, not an all-paths-healthy integrity gate; a
    /// script needing every path healthy must inspect the per-path
    /// output.
    Check {
        /// The segment store directory.
        repository: PathBuf,
        /// Content paths to verify; defaults to the whole content tree.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Also read binary content, not only resolve its records.
        #[arg(long)]
        binaries: bool,
        /// Examine at most this many revisions; omit to examine all,
        /// like oak-run.
        #[arg(long)]
        revisions: Option<usize>,
    },
    /// Show the differences between two revisions (read-only).
    Difference {
        /// The segment store directory.
        repository: PathBuf,
        /// The older revision (a record identifier, or "head").
        before: String,
        /// The newer revision (a record identifier, or "head").
        after: String,
        /// Restrict the diff to this content path.
        #[arg(long, default_value = "/")]
        path: String,
    },
    /// Show how a node changed across revisions (read-only).
    History {
        /// The segment store directory.
        repository: PathBuf,
        /// The content path to trace.
        path: String,
    },
    /// Search every node for property, child, or value matches (read-only).
    SearchNodes {
        /// The segment store directory.
        repository: PathBuf,
        /// Require a property with this name (repeatable).
        #[arg(long = "has-property")]
        has_properties: Vec<String>,
        /// Require a child with this name (repeatable).
        #[arg(long = "has-child")]
        has_children: Vec<String>,
        /// Require a property NAME=VALUE (repeatable).
        #[arg(long = "value")]
        values: Vec<String>,
        /// Stop after this many matches (0 = no limit).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Compact the repository offline (modifies the store).
    ///
    /// Reclamation may publish validated successor TARs with absent-only,
    /// same-directory hard links. A filesystem without hard-link or durable
    /// directory-fsync support can fail safely after the new compacted head is
    /// committed, leaving old source archives for a later retry. When repo.lock
    /// is absent, publication independently requires same-directory hard-link
    /// and durable directory-fsync support.
    Compact {
        /// The segment store directory.
        repository: PathBuf,
        /// Run a tail compaction instead of a full one.
        #[arg(long)]
        tail: bool,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Conservatively reclaim orphaned storage and stale repository metadata.
    ///
    /// Dry-run is strictly read-only and does not create or acquire repo.lock.
    /// Applying cleanup is beta, Unix-only offline maintenance: stop Oak/AEM,
    /// run as the operating-system owner of journal.log (normally the service
    /// account, not sudo), and keep a recoverable copy of important stores.
    /// The repository argument is resolved once to its canonical absolute
    /// target before planning or locking, and that target is shown in the plan.
    /// Recovery backups are retained unless an explicit age/count policy is
    /// supplied. Applying also requires same-directory hard-link and durable
    /// directory-fsync support when repo.lock is absent. See docs/cleanup.md
    /// for the full safety contract.
    Cleanup {
        /// The segment store directory.
        repository: PathBuf,
        /// Cleanup category to run (repeatable). Supplying any --task
        /// replaces the defaults: journal, segments, stale-archives,
        /// expired-checkpoints, and stale-temporaries.
        #[arg(long = "task", value_enum)]
        tasks: Vec<CleanupTaskArgument>,
        /// Analyze and print the plan without taking repo.lock or writing.
        #[arg(long)]
        dry_run: bool,
        /// Proceed without prompts (locked replan and verification still run).
        #[arg(long)]
        yes: bool,
        /// Remove backups at least this old; with --backup-keep-latest this
        /// enables recovery-backups in addition to the selected task set.
        #[arg(long, requires = "backup_keep_latest")]
        backup_min_age_days: Option<u64>,
        /// Retain at least this many newest backups per target (all mtime ties
        /// at the boundary are kept); with --backup-min-age-days this enables
        /// recovery-backups.
        #[arg(long, requires = "backup_min_age_days")]
        backup_keep_latest: Option<usize>,
    },
    /// Copy a repository's head into a target store (modifies the target).
    ///
    /// If repo.lock is absent in a locked store, safe lock publication requires
    /// same-directory hard-link and durable directory-fsync support.
    Backup {
        /// The source segment store directory.
        source: PathBuf,
        /// The target segment store directory (created if absent).
        target: PathBuf,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Restore a backup into an existing store (modifies the target).
    ///
    /// If repo.lock is absent in a locked store, safe lock publication requires
    /// same-directory hard-link and durable directory-fsync support.
    Restore {
        /// The backup segment store directory.
        backup: PathBuf,
        /// The target segment store directory.
        target: PathBuf,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Rebuild journal.log from the segments (modifies the store).
    ///
    /// If repo.lock is absent, safe lock publication requires same-directory
    /// hard-link and durable directory-fsync support.
    RecoverJournal {
        /// The segment store directory.
        repository: PathBuf,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Manage checkpoints (list is read-only; the rest modify the store).
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointAction,
    },
}

/// The formats `froe export` writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
enum ExportFormat {
    /// One JSON object per node.
    JsonLines,
    /// Two Parquet tables for analytical SQL: nodes.parquet and
    /// properties.parquet.
    Parquet,
    /// One SQLite database file: interned nodes and properties tables
    /// with node_paths and properties_expanded views on top.
    #[allow(
        clippy::doc_markdown,
        reason = "SQLite is a proper noun; this doc comment doubles as the --help text"
    )]
    Sqlite,
}

#[derive(Subcommand)]
enum CheckpointAction {
    /// List the checkpoints.
    List {
        /// The segment store directory.
        repository: PathBuf,
    },
    /// Create a checkpoint.
    ///
    /// If repo.lock is absent, safe lock publication requires same-directory
    /// hard-link and durable directory-fsync support.
    Create {
        /// The segment store directory.
        repository: PathBuf,
        /// The checkpoint lifetime in milliseconds.
        #[arg(long, default_value_t = 1_000 * 60 * 60 * 24 * 30)]
        lifetime_milliseconds: i64,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Remove one checkpoint by name.
    ///
    /// If repo.lock is absent, safe lock publication requires same-directory
    /// hard-link and durable directory-fsync support.
    Remove {
        /// The segment store directory.
        repository: PathBuf,
        /// The checkpoint name.
        name: String,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Remove every checkpoint.
    ///
    /// If repo.lock is absent, safe lock publication requires same-directory
    /// hard-link and durable directory-fsync support.
    RemoveAll {
        /// The segment store directory.
        repository: PathBuf,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Remove checkpoints not referenced by the asynchronous indexer.
    ///
    /// If repo.lock is absent, safe lock publication requires same-directory
    /// hard-link and durable directory-fsync support.
    RemoveUnreferenced {
        /// The segment store directory.
        repository: PathBuf,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
enum CleanupTaskArgument {
    Journal,
    Segments,
    StaleArchives,
    ExpiredCheckpoints,
    StaleTemporaries,
    UnreferencedCheckpoints,
    RecoveryBackups,
}

impl From<CleanupTaskArgument> for froe::CleanupTask {
    fn from(task: CleanupTaskArgument) -> Self {
        match task {
            CleanupTaskArgument::Journal => Self::Journal,
            CleanupTaskArgument::Segments => Self::Segments,
            CleanupTaskArgument::StaleArchives => Self::StaleArchives,
            CleanupTaskArgument::ExpiredCheckpoints => Self::ExpiredCheckpoints,
            CleanupTaskArgument::StaleTemporaries => Self::StaleTemporaries,
            CleanupTaskArgument::UnreferencedCheckpoints => Self::UnreferencedCheckpoints,
            CleanupTaskArgument::RecoveryBackups => Self::RecoveryBackups,
        }
    }
}

/// Parses a `NAME=VALUE` search predicate.
fn parse_value_predicate(argument: &str) -> std::result::Result<(String, String), String> {
    argument
        .split_once('=')
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("expected NAME=VALUE, got {argument:?}"))
}

fn main() -> ExitCode {
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
    match run(command_line.command) {
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

#[allow(
    clippy::too_many_lines,
    reason = "one match arm per command reads best as a single dispatch"
)]
fn run(command: Command) -> froe::Result<ExitCode> {
    match command {
        Command::Summary { repository } => {
            inspection::print_summary(&Repository::open(&repository)?)?;
        }
        Command::Journal { repository, limit } => {
            inspection::print_journal(&Repository::open(&repository)?, limit);
        }
        Command::Archives { repository } => {
            inspection::print_archives(&Repository::open(&repository)?);
        }
        Command::Segments { repository } => {
            inspection::print_segments(&Repository::open(&repository)?);
        }
        Command::Segment {
            repository,
            identifier,
        } => {
            let segment_identifier = identifier.to_lowercase().parse()?;
            inspection::print_segment(&Repository::open(&repository)?, segment_identifier)?;
        }
        Command::Node { repository, path } => {
            if !content_display::print_node(&Repository::open(&repository)?, &path)? {
                eprintln!("froe: no node at {}", output::sanitize_terminal_text(&path));
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Tree {
            repository,
            path,
            depth,
        } => {
            if !content_display::print_tree(&Repository::open(&repository)?, &path, depth)? {
                eprintln!("froe: no node at {}", output::sanitize_terminal_text(&path));
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Checkpoints { repository } => {
            inspection::print_checkpoints(&Repository::open(&repository)?)?;
        }
        Command::Export {
            repository: repository_path,
            path,
            depth,
            format,
            output,
            full,
            quiet,
        } => {
            if full && format != ExportFormat::Parquet {
                eprintln!("froe: --full applies only to the parquet format");
                return Ok(ExitCode::FAILURE);
            }
            let run = match format {
                ExportFormat::JsonLines => {
                    run_json_lines_export(&repository_path, &path, depth, output.as_deref(), quiet)?
                }
                ExportFormat::Parquet => {
                    let Some(output_directory) = output.as_deref() else {
                        eprintln!(
                            "froe: the parquet format writes nodes.parquet and \
                             properties.parquet; pass --output <directory>"
                        );
                        return Ok(ExitCode::FAILURE);
                    };
                    run_parquet_export(
                        &repository_path,
                        &path,
                        depth,
                        output_directory,
                        full,
                        quiet,
                    )?
                }
                ExportFormat::Sqlite => {
                    let Some(output_path) = output.as_deref() else {
                        eprintln!(
                            "froe: the sqlite format writes a single database file; \
                             pass --output <file>"
                        );
                        return Ok(ExitCode::FAILURE);
                    };
                    run_sqlite_export(&repository_path, &path, depth, output_path, quiet)?
                }
            };
            match run {
                ExportRun::Exported { nodes, elapsed } => {
                    let rate = if elapsed.is_zero() {
                        0.0
                    } else {
                        // A node count is a display figure; the precision
                        // loss of the cast is irrelevant at the reported
                        // scale.
                        #[allow(clippy::cast_precision_loss)]
                        let rate = nodes as f64 / elapsed.as_secs_f64();
                        rate
                    };
                    match &output {
                        Some(destination) => eprintln!(
                            "froe: exported {nodes} nodes to {} in {:.1}s ({:.0} nodes/s)",
                            destination.display(),
                            elapsed.as_secs_f64(),
                            rate
                        ),
                        None => eprintln!(
                            "froe: exported {nodes} nodes in {:.1}s ({:.0} nodes/s)",
                            elapsed.as_secs_f64(),
                            rate
                        ),
                    }
                }
                // A refresh reports itself.
                ExportRun::Reported => {}
                ExportRun::Missing => {
                    eprintln!("froe: no node at {}", output::sanitize_terminal_text(&path));
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        Command::Check {
            repository,
            paths,
            binaries,
            revisions,
        } => {
            // Absent limit = examine every revision, oak-run's default.
            let revision_limit = revisions.unwrap_or(usize::MAX);
            if !tooling_display::print_check(&repository, &paths, binaries, revision_limit)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Difference {
            repository,
            before,
            after,
            path,
        } => {
            tooling_display::print_difference(&repository, &before, &after, &path)?;
        }
        Command::History { repository, path } => {
            tooling_display::print_history(&repository, &path)?;
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
            )?;
        }
        Command::Compact {
            repository,
            tail,
            yes,
        } => {
            if !mutation::run_compact(&repository, tail, yes)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Cleanup {
            repository,
            tasks,
            dry_run,
            yes,
            backup_min_age_days,
            backup_keep_latest,
        } => {
            let recovery_backups_selected = tasks.contains(&CleanupTaskArgument::RecoveryBackups);
            if recovery_backups_selected
                && (backup_min_age_days.is_none() || backup_keep_latest.is_none())
            {
                eprintln!(
                    "froe: --task recovery-backups requires --backup-min-age-days and --backup-keep-latest"
                );
                return Ok(ExitCode::FAILURE);
            }
            let mut options = froe::CleanupOptions::default();
            if !tasks.is_empty() {
                options = options.with_tasks(tasks.into_iter().map(Into::into));
            }
            match (backup_min_age_days, backup_keep_latest) {
                (Some(days), Some(keep_latest)) => {
                    let Some(seconds) = days.checked_mul(24 * 60 * 60) else {
                        eprintln!("froe: --backup-min-age-days is too large");
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
                        "froe: --backup-min-age-days and --backup-keep-latest must be supplied together"
                    );
                    return Ok(ExitCode::FAILURE);
                }
            }
            if !mutation::run_cleanup(&repository, options, dry_run, yes)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Backup {
            source,
            target,
            yes,
        } => {
            if !mutation::run_backup(&source, &target, yes)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Restore {
            backup,
            target,
            yes,
        } => {
            if !mutation::run_restore(&backup, &target, yes)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::RecoverJournal { repository, yes } => {
            if !mutation::run_recover_journal(&repository, yes)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Checkpoint { action } => {
            if !run_checkpoint(action)? {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// What an export run did, for the summary the caller prints.
enum ExportRun {
    /// Rows streamed out; the caller prints the node-count summary.
    Exported {
        /// How many nodes were written.
        nodes: u64,
        /// How long the export took.
        elapsed: std::time::Duration,
    },
    /// The run reported itself (a Parquet refresh).
    Reported,
    /// The path does not exist.
    Missing,
}

/// Streams a JSON lines export to `output`, or to standard output when
/// `output` is `None`; a freshly created output file never lingers
/// after either failure shape.
fn run_json_lines_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output: Option<&Path>,
    quiet: bool,
) -> froe::Result<ExportRun> {
    let repository = Repository::open(repository_path)?;
    if let Some(output_path) = output {
        let file = froe_export::create_export_output(repository_path, output_path)?;
        let mut sink = progress::ProgressSink::new(
            froe_export::JsonLinesSink::new(std::io::BufWriter::with_capacity(1 << 20, file)),
            quiet,
        );
        match froe_export::export_subtree(&repository, path, depth, &mut sink) {
            Ok(written) => {
                let elapsed = sink.elapsed();
                if written.is_none() {
                    // Nothing was exported: the freshly created, empty
                    // output file must not linger either.
                    drop(sink);
                    let _ = std::fs::remove_file(output_path);
                }
                Ok(match written {
                    Some(nodes) => ExportRun::Exported { nodes, elapsed },
                    None => ExportRun::Missing,
                })
            }
            Err(error) => {
                // The file was freshly created above; a partial export
                // must not linger as if complete.
                drop(sink);
                let _ = std::fs::remove_file(output_path);
                Err(error)
            }
        }
    } else {
        let standard_output = std::io::stdout();
        let mut sink = progress::ProgressSink::new(
            froe_export::JsonLinesSink::new(std::io::BufWriter::with_capacity(
                1 << 20,
                standard_output.lock(),
            )),
            quiet,
        );
        let written = froe_export::export_subtree(&repository, path, depth, &mut sink)?;
        Ok(match written {
            Some(nodes) => ExportRun::Exported {
                nodes,
                elapsed: sink.elapsed(),
            },
            None => ExportRun::Missing,
        })
    }
}

/// Brings the Parquet export in `output_directory` up to date: an
/// existing, usable export is refreshed in place — decoding only what
/// changed since it was taken — and anything else (a first export, an
/// unusable base, `--full`) falls to a from-scratch export that
/// replaces the previous tables, one atomic swap per file.
fn run_parquet_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output_directory: &Path,
    full: bool,
    quiet: bool,
) -> froe::Result<ExportRun> {
    let repository = Repository::open(repository_path)?;
    froe_export::create_export_directory(repository_path, output_directory)?;
    if full {
        let FullExport::Completed(run) =
            run_full_parquet_export(&repository, path, depth, output_directory, true, quiet)?
        else {
            unreachable!("a forced export never defers to a refresh");
        };
        return Ok(run);
    }
    let mut reporter = progress::Reporter::new(quiet, "re-exported");
    // A rival writer can turn the directory back into a valid export
    // between the refresh attempt and the fallback's lock; a few
    // rounds settle it either way.
    for _attempt in 0..4 {
        let refresh = froe_export::refresh_parquet_export(
            &repository,
            path,
            depth,
            output_directory,
            &froe_export::ParquetExportOptions::default(),
            &mut |nodes| reporter.report(nodes),
        )?;
        reporter.finish_line();
        match refresh {
            froe_export::ParquetRefresh::Missing => {
                // The full export's own missing-path verdict, reached
                // through the refresh — the existing export is intact.
                return Ok(ExportRun::Missing);
            }
            froe_export::ParquetRefresh::Current { revision } => {
                eprintln!(
                    "froe: the export in {} is already current ({revision})",
                    output_directory.display()
                );
                return Ok(ExportRun::Reported);
            }
            froe_export::ParquetRefresh::Refreshed {
                revision,
                ranges,
                nodes,
            } => {
                eprintln!(
                    "froe: refreshed the export in {} to {revision}: \
                     {ranges} changed ranges, {nodes} nodes re-exported in {:.1}s",
                    output_directory.display(),
                    reporter.elapsed().as_secs_f64()
                );
                return Ok(ExportRun::Reported);
            }
            froe_export::ParquetRefresh::NotReusable {
                reason,
                replaceable,
            } => {
                if !replaceable {
                    // Foreign data and other scopes are never replaced
                    // uninvited — the same hard refusal the streaming
                    // formats give an existing output file.
                    return Err(froe::Error::InvalidFormat {
                        details: format!(
                            "{reason}; refusing to replace it — pass --full to rebuild anyway"
                        ),
                    });
                }
                // A first export is the quiet case; anything present
                // but unusable deserves the reason before it is
                // replaced.
                let leftovers = [
                    froe_export::NODES_FILE_NAME,
                    froe_export::PROPERTIES_FILE_NAME,
                ]
                .iter()
                .any(|name| output_directory.join(name).exists());
                if leftovers {
                    eprintln!("froe: {reason}; exporting from scratch");
                }
                match run_full_parquet_export(
                    &repository,
                    path,
                    depth,
                    output_directory,
                    false,
                    quiet,
                )? {
                    FullExport::Completed(run) => return Ok(run),
                    // A valid export appeared under the lock; the next
                    // round refreshes it.
                    FullExport::RefreshInstead => {}
                }
            }
        }
    }
    Err(froe::Error::InvalidFormat {
        details: format!(
            "the export at {} keeps changing underneath; re-run",
            output_directory.display()
        ),
    })
}

/// Authorizes an automatic (unforced) replacement under the held
/// export lock, against the files as they are now: `false` defers to a
/// refresh round, a guarded directory is the hard refusal — decided
/// under the lock, not inherited from the earlier refresh attempt.
fn authorize_replacement(
    repository: &Repository,
    output_directory: &Path,
    path: &str,
    depth: Option<usize>,
) -> froe::Result<bool> {
    match froe_export::assess_export(repository, output_directory, path, depth) {
        froe_export::ExportAssessment::Reusable => Ok(false),
        froe_export::ExportAssessment::Replaceable(_) => Ok(true),
        froe_export::ExportAssessment::Guarded(reason) => Err(froe::Error::InvalidFormat {
            details: format!("{reason}; refusing to replace it — pass --full to rebuild anyway"),
        }),
    }
}

/// The outcome of a full Parquet export.
enum FullExport {
    /// The export ran; the usual run outcome.
    Completed(ExportRun),
    /// Under the replacement lock, the directory turned out to hold a
    /// valid, refreshable export again — a rival writer published one
    /// between the refresh attempt and this fallback — so the caller
    /// defers to a refresh round instead of replacing it.
    RefreshInstead,
}

/// Exports the Parquet tables from scratch into `output_directory`.
/// The new files are written under temporary names and atomically
/// moved over any existing export only once complete: a failure before
/// the swap leaves the previous export untouched, and a failure between
/// the two swaps leaves disagreeing stamps, which the next refresh
/// rebuilds from. The directory's export lock is held throughout,
/// serializing concurrent writers.
///
/// Unless `forced`, the replacement is authorized afresh under the
/// lock — a verdict the earlier refresh attempt reached before its own
/// lock was released cannot be trusted by the time this lock is held.
/// Finding a valid, refreshable export there defers to a refresh round
/// rather than bulldozing it with a staler full export.
fn run_full_parquet_export(
    repository: &Repository,
    path: &str,
    depth: Option<usize>,
    output_directory: &Path,
    forced: bool,
    quiet: bool,
) -> froe::Result<FullExport> {
    let _lock = froe_export::lock_export_directory(output_directory)?;
    if !forced && !authorize_replacement(repository, output_directory, path, depth)? {
        return Ok(FullExport::RefreshInstead);
    }
    for file_name in [
        froe_export::NODES_FILE_NAME,
        froe_export::PROPERTIES_FILE_NAME,
    ] {
        froe_export::sweep_temporary_outputs(output_directory, file_name)?;
    }
    let nodes_temporary = output_directory.join(froe_export::temporary_output_name(
        froe_export::NODES_FILE_NAME,
    ));
    let properties_temporary = output_directory.join(froe_export::temporary_output_name(
        froe_export::PROPERTIES_FILE_NAME,
    ));
    let remove_temporaries = || {
        let _ = std::fs::remove_file(&nodes_temporary);
        let _ = std::fs::remove_file(&properties_temporary);
    };

    let nodes_file =
        match froe_export::create_export_output(repository.directory(), &nodes_temporary) {
            Ok(file) => file,
            Err(error) => {
                remove_temporaries();
                return Err(error);
            }
        };
    let properties_file =
        match froe_export::create_export_output(repository.directory(), &properties_temporary) {
            Ok(file) => file,
            Err(error) => {
                remove_temporaries();
                return Err(error);
            }
        };
    let provenance = froe_export::ExportProvenance::new(
        repository.head_record_identifier().to_string(),
        path,
        depth,
    );
    let parquet_sink = match froe_export::ParquetSink::new_with_provenance(
        std::io::BufWriter::with_capacity(1 << 20, nodes_file),
        std::io::BufWriter::with_capacity(1 << 20, properties_file),
        &froe_export::ParquetExportOptions::default(),
        &provenance,
    ) {
        Ok(sink) => sink,
        Err(error) => {
            remove_temporaries();
            return Err(error);
        }
    };
    let mut sink = progress::ProgressSink::new(parquet_sink, quiet);
    match froe_export::export_subtree(repository, path, depth, &mut sink) {
        Ok(written) => {
            let elapsed = sink.elapsed();
            // Close the files before the rename: the sink's finish has
            // flushed them, and an open handle would block the move on
            // Windows.
            drop(sink);
            let Some(nodes) = written else {
                remove_temporaries();
                return Ok(FullExport::Completed(ExportRun::Missing));
            };
            let renamed = froe_export::replace_export_output(
                &nodes_temporary,
                &output_directory.join(froe_export::NODES_FILE_NAME),
            )
            .and_then(|()| {
                froe_export::replace_export_output(
                    &properties_temporary,
                    &output_directory.join(froe_export::PROPERTIES_FILE_NAME),
                )
            });
            match renamed {
                Ok(()) => Ok(FullExport::Completed(ExportRun::Exported {
                    nodes,
                    elapsed,
                })),
                Err(error) => {
                    remove_temporaries();
                    Err(error)
                }
            }
        }
        Err(error) => {
            // The temporary files hold a partial export; the existing
            // export, if any, stays untouched.
            drop(sink);
            remove_temporaries();
            Err(error)
        }
    }
}

/// Exports the `SQLite` database into the single file `output_path`.
/// A freshly created file never lingers after either failure shape —
/// the sink's drop implementation owns that cleanup.
fn run_sqlite_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output_path: &Path,
    quiet: bool,
) -> froe::Result<ExportRun> {
    let repository = Repository::open(repository_path)?;
    let mut sink = progress::ProgressSink::new(
        froe_export::SqliteSink::create(
            repository_path,
            output_path,
            froe_export::SqliteExportOptions::default(),
        )?,
        quiet,
    );
    let written = froe_export::export_subtree(&repository, path, depth, &mut sink)?;
    Ok(match written {
        Some(nodes) => ExportRun::Exported {
            nodes,
            elapsed: sink.elapsed(),
        },
        None => ExportRun::Missing,
    })
}

/// Dispatches a checkpoint subcommand. Returns whether it succeeded.
fn run_checkpoint(action: CheckpointAction) -> froe::Result<bool> {
    match action {
        CheckpointAction::List { repository } => {
            // Read-only, exactly like `froe checkpoints`: listing must
            // never take the lock or touch the manifest.
            inspection::print_checkpoints(&Repository::open(&repository)?)?;
            Ok(true)
        }
        CheckpointAction::Create {
            repository,
            lifetime_milliseconds,
            yes,
        } => mutation::run_checkpoint_create(&repository, lifetime_milliseconds, yes),
        CheckpointAction::Remove {
            repository,
            name,
            yes,
        } => mutation::run_checkpoint_remove(&repository, &CheckpointRemoval::Named(name), yes),
        CheckpointAction::RemoveAll { repository, yes } => {
            mutation::run_checkpoint_remove(&repository, &CheckpointRemoval::All, yes)
        }
        CheckpointAction::RemoveUnreferenced { repository, yes } => {
            mutation::run_checkpoint_remove(&repository, &CheckpointRemoval::Unreferenced, yes)
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{CleanupTaskArgument, Command, CommandLine, ExportFormat};

    #[test]
    fn extract_parses_as_the_hidden_export_alias() {
        let parsed = CommandLine::try_parse_from([
            "froe",
            "extract",
            "/store",
            "--path",
            "/content",
            "--depth",
            "2",
            "--output",
            "out.jsonl",
        ])
        .expect("the v0.1.0 extract invocation must keep parsing");
        let Command::Export {
            repository,
            path,
            depth,
            format,
            output,
            full,
            quiet,
        } = parsed.command
        else {
            panic!("extract must dispatch to export");
        };
        assert_eq!(repository, std::path::PathBuf::from("/store"));
        assert_eq!(path, "/content");
        assert_eq!(depth, Some(2));
        assert_eq!(format, ExportFormat::JsonLines);
        assert_eq!(output, Some(std::path::PathBuf::from("out.jsonl")));
        assert!(!full);
        assert!(!quiet);
    }

    #[test]
    fn export_parses_the_quiet_flag() {
        let parsed = CommandLine::try_parse_from([
            "froe",
            "export",
            "/store",
            "--quiet",
            "--output",
            "out.jsonl",
        ])
        .expect("the quiet flag must parse");
        let Command::Export { quiet, .. } = parsed.command else {
            panic!("export must dispatch");
        };
        assert!(quiet);
    }

    #[test]
    fn the_alias_stays_out_of_the_help_text() {
        let mut help = Vec::new();
        <CommandLine as clap::CommandFactory>::command()
            .write_long_help(&mut help)
            .expect("render help");
        let help = String::from_utf8(help).expect("valid UTF-8");
        assert!(help.contains("export"));
        assert!(
            !help.contains("extract"),
            "the compatibility alias must stay undocumented"
        );
    }

    #[test]
    fn cleanup_parses_repeatable_tasks_and_backup_policy() {
        let parsed = CommandLine::try_parse_from([
            "froe",
            "cleanup",
            "/store",
            "--task",
            "journal",
            "--task",
            "recovery-backups",
            "--backup-min-age-days",
            "30",
            "--backup-keep-latest",
            "3",
            "--dry-run",
        ])
        .expect("cleanup arguments parse");
        let Command::Cleanup {
            repository,
            tasks,
            dry_run,
            yes,
            backup_min_age_days,
            backup_keep_latest,
        } = parsed.command
        else {
            panic!("cleanup must dispatch");
        };
        assert_eq!(repository, std::path::PathBuf::from("/store"));
        assert_eq!(
            tasks,
            [
                CleanupTaskArgument::Journal,
                CleanupTaskArgument::RecoveryBackups
            ]
        );
        assert!(dry_run);
        assert!(!yes);
        assert_eq!(backup_min_age_days, Some(30));
        assert_eq!(backup_keep_latest, Some(3));
    }

    #[test]
    fn cleanup_help_states_the_offline_safety_preconditions() {
        let mut command = <CommandLine as clap::CommandFactory>::command();
        let cleanup = command
            .find_subcommand_mut("cleanup")
            .expect("cleanup subcommand");
        let mut help = Vec::new();
        cleanup.write_long_help(&mut help).expect("render help");
        let help = String::from_utf8(help).expect("valid UTF-8");
        for required in [
            "Unix-only offline maintenance",
            "stop Oak/AEM",
            "owner of journal.log",
            "Recovery backups are retained",
            "strictly read-only",
            "canonical absolute",
            "enables recovery-backups",
        ] {
            assert!(
                help.contains(required),
                "cleanup help omitted {required:?}: {help}"
            );
        }
    }

    #[test]
    fn compact_help_states_archive_publication_requirements() {
        let mut command = <CommandLine as clap::CommandFactory>::command();
        let compact = command
            .find_subcommand_mut("compact")
            .expect("compact subcommand");
        let mut help = Vec::new();
        compact.write_long_help(&mut help).expect("render help");
        let help = String::from_utf8(help).expect("valid UTF-8");
        for required in [
            "same-directory hard links",
            "directory-fsync",
            "fail safely",
        ] {
            assert!(
                help.contains(required),
                "compact help omitted {required:?}: {help}"
            );
        }
    }

    #[test]
    fn every_mutating_command_help_states_absent_lock_requirements() {
        fn long_help(path: &[&str]) -> String {
            let mut command = <CommandLine as clap::CommandFactory>::command();
            let mut selected = &mut command;
            for component in path {
                selected = selected
                    .find_subcommand_mut(component)
                    .unwrap_or_else(|| panic!("missing subcommand path {path:?}"));
            }
            let mut help = Vec::new();
            selected.write_long_help(&mut help).expect("render help");
            String::from_utf8(help).expect("valid UTF-8")
        }

        for path in [
            &["compact"][..],
            &["cleanup"],
            &["backup"],
            &["restore"],
            &["recover-journal"],
            &["checkpoint", "create"],
            &["checkpoint", "remove"],
            &["checkpoint", "remove-all"],
            &["checkpoint", "remove-unreferenced"],
        ] {
            let help = long_help(path);
            for required in [
                "repo.lock is absent",
                "same-directory hard-link",
                "directory-fsync",
            ] {
                assert!(
                    help.contains(required),
                    "help for {path:?} omitted {required:?}: {help}"
                );
            }
        }
    }
}
