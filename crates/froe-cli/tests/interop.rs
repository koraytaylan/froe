//! End-to-end interop tests against a real Apache Sling / Jackrabbit Oak
//! `TarMK` store.
//!
//! These tests verify that froe can read stores written by Oak, write stores
//! that Oak can read, and perform maintenance operations (compact, cleanup,
//! backup, restore, recover-journal) that leave the store in a state Oak
//! boots against cleanly.
//!
//! # Prerequisites
//!
//! - `podman` installed and runnable by the current user.
//! - Network access to pull `docker.io/apache/sling:14` once.
//! - The `interop` feature enabled: `cargo test -p froe-cli --features interop`.
//!
//! # Running
//!
//! ```console
//! $ cargo test -p froe-cli --features interop -- --ignored
//! ```
//!
//! Or an individual phase:
//!
//! ```console
//! $ cargo test -p froe-cli --features interop -- --ignored interop::read
//! ```
//!
//! # Dependency chain
//!
//! The tests run in a strict dependency chain. Each phase depends on the
//! previous one and aborts the chain on failure:
//!
//! 1. **`generate`** — Boot Sling, populate content, churn subtrees, stop.
//!    Produces the shared Oak store fixture. If this fails, nothing else
//!    can run because every later phase reads this store.
//!
//! 2. **`read`** — froe reads the Oak store: summary, tree, check, search,
//!    export. If this fails, froe cannot read Oak's format and no write-path
//!    verification is meaningful — there is no way to confirm that froe's
//!    output is correct without a working reader.
//!
//! 3. **`commit`** — froe adds nodes with typed properties to the content
//!    tree via the library's commit API, then Sling reads them back. If
//!    this fails, froe cannot write content that Oak reads — the core
//!    interop claim. There is no point testing checkpoint, compact,
//!    cleanup, backup, or recover if the writer cannot produce content
//!    Oak reads.
//!
//! 4. **`checkpoint`** — froe writes a checkpoint against the Oak store.
//!    A metadata-only write-path test (logical head update). If this
//!    fails, the writer's checkpoint machinery is broken, which affects
//!    cleanup's expired-checkpoint test and compact's checkpoint
//!    preservation.
//!
//! 5. **`compact`** — froe compacts a copy of the store and Sling boots
//!    against the result. Depends on `read` (to verify the result) and
//!    `commit` (to trust the writer). If this fails, cleanup's
//!    multi-generational fixture cannot be built (it uses two compactions).
//!
//! 6. **`cleanup`** — froe cleanup against a multi-generational store built
//!    by two compactions, with an expired checkpoint, a stale archive, a
//!    truncated journal, and corrupt journal lines. Depends on `compact`
//!    (to build the gen 0→1→2 fixture) and `checkpoint` (for the expired
//!    checkpoint). If this fails, the write path's plan-and-apply
//!    machinery is broken.
//!
//! 7. **`backup`** — froe backup + restore, Sling boots against the
//!    restored store. Depends on `read` and `commit`. Independent of
//!    compact/cleanup but later in the chain because it is lower-risk.
//!
//! 8. **`recover`** — froe recover-journal after deleting journal.log,
//!    Sling boots against the recovered store. Depends on `read`. Last
//!    because it is the most destructive (deletes the journal).
//!
//! All code in the loop is Apache-2.0 (Apache Sling + Apache Jackrabbit
//! Oak); no Adobe license is involved at any point.

#![cfg(feature = "interop")]
#![allow(clippy::case_sensitive_file_extension_comparisons)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use froe::content::PropertyType;
use froe::writer::commit::rewrite_node_with_child_edits;
use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use froe::writer::store_writer::WritableRepository;

/// The Sling Docker image. Apache-2.0; boots Oak with `TarMK` by default.
const SLING_IMAGE: &str = "docker.io/apache/sling:14";

/// How long to wait for Sling to finish booting.
const SLING_BOOT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for a froe command to finish.
const FROE_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Shared fixture
// ---------------------------------------------------------------------------

/// The shared Oak store directory, produced by the `generate` test.
/// All later tests read from or copy this store.
static OAK_STORE: OnceLock<PathBuf> = OnceLock::new();

/// The work root for all interop artifacts.
fn work_root() -> PathBuf {
    let root = std::env::var("FROE_INTEROP_WORK_ROOT").map_or_else(
        |_| std::env::temp_dir().join(format!("froe-interop-{}", std::process::id())),
        PathBuf::from,
    );
    std::fs::create_dir_all(&root).expect("create work root");
    root
}

/// The froe binary path, resolved from the cargo build.
fn froe_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_froe"))
}

