//! The compact command end to end: the dry run that takes no lock, the
//! confirmed run, and what a partial deletion reports.

use super::*;

#[test]
pub(crate) fn compact_dry_run_plans_the_whole_pipeline_without_taking_the_lock_or_writing() {
    let directory = TestDirectory::new("cleanup-dry-run");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::remove_file(store.join("repo.lock")).expect("remove bootstrap lock inode");
    let missing = "00000000-0000-0007-a000-000000000007:0 root 123\n";
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("open journal")
        .write_all(missing.as_bytes())
        .expect("append dangling line");
    let before: Vec<(std::ffi::OsString, Vec<u8>)> = {
        let mut files: Vec<_> = std::fs::read_dir(&store)
            .expect("read store")
            .map(|entry| {
                let entry = entry.expect("entry");
                (
                    entry.file_name(),
                    std::fs::read(entry.path()).expect("read file"),
                )
            })
            .collect();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    };

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--dry-run"])
        .output()
        .expect("run cleanup dry-run");

    assert!(
        run.status.success(),
        "dry-run must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    // A compacting run retires every revision by policy rather than pruning
    // the ones that cannot resolve, and says so — this is the irreversible
    // half of the run.
    assert!(
        stdout.contains("retire all 2 journal lines, keeping only the compacted head"),
        "{stdout}"
    );
    assert!(
        stdout.contains("not recoverable from the store"),
        "{stdout}"
    );
    assert!(
        stdout.contains("journal line 2: missing segment"),
        "{stdout}"
    );
    assert!(
        stdout.contains("00000000-0000-0007-a000-000000000007:0"),
        "{stdout}"
    );
    assert!(stdout.contains("repository was not modified"), "{stdout}");
    assert!(!store.join("repo.lock").exists());
    let mut after: Vec<_> = std::fs::read_dir(&store)
        .expect("read store")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name(),
                std::fs::read(entry.path()).expect("read file"),
            )
        })
        .collect();
    after.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(after, before);
}

/// The plan states both sides of the copy-and-reclaim trade and each
/// prediction step concludes with its result, so an operator reads what the
/// run will actually change rather than only what it removes. The field
/// motivation: two 41-minute runs whose "estimated reclaimable: 6.9 GiB"
/// concealed that each wrote a generation as large as the one it removed.
#[test]
pub(crate) fn compact_plan_states_the_copy_cost_the_net_change_and_the_step_conclusions() {
    let directory = TestDirectory::new("cleanup-plan-net-change");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::remove_file(store.join("repo.lock")).expect("remove bootstrap lock inode");

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store.to_str().expect("path"),
            "--dry-run",
            "--progress",
            "always",
        ])
        .output()
        .expect("run cleanup dry-run");

    assert!(
        run.status.success(),
        "dry-run must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("the copy writes about "),
        "the plan must state what the copy costs: {stdout}"
    );
    assert!(
        stdout.contains("the sweep reclaims about "),
        "the plan must state what the sweep removes: {stdout}"
    );
    assert!(
        stdout.contains("estimated net change: about "),
        "the plan must state the net direction: {stdout}"
    );
    assert!(
        !stdout.contains("estimated reclaimable:"),
        "a copying plan replaces the one-sided estimate with the net form: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("tracing segments reachable from the head:")
            && stderr.contains("; the head reaches "),
        "the trace must conclude with the head's composition: {stderr}"
    );
    assert!(
        stderr.contains("; no pre-existing bulk segments will be shared in place"),
        "the sharing prediction must conclude even when it shares nothing: {stderr}"
    );
    assert!(
        stderr.contains("predicting the reclamation:") && stderr.contains("; the sweep "),
        "the reclamation prediction must conclude with the sweep's totals: {stderr}"
    );
}

