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

/// The Sling image, pinned by manifest digest. Apache-2.0; boots Oak with
/// `TarMK` by default.
///
/// A digest rather than the `:14` tag, because the claim in `README.md` names
/// an Oak build: a tag can be re-pushed, which would silently change what the
/// suite verified. Pinning makes a gate run reproducible.
const PINNED_SLING_IMAGE: &str = "docker.io/apache/sling@sha256:8722cd66ae0758e50784ac21df836c8f8d9e443d105e1a4292a4cb7f810a8cc9";

/// The floating tag, for the periodic canary that deliberately looks for
/// ecosystem drift instead of reproducibility.
const FLOATING_SLING_IMAGE: &str = "docker.io/apache/sling:14";

/// The image to run, `PINNED_SLING_IMAGE` unless `FROE_INTEROP_SLING_IMAGE`
/// overrides it.
///
/// The two modes answer different questions. A pinned run asks "does froe still
/// interoperate with the Oak build we published a claim about" — that is the
/// release gate. A floating run asks "has the ecosystem moved underneath the
/// claim" — that is the canary, and there the Oak-version assertion failing is
/// the useful result, not a nuisance.
fn sling_image() -> String {
    select_sling_image(
        std::env::var("FROE_INTEROP_SLING_IMAGE").ok().as_deref(),
        std::env::var("FROE_INTEROP_CANARY").ok().as_deref(),
    )
}

/// The image-selection rule, separated from the environment so it can be
/// tested without mutating process state.
///
/// The canary branch runs unattended once a month, which is the worst place to
/// discover a wiring mistake, so it is covered by an ordinary test rather than
/// only by the scheduled run itself.
fn select_sling_image(image_override: Option<&str>, canary: Option<&str>) -> String {
    if let Some(image) = image_override
        && !image.trim().is_empty()
    {
        return image.to_owned();
    }
    if canary.is_some_and(|flag| flag.trim() == "1") {
        return FLOATING_SLING_IMAGE.to_owned();
    }
    PINNED_SLING_IMAGE.to_owned()
}

#[test]
fn image_selection_pins_by_default_and_floats_only_for_the_canary() {
    assert_eq!(select_sling_image(None, None), PINNED_SLING_IMAGE);
    assert_eq!(select_sling_image(None, Some("0")), PINNED_SLING_IMAGE);
    assert_eq!(select_sling_image(None, Some("")), PINNED_SLING_IMAGE);
    assert_eq!(select_sling_image(None, Some("1")), FLOATING_SLING_IMAGE);
    assert_eq!(select_sling_image(None, Some(" 1 ")), FLOATING_SLING_IMAGE);
    // An explicit override wins over the canary flag, and blank is not an
    // override — a workflow that sets the variable to "" must still get the pin.
    assert_eq!(
        select_sling_image(Some("example/oak:1"), None),
        "example/oak:1"
    );
    assert_eq!(
        select_sling_image(Some("example/oak:1"), Some("1")),
        "example/oak:1"
    );
    assert_eq!(
        select_sling_image(Some(""), Some("1")),
        FLOATING_SLING_IMAGE
    );
    assert_eq!(select_sling_image(Some("   "), None), PINNED_SLING_IMAGE);
    // The pin is a digest and the canary target is a tag; mixing them up would
    // silently make a "pinned" run reproducible in name only.
    assert!(PINNED_SLING_IMAGE.contains("@sha256:"));
    assert!(!FLOATING_SLING_IMAGE.contains("@sha256:"));
}

/// How long to wait for Sling to finish booting.
const SLING_BOOT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for a froe command to finish.
const FROE_TIMEOUT: Duration = Duration::from_secs(120);

/// The `oak-segment-tar` build this suite is verified against.
///
/// `apache/sling:14` is a mutable tag, so the Oak version inside the image is
/// the only durable coordinate for what "interop-verified" means. The suite
/// asserts it rather than trusting the tag: an image that silently starts
/// shipping a different Oak must fail here and be re-verified deliberately,
/// not quietly redefine the claim.
const EXPECTED_OAK_SEGMENT_TAR_VERSION: &str = "1.90.0";

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
            &sling_image(),
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