/// Run froe with the given arguments; assert success and return stdout.
fn froe(args: &[&str]) -> String {
    let start = Instant::now();
    let output = Command::new(froe_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn froe {args:?}: {error}"));
    let elapsed = start.elapsed();
    assert!(
        elapsed < FROE_TIMEOUT,
        "froe {args:?} timed out after {elapsed:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "froe {args:?} exited with {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status = output.status
    );
    stdout
}

// ---------------------------------------------------------------------------
// Podman orchestration
// ---------------------------------------------------------------------------

/// Run a podman command; assert success and return stdout.
fn podman(args: &[&str]) -> String {
    let output = Command::new("podman")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn podman {args:?}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "podman {args:?} exited with {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status = output.status
    );
    stdout
}

/// A podman volume that is removed on drop.
struct PodmanVolume {
    name: String,
}

impl PodmanVolume {
    fn new(name: &str) -> Self {
        // Remove any leftover volume from a previous run; ignore failure
        // because the volume may not exist.
        let _ = Command::new("podman").args(["volume", "rm", name]).output();
        podman(&["volume", "create", name]);
        Self {
            name: name.to_owned(),
        }
    }
}

impl Drop for PodmanVolume {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["volume", "rm", &self.name])
            .output();
    }
}

/// A podman container that is stopped and removed on drop.
struct PodmanContainer {
    name: String,
}

impl PodmanContainer {
    fn run_detached(name: &str, port: u16, volume: &str) -> Self {
        let port_arg = format!("{port}:8080");
        let volume_arg = format!("{volume}:/opt/sling/launcher");
        podman(&[
            "run",
            "-d",
            "--name",
            name,
            "-p",
            &port_arg,
            "-v",
            &volume_arg,
            SLING_IMAGE,
        ]);
        Self {
            name: name.to_owned(),
        }
    }

    fn stop(&self) {
        let _ = Command::new("podman").args(["stop", &self.name]).output();
        let _ = Command::new("podman").args(["rm", &self.name]).output();
    }
}

impl Drop for PodmanContainer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Wait for Sling to finish booting and report all bundles active.
fn wait_for_sling(port: u16, container_name: &str) {
    let deadline = Instant::now() + SLING_BOOT_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            let logs = Command::new("podman")
                .args(["logs", container_name])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            panic!(
                "Sling did not come up at :{port} within {SLING_BOOT_TIMEOUT:?}\nlast logs:\n{logs}"
            );
        }

        let output = Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-u",
                "admin:admin",
                &format!("http://localhost:{port}/system/console/bundles.json"),
            ])
            .output();

        if let Ok(out) = output {
            let code = String::from_utf8_lossy(&out.stdout);
            if code.trim() == "200" {
                // Confirm all bundles are active (one fragment may stay resolved).
                let json = Command::new("curl")
                    .args([
                        "-s",
                        "-u",
                        "admin:admin",
                        &format!("http://localhost:{port}/system/console/bundles.json"),
                    ])
                    .output()
                    .expect("curl bundles.json");
                let body = String::from_utf8_lossy(&json.stdout);
                // The "s" field is [total, active, active.fragments, ...].
                // Sling ships one fragment; ready when resolved count is 0.
                if let Some(resolved) = extract_bundle_count(&body, 3)
                    && resolved == 0
                {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Parse the Felix web console JSON to extract a count from the "s" array.
fn extract_bundle_count(json: &str, index: usize) -> Option<i64> {
    // The JSON has "s":[total, active, fragments, resolved, ...].
    // Rather than pull in a JSON dependency, find the array by key.
    let key = "\"s\":[";
    let pos = json.find(key)?;
    let rest = &json[pos + key.len()..];
    let end = rest.find(']')?;
    let numbers: Vec<i64> = rest[..end]
        .split(',')
        .map(|s| s.trim().parse().ok())
        .collect::<Option<Vec<i64>>>()?;
    numbers.get(index).copied()
}

// ---------------------------------------------------------------------------
// Content population
// ---------------------------------------------------------------------------

/// Create content nodes via the `SlingPostServlet`.
fn sling_post(port: u16, path: &str, primary_type: &str, title: &str) {
    let url = format!("http://localhost:{port}{path}");
    let _ = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-u",
            "admin:admin",
            "-F",
            &format!("jcr:primaryType={primary_type}"),
            "-F",
            &format!("jcr:title={title}"),
            &url,
        ])
        .status()
        .expect("curl POST");
}

/// Churn content: create subtrees, then delete them. Produces orphaned
/// segments that compaction can later reclaim.
fn churn_content(port: u16) {
    for round in 0..3u32 {
        eprintln!(
            "  churn round {}/{round}: creating 20 throwaway subtrees",
            round + 1
        );
        for i in 0..20u32 {
            let path = format!("/content/interop/throwaway/{round}/{i}");
            sling_post(
                port,
                &path,
                "sling:Folder",
                &format!("Throwaway {round}.{i}"),
            );
            for k in 1..=5u32 {
                let child = format!("{path}/child{k}");
                sling_post(
                    port,
                    &child,
                    "sling:OrderedFolder",
                    &format!("Child {round}.{i}.{k}"),
                );
            }
        }
        eprintln!(
            "  churn round {}/{round}: deleting 20 throwaway subtrees",
            round + 1
        );
        for i in 0..20u32 {
            let url = format!("http://localhost:{port}/content/interop/throwaway/{round}/{i}");
            let _ = Command::new("curl")
                .args([
                    "-s",
                    "-o",
                    "/dev/null",
                    "-X",
                    "DELETE",
                    "-u",
                    "admin:admin",
                    &url,
                ])
                .status();
        }
    }
}

/// Populate a realistic content tree under /content/interop.
fn populate_content(port: u16) {
    sling_post(port, "/content/interop", "sling:Folder", "Interop Fixture");
    sling_post(port, "/content/interop/pages", "sling:Folder", "Test Pages");
    for i in 1..=5u32 {
        sling_post(
            port,
            &format!("/content/interop/pages/page{i}"),
            "sling:OrderedFolder",
            &format!("Page {i}"),
        );
    }
    // Binary node: nt:file with inline jcr:data.
    let binary_path = work_root().join("binary.txt");
    std::fs::write(
        &binary_path,
        "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
         Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n",
    )
    .expect("write binary.txt");
    let _ = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-u",
            "admin:admin",
            "-F",
            "jcr:primaryType=sling:Folder",
            "-F",
            &format!("file=@{}", binary_path.display()),
            &format!("http://localhost:{port}/content/interop/files"),
        ])
        .status()
        .expect("curl upload binary");
}