#[test]
pub(crate) fn compact_yes_applies_the_locked_plan_and_reopens_healthy() {
    let directory = TestDirectory::new("cleanup-apply");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let missing = "00000000-0000-0007-a000-000000000007:0 root 123\n";
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("open journal")
        .write_all(missing.as_bytes())
        .expect("append dangling line");

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run cleanup");

    assert!(
        run.status.success(),
        "cleanup must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("compaction complete"), "{stdout}");
    // The merged run compacts, so the superseded generation is reclaimed in
    // the same pass; a zero here would mean the copy freed nothing.
    assert!(
        stdout.contains("orphan segments removed") && !stdout.contains("0 orphan segments removed"),
        "{stdout}"
    );
    assert!(stdout.contains("journal recovery backup:"), "{stdout}");
    assert!(stdout.contains("journal.log.bak.000"), "{stdout}");
    assert!(
        !std::fs::read_to_string(store.join("journal.log"))
            .expect("journal")
            .contains("00000000-0000-0007-a000-000000000007")
    );
    assert!(store.join("journal.log.bak.000").is_file());
    froe::Repository::open(&store).expect("repository remains healthy");
}

/// The field scenario that motivated the convergence gate, in miniature:
/// the first run copies and says what remains; the second proves the
/// compaction converged and does nothing — no 6.34 GiB swap, no archive
/// churn, a byte-identical store — and `--always-copy` still forces a copy
/// for the operator who wants one.
#[test]
pub(crate) fn a_second_run_on_a_compacted_store_does_nothing_unless_forced() {
    let directory = TestDirectory::new("cleanup-convergence");
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
        first_stdout.contains(
            "the store is now fully compacted; a repeat run has only recovery backups left to remove"
        ),
        "the first run wrote a journal backup and must say that is all a repeat would touch: {first_stdout}"
    );

    let before: Vec<(std::ffi::OsString, Vec<u8>)> = {
        let mut files: Vec<_> = std::fs::read_dir(&store)
            .expect("read store")
            .map(|entry| {
                let entry = entry.expect("entry");
                (
                    entry.file_name(),
                    std::fs::read(entry.path()).expect("read file"),
                )
            })
            .collect();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    };
    // With backup removal skipped, the repeat proves the compaction itself
    // converged: byte-identical store, nothing to do.
    let second = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store.to_str().expect("path"),
            "--yes",
            "--skip-removing-recovery-backups",
        ])
        .output()
        .expect("run the second compaction");
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second.status.success(),
        "{second_stdout}
{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        second_stdout.contains("the head is already fully compacted"),
        "the plan must state the gate's verdict: {second_stdout}"
    );
    assert!(
        second_stdout.contains("the store is already fully compacted; nothing to do"),
        "the second run must be a stated no-op: {second_stdout}"
    );
    assert!(
        !second_stdout.contains("copy the head into generation"),
        "the gate must drop the pointless copy: {second_stdout}"
    );
    let mut after: Vec<_> = std::fs::read_dir(&store)
        .expect("read store")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name(),
                std::fs::read(entry.path()).expect("read file"),
            )
        })
        .collect();
    after.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(after, before, "a gated run must not touch the store");

    let forced = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store.to_str().expect("path"),
            "--yes",
            "--always-copy",
        ])
        .output()
        .expect("run the forced compaction");
    let forced_stdout = String::from_utf8_lossy(&forced.stdout);
    assert!(
        forced.status.success(),
        "{forced_stdout}
{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(
        forced_stdout.contains("--always-copy forces the copy regardless")
            && forced_stdout.contains("copy the head into generation"),
        "the override must name itself and plan the copy: {forced_stdout}"
    );
    assert!(
        forced_stdout.contains("compaction complete"),
        "{forced_stdout}"
    );
    froe::Repository::open(&store).expect("the forced store reopens");
}

/// The one plan the operator confirms is built under the held lock, so a
/// non-cooperating change during the prompt cannot be silently replanned —
/// it is refused before a single planned mutation, and the store is left
/// byte-identical.
#[test]
pub(crate) fn a_change_during_confirmation_is_refused_rather_than_replanned() {
    let directory = TestDirectory::new("cleanup-change-during-prompt");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let mut child = InteractiveCleanup::spawn(&store);
    child.expect_prompt(
        "the purge question",
        &["purge orphaned version histories, if any are found?"],
    );
    child.send(b"n\n", "declining the purge");
    child.expect_prompt(
        "confirmation prompt",
        &["about to apply this compaction plan"],
    );

    // The plan is complete and the command is blocked on confirmation. This
    // simulates a non-cooperating writer appending to the journal while the
    // operator is reading the plan.
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("reopen journal")
        .write_all(b"appended-during-prompt\n")
        .expect("append during the prompt");
    let files_after_change: Vec<(std::ffi::OsString, Vec<u8>)> = {
        let mut files: Vec<_> = std::fs::read_dir(&store)
            .expect("read store")
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|entry| entry.file_name() != "repo.lock")
            })
            .map(|entry| {
                let entry = entry.expect("entry");
                (
                    entry.file_name(),
                    std::fs::read(entry.path()).expect("read file"),
                )
            })
            .collect();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    };
    child.send(b"y\n", "affirmative answer after the change");
    let output = child.finish();
    let diagnostic = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "a stale plan must be refused, not applied\n{diagnostic}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the repository changed after the authoritative cleanup plan was built"),
        "the refusal must name the change\n{diagnostic}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.matches("compaction plan for ").count(),
        1,
        "exactly one plan is ever shown\n{diagnostic}"
    );
    let mut files_after_refusal: Vec<_> = std::fs::read_dir(&store)
        .expect("read store")
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.file_name() != "repo.lock")
        })
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name(),
                std::fs::read(entry.path()).expect("read file"),
            )
        })
        .collect();
    files_after_refusal.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        files_after_refusal, files_after_change,
        "a refused apply must not touch the store\n{diagnostic}"
    );
}

