//! How `froe compact` settles its three default cleanups — the orphan
//! purge, the archive-index repair, and the recovery-backup removal:
//! `--yes` answers every question, an interactive run asks each one, and a
//! `--skip-*` flag removes one from the run with a stated reason.

use super::*;

/// `--yes` alone removes the recovery backups earlier runs left behind —
/// no further flags — and a store holding only its own fresh journal
/// backup says so.
#[test]
pub(crate) fn recovery_backups_from_an_earlier_run_are_removed_by_default() {
    let directory = TestDirectory::new("cleanup-default-backup-removal");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let first = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run the first compaction");
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    assert!(first.status.success(), "{first_stdout}");
    assert!(
        store.join("journal.log.bak.000").is_file(),
        "the first run leaves its journal backup"
    );
    assert!(
        first_stdout.contains("recovery backups on disk:")
            && first_stdout.contains("written by this run"),
        "a store holding only this run's own backup says so: {first_stdout}"
    );

    let second = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run the second compaction");
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second.status.success(),
        "{second_stdout}\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        second_stdout.contains("remove old recovery backup journal.log.bak.000"),
        "the plan lists the removal: {second_stdout}"
    );
    assert!(
        second_stdout.contains("files removed: 1 recovery backup"),
        "the summary counts it, singular and all: {second_stdout}"
    );
    assert!(
        !store.join("journal.log.bak.000").exists(),
        "the earlier run's backup is gone"
    );

    let third = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run the third compaction");
    let third_stdout = String::from_utf8_lossy(&third.stdout);
    assert!(third.status.success(), "{third_stdout}");
    assert!(
        third_stdout.contains("the store is already fully compacted; nothing to do"),
        "with the backups gone the run converges: {third_stdout}"
    );
}

/// The interactive conversation, end to end: the purge question, the
/// backup question with the surveyed count and size, and the final plan
/// confirmation. A declined question keeps its cleanup out of the run and
/// the plan says why.
#[test]
pub(crate) fn an_interactive_run_asks_each_cleanup_question() {
    let directory = TestDirectory::new("cleanup-interactive-questions");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::write(store.join("journal.log.bak.000"), b"earlier run's journal")
        .expect("plant an earlier run's recovery backup");

    let mut child = InteractiveCleanup::spawn(&store);
    child.expect_prompt(
        "the purge question",
        &[
            "purge orphaned version histories, if any are found?",
            "removal is permanent",
        ],
    );
    child.send(b"n\n", "declining the purge");
    child.expect_prompt(
        "the backup question",
        &["remove the 1 recovery backup (21 bytes) earlier runs left behind?"],
    );
    child.send(b"y\n", "accepting the backup removal");
    child.expect_prompt(
        "the final confirmation",
        &[
            "about to apply this compaction plan",
            "this modifies the repository",
        ],
    );
    child.send(b"y\n", "confirming the plan");
    let run = child.finish();
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        stdout.contains("remove old recovery backup journal.log.bak.000 (21 bytes)"),
        "the accepted removal is listed in the plan: {stdout}"
    );
    assert!(
        stdout.contains("compaction complete"),
        "the confirmed plan applies: {stdout}"
    );
    assert!(
        !store.join("journal.log.bak.000").exists(),
        "the accepted removal happened"
    );
}

/// A run scripted without `--yes` has no answers to give: nothing is
/// applied, and the cancellation names the flag that supplies them instead
/// of implying the operator declined.
#[test]
pub(crate) fn a_scripted_run_without_yes_cancels_and_names_the_flag() {
    let directory = TestDirectory::new("cleanup-unconfirmed");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .arg("compact")
        .arg(&store)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the unconfirmed compaction");

    assert!(!run.status.success(), "an unconfirmed run exits nonzero");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("compaction cancelled"),
        "the run is cancelled, not errored: {stderr}"
    );
    assert!(
        stderr.contains("no confirmation was available on standard input")
            && stderr.contains("rerun with --yes"),
        "the cancellation names the missing answer and the flag that gives it: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("compaction plan for"),
        "the plan is still shown, so the operator knows what --yes would apply: {stdout}"
    );
}

/// A store a killed writer left behind is healed in the same `--yes` run —
/// no extra flag — and the recovery backups wait for the next run, with
/// both facts stated.
#[test]
pub(crate) fn a_yes_run_repairs_a_missing_index_and_defers_backup_removal() {
    let directory = TestDirectory::new("cleanup-default-repair");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::write(store.join("journal.log.bak.000"), b"earlier run's journal")
        .expect("plant an earlier run's recovery backup");
    break_index_magic(&store.join("data00000a.tar"));

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run the repairing compaction");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("archive indexes rebuilt: 1"),
        "the rebuild is reported: {stdout}"
    );
    assert!(
        store.join("data00000a.tar.bak").is_file(),
        "the original bytes are retained beside the rebuilt archive"
    );
    assert!(
        store.join("journal.log.bak.000").is_file(),
        "a repairing run must not also remove recovery backups"
    );
    assert!(
        stdout.contains("kept this run because the index repair writes its own"),
        "the deferral is stated, not silent: {stdout}"
    );

    let second = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run the follow-up compaction");
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second.status.success(),
        "{second_stdout}\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        !store.join("data00000a.tar.bak").exists() && !store.join("journal.log.bak.000").exists(),
        "the next run removes what the repairing run had to keep: {second_stdout}"
    );
    froe::Repository::open(&store).expect("the healed store reopens");
}

/// `--skip-repairing-archive-indexes` refuses a damaged store outright —
/// fast, before the verification walks — with the census and the remedy.
#[test]
pub(crate) fn skipping_the_repair_refuses_a_damaged_store_with_the_census() {
    let directory = TestDirectory::new("cleanup-skip-repair");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    break_index_magic(&store.join("data00000a.tar"));

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store.to_str().expect("path"),
            "--yes",
            "--skip-repairing-archive-indexes",
        ])
        .output()
        .expect("run the skipping compaction");

    assert!(!run.status.success(), "a damaged store must be refused");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("1 of 1 active archive numbers has no index metadata (data00000a.tar)"),
        "the refusal carries the census: {stderr}"
    );
    assert!(
        stderr.contains("authorize the repair"),
        "the refusal names the remedy: {stderr}"
    );
    froe::Repository::open(&store).expect("the refused store is untouched and reopens");
}