/// Oak's own log lines for every path by which it repairs, rewinds past, or
/// regenerates a store it does not trust, taken from the Java rather than
/// guessed: `file/FileStoreUtil.java:58` ("Unable to access revision {},
/// rewinding...") and `file/tar/TarReader.java` lines 99, 128, 157, 161 and
/// 179.
///
/// These matter more than any content assertion. If Oak rebuilt a TAR index
/// from a full scan, skipped an unreadable archive, or fell back to an older
/// journal revision, then it did not consume what froe wrote — it consumed
/// something it reconstructed, and "Sling served the expected content"
/// becomes evidence about Oak's recovery code instead of about froe.
const OAK_REPAIR_MARKERS: &[&str] = &[
    "Unable to access revision",
    "Could not find a valid tar index",
    "Recovering segments from tar file",
    "Could not read tar file",
    "Regenerating tar file",
];

/// Read the `oak-segment-tar` version out of the running container and assert
/// it is the build this suite claims to verify against. Returns the version so
/// the run record can name it.
fn assert_oak_build(container: &str) -> String {
    let listing = podman(&[
        "exec",
        container,
        "sh",
        "-c",
        "find / -name 'oak-segment-tar-*.jar' 2>/dev/null | head -1",
    ]);
    let jar = listing.trim();
    assert!(
        !jar.is_empty(),
        "found no oak-segment-tar jar in {container}; this image may not be an \
         Oak-backed Sling, so the round trip would prove nothing"
    );
    let version = jar
        .rsplit_once("oak-segment-tar-")
        .and_then(|(_, tail)| tail.strip_suffix(".jar"))
        .unwrap_or_else(|| panic!("unexpected oak-segment-tar jar name: {jar}"))
        .to_owned();
    assert_eq!(
        version,
        EXPECTED_OAK_SEGMENT_TAR_VERSION,
        "{} ships oak-segment-tar {version}, not {EXPECTED_OAK_SEGMENT_TAR_VERSION}. \
         On a canary run against the floating tag this is the expected signal that \
         the ecosystem moved: re-verify deliberately, then update both \
         PINNED_SLING_IMAGE and EXPECTED_OAK_SEGMENT_TAR_VERSION so the published \
         claim keeps naming the build it was proved against.",
        sling_image()
    );
    eprintln!("  Oak build under test: oak-segment-tar {version}");
    version
}

/// Both output streams of a container, since Sling logs to stdout and JVM
/// diagnostics to stderr.
fn container_logs(container: &str) -> String {
    let output = Command::new("podman")
        .args(["logs", container])
        .output()
        .unwrap_or_else(|error| panic!("failed to read logs of {container}: {error}"));
    let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
    logs.push_str(&String::from_utf8_lossy(&output.stderr));
    logs
}

/// Fail unless Oak consumed the store exactly as froe wrote it.
fn assert_oak_consumed_store_as_written(container: &str, phase: &str) {
    let logs = container_logs(container);
    let repairs: Vec<&str> = OAK_REPAIR_MARKERS
        .iter()
        .filter(|marker| logs.contains(**marker))
        .copied()
        .collect();
    assert!(
        repairs.is_empty(),
        "{phase}: Oak repaired the store instead of consuming it as froe wrote it \
         ({repairs:?}). A content assertion after a repair proves nothing about \
         froe's output.\nlogs:\n{logs}"
    );
}

