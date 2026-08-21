//! The command line froe accepts: every command, its flags, and the
//! value predicate a search takes.

use super::{Parser, PathBuf, ProgressWhen, Subcommand};

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
                  against a stopped repository. `compact` is the one maintenance command: it \
                  compacts and reclaims in a single run, and `--dry-run` previews it read-only. If repo.lock is absent, every mutating command \
                  requires same-directory hard-link and durable directory-fsync support to publish \
                  that lock safely."
)]
pub(crate) struct CommandLine {
    #[command(subcommand)]
    pub(crate) command: Command,
    /// Suppress progress and status reports on standard error. Errors,
    /// warnings, confirmation prompts, and every command's own output are
    /// unaffected: --silent hides what froe is doing, never what it found
    /// or what it is about to change.
    #[arg(long, short = 's', global = true, alias = "quiet")]
    pub(crate) silent: bool,
    /// When to report progress on standard error. "auto" animates a live
    /// line on a terminal, writes plain throttled lines elsewhere, and
    /// stays quiet about anything that finishes promptly; "always" reports
    /// every step from the moment it begins, for logs and scripts.
    #[arg(
        long,
        value_enum,
        global = true,
        default_value = "auto",
        conflicts_with = "silent"
    )]
    pub(crate) progress: ProgressWhen,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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
    /// Show one segment's structure, or its Oak-compatible raw hex dump.
    Segment {
        /// The segment store directory.
        repository: PathBuf,
        /// The segment UUID, for example f81378fb-92b1-4b52-a5c8-e0a67152ed2c.
        identifier: String,
        /// Print the full SegmentDump-style header, record table, and bytes.
        #[arg(long)]
        hex: bool,
    },
    /// Attribute current-head paths to records in one or more TAR archives.
    Debug {
        /// The segment store directory.
        repository: PathBuf,
        /// Canonical archive names, for example data00000a.tar; missing or
        /// inactive archives are reported and skipped.
        #[arg(required = true)]
        archives: Vec<String>,
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
    /// Render the repository's content canonically, for comparison
    /// before and after an operation (read-only).
    ///
    /// Maintenance is supposed to move bytes without changing content.
    /// `check` proves every record still parses, which a store whose
    /// properties decoded at the wrong arity also does. This renders what
    /// the content actually is — every node, property, type, arity, value
    /// and binary, in the head and in every checkpoint — so the two can be
    /// compared directly.
    Digest {
        /// The segment store directory.
        repository: PathBuf,
        /// Write the digest here instead of to standard output.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Compare against a digest written earlier and report the paths
        /// that differ. Exits non-zero when anything differs.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Omit a content subtree from the rendering; repeatable. The
        /// digest then opens with a header naming every exclusion, so it
        /// can never be compared blind against a full one. This is how a
        /// digest taken before a version-history purge is compared against
        /// one taken after it, with the purge and nothing else excused.
        #[arg(long = "exclude-subtree")]
        exclude_subtrees: Vec<String>,
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
        /// Stop after this many matches (0 = no limit, which buffers every
        /// match in memory and is unbounded on a large store).
        #[arg(long, default_value_t = 10_000)]
        limit: usize,
    },
    /// Compact the repository and reclaim everything the compacted head does
    /// not reach (modifies the store).
    ///
    /// This is froe's one maintenance command. It deep-copies the head and
    /// every retained checkpoint into a fresh garbage-collection generation,
    /// retires the journal to that single revision, and removes every archive
    /// the new head does not need — reclaiming orphan storage, stale archive
    /// leftovers, expired checkpoints and proven-redundant staging files in the
    /// same run. Exactly one generation is retained, which is what Apache Oak's
    /// own offline compact tool does (SegmentGCOptions.setOffline sets
    /// retainedGenerations = 1).
    ///
    /// A full run is the default. Three further cleanups are part of it, each
    /// gated by its own yes/no question on an interactive run and all answered
    /// at once by --yes: purging orphaned version histories, rebuilding a
    /// missing archive index (what a killed writer leaves behind), and
    /// removing recovery backups left by earlier runs. Each has a --skip-*
    /// flag that removes it from the run entirely.
    ///
    /// --dry-run is strictly read-only: it writes no bytes, creates no files,
    /// and does not create or acquire repo.lock; it previews the full default
    /// run, honoring the --skip-* flags. Applying is Unix-only offline
    /// maintenance: stop Oak/AEM, run as the operating-system owner of
    /// journal.log (normally the service account, not sudo), and keep a
    /// recoverable copy of important stores. The repository argument is
    /// resolved once to its canonical absolute target before planning or
    /// locking, and that target is shown in the plan.
    ///
    /// Journal history is retired, not preserved: after a successful run
    /// journal.log holds exactly one line, naming the compacted head. The
    /// removed history is not recoverable from the store afterwards;
    /// journal.log is copied to a numbered .bak first. Recovery backups are
    /// retained by a run that also repairs an archive index, by
    /// --skip-removing-recovery-backups, and by the
    /// --backup-minimum-age-days / --backup-keep-latest retention window.
    ///
    /// Reclamation publishes validated successor TARs with absent-only,
    /// same-directory hard links. A filesystem without hard-link or durable
    /// directory-fsync support can fail safely after the compacted head is
    /// committed, leaving the source archives for a later retry. When repo.lock
    /// is absent, publication independently requires same-directory hard-link
    /// and durable directory-fsync support.
    Compact {
        /// The segment store directory.
        repository: PathBuf,
        /// Run a tail compaction instead of a full one. A tail run keeps the
        /// shared full generation, so it never purges orphaned version
        /// histories; that waits for a full compaction.
        #[arg(long)]
        tail: bool,
        /// Copy the head into a fresh generation even when the store is
        /// already fully compacted and the swap would net nothing.
        ///
        /// Never needed for space: the convergence gate only ever drops a
        /// copy whose output would replace an identical generation. This is
        /// for the operator who wants a rewrite anyway, for example after
        /// suspected mapping-level damage.
        #[arg(long)]
        always_copy: bool,
        /// Keep every orphaned version history instead of purging them: the
        /// nt:versionHistory subtrees whose jcr:versionableUuid matches no
        /// live jcr:uuid, which pin the version payloads and binaries of
        /// everything ever deleted.
        ///
        /// The purge is otherwise part of every full compaction, after its
        /// own confirmation (--yes confirms it), and the plan lists it with
        /// exact counts before anything is applied. Detection runs and is
        /// reported on every plan regardless. Removal is permanent, and
        /// forfeits the re-attachment a versionable recreated with its old
        /// identifier — a content package reinstall — would otherwise get.
        /// Histories that freeze nt:configuration versionables, and
        /// histories that REFERENCE or WEAKREFERENCE values outside version
        /// storage still point into, are always kept with a warning.
        #[arg(long)]
        skip_purging_orphaned_version_histories: bool,
        /// Purge only histories whose newest version was created at least
        /// this many days ago, guarding the window where content deleted
        /// moments ago is about to be restored from a package. A history
        /// without a parsable version creation date is kept. Passing this
        /// selects the purge without asking.
        #[arg(long, conflicts_with_all = ["skip_purging_orphaned_version_histories", "tail"])]
        purged_history_minimum_age_days: Option<u64>,
        /// Print the complete plan and exit without taking repo.lock or
        /// writing a byte.
        #[arg(long)]
        dry_run: bool,
        /// Answer yes to every question: the plan confirmation, and the
        /// per-cleanup questions an interactive run asks first.
        #[arg(long)]
        yes: bool,
        /// Never rebuild the index of an active archive that has none — the
        /// state a killed Oak writer leaves behind. Nothing index-dependent
        /// can be planned in that state, so a damaged store is then refused
        /// with nothing changed.
        ///
        /// The rebuild is otherwise part of every run that needs one, after
        /// its own confirmation (--yes confirms it): it runs under the lock
        /// before the one plan is built, keeps the original bytes under a
        /// .bak name, and the plan you confirm already reflects the rebuilt
        /// indexes. A store damaged in the middle rather than at the tail is
        /// still worth looking at before authorizing — the question and the
        /// refusal both say which case this store is.
        #[arg(long)]
        skip_repairing_archive_indexes: bool,
        /// Copy checkpoints whose valid timestamp has passed into the fresh
        /// generation instead of dropping them.
        ///
        /// Dropping is the default: an expired checkpoint's content is
        /// otherwise copied into the new generation, where one retained
        /// generation can never reclaim it.
        #[arg(long)]
        keep_expired_checkpoints: bool,
        /// Also drop checkpoints that no string value under /:async
        /// references. Not a default: an operator-created checkpoint held for
        /// a backup is unreferenced by that rule.
        #[arg(long)]
        remove_unreferenced_checkpoints: bool,
        /// Keep every recovery backup — journal.log.bak.NNN and the archive
        /// .bak spellings — instead of removing them.
        ///
        /// Removal is otherwise part of every run, after its own
        /// confirmation (--yes confirms it), and always as the run's last
        /// mutation, once the store has verified. A run that repairs an
        /// archive index keeps them regardless: the repair writes the .bak
        /// files a removal could otherwise delete in the same run, so their
        /// removal waits for the next one.
        #[arg(long, conflicts_with_all = ["backup_minimum_age_days", "backup_keep_latest"])]
        skip_removing_recovery_backups: bool,
        /// Remove only recovery backups at least this many days old.
        /// Without it age protects nothing; future-dated backups are never
        /// old enough.
        #[arg(long)]
        backup_minimum_age_days: Option<u64>,
        /// Retain at least this many newest backups per original target
        /// (all modification-time ties at the boundary are kept). Without it
        /// no backup is retained by count.
        #[arg(long)]
        backup_keep_latest: Option<usize>,
        /// The spelling from when the purge was opt-in rather than default:
        /// passing it selects the purge without asking, exactly as it
        /// always did. Hidden compatibility flag.
        #[arg(long, hide = true, conflicts_with_all = ["skip_purging_orphaned_version_histories", "tail"])]
        purge_orphaned_version_histories: bool,
        /// The spelling from when the repair was opt-in rather than
        /// default: passing it authorizes the rebuild without asking,
        /// exactly as it always did. Hidden compatibility flag.
        #[arg(long, hide = true, conflicts_with = "skip_repairing_archive_indexes")]
        repair_archive_indexes: bool,
        /// Which archives holding reclaimable segments may be rewritten.
        ///
        /// every-reclaimable-archive, the default, rewrites any archive that
        /// holds identified garbage. oak-savings-gate reproduces Apache Oak's
        /// heuristic, which leaves an archive untouched unless the rewrite
        /// would shrink it by at least a quarter — so an archive holding live
        /// binary content beside dead node segments keeps that garbage for the
        /// life of the store, whatever it is asked. The policy applies to the
        /// one sweep this command performs.
        #[arg(long, value_enum, default_value_t = ArchiveRewritePolicyArgument::EveryReclaimableArchive)]
        archive_rewrite_policy: ArchiveRewritePolicyArgument,
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
pub(crate) enum ExportFormat {
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

#[derive(Debug, Subcommand)]
pub(crate) enum CheckpointAction {
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
pub(crate) enum ArchiveRewritePolicyArgument {
    /// Rewrite every archive holding reclaimable segments.
    EveryReclaimableArchive,
    /// Apply Oak's 25% savings heuristic.
    OakSavingsGate,
}

/// Parses a `NAME=VALUE` search predicate.
pub(crate) fn parse_value_predicate(
    argument: &str,
) -> std::result::Result<(String, String), String> {
    argument
        .split_once('=')
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("expected NAME=VALUE, got {argument:?}"))
}

#[cfg(test)]
mod tests {
    use super::{ArchiveRewritePolicyArgument, Command, CommandLine, ExportFormat};
    use crate::ProgressWhen;
    use clap::Parser;

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
        assert!(!parsed.silent);
        let Command::Export {
            repository,
            path,
            depth,
            format,
            output,
            full,
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
    }

    #[test]
    fn the_export_quiet_flag_still_parses_as_silent() {
        // `--quiet` was `export`'s own flag before reporting became
        // uniform; the invocation must keep working.
        let parsed = CommandLine::try_parse_from([
            "froe",
            "export",
            "/store",
            "--quiet",
            "--output",
            "out.jsonl",
        ])
        .expect("the v0.6.0 quiet flag must keep parsing");
        assert!(parsed.silent);
        assert!(matches!(parsed.command, Command::Export { .. }));
    }

    #[test]
    fn silence_is_global_and_abbreviated() {
        for arguments in [
            ["froe", "compact", "/store", "--silent"],
            ["froe", "compact", "/store", "-s"],
            ["froe", "compact", "/store", "--quiet"],
        ] {
            let parsed =
                CommandLine::try_parse_from(arguments).expect("silence parses on every command");
            assert!(parsed.silent, "{arguments:?} did not request silence");
            assert_eq!(parsed.progress, ProgressWhen::Auto);
        }
    }

    #[test]
    fn progress_is_global_and_defaults_to_auto() {
        let parsed =
            CommandLine::try_parse_from(["froe", "summary", "/store"]).expect("the default parses");
        assert_eq!(parsed.progress, ProgressWhen::Auto);
        for (argument, expected) in [
            ("always", ProgressWhen::Always),
            ("never", ProgressWhen::Never),
            ("auto", ProgressWhen::Auto),
        ] {
            let parsed =
                CommandLine::try_parse_from(["froe", "compact", "/store", "--progress", argument])
                    .expect("every progress mode parses");
            assert_eq!(parsed.progress, expected);
        }
    }

    #[test]
    fn silence_and_an_explicit_progress_mode_are_refused_together() {
        let parsed = CommandLine::try_parse_from([
            "froe",
            "compact",
            "/store",
            "--silent",
            "--progress",
            "always",
        ]);
        let Err(error) = parsed else {
            panic!("contradictory reporting requests must be refused");
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn the_reporting_flags_are_documented_on_every_command() {
        let mut command = <CommandLine as clap::CommandFactory>::command();
        // Global arguments reach the subcommands only once the command is
        // built; an unbuilt tree would report them missing.
        command.build();
        for path in [&["export"][..], &["summary"], &["compact"]] {
            let mut selected = &mut command;
            for component in path {
                selected = selected
                    .find_subcommand_mut(component)
                    .unwrap_or_else(|| panic!("missing subcommand path {path:?}"));
            }
            let mut help = Vec::new();
            selected.write_long_help(&mut help).expect("render help");
            let help = String::from_utf8(help).expect("valid UTF-8");
            for required in ["--silent", "--progress"] {
                assert!(
                    help.contains(required),
                    "help for {path:?} omitted {required}: {help}"
                );
            }
            assert!(
                !help.contains("--quiet"),
                "the compatibility alias must stay undocumented: {help}"
            );
        }
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
    fn compact_parses_the_backup_retention_policy() {
        let parsed = CommandLine::try_parse_from([
            "froe",
            "compact",
            "/store",
            "--backup-minimum-age-days",
            "30",
            "--backup-keep-latest",
            "3",
            "--dry-run",
        ])
        .expect("compact arguments parse");
        let Command::Compact {
            repository,
            tail,
            always_copy,
            skip_purging_orphaned_version_histories,
            purged_history_minimum_age_days,
            dry_run,
            yes,
            skip_repairing_archive_indexes,
            keep_expired_checkpoints,
            remove_unreferenced_checkpoints,
            skip_removing_recovery_backups,
            backup_minimum_age_days,
            backup_keep_latest,
            purge_orphaned_version_histories,
            repair_archive_indexes,
            archive_rewrite_policy,
        } = parsed.command
        else {
            panic!("compact must dispatch");
        };
        assert_eq!(repository, std::path::PathBuf::from("/store"));
        assert!(dry_run);
        assert!(!yes);
        assert!(!tail, "a full compaction is the default");
        assert!(!always_copy, "the convergence gate is on by default");
        assert!(
            !skip_purging_orphaned_version_histories
                && !skip_repairing_archive_indexes
                && !skip_removing_recovery_backups,
            "nothing is skipped unless asked"
        );
        assert!(purged_history_minimum_age_days.is_none());
        assert_eq!(backup_minimum_age_days, Some(30));
        assert_eq!(backup_keep_latest, Some(3));
        assert_eq!(
            archive_rewrite_policy,
            ArchiveRewritePolicyArgument::EveryReclaimableArchive,
            "reclaiming every identified segment is the default"
        );
        assert!(
            !purge_orphaned_version_histories && !repair_archive_indexes,
            "the hidden compatibility spellings default off"
        );
        assert!(
            !keep_expired_checkpoints,
            "an expired checkpoint is dropped from the copy by default"
        );
        assert!(
            !remove_unreferenced_checkpoints,
            "dropping an unreferenced checkpoint stays opt-in"
        );
    }

    /// Each retention flag stands alone now that backup removal is a
    /// default rather than something the pair enables: an age bound
    /// without a count, or a count without an age, is a valid narrowing.
    #[test]
    fn a_lone_backup_retention_flag_parses() {
        for arguments in [
            vec![
                "froe",
                "compact",
                "/store",
                "--backup-minimum-age-days",
                "30",
            ],
            vec!["froe", "compact", "/store", "--backup-keep-latest", "3"],
        ] {
            assert!(
                CommandLine::try_parse_from(arguments.clone()).is_ok(),
                "a lone retention bound narrows the default removal: {arguments:?}"
            );
        }
    }

    /// A skip flag contradicts the flags that tune what it skips, and the
    /// contradiction is refused before froe opens the store.
    #[test]
    fn skip_flags_conflict_with_what_they_skip() {
        for arguments in [
            vec![
                "froe",
                "compact",
                "/store",
                "--skip-removing-recovery-backups",
                "--backup-minimum-age-days",
                "30",
            ],
            vec![
                "froe",
                "compact",
                "/store",
                "--skip-removing-recovery-backups",
                "--backup-keep-latest",
                "3",
            ],
            vec![
                "froe",
                "compact",
                "/store",
                "--skip-purging-orphaned-version-histories",
                "--purged-history-minimum-age-days",
                "30",
            ],
            vec![
                "froe",
                "compact",
                "/store",
                "--tail",
                "--purged-history-minimum-age-days",
                "30",
            ],
            vec![
                "froe",
                "compact",
                "/store",
                "--skip-purging-orphaned-version-histories",
                "--purge-orphaned-version-histories",
            ],
            vec![
                "froe",
                "compact",
                "/store",
                "--skip-repairing-archive-indexes",
                "--repair-archive-indexes",
            ],
            vec![
                "froe",
                "compact",
                "/store",
                "--tail",
                "--purge-orphaned-version-histories",
            ],
        ] {
            let parsed = CommandLine::try_parse_from(arguments.clone());
            let Err(error) = parsed else {
                panic!("contradictory flags must be refused: {arguments:?}");
            };
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{arguments:?}"
            );
        }
    }

    /// The spellings from when the purge and the repair were opt-in stay
    /// parseable — they now authorize without asking — and stay out of the
    /// help text.
    #[test]
    fn the_pre_default_authorization_spellings_parse_and_stay_undocumented() {
        let parsed = CommandLine::try_parse_from([
            "froe",
            "compact",
            "/store",
            "--purge-orphaned-version-histories",
            "--repair-archive-indexes",
            "--dry-run",
        ])
        .expect("the compatibility spellings must keep parsing");
        let Command::Compact {
            purge_orphaned_version_histories,
            repair_archive_indexes,
            ..
        } = parsed.command
        else {
            panic!("compact must dispatch");
        };
        assert!(purge_orphaned_version_histories);
        assert!(repair_archive_indexes);

        let mut command = <CommandLine as clap::CommandFactory>::command();
        let compact = command
            .find_subcommand_mut("compact")
            .expect("compact subcommand");
        let mut help = Vec::new();
        compact.write_long_help(&mut help).expect("render help");
        let help = String::from_utf8(help).expect("valid UTF-8");
        for hidden in [
            "--purge-orphaned-version-histories",
            "--repair-archive-indexes",
        ] {
            assert!(
                !help.contains(hidden),
                "the compatibility spelling {hidden} must stay undocumented: {help}"
            );
        }
        for documented in [
            "--skip-purging-orphaned-version-histories",
            "--skip-repairing-archive-indexes",
            "--skip-removing-recovery-backups",
        ] {
            assert!(
                help.contains(documented),
                "help must document {documented}: {help}"
            );
        }
    }

    /// Everything the one maintenance command's help owes an operator before
    /// they authorize it: what it is, what it destroys irreversibly, what it
    /// requires of the host, and what stays opt-in.
    #[test]
    fn compact_help_states_the_offline_safety_preconditions_and_what_it_retires() {
        let mut command = <CommandLine as clap::CommandFactory>::command();
        let compact = command
            .find_subcommand_mut("compact")
            .expect("compact subcommand");
        let mut help = Vec::new();
        compact.write_long_help(&mut help).expect("render help");
        let help = String::from_utf8(help).expect("valid UTF-8");
        for required in [
            // The offline preconditions the cleanup help used to carry.
            "Unix-only offline maintenance",
            "stop Oak/AEM",
            "owner of journal.log",
            "Recovery backups are retained",
            "strictly read-only",
            "canonical absolute",
            // The history it retires, which is the irreversible part.
            "not recoverable",
            "exactly one line",
            "journal.log is copied to a numbered .bak",
            // And the retention value, which is the safety argument.
            "retainedGenerations = 1",
        ] {
            assert!(
                help.contains(required),
                "compact help omitted {required:?}: {help}"
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
