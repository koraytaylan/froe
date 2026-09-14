//! The command-line parser's own tests.
//!
//! They live beside `command_line.rs` rather than inside it because the
//! module is where every new command lands, and the thousand-line gate
//! `scripts/oversized-files.sh` enforces would otherwise be spent on
//! tests rather than on the commands they cover.

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
