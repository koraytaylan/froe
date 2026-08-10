//! Command-line interface for inspecting, exporting from, and maintaining
//! Apache Jackrabbit Oak `segment-tar` (`TarMK`) repositories.
//!
//! Inspection and export commands are read-only: the repository lock
//! is never taken, so they are safe against a live repository (archives
//! are memory-mapped under the store's never-modify-in-place file
//! protocol, the same reliance a running Oak instance has). The
//! maintenance commands — `compact`, `backup`, `restore`,
//! `recover-journal`, and `checkpoint` — take the exclusive repository
//! lock and modify the store, so they must only be run against a *stopped*
//! repository, and each asks for confirmation first.

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
                  recover-journal, and checkpoint commands modify the store and must be run \
                  against a stopped repository."
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
        /// Where the export goes; never an existing file — the export
        /// never overwrites. For json-lines a file (standard output when
        /// omitted); for parquet the directory receiving nodes.parquet
        /// and properties.parquet (created if absent, required); for
        /// sqlite the database file (required).
        #[arg(long)]
        output: Option<PathBuf>,
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
    /// Copy a repository's head into a target store (modifies the target).
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
    RemoveAll {
        /// The segment store directory.
        repository: PathBuf,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Remove checkpoints not referenced by the asynchronous indexer.
    RemoveUnreferenced {
        /// The segment store directory.
        repository: PathBuf,
        /// Proceed without the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
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
    let command_line = CommandLine::parse();
    match run(command_line.command) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("froe: {error}");
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
            quiet,
        } => {
            let written = match format {
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
                    run_parquet_export(&repository_path, &path, depth, output_directory, quiet)?
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
            if let Some((node_count, elapsed)) = written {
                let rate = if elapsed.is_zero() {
                    0.0
                } else {
                    // A node count is a display figure; the precision
                    // loss of the cast is irrelevant at the reported
                    // scale.
                    #[allow(clippy::cast_precision_loss)]
                    let rate = node_count as f64 / elapsed.as_secs_f64();
                    rate
                };
                match &output {
                    Some(destination) => eprintln!(
                        "froe: exported {node_count} nodes to {} in {:.1}s ({:.0} nodes/s)",
                        destination.display(),
                        elapsed.as_secs_f64(),
                        rate
                    ),
                    None => eprintln!(
                        "froe: exported {node_count} nodes in {:.1}s ({:.0} nodes/s)",
                        elapsed.as_secs_f64(),
                        rate
                    ),
                }
            } else {
                eprintln!("froe: no node at {}", output::sanitize_terminal_text(&path));
                return Ok(ExitCode::FAILURE);
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

/// Streams a JSON lines export to `output`, or to standard output when
/// `output` is `None`. Returns the node count and elapsed time, or `None`
/// when the path does not exist; a freshly created output file never
/// lingers after either failure shape.
fn run_json_lines_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output: Option<&Path>,
    quiet: bool,
) -> froe::Result<Option<(u64, std::time::Duration)>> {
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
                Ok(written.map(|count| (count, elapsed)))
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
        froe_export::export_subtree(&repository, path, depth, &mut sink)
            .map(|written| written.map(|count| (count, sink.elapsed())))
    }
}

/// Exports the Parquet tables into `output_directory` (created if
/// absent). Returns the node count and elapsed time, or `None` when the
/// path does not exist; freshly created files never linger after either
/// failure shape.
fn run_parquet_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output_directory: &Path,
    quiet: bool,
) -> froe::Result<Option<(u64, std::time::Duration)>> {
    let repository = Repository::open(repository_path)?;
    froe_export::create_export_directory(repository_path, output_directory)?;
    let nodes_path = output_directory.join("nodes.parquet");
    let properties_path = output_directory.join("properties.parquet");
    let remove_outputs = || {
        let _ = std::fs::remove_file(&nodes_path);
        let _ = std::fs::remove_file(&properties_path);
    };

    let nodes_file = froe_export::create_export_output(repository_path, &nodes_path)?;
    let properties_file = match froe_export::create_export_output(repository_path, &properties_path)
    {
        Ok(file) => file,
        Err(error) => {
            // Only the nodes file is fresh; the properties path
            // holds someone else's data and must stay.
            let _ = std::fs::remove_file(&nodes_path);
            return Err(error);
        }
    };
    let parquet_sink = match froe_export::ParquetSink::new(
        std::io::BufWriter::with_capacity(1 << 20, nodes_file),
        std::io::BufWriter::with_capacity(1 << 20, properties_file),
        &froe_export::ParquetExportOptions::default(),
    ) {
        Ok(sink) => sink,
        Err(error) => {
            remove_outputs();
            return Err(error);
        }
    };
    let mut sink = progress::ProgressSink::new(parquet_sink, quiet);
    match froe_export::export_subtree(&repository, path, depth, &mut sink) {
        Ok(written) => {
            let elapsed = sink.elapsed();
            if written.is_none() {
                // Nothing was exported: the freshly created files must
                // not linger.
                drop(sink);
                remove_outputs();
            }
            Ok(written.map(|count| (count, elapsed)))
        }
        Err(error) => {
            // The files were freshly created above; a partial export
            // must not linger as if complete.
            drop(sink);
            remove_outputs();
            Err(error)
        }
    }
}

/// Exports the `SQLite` database into the single file `output_path`.
/// Returns the node count and elapsed time, or `None` when the path does
/// not exist; a freshly created file never lingers after either failure
/// shape — the sink's drop implementation owns that cleanup.
fn run_sqlite_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output_path: &Path,
    quiet: bool,
) -> froe::Result<Option<(u64, std::time::Duration)>> {
    let repository = Repository::open(repository_path)?;
    let mut sink = progress::ProgressSink::new(
        froe_export::SqliteSink::create(
            repository_path,
            output_path,
            froe_export::SqliteExportOptions::default(),
        )?,
        quiet,
    );
    froe_export::export_subtree(&repository, path, depth, &mut sink)
        .map(|written| written.map(|count| (count, sink.elapsed())))
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

    use super::{Command, CommandLine, ExportFormat};

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
}