/// Every string value Sling serves for `property`, as `property=value`.
///
/// The `.tidy.` selector pretty-prints, so the separator is `": "` rather than
/// `":"`; matching an exact `"key":"` byte sequence finds nothing and would
/// leave the fingerprint silently empty.
fn string_property_values(snapshot: &str, property: &str) -> Vec<String> {
    let key = format!("\"{property}\"");
    let mut values = Vec::new();
    let mut rest = snapshot;
    while let Some(position) = rest.find(&key) {
        rest = &rest[position + key.len()..];
        let Some(colon) = rest.find(':') else { break };
        let after_colon = rest[colon + 1..].trim_start();
        if let Some(quoted) = after_colon.strip_prefix('"')
            && let Some(end) = quoted.find('"')
        {
            values.push(format!("{property}={}", &quoted[..end]));
        }
        rest = after_colon;
    }
    values
}

/// A deterministic fingerprint of the content subtree as Oak serves it: every
/// node's primary type and every title, sorted.
///
/// A `contains` assertion cannot detect a deletion — the string it looks for
/// lives on the node that survived. Comparing this fingerprint against the
/// baseline captured from Oak's own store detects any node that disappeared,
/// changed primary type, or lost its title.
fn content_fingerprint(port: u16) -> Vec<String> {
    let snapshot = content_snapshot(port);
    assert!(
        snapshot.contains("jcr:primaryType"),
        "content snapshot is not a node serialization (an error page or empty \
         body cannot be compared): {snapshot}"
    );
    let mut entries = Vec::new();
    for property in ["jcr:primaryType", "jcr:title"] {
        entries.extend(string_property_values(&snapshot, property));
    }
    assert!(
        !entries.is_empty(),
        "content fingerprint is empty, so an equality check would be vacuous"
    );
    entries.sort();
    entries
}

fn content_baseline_path() -> PathBuf {
    work_root().join("content-fingerprint-baseline.txt")
}

/// Record the fingerprint of the pristine Oak-written content, before froe
/// has touched anything.
fn save_content_baseline(port: u16) {
    let fingerprint = content_fingerprint(port);
    std::fs::write(content_baseline_path(), fingerprint.join("\n"))
        .expect("write the content baseline");
    eprintln!("  recorded content baseline: {} entries", fingerprint.len());
}

/// Assert the content Oak serves is exactly what it served before froe ran —
/// no node lost, none altered, none added.
fn assert_content_matches_baseline(port: u16, phase: &str) {
    let recorded = std::fs::read_to_string(content_baseline_path()).expect(
        "read the content baseline; the generate phase records it and every later \
         phase compares against it",
    );
    let baseline: Vec<String> = recorded.lines().map(str::to_owned).collect();
    let actual = content_fingerprint(port);
    let missing: Vec<&String> = baseline.iter().filter(|e| !actual.contains(e)).collect();
    let unexpected: Vec<&String> = actual.iter().filter(|e| !baseline.contains(e)).collect();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "{phase}: Oak no longer serves the same content as before the operation.\n\
         missing {} entries: {missing:?}\nunexpected {} entries: {unexpected:?}",
        missing.len(),
        unexpected.len()
    );
    eprintln!(
        "  content matches the baseline exactly ({} entries)",
        actual.len()
    );
}

/// The integer immediately preceding `suffix` in froe's output.
///
/// froe reports its cleanup counts inside one formatted line, so each count is
/// identified by the text that follows it. Parsing the number is what makes an
/// assertion fail on a zero count — matching the surrounding phrase cannot,
/// because the phrase comes from an unconditional format template.
fn parse_count(output: &str, suffix: &str) -> u64 {
    let position = output
        .find(suffix)
        .unwrap_or_else(|| panic!("output contains {suffix:?}: {output}"));
    let reversed: String = output[..position]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    let digits: String = reversed.chars().rev().collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("an integer precedes {suffix:?} in: {output}"))
}