/// The apply pipeline's verification budget: one full walk while planning,
/// one walk of the fresh copy before it is published, one walk of the
/// published store through fresh mappings — and no more. The copy walk is
/// the safety addition; the missing fourth walk is the redundancy the
/// restructure removed.
#[test]
pub(crate) fn an_apply_run_walks_the_head_three_times_with_the_copy_checked_before_publication() {
    let directory = TestDirectory::new("cleanup-verification-budget");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store.to_str().expect("path"),
            "--yes",
            "--progress",
            "always",
        ])
        .output()
        .expect("run confirmed cleanup");

    let diagnostic = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.status.success(), "apply must succeed\n{diagnostic}");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(
        stderr.matches("froe: verifying the current head:").count(),
        2,
        "one walk while planning, one for the published store\n{diagnostic}"
    );
    assert_eq!(
        stderr
            .matches("froe: verifying the compacted copy:")
            .count(),
        1,
        "the fresh copy is walked exactly once, before publication\n{diagnostic}"
    );
    assert_eq!(
        stderr.matches("froe: verifying the checkpoints:").count(),
        1,
        "lock-first means one planning pass: the plan shown is the plan applied\n{diagnostic}"
    );
    let copy_position = stderr
        .find("froe: verifying the compacted copy:")
        .expect("copy verification line");
    let reclaim_position = stderr
        .find("froe: reclaiming old generations:")
        .expect("reclaim line");
    assert!(
        copy_position < reclaim_position,
        "the copy is verified before anything is unlinked\n{diagnostic}"
    );
    froe::Repository::open(&store).expect("the compacted store reopens");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
pub(crate) fn partial_compaction_deletion_is_reported_and_exits_nonzero() {
    use std::os::unix::process::CommandExt as _;

    let directory = TestDirectory::new("cleanup-partial-deletion");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let temporary = store.join("journal.log.compacting");
    std::fs::copy(store.join("journal.log"), &temporary)
        .expect("create provably redundant journal staging file");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_froe"));
    command.args(["compact", store.to_str().expect("path"), "--yes"]);
    // SAFETY: the hook performs only async-signal-safe `prctl` syscalls and
    // returns an `io::Error` to abort exec if the filter cannot be installed.
    unsafe {
        command.pre_exec(install_unlink_denial_filter);
    }
    let run = command.output().expect("run cleanup with unlink denied");

    assert!(!run.status.success(), "partial cleanup must exit nonzero");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stdout.contains("compaction partially completed"),
        "{stdout}"
    );
    assert!(
        stderr.contains("could not delete journal.log.compacting"),
        "{stderr}"
    );
    assert!(stderr.contains("os error 13"), "{stderr}");
    assert!(stderr.contains("compaction is partial"), "{stderr}");
    assert!(temporary.is_file(), "failed deletion target must remain");
    froe::Repository::open(&store).expect("partial cleanup preserves repository health");
}

