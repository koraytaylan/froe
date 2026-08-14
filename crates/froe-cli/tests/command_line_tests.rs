//! Command-line compatibility tests: the `extract` spelling shipped in
//! v0.1.0 and must keep working as a hidden alias of `export`.

use std::io::{Read as _, Write as _};

use froe::writer::record_writer::ChildNodesToWrite;
use froe::writer::store_writer::WritableRepository;

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A child whose output is drained while the test performs bounded prompt
/// handshakes. On failure it kills and reaps the process before reporting both
/// complete output streams.
struct InteractiveCleanup {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    prompt_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    stdout_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
    stderr_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
    reaped: bool,
}

struct CapturedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

const CLEANUP_PROMPT_END: &[u8] = b"Continue? [y/N] ";

/// Drains stderr completely while framing each prompt with only the bytes
/// received since the preceding prompt. The returned transcript remains
/// cumulative so failures can report the child's complete stderr.
fn drain_cleanup_stderr<R: std::io::Read>(
    mut stderr: R,
    prompt_tx: &std::sync::mpsc::Sender<Vec<u8>>,
) -> Vec<u8> {
    let mut captured = Vec::new();
    let mut prompt_start = 0;
    loop {
        let mut byte = [0_u8; 1];
        match stderr.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                captured.push(byte[0]);
                if captured.ends_with(CLEANUP_PROMPT_END) {
                    let _ = prompt_tx.send(captured[prompt_start..].to_vec());
                    prompt_start = captured.len();
                }
            }
        }
    }
    captured
}

impl InteractiveCleanup {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    fn spawn(store: &std::path::Path) -> Self {
        Self::spawn_with(store, &[])
    }

    /// Spawns an interactive cleanup with extra command-line arguments,
    /// so a test can choose how the run reports.
    fn spawn_with(store: &std::path::Path, extra_arguments: &[&str]) -> Self {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
            .args([
                "cleanup",
                store.to_str().expect("path"),
                "--task",
                "journal",
            ])
            .args(extra_arguments)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn interactive cleanup");
        let stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let stdout_reader = std::thread::spawn(move || {
            let mut captured = Vec::new();
            let _ = stdout.read_to_end(&mut captured);
            captured
        });
        let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();
        let stderr_reader =
            std::thread::spawn(move || drain_cleanup_stderr(&mut stderr, &prompt_tx));
        Self {
            child,
            stdin: Some(stdin),
            prompt_rx,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            reaped: false,
        }
    }

    fn expect_prompt(&mut self, description: &str, expected: &[&str]) {
        let prompt = match self.prompt_rx.recv_timeout(Self::TIMEOUT) {
            Ok(prompt) => prompt,
            Err(error) => self.fail(&format!(
                "timed out waiting for {description} after {:?}: {error}",
                Self::TIMEOUT
            )),
        };
        let prompt = String::from_utf8_lossy(&prompt);
        for text in expected {
            if !prompt.contains(text) {
                self.fail(&format!(
                    "{description} did not contain {text:?}; stderr since the previous prompt:\n{prompt}"
                ));
            }
        }
    }

    fn send(&mut self, answer: &[u8], description: &str) {
        let result = self
            .stdin
            .as_mut()
            .expect("interactive stdin is available")
            .write_all(answer)
            .and_then(|()| self.stdin.as_mut().expect("stdin").flush());
        if let Err(error) = result {
            self.fail(&format!("failed to send {description}: {error}"));
        }
    }

