//! The `froe index` subcommands.
//!
//! They live here rather than in `command_line.rs` because this is the file
//! the later plans extend: plan 0008's `dump` and `import`, plan 0007's
//! `reindex`, plan 0010's transport. `command_line.rs` gains the one `Index`
//! variant and nothing more.

use std::path::PathBuf;

use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub(crate) enum IndexAction {
    /// List the index definitions and what froe can read about each
    /// (read-only).
    ///
    /// Opens the store exactly as `froe summary` does: no lock is taken, no
    /// manifest is written, and no file is created.
    List {
        /// The segment store directory.
        repository: PathBuf,
        /// Restrict to this definition path; repeatable. A path naming no
        /// definition in the store is refused by name.
        #[arg(long = "index", value_name = "PATH")]
        indexes: Vec<String>,
    },
    /// Print the index definitions in oak-run's JSON form (read-only).
    ///
    /// The output is what `oak-run index --index-definitions-file` and Oak's
    /// own definition updater consume. Opens the store exactly as
    /// `froe summary` does: no lock, no manifest write, no file created.
    Definitions {
        /// The segment store directory.
        repository: PathBuf,
        /// Write the JSON here instead of to standard output. Unlike
        /// `froe digest --output`, an existing file is refused rather than
        /// truncated: a definitions file is an artifact an operator keeps
        /// and re-imports.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Restrict to this definition path; repeatable. A path naming no
        /// definition in the store is refused by name.
        #[arg(long = "index", value_name = "PATH")]
        indexes: Vec<String>,
    },
    /// Check each index against the state it indexes (read-only).
    ///
    /// Exits 0 when every checked index is consistent, 3 when any is
    /// inconsistent, and 4 when none is inconsistent but some index with an
    /// applicable check could not be run. Opens the store exactly as
    /// `froe summary` does: no lock, no manifest write, no file created.
    Check {
        /// The segment store directory.
        repository: PathBuf,
        /// Restrict to this definition path; repeatable. A path naming no
        /// definition in the store is refused by name.
        #[arg(long = "index", value_name = "PATH")]
        indexes: Vec<String>,
    },
}

impl IndexAction {
    /// The store every variant carries.
    pub(crate) fn repository(&self) -> &std::path::Path {
        match self {
            IndexAction::List { repository, .. }
            | IndexAction::Definitions { repository, .. }
            | IndexAction::Check { repository, .. } => repository,
        }
    }
}
