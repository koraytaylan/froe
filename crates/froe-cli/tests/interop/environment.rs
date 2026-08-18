//! What the suite runs against and how long it waits: the pinned Sling
//! image, the command timeout, and the paths of the binary and work tree.

use super::*;

/// The Sling image, pinned by manifest digest. Apache-2.0; boots Oak with
/// `TarMK` by default.
///
/// A digest rather than the `:14` tag, because the claim in `README.md` names
/// an Oak build: a tag can be re-pushed, which would silently change what the
/// suite verified. Pinning makes a gate run reproducible.
pub(crate) const PINNED_SLING_IMAGE: &str = "docker.io/apache/sling@sha256:8722cd66ae0758e50784ac21df836c8f8d9e443d105e1a4292a4cb7f810a8cc9";

/// The floating tag, for the periodic canary that deliberately looks for
/// ecosystem drift instead of reproducibility.
pub(crate) const FLOATING_SLING_IMAGE: &str = "docker.io/apache/sling:14";

/// The image to run, `PINNED_SLING_IMAGE` unless `FROE_INTEROP_SLING_IMAGE`
/// overrides it.
///
/// The two modes answer different questions. A pinned run asks "does froe still
/// interoperate with the Oak build we published a claim about" — that is the
/// release gate. A floating run asks "has the ecosystem moved underneath the
/// claim" — that is the canary, and there the Oak-version assertion failing is
/// the useful result, not a nuisance.
pub(crate) fn sling_image() -> String {
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
pub(crate) fn select_sling_image(image_override: Option<&str>, canary: Option<&str>) -> String {
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
pub(crate) fn image_selection_pins_by_default_and_floats_only_for_the_canary() {
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
pub(crate) const SLING_BOOT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for a froe command to finish, when the environment does
/// not say otherwise.
///
/// This is a hang detector, not a performance budget, so it is sized for the
/// largest store anyone would point the suite at rather than the small one it
/// generates by default. The old two-minute value was below measured reality:
/// `froe compact` on a 10 GB, 41-archive Sling store takes 120–135 s here, so
/// a suite run against a realistic fixture failed on commands that had exited
/// zero — and, because the timing assertion precedes the status assertion,
/// reported "timed out" while discarding the command's own output.
pub(crate) const DEFAULT_FROE_TIMEOUT: Duration = Duration::from_secs(900);

/// How long to wait for a froe command to finish.
///
/// `FROE_INTEROP_COMMAND_TIMEOUT_SECONDS` overrides it, because the ceiling
/// depends on the fixture: a CI run over the generated store wants a tight
/// bound, while a run over a multi-gigabyte store copied off a real instance
/// needs a loose one. An unparseable or zero value falls back to the default
/// rather than disabling the detector.
pub(crate) fn froe_timeout() -> Duration {
    resolve_froe_timeout(
        std::env::var("FROE_INTEROP_COMMAND_TIMEOUT_SECONDS")
            .ok()
            .as_deref(),
    )
}

/// The timeout rule, separated from the environment so it can be tested
/// without mutating process state — the same split `select_sling_image` uses,
/// and for the same reason: these tests share one process.
pub(crate) fn resolve_froe_timeout(setting: Option<&str>) -> Duration {
    setting
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(DEFAULT_FROE_TIMEOUT, Duration::from_secs)
}

#[test]
pub(crate) fn the_command_timeout_is_overridable_and_fails_safe() {
    assert_eq!(resolve_froe_timeout(None), DEFAULT_FROE_TIMEOUT);
    assert_eq!(
        resolve_froe_timeout(Some("30")),
        Duration::from_secs(30),
        "an explicit ceiling is honoured"
    );
    assert_eq!(
        resolve_froe_timeout(Some("  45  ")),
        Duration::from_secs(45),
        "surrounding whitespace does not defeat the override"
    );
    for unusable in ["", "0", "-1", "soon", "12s"] {
        assert_eq!(
            resolve_froe_timeout(Some(unusable)),
            DEFAULT_FROE_TIMEOUT,
            "{unusable:?} must fall back rather than disable the hang detector"
        );
    }
    assert!(
        DEFAULT_FROE_TIMEOUT >= Duration::from_secs(300),
        "the default must clear a compaction of a multi-gigabyte store, measured at 120-135s"
    );
}

/// The `oak-segment-tar` build this suite is verified against.
///
/// `apache/sling:14` is a mutable tag, so the Oak version inside the image is
/// the only durable coordinate for what "interop-verified" means. The suite
/// asserts it rather than trusting the tag: an image that silently starts
/// shipping a different Oak must fail here and be re-verified deliberately,
/// not quietly redefine the claim.
pub(crate) const EXPECTED_OAK_SEGMENT_TAR_VERSION: &str = "1.90.0";

// ---------------------------------------------------------------------------
// Shared fixture
// ---------------------------------------------------------------------------

/// The shared Oak store directory, produced by the `generate` test.
/// All later tests read from or copy this store.
pub(crate) static OAK_STORE: OnceLock<PathBuf> = OnceLock::new();

/// The work root for all interop artifacts.
///
/// Deliberately **not** process-scoped. A per-process default would put
/// every `cargo test` invocation in its own directory, and the suite's own
/// wrapper runs `generate` in one process and the named phase in another —
/// so a single phase could never find the fixture the previous process
/// built. Concurrent runs are already impossible for a different reason:
/// the podman container and volume names are fixed strings, so two suites
/// on one host collide long before their work roots would.
pub(crate) fn work_root() -> PathBuf {
    let root = std::env::var("FROE_INTEROP_WORK_ROOT")
        .map_or_else(|_| std::env::temp_dir().join("froe-interop"), PathBuf::from);
    std::fs::create_dir_all(&root).expect("create work root");
    root
}

/// Where `generate` records the fixture path for later processes.
pub(crate) fn fixture_pointer_path() -> PathBuf {
    work_root().join("fixture-path")
}

/// The froe binary path, resolved from the cargo build.
pub(crate) fn froe_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_froe"))
}

/// Run froe with the given arguments; assert success and return stdout.
pub(crate) fn froe(args: &[&str]) -> String {
    let start = Instant::now();
    let output = Command::new(froe_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn froe {args:?}: {error}"));
    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Status first, then timing. A command that failed *and* was slow has a
    // diagnosis in its own output; asserting the clock first would replace
    // that diagnosis with "timed out" and discard both streams.
    assert!(
        output.status.success(),
        "froe {args:?} exited with {status} after {elapsed:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status = output.status
    );
    let timeout = froe_timeout();
    assert!(
        elapsed < timeout,
        "froe {args:?} succeeded but took {elapsed:?}, over the {timeout:?} hang-detector \
         ceiling; raise FROE_INTEROP_COMMAND_TIMEOUT_SECONDS if this fixture is simply large\
         \nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

/// Run froe expecting it to refuse; assert failure and return stderr.
///
/// A refusal is part of the contract for a destructive tool — that a run
/// declines a store it cannot safely act on is as much a claim as what it
/// does when it can — so the suite asserts refusals the same way it asserts
/// successes, rather than only exercising the happy path.
pub(crate) fn froe_failure(args: &[&str]) -> String {
    let output = Command::new(froe_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn froe {args:?}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "froe {args:?} was expected to refuse but succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stderr
}