    fn finish(mut self) -> CapturedOutput {
        self.stdin.take();
        let deadline = std::time::Instant::now() + Self::TIMEOUT;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    break status;
                }
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(None) => self.fail(&format!(
                    "cleanup did not exit within {:?} after final confirmation",
                    Self::TIMEOUT
                )),
                Err(error) => self.fail(&format!("failed to wait for cleanup: {error}")),
            }
        };
        let (stdout, stderr) = self.join_readers();
        CapturedOutput {
            status,
            stdout,
            stderr,
        }
    }

    fn fail(&mut self, reason: &str) -> ! {
        self.stdin.take();
        let _ = self.child.kill();
        let status = self.child.wait();
        self.reaped = true;
        let (stdout, stderr) = self.join_readers();
        panic!(
            "{reason}\nchild status: {status:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    }

    fn join_readers(&mut self) -> (Vec<u8>, Vec<u8>) {
        let stdout = self
            .stdout_reader
            .take()
            .expect("stdout reader")
            .join()
            .unwrap_or_else(|_| b"<stdout reader panicked>".to_vec());
        let stderr = self
            .stderr_reader
            .take()
            .expect("stderr reader")
            .join()
            .unwrap_or_else(|_| b"<stderr reader panicked>".to_vec());
        (stdout, stderr)
    }
}

impl Drop for InteractiveCleanup {
    fn drop(&mut self) {
        self.stdin.take();
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
        if self.stdout_reader.is_some() && self.stderr_reader.is_some() {
            let _ = self.join_readers();
        }
    }
}

#[test]
fn interactive_cleanup_prompt_frames_do_not_reuse_earlier_stderr() {
    let first = b"first-prompt-only\nContinue? [y/N] ";
    let second = b"second-prompt-only\nContinue? [y/N] ";
    let mut stderr = first.to_vec();
    stderr.extend_from_slice(second);
    stderr.extend_from_slice(b"after-prompts\n");
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();

    let captured = drain_cleanup_stderr(std::io::Cursor::new(&stderr), &prompt_tx);
    drop(prompt_tx);
    let prompts: Vec<_> = prompt_rx.into_iter().collect();

    assert_eq!(captured, stderr, "the complete diagnostic is retained");
    assert_eq!(prompts, [first.as_slice(), second.as_slice()]);
    assert!(
        !prompts[1]
            .windows(b"first-prompt-only".len())
            .any(|window| window == b"first-prompt-only"),
        "the second prompt frame must not reuse first-prompt stderr"
    );
}

/// Writes a store whose content tree is `/content`.
fn populate(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let content = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("content");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "content".to_owned(),
                node: content,
            },
            &[],
        )
        .expect("root");
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &[],
        )
        .expect("super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.set_head(previous, head));
    store.close().expect("close");
}

#[test]
fn clap_help_and_version_remain_successful_stdout_diagnostics() {
    for flag in ["--help", "--version"] {
        let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
            .arg(flag)
            .output()
            .expect("run clap display diagnostic");

        assert!(run.status.success(), "{flag} must exit successfully");
        assert!(run.stderr.is_empty(), "{flag} must not write stderr");
        assert!(!run.stdout.is_empty(), "{flag} must write stdout");
    }
}

#[test]
fn clap_errors_escape_bidi_values_and_keep_their_multiline_layout() {
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["cleanup", "/not-consulted", "--task", "bad\u{202e}value"])
        .output()
        .expect("run invalid clap value");

    assert_eq!(run.status.code(), Some(2));
    assert!(run.stdout.is_empty());
    let stderr = String::from_utf8(run.stderr).expect("clap diagnostic stays UTF-8");
    assert!(!stderr.contains('\u{202e}'), "raw bidi reached stderr");
    assert!(stderr.contains(r"\u{202e}"), "{stderr}");
    assert!(stderr.lines().count() > 2, "layout was flattened: {stderr}");
    assert!(stderr.contains("possible values"), "{stderr}");
}

#[test]
fn destructive_prompt_escapes_hostile_repository_path() {
    let hostile = std::env::temp_dir().join("not-opened-\u{1b}]8;;https:-bidi-\u{202e}-store");
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .arg("compact")
        .arg(hostile)
        .output()
        .expect("run cancelled compaction prompt");

    assert!(!run.status.success(), "EOF must cancel compaction");
    assert!(!run.stderr.contains(&0x1b), "raw ESC reached stderr");
    let stderr = String::from_utf8(run.stderr).expect("prompt stays UTF-8");
    assert!(!stderr.contains('\u{202e}'), "raw bidi reached stderr");
    assert!(stderr.contains(r"\u{1b}"), "{stderr}");
    assert!(stderr.contains(r"\u{202e}"), "{stderr}");
}

