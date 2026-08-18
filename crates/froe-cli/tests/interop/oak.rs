//! Proving Oak consumed the store as froe wrote it, rather than repairing
//! its way to a readable one.

use super::*;

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
pub(crate) const OAK_REPAIR_MARKERS: &[&str] = &[
    "Unable to access revision",
    "Could not find a valid tar index",
    "Recovering segments from tar file",
    "Could not read tar file",
    "Regenerating tar file",
];

/// Read the `oak-segment-tar` version out of the running container and assert
/// it is the build this suite claims to verify against. Returns the version so
/// the run record can name it.
pub(crate) fn assert_oak_build(container: &str) -> String {
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
pub(crate) fn container_logs(container: &str) -> String {
    let output = Command::new("podman")
        .args(["logs", container])
        .output()
        .unwrap_or_else(|error| panic!("failed to read logs of {container}: {error}"));
    let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
    logs.push_str(&String::from_utf8_lossy(&output.stderr));
    logs
}

/// A line every Sling boot writes, used as the log gate's positive
/// control. Captured from a real container log rather than written from
/// memory: it is the launcher's own banner, printed before any bundle
/// starts, so it is present in the log of any container that came up at
/// all.
pub(crate) const SLING_BOOT_MARKER: &str = "Apache Sling Application Launcher";

/// Fail unless Oak consumed the store exactly as froe wrote it.
pub(crate) fn assert_oak_consumed_store_as_written(container: &str, phase: &str) {
    let logs = container_logs(container);
    // The positive control. A scan for absent markers passes trivially on
    // an empty string, so without this a mistyped container name, a
    // container removed early, or a `podman logs` that failed for any
    // reason would report "Oak consumed the store as froe wrote it" while
    // having read nothing at all. Asserting a line that must be there
    // makes the absence of the repair markers evidence rather than an
    // artifact of having no log.
    assert!(
        logs.contains(SLING_BOOT_MARKER),
        "{phase}: the log of {container} does not contain {SLING_BOOT_MARKER:?}, so the \
         repair-marker scan below would be looking at nothing and would pass no matter \
         what Oak did. {} bytes were captured.",
        logs.len()
    );
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
