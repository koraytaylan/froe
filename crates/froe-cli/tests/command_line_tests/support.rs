//! The store a command runs against, and the interactive session that
//! feeds a destructive command its confirmation.

use super::*;

pub(crate) struct TestDirectory {
    pub(crate) path: std::path::PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
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
pub(crate) struct InteractiveCleanup {
    pub(crate) child: std::process::Child,
    pub(crate) stdin: Option<std::process::ChildStdin>,
    pub(crate) prompt_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    pub(crate) stdout_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
    pub(crate) stderr_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
    pub(crate) reaped: bool,
}

pub(crate) struct CapturedOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) const CLEANUP_PROMPT_END: &[u8] = b"Continue? [y/N] ";

/// Drains stderr completely while framing each prompt with only the bytes
/// received since the preceding prompt. The returned transcript remains
/// cumulative so failures can report the child's complete stderr.
pub(crate) fn drain_cleanup_stderr<R: std::io::Read>(
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
    pub(crate) const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    pub(crate) fn spawn(store: &std::path::Path) -> Self {
        Self::spawn_with(store, &[])
    }

    /// Spawns an interactive cleanup with extra command-line arguments,
    /// so a test can choose how the run reports.
    pub(crate) fn spawn_with(store: &std::path::Path, extra_arguments: &[&str]) -> Self {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
            .args(["compact", store.to_str().expect("path")])
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

    pub(crate) fn expect_prompt(&mut self, description: &str, expected: &[&str]) {
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

    pub(crate) fn send(&mut self, answer: &[u8], description: &str) {
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

    pub(crate) fn finish(mut self) -> CapturedOutput {
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

    pub(crate) fn fail(&mut self, reason: &str) -> ! {
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

    pub(crate) fn join_readers(&mut self) -> (Vec<u8>, Vec<u8>) {
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

/// Populates a store carrying one orphaned version history — a
/// `nt:versionHistory` whose `jcr:versionableUuid` matches no live
/// `jcr:uuid` — beside ordinary live content.
pub(crate) fn populate_with_orphaned_history(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let jcr_system = write_orphaned_history_system(&mut writer);
    let content = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("content");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("content".to_owned(), content),
                ("jcr:system".to_owned(), jcr_system),
            ]),
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
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush");
    store.close().expect("close");
}

/// The `jcr:system` subtree holding one orphaned version history.
fn write_orphaned_history_system(
    writer: &mut froe::writer::record_writer::RecordWriter<
        impl froe::writer::record_writer::SegmentSink,
    >,
) -> froe::RecordIdentifier {
    let orphan_versionable = "bbbbbbbb-2222-4222-8222-222222222222";
    let string_property = |writer: &mut froe::writer::record_writer::RecordWriter<_>,
                           name: &str,
                           property_type: froe::PropertyType,
                           text: &str| {
        let value = writer.write_string(text).expect("property value");
        froe::writer::record_writer::PropertyToWrite {
            name: name.to_owned(),
            property_type,
            values: froe::writer::record_writer::PropertyValuesToWrite::Single(value),
        }
    };
    let created = string_property(
        writer,
        "jcr:created",
        froe::PropertyType::Date,
        "2020-01-01T00:00:00.000Z",
    );
    let version_identifier = string_property(
        writer,
        "jcr:uuid",
        froe::PropertyType::String,
        "bbbbbbbb-2222-4222-8222-aaaaaaaaaaaa",
    );
    let root_version = writer
        .write_node(
            Some("nt:version"),
            &[],
            &ChildNodesToWrite::Zero,
            &[version_identifier, created],
        )
        .expect("root version");
    let versionable = string_property(
        writer,
        "jcr:versionableUuid",
        froe::PropertyType::String,
        orphan_versionable,
    );
    let history = writer
        .write_node(
            Some("nt:versionHistory"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:rootVersion".to_owned(),
                node: root_version,
            },
            &[versionable],
        )
        .expect("history");
    let version_storage = writer
        .write_node(
            Some("rep:versionStorage"),
            &[],
            &ChildNodesToWrite::One {
                name: orphan_versionable.to_owned(),
                node: history,
            },
            &[],
        )
        .expect("version storage");
    writer
        .write_node(
            Some("rep:system"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:versionStorage".to_owned(),
                node: version_storage,
            },
            &[],
        )
        .expect("jcr:system")
}

/// Writes a store whose content tree is `/content`.
pub(crate) fn populate(directory: &std::path::Path) {
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
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn install_unlink_denial_filter() -> std::io::Result<()> {
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

/// Commits a second revision: `/content` gains a `version` property and
/// an `added` child.
pub(crate) fn revise(directory: &std::path::Path) {
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
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
}

/// Runs `froe export --format parquet` and returns the captured stderr.
pub(crate) fn parquet_export(
    store: &std::path::Path,
    output: &std::path::Path,
    extra: &[&str],
) -> String {
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