#[test]
fn checkpoint_prompt_distinguishes_controls_from_literal_escape_text() {
    let repository = "/not-opened";
    let raw = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["checkpoint", "remove", repository, "name-\u{1b}-\u{202e}"])
        .output()
        .expect("run raw-control checkpoint prompt");
    let literal = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["checkpoint", "remove", repository, r"name-\u{1b}-\u{202e}"])
        .output()
        .expect("run literal-escape checkpoint prompt");

    assert!(!raw.status.success());
    assert!(!literal.status.success());
    assert!(!raw.stderr.contains(&0x1b), "raw ESC reached stderr");
    let raw = String::from_utf8(raw.stderr).expect("raw prompt stays UTF-8");
    let literal = String::from_utf8(literal.stderr).expect("literal prompt stays UTF-8");
    assert!(
        raw.contains(r#"checkpoint "name-\u{1b}-\u{202e}""#),
        "{raw}"
    );
    assert!(
        literal.contains(r#"checkpoint "name-\\u{1b}-\\u{202e}""#),
        "{literal}"
    );
    assert_ne!(raw, literal);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn install_unlink_denial_filter() -> std::io::Result<()> {
    const LOAD_SYSCALL_NUMBER: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JUMP_IF_EQUAL: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const RETURN: u16 = 0x06; // BPF_RET | BPF_K
    const SECCOMP_RETURN_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RETURN_ALLOW: u32 = 0x7fff_0000;

    let denied = SECCOMP_RETURN_ERRNO | libc::EACCES as u32;
    let mut instructions = [
        libc::sock_filter {
            code: LOAD_SYSCALL_NUMBER,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: JUMP_IF_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_unlink as u32,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: denied,
        },
        libc::sock_filter {
            code: JUMP_IF_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_unlinkat as u32,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: denied,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: SECCOMP_RETURN_ALLOW,
        },
    ];
    let program = libc::sock_fprog {
        len: instructions.len() as u16,
        filter: instructions.as_mut_ptr(),
    };
    // SAFETY: both `prctl` operations affect only this soon-to-exec child;
    // `program` and its instruction array remain live for the filter install.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the kernel copies the validated BPF program during this call.
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &raw const program,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[test]
fn cleanup_dry_run_plans_dangling_journal_without_taking_the_lock_or_writing() {
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
        .args([
            "cleanup",
            store.to_str().expect("path"),
            "--task",
            "journal",
            "--dry-run",
        ])
        .output()
        .expect("run cleanup dry-run");

    assert!(
        run.status.success(),
        "dry-run must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("selected tasks: journal"), "{stdout}");
    assert!(stdout.contains("prune 1 journal lines"), "{stdout}");
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
fn cleanup_yes_applies_the_locked_plan_and_reopens_healthy() {
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
        .args([
            "cleanup",
            store.to_str().expect("path"),
            "--task",
            "journal",
            "--yes",
        ])
        .output()
        .expect("run cleanup");

    assert!(
        run.status.success(),
        "cleanup must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("cleanup complete"), "{stdout}");
    assert!(stdout.contains("0 orphan segments removed"), "{stdout}");
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
fn cleanup_reconfirms_and_accepts_a_changed_authoritative_plan() {
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
        &["about to apply this cleanup plan"],
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
            "about to apply the changed authoritative cleanup plan",
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
    for expected in [
        "prune 1 journal lines",
        "prune 2 journal lines",
        "cleanup complete",
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
    assert_eq!(
        backup, journal_before_rewrite,
        "journal recovery backup must equal the exact pre-rewrite journal\n{diagnostic}"
    );
    froe::Repository::open(&store)
        .unwrap_or_else(|error| panic!("repository did not reopen: {error}\n{diagnostic}"));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn partial_cleanup_deletion_is_reported_and_exits_nonzero() {
    use std::os::unix::process::CommandExt as _;

    let directory = TestDirectory::new("cleanup-partial-deletion");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let temporary = store.join("journal.log.compacting");
    std::fs::copy(store.join("journal.log"), &temporary)
        .expect("create provably redundant journal staging file");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_froe"));
    command.args([
        "cleanup",
        store.to_str().expect("path"),
        "--task",
        "stale-temporaries",
        "--yes",
    ]);
    // SAFETY: the hook performs only async-signal-safe `prctl` syscalls and
    // returns an `io::Error` to abort exec if the filter cannot be installed.
    unsafe {
        command.pre_exec(install_unlink_denial_filter);
    }
    let run = command.output().expect("run cleanup with unlink denied");

    assert!(!run.status.success(), "partial cleanup must exit nonzero");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stdout.contains("cleanup partially completed"), "{stdout}");
    assert!(
        stderr.contains("could not delete journal.log.compacting"),
        "{stderr}"
    );
    assert!(stderr.contains("os error 13"), "{stderr}");
    assert!(stderr.contains("cleanup is partial"), "{stderr}");
    assert!(temporary.is_file(), "failed deletion target must remain");
    froe::Repository::open(&store).expect("partial cleanup preserves repository health");
}

#[test]
fn recovery_backup_task_without_policy_is_a_cli_configuration_error() {
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "cleanup",
            "/not/consulted",
            "--task",
            "recovery-backups",
            "--dry-run",
        ])
        .output()
        .expect("run invalid cleanup configuration");

    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("--task recovery-backups requires"),
        "{stderr}"
    );
    assert!(!stderr.contains("invalid segment-tar data"), "{stderr}");
    assert!(!stderr.contains("not a repository"), "{stderr}");
}

#[test]
fn a_journal_bound_beside_an_explicit_task_set_without_journal_is_refused() {
    // Bounding the journal rewrites journal.log. An operator who named an
    // explicit task set that excludes the journal did not ask for that, and
    // silently re-adding the task would delete history they never selected.
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "cleanup",
            "/not/consulted",
            "--task",
            "segments",
            "--retain-journal-revisions",
            "1",
            "--dry-run",
        ])
        .output()
        .expect("run invalid cleanup configuration");

    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("--retain-journal-revisions rewrites journal.log"),
        "{stderr}"
    );
    // Refused on the arguments alone: the store is never opened.
    assert!(!stderr.contains("not a repository"), "{stderr}");
}

#[test]
fn cleanup_preview_escapes_invalid_utf8_and_terminal_control_bytes_exactly() {
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
        .arg("cleanup")
        .arg(&store)
        .args(["--task", "journal", "--dry-run"])
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
fn segment_only_preview_does_not_disclose_journal_removal_candidates() {
    let directory = TestDirectory::new("cleanup-segments-hide-journal");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.join("journal.log"))
        .expect("open journal")
        .write_all(b"parser-skipped\n")
        .expect("append removable journal candidate");

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .arg("cleanup")
        .arg(&store)
        .args(["--task", "segments", "--dry-run"])
        .output()
        .expect("run segment-only preview");

    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8(run.stdout).expect("preview stays UTF-8");
    assert!(stdout.contains("selected tasks: segments"), "{stdout}");
    assert!(!stdout.contains("journal line"), "{stdout}");
    assert!(!stdout.contains("prune "), "{stdout}");
}

#[test]
fn cleanup_errors_escape_hostile_path_controls_and_bidi() {
    let hostile = std::env::temp_dir().join(format!(
        "missing-{}-\u{1b}-\u{202e}-store",
        std::process::id()
    ));
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .arg("cleanup")
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
fn cleanup_preview_names_every_checkpoint_before_confirmation() {
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
            "cleanup",
            store_path.to_str().expect("path"),
            "--task",
            "unreferenced-checkpoints",
            "--dry-run",
        ])
        .output()
        .expect("run cleanup preview");

    assert!(
        run.status.success(),
        "preview must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("remove 1 checkpoints"), "{stdout}");
    assert!(
        stdout.contains(&format!("checkpoint {checkpoint:?}")),
        "the destructive preview must name the exact checkpoint: {stdout}"
    );
}

#[test]
fn the_extract_alias_produces_the_export_output() {
    let directory = TestDirectory::new("extract-alias");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let binary = env!("CARGO_BIN_EXE_froe");

    let extract = std::process::Command::new(binary)
        .args(["extract", store.to_str().expect("path"), "--depth", "1"])
        .output()
        .expect("run extract");
    assert!(
        extract.status.success(),
        "extract must keep succeeding: {}",
        String::from_utf8_lossy(&extract.stderr)
    );

    let export = std::process::Command::new(binary)
        .args(["export", store.to_str().expect("path"), "--depth", "1"])
        .output()
        .expect("run export");
    assert!(export.status.success());

    assert!(
        !extract.stdout.is_empty(),
        "the export must emit JSON lines"
    );
    assert_eq!(
        extract.stdout, export.stdout,
        "both spellings must produce identical JSON lines"
    );
}

#[test]
fn the_extract_alias_writes_output_files() {
    let directory = TestDirectory::new("extract-alias-output");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("content.jsonl");

    let extract = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "extract",
            store.to_str().expect("path"),
            "--path",
            "/content",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run extract");
    assert!(
        extract.status.success(),
        "extract --output must keep succeeding: {}",
        String::from_utf8_lossy(&extract.stderr)
    );
    let written = std::fs::read_to_string(&output).expect("read output");
    assert_eq!(
        written,
        "{\"path\":\"/content\",\"properties\":{\"jcr:primaryType\":\"nt:unstructured\"}}\n"
    );
}

#[test]
fn the_sqlite_format_writes_a_database_file() {
    let directory = TestDirectory::new("sqlite-format");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("content.db");

    let export = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "sqlite",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        export.status.success(),
        "export --format sqlite must succeed: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let written = std::fs::read(&output).expect("read output");
    assert!(
        written.starts_with(b"SQLite format 3\0"),
        "the output must be a SQLite database file"
    );

    // A second export to the same path must refuse: the export never
    // overwrites.
    let rerun = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "sqlite",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("re-run export");
    assert!(!rerun.status.success());
    assert!(
        String::from_utf8_lossy(&rerun.stderr).contains("never overwrites"),
        "the rerun must refuse to overwrite: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
}

#[test]
fn the_sqlite_format_leaves_an_existing_file_untouched() {
    let directory = TestDirectory::new("sqlite-never-overwrites");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("victim.db");
    std::fs::write(&output, b"someone else's database").expect("seed");

    let export = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "sqlite",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(!export.status.success());
    assert_eq!(
        std::fs::read(&output).expect("read"),
        b"someone else's database",
        "the existing file must be untouched, not opened and modified"
    );
}

#[test]
fn the_export_reports_progress_and_a_summary_on_stderr() {
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
fn reporting_never_reaches_the_standard_output_of_a_cleanup_plan() {
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
            .args(["cleanup", store.to_str().expect("path"), "--dry-run"])
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
fn a_closed_standard_error_cannot_kill_a_destructive_cleanup() {
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
            "cleanup",
            store.to_str().expect("path"),
            "--task",
            "journal",
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
fn silence_never_hides_the_destructive_confirmation_prompt() {
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
        "the confirmation prompt of a silenced cleanup",
        &[
            "about to apply this cleanup plan",
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
        stdout.contains("cleanup plan for"),
        "the plan must still be printed under --silent: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("cleanup cancelled"),
        "a declined cleanup still says so: {stderr}"
    );
    assert!(
        !stderr.contains("froe: opening archives"),
        "--silent must still suppress the progress steps: {stderr}"
    );
}

/// An error is a fact about the repository, not a progress report, and no
/// reporting mode may hide one.
#[test]
fn silence_never_hides_an_error() {
    let failed = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["cleanup", "/not/a/repository", "--dry-run", "--silent"])
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
fn silence_mutes_every_report_without_touching_the_export() {
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

/// Commits a second revision: `/content` gains a `version` property and
/// an `added` child.
fn revise(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let added = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("added");
    let version_value = writer.write_string("2").expect("version value");
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &froe::writer::record_writer::ChildNodesToWrite::One {
                name: "added".to_owned(),
                node: added,
            },
            &[froe::writer::record_writer::PropertyToWrite {
                name: "version".to_owned(),
                property_type: froe::content::PropertyType::Long,
                values: froe::writer::record_writer::PropertyValuesToWrite::Single(version_value),
            }],
        )
        .expect("content");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "content".to_owned(),
                node: content,
            },
            &[],
        )
        .expect("root");
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &[],
        )
        .expect("super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.set_head(previous, head));
    store.close().expect("close");
}

/// Runs `froe export --format parquet` and returns the captured stderr.
fn parquet_export(store: &std::path::Path, output: &std::path::Path, extra: &[&str]) -> String {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_froe"));
    command.args([
        "export",
        store.to_str().expect("path"),
        "--format",
        "parquet",
        "--output",
        output.to_str().expect("path"),
    ]);
    command.args(extra);
    let run = command.output().expect("run export");
    assert!(
        run.status.success(),
        "the parquet export must succeed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8_lossy(&run.stderr).into_owned()
}

#[test]
fn the_parquet_export_refreshes_in_place() {
    let directory = TestDirectory::new("parquet-refresh");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");

    let first = parquet_export(&store, &output, &[]);
    assert!(
        first.contains("exported 2 nodes"),
        "the first export: {first}"
    );

    // An unchanged store: the export reports itself current and is not
    // rewritten.
    let before = std::fs::read(output.join("nodes.parquet")).expect("read");
    let second = parquet_export(&store, &output, &[]);
    assert!(
        second.contains("already current"),
        "the second export: {second}"
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        before,
        "a current export is not rewritten"
    );

    // A moved head: only the change is decoded.
    revise(&store);
    let third = parquet_export(&store, &output, &[]);
    assert!(
        third.contains("refreshed the export") && third.contains("2 changed ranges"),
        "the third export refreshes: {third}"
    );
    let revision = froe::store::Repository::open(&store)
        .expect("open")
        .head_record_identifier()
        .to_string();
    for name in ["nodes.parquet", "properties.parquet"] {
        let provenance = froe_export::read_export_provenance(&output.join(name))
            .expect("read")
            .expect("stamped");
        assert_eq!(
            provenance.revision(),
            revision,
            "{name} carries the new head's stamp"
        );
    }
    // The temporary files of the refresh never linger.
    let mut leftovers: Vec<String> = std::fs::read_dir(&output)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    leftovers.sort();
    assert_eq!(
        leftovers,
        vec![
            ".froe-export.lock".to_owned(),
            "nodes.parquet".to_owned(),
            "properties.parquet".to_owned(),
        ],
        "only the two tables and the lock file remain"
    );
}

#[test]
fn the_full_flag_rebuilds_the_export() {
    let directory = TestDirectory::new("parquet-full");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");

    parquet_export(&store, &output, &[]);
    let rebuilt = parquet_export(&store, &output, &["--full"]);
    assert!(
        rebuilt.contains("exported 2 nodes"),
        "--full runs a full export even when the existing one is current: {rebuilt}"
    );
    assert!(!rebuilt.contains("already current"), "{rebuilt}");
}

#[test]
fn a_compacted_store_requires_full_to_rebuild() {
    let directory = TestDirectory::new("parquet-compacted");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");
    parquet_export(&store, &output, &[]);

    let compact = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run compact");
    assert!(
        compact.status.success(),
        "compact must succeed: {}",
        String::from_utf8_lossy(&compact.stderr)
    );

    // Compaction rewrites the journal to one line, so the stamped
    // revision is unprovable: indistinguishable from another
    // repository's export. Rebuilding takes the explicit flag.
    let before = std::fs::read(output.join("nodes.parquet")).expect("read");
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "parquet",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        !refused.status.success(),
        "an unprovable base is refused: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("does not resolve") && stderr.contains("--full"),
        "the refusal names the reason and the flag: {stderr}"
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        before,
        "the export survives the refusal"
    );

    let rebuilt = parquet_export(&store, &output, &["--full"]);
    assert!(
        rebuilt.contains("exported 2 nodes"),
        "the explicit rebuild completes: {rebuilt}"
    );
    // And after it, refreshes work against the new head again.
    let current = parquet_export(&store, &output, &[]);
    assert!(current.contains("already current"), "refreshed: {current}");
}

#[test]
fn an_export_of_another_repository_requires_full() {
    let directory = TestDirectory::new("parquet-cross-repo");
    let store_a = directory.path.join("store-a");
    let store_b = directory.path.join("store-b");
    std::fs::create_dir_all(&store_a).expect("create store a");
    std::fs::create_dir_all(&store_b).expect("create store b");
    populate(&store_a);
    populate(&store_b);
    let output = directory.path.join("export");

    // A complete, valid, same-scope export — of store B.
    parquet_export(&store_b, &output, &[]);
    let before = std::fs::read(output.join("nodes.parquet")).expect("read");

    // Exporting store A over it without --full refuses.
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store_a.to_str().expect("path"),
            "--format",
            "parquet",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        !refused.status.success(),
        "another repository's export must be refused: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        before,
        "the foreign repository's export survives"
    );

    let rebuilt = parquet_export(&store_a, &output, &["--full"]);
    assert!(
        rebuilt.contains("exported 2 nodes"),
        "the explicit rebuild completes: {rebuilt}"
    );
}

#[test]
fn a_foreign_parquet_file_requires_full_to_replace() {
    let directory = TestDirectory::new("parquet-foreign");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");
    std::fs::create_dir_all(&output).expect("create output directory");
    std::fs::write(output.join("nodes.parquet"), b"not a parquet file").expect("seed");

    // Without --full the export refuses to destroy data it does not own.
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "parquet",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        !refused.status.success(),
        "the export must refuse foreign files"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("refusing to replace") && stderr.contains("--full"),
        "the refusal names the escape hatch: {stderr}"
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        b"not a parquet file",
        "the foreign file survives"
    );

    // With --full the same directory is rebuilt explicitly.
    let rebuilt = parquet_export(&store, &output, &["--full"]);
    assert!(
        rebuilt.contains("exported 2 nodes"),
        "the explicit rebuild completes: {rebuilt}"
    );
    assert!(
        froe_export::read_export_provenance(&output.join("nodes.parquet"))
            .expect("read")
            .is_some(),
        "the replacement is a stamped froe export"
    );
}

#[test]
fn a_removed_export_root_fails_like_a_missing_path() {
    let directory = TestDirectory::new("parquet-root-removed");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");
    parquet_export(&store, &output, &["--path", "/content"]);
    let before = std::fs::read(output.join("nodes.parquet")).expect("read");

    // Commit a revision whose content tree has no /content.
    {
        let store = WritableRepository::open(&store).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let root = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        store.close().expect("close");
    }

    // The same failure a first export of a missing path produces, and
    // the existing export stays untouched.
    let failed = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "parquet",
            "--path",
            "/content",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        !failed.status.success(),
        "a vanished export root fails: {}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("no node at /content"),
        "the reason: {stderr}"
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        before,
        "the existing export is preserved"
    );
}

#[test]
fn the_full_flag_applies_only_to_parquet() {
    let directory = TestDirectory::new("full-non-parquet");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--full",
            "--output",
            directory.path.join("content.jsonl").to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(!refused.status.success(), "--full must not apply here");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("--full applies only to the parquet format"),
        "the message: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}
