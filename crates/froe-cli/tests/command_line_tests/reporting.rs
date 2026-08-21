//! Progress and silence: what reaches standard error, what never reaches
//! standard output, and what silence may not hide.

use super::*;

#[test]
pub(crate) fn the_export_reports_progress_and_a_summary_on_stderr() {
    let directory = TestDirectory::new("export-progress");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let export = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["export", store.to_str().expect("path")])
        .output()
        .expect("run export");
    assert!(
        export.status.success(),
        "export must succeed: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let stderr = String::from_utf8_lossy(&export.stderr);
    assert!(
        stderr.contains("exported 2 nodes in "),
        "the summary must report the node count and the time it took: {stderr}"
    );
    // Two nodes in a few milliseconds have no meaningful throughput, and
    // extrapolating one would be a made-up number.
    assert!(
        !stderr.contains("nodes/s"),
        "a run too brief to measure must not claim a rate: {stderr}"
    );
}

/// Standard output is the operator's evidence for a destructive decision,
/// so no progress report may reach it. Running the same plan under every
/// reporting mode must produce byte-identical standard output; a report
/// leaking there would make the three differ.
#[test]
pub(crate) fn reporting_never_reaches_the_standard_output_of_a_compaction_plan() {
    let directory = TestDirectory::new("cleanup-stdout-purity");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("open journal")
        .write_all(b"parser-skipped\n")
        .expect("append a removable journal line");

    let plan_under = |mode: &[&str]| {
        let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
            .args(["compact", store.to_str().expect("path"), "--dry-run"])
            .args(mode)
            .output()
            .expect("run the dry-run plan");
        assert!(
            run.status.success(),
            "cleanup --dry-run {mode:?} must succeed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        (run.stdout, run.stderr)
    };

    let (reported_stdout, reported_stderr) = plan_under(&["--progress", "always"]);
    let (default_stdout, _) = plan_under(&[]);
    let (silent_stdout, silent_stderr) = plan_under(&["--silent"]);

    assert!(
        !reported_stdout.is_empty(),
        "the plan itself must reach standard output"
    );
    assert_eq!(
        reported_stdout,
        silent_stdout,
        "a reported plan must be byte-identical to a silent one: {}",
        String::from_utf8_lossy(&reported_stdout)
    );
    assert_eq!(
        reported_stdout, default_stdout,
        "the default reporting mode must not change the plan either"
    );
    // Without this the comparison above would pass with reporting off.
    assert!(
        String::from_utf8_lossy(&reported_stderr).contains("froe: verifying the current head"),
        "--progress always must actually report its steps: {}",
        String::from_utf8_lossy(&reported_stderr)
    );
    assert!(
        silent_stderr.is_empty(),
        "--silent must leave standard error empty: {}",
        String::from_utf8_lossy(&silent_stderr)
    );
}

/// Reporting must never change what a command does. Standard error is a
/// pipe often enough — `froe cleanup --yes 2>&1 | less`, quit early — and
/// `main` restores SIGPIPE to its terminating disposition so piping data
/// into `head` ends quietly. Together those once let a progress line kill
/// a destructive cleanup between its mutations, leaving the journal
/// rewrite undone.
#[test]
pub(crate) fn a_closed_standard_error_cannot_kill_a_destructive_compaction() {
    let directory = TestDirectory::new("cleanup-closed-stderr");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("open journal")
        .write_all(b"parser-skipped\n")
        .expect("append a removable journal line");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            store.to_str().expect("path"),
            "--yes",
            "--progress",
            "always",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cleanup with a piped standard error");
    // Drop the read end at once: every subsequent progress write hits a
    // pipe with no reader.
    drop(child.stderr.take());
    let status = child.wait().expect("wait for cleanup");

    assert!(
        status.success(),
        "a closed standard error must not change the outcome; exit was {status:?}"
    );
    assert!(
        store.join("journal.log.bak.000").is_file(),
        "the journal rewrite must have completed: the run was cut short"
    );
    assert!(
        !std::fs::read_to_string(store.join("journal.log"))
            .expect("read journal")
            .contains("parser-skipped"),
        "the removable journal line must be gone"
    );
    froe::Repository::open(&store).expect("the repository remains healthy");
}

/// Silence hides what froe is *doing*, never what it is about to change.
/// A destructive cleanup under `--silent` still asks, in full.
#[test]
pub(crate) fn silence_never_hides_the_destructive_confirmation_prompt() {
    let directory = TestDirectory::new("cleanup-silent-prompt");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("open journal")
        .write_all(b"parser-skipped\n")
        .expect("append a removable journal line");

    let mut child = InteractiveCleanup::spawn_with(&store, &["--silent"]);
    child.expect_prompt(
        "the purge question of a silenced cleanup",
        &["purge orphaned version histories, if any are found?"],
    );
    child.send(b"n\n", "declining the purge");
    child.expect_prompt(
        "the confirmation prompt of a silenced cleanup",
        &[
            "about to apply this compaction plan",
            "this modifies the repository",
        ],
    );
    child.send(b"n\n", "the declining answer");
    let run = child.finish();
    assert!(
        !run.status.success(),
        "a declined cleanup exits nonzero: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("compaction plan for"),
        "the plan must still be printed under --silent: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("compaction cancelled"),
        "a declined compaction still says so: {stderr}"
    );
    assert!(
        !stderr.contains("froe: opening archives"),
        "--silent must still suppress the progress steps: {stderr}"
    );
}

/// An error is a fact about the repository, not a progress report, and no
/// reporting mode may hide one.
#[test]
pub(crate) fn silence_never_hides_an_error() {
    let failed = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", "/not/a/repository", "--dry-run", "--silent"])
        .output()
        .expect("run a silenced failing cleanup");
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("not a repository directory"),
        "--silent must never hide an error: {}",
        String::from_utf8_lossy(&failed.stderr)
    );
}

/// `--silent` mutes every report; `--quiet` is its compatibility alias
/// and mutes exactly the same things. Both still write the export.
#[test]
pub(crate) fn silence_mutes_every_report_without_touching_the_export() {
    for (index, flag) in ["--silent", "-s", "--quiet"].into_iter().enumerate() {
        let directory = TestDirectory::new(&format!("export-silent-{index}"));
        let store = directory.path.join("segmentstore");
        std::fs::create_dir_all(&store).expect("create store directory");
        populate(&store);
        let output_path = directory.path.join("content.jsonl");

        let export = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
            .args([
                "export",
                store.to_str().expect("path"),
                flag,
                "--output",
                output_path.to_str().expect("path"),
            ])
            .output()
            .expect("run export");
        assert!(
            export.status.success(),
            "export {flag} must succeed: {}",
            String::from_utf8_lossy(&export.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&export.stderr),
            "",
            "{flag} must leave standard error empty"
        );
        let exported = std::fs::read_to_string(&output_path).expect("read the export");
        assert_eq!(
            exported.lines().count(),
            2,
            "{flag} must not change what was exported: {exported}"
        );
    }
}