#[test]
pub(crate) fn compact_preview_escapes_invalid_utf8_and_terminal_control_bytes_exactly() {
    let directory = TestDirectory::new("cleanup-byte-preview");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("open journal")
        .write_all(&[0xff, 0x1b, b'X', b'\n'])
        .expect("append hostile journal line");

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .arg("compact")
        .arg(&store)
        .args(["--dry-run"])
        .output()
        .expect("run hostile preview");

    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!run.stdout.contains(&0x1b), "raw ESC reached stdout");
    let stdout = String::from_utf8(run.stdout).expect("CLI output remains UTF-8");
    assert!(stdout.contains(r#"b"\xff\x1bX""#), "{stdout}");
}

#[test]
pub(crate) fn compact_errors_escape_hostile_path_controls_and_bidi() {
    let hostile = std::env::temp_dir().join(format!(
        "missing-{}-\u{1b}-\u{202e}-store",
        std::process::id()
    ));
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .arg("compact")
        .arg(hostile)
        .arg("--dry-run")
        .output()
        .expect("run cleanup against hostile path");

    assert!(!run.status.success());
    assert!(!run.stderr.contains(&0x1b), "raw ESC reached stderr");
    let stderr = String::from_utf8(run.stderr).expect("CLI error remains UTF-8");
    assert!(stderr.contains(r"\u{1b}"), "{stderr}");
    assert!(stderr.contains(r"\u{202e}"), "{stderr}");
}

#[test]
pub(crate) fn compact_preview_names_every_omitted_checkpoint_before_confirmation() {
    let directory = TestDirectory::new("cleanup-checkpoint-preview");
    let store_path = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store_path).expect("create store directory");
    populate(&store_path);
    let checkpoint = {
        let store = WritableRepository::open(&store_path).expect("open checkpoint writer");
        let checkpoint =
            froe::writer::create_checkpoint(&store, 60_000, &[]).expect("create checkpoint");
        store.close().expect("close checkpoint writer");
        checkpoint
    };

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store_path.to_str().expect("path"),
            // The checkpoint has a minute to live, so only the unreferenced
            // policy selects it — which is the opt-in this preview is about.
            "--remove-unreferenced-checkpoints",
            "--dry-run",
        ])
        .output()
        .expect("run the compaction preview");

    assert!(
        run.status.success(),
        "preview must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("omit 1 checkpoint from the copy (1 unreferenced)"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("checkpoint {checkpoint:?}")),
        "the destructive preview must name the exact checkpoint: {stdout}"
    );
}

/// The orphan report is on every plan and the purge is part of every full
/// run: the plan lists it with exact counts, the skip flag keeps the
/// histories with a stated reason, and the summary restates what was
/// purged.
#[test]
pub(crate) fn a_purge_is_reported_selected_by_default_and_summarized() {
    let directory = TestDirectory::new("cleanup-purge");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate_with_orphaned_history(&store);

    let skipped = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store.to_str().expect("path"),
            "--dry-run",
            "--skip-purging-orphaned-version-histories",
        ])
        .output()
        .expect("run the skipping dry-run");
    let skipped_stdout = String::from_utf8_lossy(&skipped.stdout);
    assert!(skipped.status.success(), "{skipped_stdout}");
    assert!(
        skipped_stdout
            .contains("orphaned version histories: 1 (their versionables no longer exist)"),
        "detection reports even when the purge is skipped: {skipped_stdout}"
    );
    assert!(
        skipped_stdout.contains("kept, as --skip-purging-orphaned-version-histories requests"),
        "the report states why nothing is purged: {skipped_stdout}"
    );
    assert!(
        !skipped_stdout.contains("purge 1 orphaned version history ("),
        "no purge is selected under the skip flag: {skipped_stdout}"
    );

    let preview = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--dry-run"])
        .output()
        .expect("run the default dry-run");
    let preview_stdout = String::from_utf8_lossy(&preview.stdout);
    assert!(preview.status.success(), "{preview_stdout}");
    assert!(
        preview_stdout
            .contains("purge 1 orphaned version history (2 nodes) by omitting it from the copy"),
        "the default plan lists the purge, singular and all: {preview_stdout}"
    );
    assert!(
        preview_stdout.contains("this run purges all of them, as listed in the plan above"),
        "the report says what this very run does instead of advising a flag: {preview_stdout}"
    );

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run the purge");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        stdout.contains("purge 1 orphaned version history (2 nodes) by omitting it from the copy"),
        "the purge is a listed action: {stdout}"
    );
    assert!(
        stdout.contains("removal is permanent"),
        "the irreversibility is stated: {stdout}"
    );
    assert!(
        stdout.contains("purged: 1 orphaned version history (2 nodes omitted from the copy)"),
        "the summary states the content delta: {stdout}"
    );
    froe::Repository::open(&store).expect("the purged store reopens");

    let repeat = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store.to_str().expect("path"),
            "--yes",
            "--skip-removing-recovery-backups",
        ])
        .output()
        .expect("run the repeat");
    let repeat_stdout = String::from_utf8_lossy(&repeat.stdout);
    assert!(repeat.status.success(), "{repeat_stdout}");
    assert!(
        !repeat_stdout.contains("orphaned version histories:"),
        "nothing is left to report: {repeat_stdout}"
    );
    assert!(
        repeat_stdout.contains("the store is already fully compacted; nothing to do"),
        "the purge converges with the gate: {repeat_stdout}"
    );
}
