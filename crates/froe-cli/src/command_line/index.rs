//! The `froe index` subcommands.
//!
//! They live here rather than in `command_line.rs` because this is the file
//! the later plans extend: plan 0008's `dump` and `import`, plan 0007's
//! `reindex`, plan 0010's transport. `command_line.rs` gains the one `Index`
//! variant and nothing more.

use std::path::PathBuf;

use clap::Subcommand;

/// What a binary property contributes to a Lucene index.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum BinaryTextChoice {
    /// Oak's own `TextExtractionError` marker, which keeps the field
    /// present and makes the absence of text visible to a query.
    Marker,
    /// Nothing: the binary contributes no field, which is what Oak does
    /// for a type Tika does not support.
    Skip,
}

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
    /// Write Lucene index data to the filesystem in oak-run's layout
    /// (read-only).
    ///
    /// Opens the store exactly as `froe summary` does: no lock, no manifest
    /// write, and not one byte written inside the repository. Everything
    /// goes under `--output`, which must not be inside the store.
    ///
    /// The directory oak-run's importer reads is `<output>/index-dumps`;
    /// pass that to `froe index import --input` or to oak-run's
    /// `--index-import-dir`.
    ///
    /// An existing dump is never written over. A run that was interrupted
    /// leaves a partial file set, and the next run refuses it rather than
    /// completing it into a directory that is part one dump and part
    /// another — delete the output directory and rerun.
    Dump {
        /// The segment store directory.
        repository: PathBuf,
        /// Where to write. Must not be inside the store.
        #[arg(long)]
        output: PathBuf,
        /// Restrict to this definition path; repeatable. A path naming a
        /// definition that is not `lucene` is refused by name.
        ///
        /// `indexer-info.properties` names one checkpoint for the whole
        /// directory, so dump one lane at a time when the store has
        /// definitions on several: a mixed selection is still written as a
        /// backup, but without that file it cannot be imported.
        #[arg(long = "index", value_name = "PATH")]
        indexes: Vec<String>,
    },
    /// Rebuild flagged indexes offline, from the state Oak's own editors
    /// would index.
    ///
    /// Takes the repository lock and moves the head exactly once. The
    /// repository must be offline: no Oak instance may be running against
    /// it, and no other froe run may hold the lock.
    ///
    /// The one irreversible consequence: the index records this run
    /// replaces become unreachable from the head. They stay live through
    /// every checkpoint that references them — each lane's checkpoint does,
    /// by construction, since a checkpoint pins the content root — and are
    /// reclaimed only by a `froe compact` run after those checkpoints are
    /// released. A reindex therefore grows the store until then, and the
    /// summary names the checkpoints that pin the old records.
    Reindex {
        /// The segment store directory.
        repository: PathBuf,
        /// Restrict to this definition path; repeatable. A definition named
        /// here is always answered — rebuilt, or refused by name with the
        /// reason. Without it, every definition flagged `reindex = true` is
        /// considered, exactly as Oak's own cycle considers them.
        #[arg(long = "index", value_name = "PATH")]
        indexes: Vec<String>,
        /// Plan without taking the lock and without writing anything, then
        /// print what a run would do.
        #[arg(long)]
        dry_run: bool,
        /// Answer yes to the plan confirmation.
        #[arg(long)]
        yes: bool,
        /// Spill the sort's runs here instead of in the system temporary
        /// directory.
        ///
        /// The run creates one subdirectory named from the store's path and
        /// holds a lock in it, so runs on different stores never collide
        /// and a live run is never mistaken for residue. A froe-named
        /// subdirectory left by an earlier run is refused here, because the
        /// directory is yours: under the default it is only a warning.
        ///
        /// The default is the system temporary directory, which on many
        /// Linux systems is a tmpfs held in memory — a large reindex can
        /// exhaust it. Name a directory on disk for a large store.
        #[arg(long, value_name = "DIRECTORY")]
        work_directory: Option<PathBuf>,
        /// Rebuild from the head when a definition's lane checkpoint is
        /// dangling, or its lane is absent from `/:async`.
        ///
        /// Consulted only then: a definition whose lane resolves is always
        /// rebuilt from that lane's checkpoint, and this flag is ignored.
        ///
        /// For a mirror or unique index this is the explicit choice to
        /// index the head instead: the lane's own replay leaves every entry
        /// unchanged, and only the randomized `:count_*` estimates drift.
        /// For a counter it is the choice to *reset* — the hidden children
        /// are removed and nothing is built, so Oak's own replay rebuilds
        /// the counter from scratch. A rebuilt counter would be doubled by
        /// that replay whether or not froe ran.
        #[arg(long)]
        from_head: bool,
        /// How much of the sort may stay resident before it spills, in
        /// mebibytes. Higher is faster and uses more memory; the run's
        /// residency does not otherwise grow with the number of indexed
        /// nodes.
        #[arg(long, value_name = "N")]
        sort_budget_mebibytes: Option<usize>,
        /// What a binary property contributes to a Lucene index.
        ///
        /// **Required for every Lucene definition**, binaries or not:
        /// froe extracts no text, so this is a decision only you can make
        /// and `skip` is how you state that a definition indexes none.
        ///
        /// Oak runs Tika over a binary and indexes the text it gets; where
        /// Tika does not support the type it indexes nothing, and where
        /// Tika threw it indexes the marker `TextExtractionError`. So
        /// `skip` reproduces Oak exactly for the unsupported types and
        /// `marker` reproduces only the failed-extraction case — neither
        /// reproduces a successful extraction, which is what
        /// `--pre-extracted-text-directory` is for.
        ///
        /// A binary on a node with no `jcr:mimeType` is never indexed
        /// whatever you choose here, because Oak's own extraction stops
        /// there first.
        #[arg(long, value_name = "marker|skip")]
        binary_text: Option<BinaryTextChoice>,
        /// Read Oak's own pre-extracted text store first, and fall back to
        /// `--binary-text` for a blob it does not cover.
        ///
        /// Text in that store is text Oak extracted, so an index built
        /// with it answers as Oak's does for every blob it holds. An
        /// inline segment blob is never in it: the store is keyed by a
        /// blob's content identity, which an inlined value has none of.
        #[arg(long, value_name = "DIRECTORY", requires = "binary_text")]
        pre_extracted_text_directory: Option<PathBuf>,
    },
    /// Import Lucene index data built out of band back into a stopped
    /// store.
    ///
    /// The store must be offline: the run holds the repository lock from
    /// planning through publication, and no Oak instance may be running
    /// against it.
    ///
    /// froe does not bring an imported index up to date, as oak-run does
    /// by replaying commits against a live repository. It requires instead
    /// that there is nothing to catch up on: **the index must have been
    /// built at the checkpoint this store is at**, meaning the checkpoint
    /// named in `indexer-info.properties` must resolve to the same state
    /// the definition's lane will resume from. A directory built at any
    /// other state is refused, naming both checkpoints.
    ///
    /// `index-definitions.json` must describe the definitions as this
    /// store holds them. froe imports index *data*, never a definition
    /// change: make definition changes through oak-run or AEM first. The
    /// properties an out-of-band build legitimately rewrites —
    /// `reindexCount`, `refresh`, a created `seed`, a cleared `corrupt` or
    /// `indexImportState`, and a `facets` subtree — are accepted in the
    /// direction they happen.
    ///
    /// No checkpoint is released, and `:suggest-data` is never imported:
    /// Oak's own writer rebuilds the suggestions on its next cycle.
    Import {
        /// The segment store directory.
        repository: PathBuf,
        /// The directory to import from — oak-run's `index-dumps`, or the
        /// one `froe index dump` wrote.
        #[arg(long)]
        input: PathBuf,
        /// Restrict to this definition path; repeatable. A path with no
        /// index directory in the input is refused by name.
        #[arg(long = "index", value_name = "PATH")]
        indexes: Vec<String>,
        /// Plan without taking the lock and without writing anything, then
        /// print what a run would do.
        #[arg(long)]
        dry_run: bool,
        /// Answer yes to the plan confirmation.
        #[arg(long)]
        yes: bool,
    },
}

impl IndexAction {
    /// The store every variant carries.
    pub(crate) fn repository(&self) -> &std::path::Path {
        match self {
            IndexAction::List { repository, .. }
            | IndexAction::Definitions { repository, .. }
            | IndexAction::Check { repository, .. }
            | IndexAction::Reindex { repository, .. }
            | IndexAction::Dump { repository, .. }
            | IndexAction::Import { repository, .. } => repository,
        }
    }
}
