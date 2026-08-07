//! Command-line interface for inspecting and extracting data from Apache
//! Jackrabbit Oak `segment-tar` (`TarMK`) repositories.
//!
//! Every command is read-only: the repository lock is never taken and no
//! file is ever modified, so pointing `froe` at a live repository is safe.

mod content_display;
mod extraction;
mod inspection;
mod output;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use froe::store::Repository;

#[derive(Parser)]
#[command(
    name = "froe",
    version,
    about = "Read-only tooling for Apache Jackrabbit Oak segment-tar (TarMK) repositories",
    long_about = "Read-only tooling for Apache Jackrabbit Oak segment-tar (TarMK) repositories, \
                  the storage format of Apache Jackrabbit Oak and Adobe Experience Manager. \
                  All commands open the repository without locking and never modify any file."
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
    /// Extract node data as JSON lines (one object per node).
    Extract {
        /// The segment store directory.
        repository: PathBuf,
        /// The content path to extract from.
        #[arg(long, default_value = "/")]
        path: String,
        /// Bound the extraction depth; omit to extract the whole subtree.
        #[arg(long)]
        depth: Option<usize>,
        /// Write to this file instead of standard output.
        #[arg(long)]
        output: Option<PathBuf>,
    },
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
                eprintln!("froe: no node at {path}");
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Tree {
            repository,
            path,
            depth,
        } => {
            if !content_display::print_tree(&Repository::open(&repository)?, &path, depth)? {
                eprintln!("froe: no node at {path}");
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Checkpoints { repository } => {
            inspection::print_checkpoints(&Repository::open(&repository)?)?;
        }
        Command::Extract {
            repository,
            path,
            depth,
            output,
        } => {
            let repository = Repository::open(&repository)?;
            let written = if let Some(output_path) = output {
                let file = std::fs::File::create(&output_path)?;
                let mut sink = std::io::BufWriter::with_capacity(1 << 20, file);
                let written = extraction::extract_json_lines(&repository, &path, depth, &mut sink)?;
                sink.flush()?;
                written
            } else {
                let standard_output = std::io::stdout();
                let mut sink = std::io::BufWriter::with_capacity(1 << 20, standard_output.lock());
                let written = extraction::extract_json_lines(&repository, &path, depth, &mut sink)?;
                sink.flush()?;
                written
            };
            if let Some(node_count) = written {
                eprintln!("froe: extracted {node_count} nodes");
            } else {
                eprintln!("froe: no node at {path}");
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