/// Fetch the content tree JSON from Sling for verification.
fn content_snapshot(port: u16) -> String {
    let output = Command::new("curl")
        .args([
            "-s",
            "-u",
            "admin:admin",
            &format!("http://localhost:{port}/content/interop.tidy.-1.json"),
        ])
        .output()
        .expect("curl content snapshot");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// Store manipulation helpers
// ---------------------------------------------------------------------------

/// Copy a store directory, removing repo.lock.
fn copy_store(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dst");
    copy_dir_recursive(src, dst);
    let _ = std::fs::remove_file(dst.join("repo.lock"));
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    for entry in std::fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("file_type").is_dir() {
            std::fs::create_dir_all(&target).expect("create_dir");
            copy_dir_recursive(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// Copy a store from a podman volume to a host directory.
fn store_from_volume(volume: &str, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dst");
    let src_mount = "/sling/repository/segmentstore";
    podman(&[
        "run",
        "--rm",
        "-v",
        &format!("{volume}:/sling"),
        "-v",
        &format!("{}:/out", dst.display()),
        "alpine:latest",
        "sh",
        "-c",
        &format!("cp -r {src_mount}/. /out/ && rm -f /out/repo.lock"),
    ]);
}

/// Copy a store from a host directory into a podman volume, chowned as
/// the sling user (UID 999).
fn store_into_volume(src: &Path, volume: &str) {
    let script = "rm -f /sling/repository/segmentstore/data*.tar \
         /sling/repository/segmentstore/journal.log \
         /sling/repository/segmentstore/manifest \
         /sling/repository/segmentstore/repo.lock \
         && cp /src/data*.tar /sling/repository/segmentstore/ 2>/dev/null || true \
         && cp /src/journal.log /sling/repository/segmentstore/ 2>/dev/null || true \
         && cp /src/manifest /sling/repository/segmentstore/ 2>/dev/null || true \
         && chown -R 999:999 /sling/repository/segmentstore/"
        .to_string();
    podman(&[
        "run",
        "--rm",
        "-v",
        &format!("{volume}:/sling"),
        "-v",
        &format!("{}:/src:ro", src.display()),
        "alpine:latest",
        "sh",
        "-c",
        &script,
    ]);
}

/// Make a stale archive: copy the active data*.tar to the next generation
/// letter. This is the on-disk condition Oak leaves behind after its own
/// compaction publishes a newer generation.
fn make_stale_archive(store: &Path) {
    let active = std::fs::read_dir(store)
        .expect("read_dir")
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            name.starts_with("data") && name.ends_with(".tar")
        })
        .min()
        .expect("at least one data*.tar");
    let base = active.file_name().unwrap().to_string_lossy().into_owned();
    let letter = base.as_bytes()[base.len() - 5];
    assert!(letter.is_ascii_lowercase(), "invalid generation letter");
    if letter == b'z' {
        eprintln!("  active archive at generation z; skipping stale-archive simulation");
        return;
    }
    let next_letter = (letter + 1) as char;
    let stale_name = format!("data{}{}.tar", &base[4..base.len() - 5], next_letter);
    let stale_path = store.join(&stale_name);
    eprintln!("  creating stale archive: {base} -> {stale_name}");
    std::fs::copy(&active, &stale_path).expect("copy stale archive");
}

/// Truncate the journal to just the head line, exactly as Oak's `compact`
/// tool does after compaction.
fn truncate_journal_to_head(store: &Path) {
    let journal = store.join("journal.log");
    let content = std::fs::read_to_string(&journal).expect("read journal");
    let head_line = content
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .expect("journal has at least one non-empty line");
    eprintln!("  truncating journal to head: {head_line}");
    std::fs::write(&journal, format!("{head_line}\n")).expect("write journal");
}

/// Append two corrupt journal lines to test the journal cleanup task's
/// parser-skipped and invalid-record-identifier removal paths.
fn append_corrupt_journal_lines(store: &Path) {
    let journal = store.join("journal.log");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .expect("open journal for append");
    // ParserSkippedNoSpace: a line with no ASCII space.
    file.write_all(b"this_line_has_no_space\n")
        .expect("write corrupt line 1");
    // InvalidRecordIdentifier: first field is not a record id.
    file.write_all(b"not-a-uuid:bad root 1234567890\n")
        .expect("write corrupt line 2");
}

// ---------------------------------------------------------------------------
// Test phases
// ---------------------------------------------------------------------------

/// Phase 1: Generate the Oak store fixture.
///
/// Boots Sling with `TarMK`, populates content under /content/interop,
/// churns content to produce orphaned segments, and stops cleanly. The
/// resulting store is the shared fixture for all later phases.
#[test]
#[ignore = "requires podman and the apache/sling:14 image"]
fn generate() {
    let root = work_root();
    let volume = PodmanVolume::new("froe-interop-generate");
    eprintln!("  starting Sling on :8080");
    let sling = PodmanContainer::run_detached("froe-interop-gen", 8080, &volume.name);
    wait_for_sling(8080, "froe-interop-gen");

    eprintln!("  populating content");
    populate_content(8080);

    eprintln!("  churning content to produce orphaned segments");
    churn_content(8080);

    eprintln!("  stopping Sling cleanly");
    sling.stop();

    let store = root.join("oak-store");
    eprintln!("  extracting store to {}", store.display());
    store_from_volume(&volume.name, &store);

    let entries = std::fs::read_dir(&store)
        .expect("read_dir")
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        entries
            .iter()
            .any(|n| n.starts_with("data") && n.ends_with(".tar")),
        "store has at least one data*.tar archive: {entries:?}"
    );
    assert!(
        entries.contains(&"journal.log".to_owned()),
        "store has journal.log: {entries:?}"
    );
    assert!(
        entries.contains(&"manifest".to_owned()),
        "store has manifest: {entries:?}"
    );

    OAK_STORE
        .set(store.clone())
        .expect("store the Oak store path");

    eprintln!("  Oak store generated at {}", store.display());
}

/// Get the shared Oak store, or panic with a clear message.
fn oak_store() -> PathBuf {
    OAK_STORE.get().cloned().unwrap_or_else(|| {
        panic!(
            "Oak store not generated. Run the `generate` test first:\n\
                 cargo test -p froe-cli --features interop -- --ignored interop::generate"
        )
    })
}

/// Phase 2: froe reads the Oak-written store.
///
/// If this fails, froe cannot read Oak's format and no write-path
/// verification is meaningful — there is no way to confirm that froe's
/// output is correct without a working reader.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn read() {
    let store = oak_store();
    eprintln!("  froe summary");
    let summary = froe(&["summary", store.to_str().unwrap()]);
    assert!(summary.contains("archives"), "summary has archives line");
    assert!(summary.contains("segments"), "summary has segments line");
    assert!(summary.contains("head"), "summary has head line");

    eprintln!("  froe tree /content/interop (depth 3)");
    let tree = froe(&[
        "tree",
        store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "3",
    ]);
    assert!(tree.contains("sling:Folder"), "tree shows sling:Folder");
    assert!(
        tree.contains("sling:OrderedFolder"),
        "tree shows OrderedFolder"
    );
    assert!(tree.contains("nt:file"), "tree shows nt:file");

    eprintln!("  froe check (expect exit 0)");
    froe(&["check", store.to_str().unwrap()]);

    eprintln!("  froe search-nodes");
    let search = froe(&[
        "search-nodes",
        store.to_str().unwrap(),
        "--has-property",
        "jcr:primaryType",
        "--value",
        "jcr:primaryType=sling:OrderedFolder",
        "--limit",
        "5",
    ]);
    assert!(!search.trim().is_empty(), "search found at least one node");

    eprintln!("  froe export (json-lines)");
    let export_path = work_root().join("oak-export.jsonl");
    froe(&[
        "export",
        store.to_str().unwrap(),
        "--path",
        "/content/interop",
        "--output",
        export_path.to_str().unwrap(),
        "--quiet",
    ]);
    let export = std::fs::read_to_string(&export_path).expect("read export");
    assert!(!export.is_empty(), "export produced output");

    eprintln!("  read phase passed");
}