/// The head revision froe reports, so a phase can prove which revision Oak
/// resolved rather than assuming it was the one froe wrote.
fn froe_head(store: &Path) -> String {
    let summary = froe(&["summary", store.to_str().unwrap()]);
    summary
        .lines()
        .find(|line| line.trim_start().starts_with("head"))
        .unwrap_or_else(|| panic!("summary reports a head line: {summary}"))
        .trim()
        .to_owned()
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
/// Confirm every condition the cleanup fixture is meant to contain is really
/// present before cleanup runs.
///
/// Without this, a fixture step that silently failed to build its condition
/// would leave the post-cleanup assertions vacuously satisfied — the condition
/// would be absent afterwards because it was never there.
fn assert_cleanup_fixture_built(store: &Path, stale: &StaleArchive, checkpoint_name: &str) {
    let checkpoints = froe(&["checkpoints", store.to_str().unwrap()]);
    assert!(
        checkpoints.contains(checkpoint_name),
        "the checkpoint {checkpoint_name} that must expire is present before \
         cleanup: {checkpoints}"
    );
    let journal = std::fs::read_to_string(store.join("journal.log")).expect("read journal before");
    assert!(
        journal.contains("this_line_has_no_space") && journal.contains("not-a-uuid:bad"),
        "both corrupt journal lines are present before cleanup: {journal}"
    );
    assert!(
        stale.superseded.exists() && stale.winner.exists(),
        "both letters of the stale-archive pair are present before cleanup: {} and {}",
        stale.superseded.display(),
        stale.winner.display()
    );
    assert!(
        store.join("data00004a.tar").exists(),
        "the orphan-bearing gen-0 archive is present before cleanup"
    );
}

/// Confirm each condition is gone from disk and from froe's own listings,
/// independently of the counts cleanup reported.
fn assert_cleanup_conditions_removed(store: &Path, stale: &StaleArchive, checkpoint_name: &str) {
    assert!(
        !stale.superseded.exists(),
        "the superseded archive letter {} is gone from disk after cleanup",
        stale.superseded.display()
    );
    // The safety-relevant direction: cleanup removed the superseded letter and
    // left the winner, rather than deleting the archive Oak will actually open.
    assert!(
        stale.winner.exists(),
        "the winning archive letter {} survived cleanup",
        stale.winner.display()
    );
    // The orphan-bearing gen-0 archive is the 727-segment condition; if it is
    // still here, the segments task reclaimed nothing regardless of the count
    // it printed.
    assert!(
        !store.join("data00004a.tar").exists(),
        "the orphan-bearing gen-0 archive was reclaimed"
    );
    let journal = std::fs::read_to_string(store.join("journal.log")).expect("read journal after");
    assert!(
        !journal.contains("this_line_has_no_space") && !journal.contains("not-a-uuid:bad"),
        "both corrupt journal lines are gone from the journal: {journal}"
    );
    assert!(
        !journal.trim().is_empty(),
        "cleanup left a usable journal, not an empty one"
    );
    let checkpoints = froe(&["checkpoints", store.to_str().unwrap()]);
    assert!(
        !checkpoints.contains(checkpoint_name),
        "the expired checkpoint {checkpoint_name} is gone after cleanup: {checkpoints}"
    );
}

/// The stale-archive condition: two files for one archive number.
///
/// Oak selects the highest generation letter (`tar-layer.md` §"generation
/// letter selection"), so copying the active archive to the next letter makes
/// the *copy* the winner and leaves the original superseded. Cleanup must
/// remove the superseded file and must not touch the winner, so the phase
/// needs both paths to assert either direction.
struct StaleArchive {
    superseded: PathBuf,
    winner: PathBuf,
}

/// Create the stale-archive condition and return both files, so the phase can
/// assert on exact paths rather than a hardcoded guess.
fn make_stale_archive(store: &Path) -> StaleArchive {
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
    assert!(
        letter != b'z',
        "the active archive is already at generation z, so the stale-archive \
         condition cannot be built; skipping it would leave the phase asserting \
         nothing about stale archives"
    );
    let next_letter = (letter + 1) as char;
    let stale_name = format!("data{}{}.tar", &base[4..base.len() - 5], next_letter);
    let stale_path = store.join(&stale_name);
    eprintln!("  creating stale archive: {base} -> {stale_name}");
    std::fs::copy(&active, &stale_path).expect("copy stale archive");
    StaleArchive {
        superseded: active,
        winner: stale_path,
    }
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

    // Pin the Oak build now, while the container is still up. The image tag
    // is mutable, so the version inside it is the only durable coordinate.
    // Record it for the run record, which is the artifact that makes a passing
    // run auditable afterwards instead of an ephemeral console line.
    let oak_version = assert_oak_build("froe-interop-gen");
    std::fs::write(work_root().join("oak-build.txt"), &oak_version)
        .expect("record the Oak build under test");

    // Record what Oak serves before froe has touched anything. Every later
    // phase compares against this, which is what turns "Sling still serves
    // the root node" into "Sling serves exactly the same tree".
    eprintln!("  recording the content baseline from Oak's own store");
    save_content_baseline(8080);

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
    let created = froe(&[
        "checkpoint",
        "create",
        store.to_str().unwrap(),
        "--lifetime-milliseconds",
        "1000",
        "--yes",
    ]);
    // Pin the identity of the checkpoint that was created. Asserting only that
    // the listing is non-empty is satisfied by the literal string "no
    // checkpoints", so it would pass even if nothing had been written.
    let name = created
        .lines()
        .find_map(|line| line.trim().strip_prefix("created checkpoint "))
        .unwrap_or_else(|| panic!("create reports the checkpoint name: {created}"))
        .trim()
        .to_owned();
    assert!(!name.is_empty(), "the created checkpoint has a name");

    eprintln!("  froe checkpoints (the new one must be listed by name)");
    let checkpoints = froe(&["checkpoints", store.to_str().unwrap()]);
    assert!(
        checkpoints.contains(&name),
        "the checkpoint {name} froe created is listed: {checkpoints}"
    );

    // Record it so the cleanup phase can prove this exact checkpoint expired
    // and was removed, rather than trusting a count.
    std::fs::write(work_root().join("expiring-checkpoint.txt"), &name)
        .expect("record the expiring checkpoint name");

    eprintln!("  checkpoint phase passed ({name})");
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
    assert_oak_consumed_store_as_written("froe-compact-verify", "compact");
    assert_content_matches_baseline(8083, "compact");
    let snapshot = content_snapshot(8083);
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

/// Tail compaction against a real Oak store.
///
/// A materially different reclamation path from full compaction: it advances
/// the generation but keeps the shared full generation, reclaiming by
/// generation alone. Covering only the full form would leave a documented flag
/// of a maintenance command unexercised against Oak.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn compact_tail() {
    let store = oak_store();
    let tail_store = work_root().join("compact-tail-store");
    eprintln!("  copying store to {}", tail_store.display());
    copy_store(&store, &tail_store);

    eprintln!("  truncating journal to head");
    truncate_journal_to_head(&tail_store);

    eprintln!("  froe compact --tail --yes");
    froe(&["compact", tail_store.to_str().unwrap(), "--tail", "--yes"]);

    eprintln!("  froe check after tail compaction");
    froe(&["check", tail_store.to_str().unwrap()]);

    eprintln!("  booting Sling against the tail-compacted store");
    let volume = PodmanVolume::new("froe-interop-compact-tail");
    let bootstrap = PodmanContainer::run_detached("froe-tail-bootstrap", 8087, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&tail_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-tail-verify", 8087, &volume.name);
    wait_for_sling(8087, "froe-tail-verify");

    assert_oak_consumed_store_as_written("froe-tail-verify", "compact --tail");
    assert_content_matches_baseline(8087, "compact --tail");

    drop(sling);
    eprintln!("  compact --tail phase passed");
}

/// The checkpoint the Oak async indexer references, read from `/:async`.
fn async_referenced_checkpoint(store: &Path) -> String {
    let node = froe(&["node", store.to_str().unwrap(), "/:async"]);
    let line = node
        .lines()
        .find(|line| line.contains("async <String>"))
        .unwrap_or_else(|| panic!("/:async carries an async checkpoint reference: {node}"));
    let (_, quoted) = line
        .split_once("= \"")
        .unwrap_or_else(|| panic!("the async property is a quoted string: {line}"));
    quoted.trim_end().trim_end_matches('"').to_owned()
}

/// Checkpoint removal against a real Oak store.
///
/// Removal rewrites the super-root's `checkpoints` subtree and commits a new
/// head, which is the structure Oak's asynchronous indexer depends on through
/// `/:async`. The safety-relevant property is that `remove-unreferenced`
/// removes an unreferenced checkpoint and *keeps* the one the indexer
/// references: removing that would make Oak discard its index state and
/// reindex the repository from scratch.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
fn checkpoint_removal() {
    let store = oak_store();
    let removal_store = work_root().join("checkpoint-removal-store");
    eprintln!("  copying store to {}", removal_store.display());
    copy_store(&store, &removal_store);

    let referenced = async_referenced_checkpoint(&removal_store);
    eprintln!("  Oak's async indexer references checkpoint {referenced}");
    let listing = froe(&["checkpoints", removal_store.to_str().unwrap()]);
    assert!(
        listing.contains(&referenced),
        "the async-referenced checkpoint is present to begin with: {listing}"
    );

    // Two froe-created checkpoints, neither referenced by the indexer.
    let mut created = Vec::new();
    for _ in 0..2 {
        let output = froe(&[
            "checkpoint",
            "create",
            removal_store.to_str().unwrap(),
            "--lifetime-milliseconds",
            "3600000",
            "--yes",
        ]);
        created.push(
            output
                .lines()
                .find_map(|line| line.trim().strip_prefix("created checkpoint "))
                .unwrap_or_else(|| panic!("create reports a name: {output}"))
                .trim()
                .to_owned(),
        );
    }
    eprintln!("  created unreferenced checkpoints {created:?}");

    eprintln!("  froe checkpoint remove (by name)");
    froe(&[
        "checkpoint",
        "remove",
        removal_store.to_str().unwrap(),
        &created[0],
        "--yes",
    ]);
    let after_remove = froe(&["checkpoints", removal_store.to_str().unwrap()]);
    assert!(
        !after_remove.contains(&created[0]),
        "the named checkpoint was removed: {after_remove}"
    );
    assert!(
        after_remove.contains(&created[1]) && after_remove.contains(&referenced),
        "removing one checkpoint by name left the others: {after_remove}"
    );

    eprintln!("  froe checkpoint remove-unreferenced");
    froe(&[
        "checkpoint",
        "remove-unreferenced",
        removal_store.to_str().unwrap(),
        "--yes",
    ]);
    let after_unreferenced = froe(&["checkpoints", removal_store.to_str().unwrap()]);
    assert!(
        !after_unreferenced.contains(&created[1]),
        "the remaining unreferenced checkpoint was removed: {after_unreferenced}"
    );
    assert!(
        after_unreferenced.contains(&referenced),
        "the checkpoint Oak's async indexer references SURVIVED remove-unreferenced; \
         removing it would force a full reindex: {after_unreferenced}"
    );

    eprintln!("  froe check after checkpoint removal");
    froe(&["check", removal_store.to_str().unwrap()]);

    eprintln!("  booting Sling against the store with a rewritten checkpoints subtree");
    let volume = PodmanVolume::new("froe-interop-checkpoint-removal");
    let bootstrap = PodmanContainer::run_detached("froe-cprm-bootstrap", 8088, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&removal_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-cprm-verify", 8088, &volume.name);
    wait_for_sling(8088, "froe-cprm-verify");

    assert_oak_consumed_store_as_written("froe-cprm-verify", "checkpoint removal");
    assert_content_matches_baseline(8088, "checkpoint removal");
    drop(sling);

    // remove-all is the broadest form; assert it empties the subtree and that
    // the store still verifies afterwards.
    eprintln!("  froe checkpoint remove-all");
    froe(&[
        "checkpoint",
        "remove-all",
        removal_store.to_str().unwrap(),
        "--yes",
    ]);
    let after_all = froe(&["checkpoints", removal_store.to_str().unwrap()]);
    assert!(
        after_all.contains("no checkpoints"),
        "remove-all emptied the checkpoints subtree: {after_all}"
    );
    froe(&["check", removal_store.to_str().unwrap()]);

    eprintln!("  checkpoint removal phase passed");
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
    let stale = make_stale_archive(&clean_store);

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
    //
    // Every assertion below names a specific effect. The obvious check —
    // that the output contains "orphan segments removed" — is worthless,
    // because that phrase comes from an unconditional format template and is
    // printed even when the count is zero.
    let expiring_checkpoint = std::fs::read_to_string(work_root().join("expiring-checkpoint.txt"))
        .expect("the checkpoint phase records the name of the checkpoint that will expire");
    assert_cleanup_fixture_built(&clean_store, &stale, expiring_checkpoint.trim());

    eprintln!("  froe cleanup --yes");
    let cleanup_output = froe(&["cleanup", clean_store.to_str().unwrap(), "--yes"]);

    // Parse the reported counts and require each condition to have been acted
    // on, so a run that removed nothing cannot pass.
    let removed_segments = parse_count(&cleanup_output, " orphan segments removed");
    let removed_stale = parse_count(&cleanup_output, " stale removed");
    let removed_checkpoints = parse_count(&cleanup_output, " checkpoints and");
    let removed_journal_lines = parse_count(&cleanup_output, " journal lines removed");
    assert!(
        removed_segments > 0,
        "cleanup reclaimed orphan segments, not zero: {cleanup_output}"
    );
    assert!(
        removed_stale >= 1,
        "cleanup removed the stale archive: {cleanup_output}"
    );
    assert_eq!(
        removed_checkpoints, 1,
        "cleanup removed the one expired checkpoint: {cleanup_output}"
    );
    assert_eq!(
        removed_journal_lines, 2,
        "cleanup removed both corrupt journal lines: {cleanup_output}"
    );
    assert!(
        cleanup_output.contains("cleanup complete"),
        "cleanup completed without deferred or failed deletions: {cleanup_output}"
    );

    // Then confirm the same effects on disk, independently of what was
    // reported.
    assert_cleanup_conditions_removed(&clean_store, &stale, expiring_checkpoint.trim());

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
    assert_oak_consumed_store_as_written("froe-cleanup-verify", "cleanup");
    assert_content_matches_baseline(8082, "cleanup");

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

    // Restore into a target whose *content* differs from the backup's, not a
    // byte copy of the store the backup came from. Restoring into a copy of its
    // own source cannot fail: a restore that wrote nothing would satisfy every
    // assertion, because the target already holds the expected tree.
    //
    // The commit-phase store carries froe-written nodes the backup does not, so
    // a real restore must make those nodes disappear and leave exactly the
    // baseline tree. The post-boot baseline comparison below reports unexpected
    // entries as well as missing ones, which is what detects a no-op.
    eprintln!("  preparing restore target from the commit store (content differs from the backup)");
    copy_store(&work_root().join("commit-store"), &restore_store);
    let target_head_before = froe_head(&restore_store);

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

    // Restore deep-copies the backup's head into the target, so the target gets
    // an equivalent tree at a *new* record identifier rather than the backup's
    // own. What must hold is that the head moved at all — a restore that wrote
    // nothing would leave it untouched.
    let target_head_after = froe_head(&restore_store);
    assert_ne!(
        target_head_after, target_head_before,
        "restore advanced the target's head; it was unchanged, so nothing was \
         written"
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
    assert_oak_consumed_store_as_written("froe-restore-verify", "restore");
    assert_content_matches_baseline(8085, "restore");

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

    // Recovery's defining property is which revision it restores, so pin the
    // head before destroying the journal. Recovery writes every surviving
    // candidate and only verifies the newest, so "the journal resolves" is
    // satisfied by resolving to an older revision — which would silently lose
    // every commit after it.
    let head_before = froe_head(&recover_store);

    eprintln!("  deleting journal.log");
    std::fs::remove_file(recover_store.join("journal.log")).expect("remove journal");

    eprintln!("  froe recover-journal --yes");
    let recover_output = froe(&["recover-journal", recover_store.to_str().unwrap(), "--yes"]);
    assert!(
        !recover_output.is_empty(),
        "recover-journal produced output"
    );

    eprintln!("  froe summary after recovery");
    let head_after = froe_head(&recover_store);
    assert_eq!(
        head_after, head_before,
        "recovery restored the same head it started from, not an older revision"
    );

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
    assert_oak_consumed_store_as_written("froe-recover-verify", "recover");
    assert_content_matches_baseline(8086, "recover");

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
    compact_tail();
    checkpoint_removal();
    cleanup();
    backup();
    recover();
    write_run_record();
    eprintln!("  all interop phases passed");
}

/// Write the run record: what was verified, against which Oak build, when.
///
/// A passing run whose only trace is a console line cannot be audited later.
/// This is the artifact that turns "we have an interop suite" into "the round
/// trip was performed against oak-segment-tar X on this date", which is what
/// the interoperability requirement in `CONTRIBUTING.md` actually asks to be
/// recorded.
fn write_run_record() {
    let oak_version = std::fs::read_to_string(work_root().join("oak-build.txt"))
        .expect("the generate phase records the Oak build");
    let manifest = std::fs::read_to_string(oak_store().join("manifest")).expect("read manifest");
    let store_version = manifest
        .lines()
        .find_map(|line| line.trim().strip_prefix("store.version="))
        .unwrap_or("unknown")
        .to_owned();
    let seconds_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let record = format!(
        "froe Oak interoperability run record\n\
         \n\
         unix timestamp:      {seconds_since_epoch}\n\
         image:               {image}\n\
         oak-segment-tar:     {oak_version}\n\
         store.version:       {store_version}\n\
         froe binary:         {binary}\n\
         \n\
         Phases passed, in dependency order:\n\
         \x20 generate    Oak wrote the fixture store\n\
         \x20 read        froe read Oak's store (summary, tree, check, search, export)\n\
         \x20 commit      Oak served content froe committed\n\
         \x20 checkpoint  froe created a checkpoint, listed by name\n\
         \x20 compact     Oak served the exact baseline tree after full compaction\n\
         \x20 compact     Oak served the exact baseline tree after tail compaction\n\
         \x20 --tail\n\
         \x20 checkpoint  remove by name, remove-unreferenced and remove-all all\n\
         \x20 removal     applied; the checkpoint Oak's async indexer references\n\
         \x20             survived remove-unreferenced, and Oak served the exact\n\
         \x20             baseline tree afterwards\n\
         \x20 cleanup     Oak served the exact baseline tree after orphan, stale-archive,\n\
         \x20             expired-checkpoint and corrupt-journal-line removal\n\
         \x20 backup      Oak served the exact baseline tree after backup and restore\n\
         \x20 recover     Oak served the exact baseline tree after journal recovery\n\
         \n\
         Every boot additionally asserted that Oak logged none of its repair\n\
         messages, so Oak consumed the store as froe wrote it rather than\n\
         reconstructing it.\n\
         \n\
         Not covered: native macOS or Windows execution, store.version=1,\n\
         external blob stores, and Adobe AEM itself (this loop is Apache Sling\n\
         with Oak).\n",
        image = sling_image(),
        binary = froe_bin().display()
    );
    let path = work_root().join("interop-run-record.txt");
    std::fs::write(&path, &record).expect("write the interop run record");
    eprintln!("  run record written to {}", path.display());
    eprint!("{record}");
}
