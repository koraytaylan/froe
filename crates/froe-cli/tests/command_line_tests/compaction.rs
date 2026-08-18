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

#[test]
pub(crate) fn compact_reconfirms_and_accepts_a_changed_authoritative_plan() {
    let directory = TestDirectory::new("cleanup-reconfirmation");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("open journal")
        .write_all(b"first-parser-skipped\n")
        .expect("append first removable line");

    let mut child = InteractiveCleanup::spawn(&store);
    child.expect_prompt(
        "initial confirmation prompt",
        &["about to apply this compaction plan"],
    );

    // The first preview is complete and the command is blocked on confirmation,
    // so this simulates a non-cooperating change before it acquires repo.lock.
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("reopen journal")
        .write_all(b"second-parser-skipped\n")
        .expect("append second removable line");
    let journal_before_rewrite =
        std::fs::read(store.join("journal.log")).expect("capture the exact pre-rewrite journal");
    child.send(b"y\n", "initial affirmative answer");

    child.expect_prompt(
        "changed-plan confirmation prompt",
        &[
            "repository state changed before the lock was acquired",
            "about to apply the changed authoritative compaction plan",
        ],
    );
    child.send(b"yes\n", "authoritative-plan affirmative answer");
    let output = child.finish();
    let diagnostic = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "interactive cleanup failed with {}\n{diagnostic}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The preview and the reconfirmed authoritative plan each state the
    // retirement, and the second sees one more line than the first because the
    // fixture appends one between them — which is exactly the change the
    // reconfirmation exists to surface.
    for expected in [
        "retire all 2 journal lines",
        "retire all 3 journal lines",
        "compaction complete",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?}\n{diagnostic}"
        );
    }
    let journal = std::fs::read(store.join("journal.log")).expect("read rewritten journal");
    assert!(
        !journal
            .windows(b"first-parser-skipped".len())
            .any(|window| window == b"first-parser-skipped"),
        "first removable line survived\n{diagnostic}"
    );
    assert!(
        !journal
            .windows(b"second-parser-skipped".len())
            .any(|window| window == b"second-parser-skipped"),
        "second removable line survived\n{diagnostic}"
    );
    let backup = std::fs::read(store.join("journal.log.bak.000"))
        .unwrap_or_else(|error| panic!("read journal recovery backup: {error}\n{diagnostic}"));
    // The backup is the journal as it stood immediately before the rewrite.
    // A merged run appends the compacted head's line first, so the backup is
    // the pre-run journal plus exactly that one line — which is what makes it
    // a usable recovery artefact rather than a stale snapshot.
    assert!(
        backup.starts_with(&journal_before_rewrite),
        "journal recovery backup must extend the pre-run journal\n{diagnostic}"
    );
    let appended = &backup[journal_before_rewrite.len()..];
    assert_eq!(
        appended.split_inclusive(|byte| *byte == b'\n').count(),
        1,
        "exactly the compacted head's line was appended before the rewrite\n{diagnostic}"
    );
    let retained = String::from_utf8(journal.clone()).expect("the journal stays UTF-8");
    assert_eq!(
        retained.lines().count(),
        1,
        "a completed compaction retires every earlier revision\n{diagnostic}"
    );
    froe::Repository::open(&store)
        .unwrap_or_else(|error| panic!("repository did not reopen: {error}\n{diagnostic}"));
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
        stdout.contains("omit 1 checkpoints from the copy"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("checkpoint {checkpoint:?}")),
        "the destructive preview must name the exact checkpoint: {stdout}"
    );
}