/// Phase 4: froe writes a checkpoint against the Oak store.
///
/// A metadata-only write-path test (logical head update). If this fails,
/// the writer's checkpoint machinery is broken, which affects cleanup's
/// expired-checkpoint test and compact's checkpoint preservation.
/// Depends on `commit` (the writer can already produce content Oak reads).
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn checkpoint() {
    let store = oak_store();

    eprintln!("  froe checkpoint create (lifetime 1000 ms)");
    froe(&[
        "checkpoint",
        "create",
        store.to_str().unwrap(),
        "--lifetime-milliseconds",
        "1000",
        "--yes",
    ]);

    eprintln!("  froe checkpoints (the new one should be present)");
    let checkpoints = froe(&["checkpoints", store.to_str().unwrap()]);
    assert!(
        !checkpoints.trim().is_empty(),
        "at least one checkpoint exists after create"
    );

    eprintln!("  checkpoint phase passed");
}

/// Phase 3: froe adds nodes with typed properties to the content tree.
///
/// Uses the library's commit API (`rewrite_node_with_child_edits`) to
/// add a subtree under `/content/interop/froe-written` with string, long,
/// and boolean properties. Then boots Sling against the modified store
/// and verifies Oak reads the froe-written nodes back correctly.
///
/// If this fails, froe cannot write content that Oak reads — the core
/// interop claim. There is no point testing checkpoint, compact, cleanup,
/// backup, or recover if the writer cannot produce content Oak reads.
/// Depends on `read` (to verify).
#[test]
#[allow(clippy::too_many_lines, reason = "an end-to-end interop test")]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn commit() {
    let store = oak_store();
    let commit_store = work_root().join("commit-store");
    eprintln!("  copying store to {}", commit_store.display());
    copy_store(&store, &commit_store);

    eprintln!("  opening store for writing");
    let writable = WritableRepository::open(&commit_store).expect("open writable");
    let generation = writable.writing_generation().expect("generation");
    let mut writer = writable.record_writer(generation);

    // Build a leaf node with typed properties.
    let title_value = writer
        .write_string("Froe-Written Node")
        .expect("title value");
    let count_value = writer.write_string("42").expect("count value");
    let active_value = writer.write_string("true").expect("active value");
    let leaf = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Zero,
            &[
                PropertyToWrite {
                    name: "jcr:title".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::Single(title_value),
                },
                PropertyToWrite {
                    name: "count".to_owned(),
                    property_type: PropertyType::Long,
                    values: PropertyValuesToWrite::Single(count_value),
                },
                PropertyToWrite {
                    name: "active".to_owned(),
                    property_type: PropertyType::Boolean,
                    values: PropertyValuesToWrite::Single(active_value),
                },
            ],
        )
        .expect("write leaf node");

    // Build a parent folder containing the leaf.
    let folder = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::One {
                name: "node".to_owned(),
                node: leaf,
            },
            &[],
        )
        .expect("write folder node");

    // Commit: add the new folder as a child of /content/interop.
    //
    // The Oak super-root has children `root` (the content tree root, `/`)
    // and `checkpoints`. We need to rewrite the spine:
    //
    //   super-root → root (/) → content → interop → [froe-written]
    //
    // 1. Rewrite /content/interop to add the froe-written child.
    // 2. Rewrite /content to point at the new /content/interop.
    // 3. Rewrite / (root) to point its content child at the new /content.
    // 4. Rewrite the super-root to point its root child at the new root.
    let head = writable.head();
    let interop_path = froe::content::path::normalized_path("/content/interop");
    let content_path = froe::content::path::normalized_path("/content");
    let root_path = froe::content::path::normalized_path("/");

    // Resolve the record identifiers of the three ancestor nodes.
    let (interop_record, content_record, root_record) = {
        let repository = froe::store::Repository::open(&commit_store).expect("open reader");
        let interop_node = repository
            .node_at_path(&interop_path)
            .expect("resolve /content/interop")
            .expect("/content/interop exists");
        let content_node = repository
            .node_at_path(&content_path)
            .expect("resolve /content")
            .expect("/content exists");
        let root_node = repository
            .node_at_path(&root_path)
            .expect("resolve /")
            .expect("/ exists");
        (
            interop_node.record_identifier(),
            content_node.record_identifier(),
            root_node.record_identifier(),
        )
    };

    // 1. Rewrite /content/interop to add the new child.
    let mut edits = froe::writer::commit::ChildEdits::new();
    edits.insert("froe-written".to_owned(), Some(folder));
    let new_interop =
        rewrite_node_with_child_edits(&writable, &mut writer, Some(interop_record), &edits)
            .expect("rewrite interop node");

    // 2. Rewrite /content to point at the new /content/interop.
    let mut content_edits = froe::writer::commit::ChildEdits::new();
    content_edits.insert("interop".to_owned(), Some(new_interop));
    let new_content =
        rewrite_node_with_child_edits(&writable, &mut writer, Some(content_record), &content_edits)
            .expect("rewrite /content");

    // 3. Rewrite / (root) to point its content child at the new /content.
    let mut root_content_edits = froe::writer::commit::ChildEdits::new();
    root_content_edits.insert("content".to_owned(), Some(new_content));
    let new_root = rewrite_node_with_child_edits(
        &writable,
        &mut writer,
        Some(root_record),
        &root_content_edits,
    )
    .expect("rewrite root");

    // 4. Rewrite the super-root to point its `root` child at the new root.
    let mut super_root_edits = froe::writer::commit::ChildEdits::new();
    super_root_edits.insert("root".to_owned(), Some(new_root));
    let new_super_root =
        rewrite_node_with_child_edits(&writable, &mut writer, Some(head), &super_root_edits)
            .expect("rewrite super-root");

    writer.finish().expect("finish writer");
    assert!(
        writable.set_head(head, new_super_root),
        "head CAS succeeded (single-writer, no contention)"
    );
    writable.flush().expect("flush");
    writable.close().expect("close");

    eprintln!("  froe-written nodes committed");

    // Verify froe can read its own writes.
    eprintln!("  froe tree /content/interop/froe-written (depth 2)");
    let tree = froe(&[
        "tree",
        commit_store.to_str().unwrap(),
        "/content/interop/froe-written",
        "--depth",
        "2",
    ]);
    assert!(
        tree.contains("nt:unstructured"),
        "tree shows the node: {tree}"
    );

    eprintln!("  froe node /content/interop/froe-written/node");
    let node = froe(&[
        "node",
        commit_store.to_str().unwrap(),
        "/content/interop/froe-written/node",
    ]);
    assert!(
        node.contains("Froe-Written Node"),
        "froe reads the title property: {node}"
    );
    assert!(
        node.contains("count") && node.contains("Long"),
        "froe reads the long property: {node}"
    );
    assert!(
        node.contains("active") && node.contains("Boolean"),
        "froe reads the boolean property: {node}"
    );

    // Boot Sling against the froe-written store and verify Oak reads the
    // froe-written nodes.
    eprintln!("  booting Sling against the froe-written store");
    let volume = PodmanVolume::new("froe-interop-commit");
    let bootstrap = PodmanContainer::run_detached("froe-commit-bootstrap", 8084, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&commit_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-commit-verify", 8084, &volume.name);
    wait_for_sling(8084, "froe-commit-verify");

    eprintln!("  fetching froe-written node from Sling");
    let sling_node = Command::new("curl")
        .args([
            "-s",
            "-u",
            "admin:admin",
            "http://localhost:8084/content/interop/froe-written/node.tidy.json",
        ])
        .output()
        .expect("curl froe-written node");
    let sling_response = String::from_utf8_lossy(&sling_node.stdout);
    assert!(
        sling_response.contains("Froe-Written Node"),
        "Sling reads the froe-written title: {sling_response}"
    );

    drop(sling);
    eprintln!("  commit phase passed");
}

/// Phase 5: froe compacts a copy of the store and Sling boots against it.
///
/// Depends on `read` (to verify the result) and `checkpoint` (to trust
/// the writer). If this fails, cleanup's multi-generational fixture cannot
/// be built (it uses two compactions).
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn compact() {
    let store = oak_store();
    let compact_store = work_root().join("compact-store");
    eprintln!("  copying store to {}", compact_store.display());
    copy_store(&store, &compact_store);

    // Truncate journal so churned orphans are truly unreachable.
    eprintln!("  truncating journal to head");
    truncate_journal_to_head(&compact_store);

    eprintln!("  froe summary before compaction");
    let _before = froe(&["summary", compact_store.to_str().unwrap()]);

    eprintln!("  froe compact --yes");
    let compact_output = froe(&["compact", compact_store.to_str().unwrap(), "--yes"]);
    assert!(
        compact_output.contains("compacted"),
        "compaction reported success: {compact_output}"
    );

    eprintln!("  froe summary after compaction");
    let after = froe(&["summary", compact_store.to_str().unwrap()]);
    assert!(
        after.contains("journal entries   1"),
        "journal collapsed to 1 entry: {after}"
    );

    eprintln!("  froe check after compaction");
    froe(&["check", compact_store.to_str().unwrap()]);

    eprintln!("  froe tree /content/interop after compaction (content preserved)");
    let tree = froe(&[
        "tree",
        compact_store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "3",
    ]);
    assert!(tree.contains("sling:Folder"), "content tree preserved");

    // Boot Sling against the compacted store.
    eprintln!("  booting Sling against the froe-compacted store");
    let volume = PodmanVolume::new("froe-interop-compact");
    let bootstrap = PodmanContainer::run_detached("froe-compact-bootstrap", 8083, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&compact_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-compact-verify", 8083, &volume.name);
    wait_for_sling(8083, "froe-compact-verify");

    eprintln!("  content snapshot from Sling after compaction");
    let snapshot = content_snapshot(8083);
    assert!(
        snapshot.contains("Interop Fixture"),
        "content preserved: {snapshot}"
    );
    assert!(snapshot.contains("Page 1"), "page 1 preserved: {snapshot}");

    // Verify the binary round-tripped.
    let binary = Command::new("curl")
        .args([
            "-s",
            "-u",
            "admin:admin",
            "http://localhost:8083/content/interop/files/file/jcr:content",
        ])
        .output()
        .expect("curl binary");
    let binary_text = String::from_utf8_lossy(&binary.stdout);
    assert!(
        binary_text.contains("Lorem ipsum"),
        "binary content round-tripped: {binary_text}"
    );

    drop(sling);
    eprintln!("  compact phase passed");
}

/// Phase 6: froe cleanup against a multi-generational store.
///
/// Builds a gen 0→1→2 store by compacting twice, then restoring the
/// original gen-0 archive. Those gen-0 segments are 2 full generations
/// behind the head, not protected by journal history (truncated), and
/// not referenced by surviving gen-2 segments — true orphans the
/// `segments` task reclaims. Also tests expired checkpoints, stale
/// archives, and corrupt journal lines.
///
/// Depends on `compact` (to build the fixture). If this fails, the
/// write path's plan-and-apply machinery is broken.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn cleanup() {
    let store = oak_store();
    let clean_store = work_root().join("cleanup-store");
    eprintln!("  copying store to {}", clean_store.display());
    copy_store(&store, &clean_store);

    // Save the original gen-0 archive before compaction.
    let gen0_archive = work_root().join("gen0-archive-backup.tar");
    std::fs::copy(clean_store.join("data00000a.tar"), &gen0_archive).expect("backup gen-0 archive");

    // Build a multi-generational store: compact twice to advance the head
    // to full_generation=2.
    eprintln!("  step 1: froe compact (gen 0 -> 1)");
    froe(&["compact", clean_store.to_str().unwrap(), "--yes"]);

    eprintln!("  step 2: froe compact again (gen 1 -> 2)");
    froe(&["compact", clean_store.to_str().unwrap(), "--yes"]);

    // Restore the gen-0 archive at a higher archive number so the segments
    // task finds it as a separate archive with orphan segments, not as a
    // stale letter of the active archive.
    eprintln!("  step 3: restore gen-0 archive at data00004a.tar");
    std::fs::copy(&gen0_archive, clean_store.join("data00004a.tar"))
        .expect("restore gen-0 archive");

    // Wait for the checkpoint from phase 3 to expire.
    eprintln!("  waiting 2s for the checkpoint to expire");
    std::thread::sleep(Duration::from_secs(2));

    // Add the remaining simulation conditions.
    eprintln!("  making stale archive");
    make_stale_archive(&clean_store);

    eprintln!("  truncating journal to head");
    truncate_journal_to_head(&clean_store);

    eprintln!("  appending corrupt journal lines");
    append_corrupt_journal_lines(&clean_store);

    // Dry-run: verify the plan sees the orphan segments.
    eprintln!("  froe cleanup --dry-run");
    let dry_run = froe(&["cleanup", clean_store.to_str().unwrap(), "--dry-run"]);
    assert!(
        dry_run.contains("orphan segments"),
        "dry-run found orphan segments: {dry_run}"
    );

    // Apply: reclaim the orphan segments, stale archive, expired checkpoint,
    // and corrupt journal lines.
    eprintln!("  froe cleanup --yes");
    let cleanup_output = froe(&["cleanup", clean_store.to_str().unwrap(), "--yes"]);
    assert!(
        cleanup_output.contains("orphan segments removed"),
        "cleanup removed orphan segments: {cleanup_output}"
    );

    eprintln!("  froe summary after cleanup");
    froe(&["summary", clean_store.to_str().unwrap()]);

    eprintln!("  froe check after cleanup");
    froe(&["check", clean_store.to_str().unwrap()]);

    // Boot Sling against the cleaned store.
    eprintln!("  booting Sling against the froe-cleaned store");
    let volume = PodmanVolume::new("froe-interop-cleanup");
    let bootstrap = PodmanContainer::run_detached("froe-cleanup-bootstrap", 8082, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&clean_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-cleanup-verify", 8082, &volume.name);
    wait_for_sling(8082, "froe-cleanup-verify");

    eprintln!("  content snapshot from Sling after cleanup");
    let snapshot = content_snapshot(8082);
    assert!(
        snapshot.contains("Interop Fixture"),
        "content preserved: {snapshot}"
    );

    drop(sling);
    eprintln!("  cleanup phase passed");
}

/// Phase 7: froe backup and restore.
///
/// Depends on `read` and `checkpoint` (writer). Independent of compact/
/// cleanup but later in the chain because it is lower-risk.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn backup() {
    let store = oak_store();
    let backup_dir = work_root().join("backup-store");
    let restore_store = work_root().join("restore-store");

    eprintln!("  froe backup");
    let backup_output = froe(&[
        "backup",
        store.to_str().unwrap(),
        backup_dir.to_str().unwrap(),
        "--yes",
    ]);
    assert!(
        backup_output.contains("backup complete"),
        "backup succeeded: {backup_output}"
    );

    eprintln!("  froe check of the backup");
    froe(&["check", backup_dir.to_str().unwrap()]);

    eprintln!("  preparing restore target (copy of the original store)");
    copy_store(&store, &restore_store);

    eprintln!("  froe restore");
    let restore_output = froe(&[
        "restore",
        backup_dir.to_str().unwrap(),
        restore_store.to_str().unwrap(),
        "--yes",
    ]);
    assert!(
        restore_output.contains("restore complete"),
        "restore succeeded: {restore_output}"
    );

    eprintln!("  froe check after restore");
    froe(&["check", restore_store.to_str().unwrap()]);

    eprintln!("  froe tree /content/interop after restore");
    let tree = froe(&[
        "tree",
        restore_store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "3",
    ]);
    assert!(
        tree.contains("sling:Folder"),
        "content tree preserved after restore"
    );

    // Boot Sling against the restored store.
    eprintln!("  booting Sling against the froe-restored store");
    let volume = PodmanVolume::new("froe-interop-restore");
    let bootstrap = PodmanContainer::run_detached("froe-restore-bootstrap", 8085, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&restore_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-restore-verify", 8085, &volume.name);
    wait_for_sling(8085, "froe-restore-verify");

    eprintln!("  content snapshot from Sling after restore");
    let snapshot = content_snapshot(8085);
    assert!(
        snapshot.contains("Interop Fixture"),
        "content preserved: {snapshot}"
    );

    drop(sling);
    eprintln!("  backup phase passed");
}

/// Phase 8: froe recover-journal.
///
/// Deletes journal.log, then rebuilds it from the segments. Depends on
/// `read`. Last because it is the most destructive (deletes the journal).
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn recover() {
    let store = oak_store();
    let recover_store = work_root().join("recover-store");
    eprintln!("  copying store to {}", recover_store.display());
    copy_store(&store, &recover_store);

    eprintln!("  deleting journal.log");
    std::fs::remove_file(recover_store.join("journal.log")).expect("remove journal");

    eprintln!("  froe recover-journal --yes");
    let recover_output = froe(&["recover-journal", recover_store.to_str().unwrap(), "--yes"]);
    assert!(
        !recover_output.is_empty(),
        "recover-journal produced output"
    );

    eprintln!("  froe summary after recovery");
    froe(&["summary", recover_store.to_str().unwrap()]);

    eprintln!("  froe check after recovery");
    froe(&["check", recover_store.to_str().unwrap()]);

    eprintln!("  froe tree /content/interop after recovery");
    let tree = froe(&[
        "tree",
        recover_store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "3",
    ]);
    assert!(
        tree.contains("sling:Folder"),
        "content tree preserved after recovery"
    );

    // Boot Sling against the recovered store.
    eprintln!("  booting Sling against the froe-recovered store");
    let volume = PodmanVolume::new("froe-interop-recover");
    let bootstrap = PodmanContainer::run_detached("froe-recover-bootstrap", 8086, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&recover_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-recover-verify", 8086, &volume.name);
    wait_for_sling(8086, "froe-recover-verify");

    eprintln!("  content snapshot from Sling after recovery");
    let snapshot = content_snapshot(8086);
    assert!(
        snapshot.contains("Interop Fixture"),
        "content preserved: {snapshot}"
    );

    drop(sling);
    eprintln!("  recover phase passed");
}

/// Run all interop phases in order.
///
/// This is a convenience wrapper that runs the phases in dependency order
/// within a single test. Individual phases can be run separately for
/// debugging.
#[test]
#[ignore = "requires podman and the apache/sling:14 image"]
fn interop_full() {
    generate();
    read();
    commit();
    checkpoint();
    compact();
    cleanup();
    backup();
    recover();
    eprintln!("  all interop phases passed");
}
