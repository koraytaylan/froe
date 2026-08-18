//! What Sling reports about the content tree, and the baseline every
//! later phase is measured against.

use super::*;

/// Every string value Sling serves for `property`, as `property=value`.
///
/// The `.tidy.` selector pretty-prints, so the separator is `": "` rather than
/// `":"`; matching an exact `"key":"` byte sequence finds nothing and would
/// leave the fingerprint silently empty.
pub(crate) fn string_property_values(snapshot: &str, property: &str) -> Vec<String> {
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
pub(crate) fn content_fingerprint(port: u16) -> Vec<String> {
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

/// Fetch the uploaded binary back from Oak and require every byte to match
/// what was uploaded.
///
/// A substring check cannot carry this claim: the fixture's binary is the
/// same 122-byte sentence repeated 16384 times, so `contains("Lorem
/// ipsum")` passes on a stream that lost all but the first block, was
/// truncated at any point, or had whole blocks reordered — precisely the
/// damage a block-list bug produces. Comparing the bytes is what makes
/// "round-trips byte-for-byte" a statement about the run rather than about
/// the first fifty characters of it.
pub(crate) fn assert_binary_round_trips_byte_for_byte(port: u16, phase: &str) {
    let expected = std::fs::read(work_root().join("binary.txt")).expect("read the uploaded binary");
    let served_path = work_root().join(format!("served-binary-{port}.bin"));
    let status = Command::new("curl")
        .args([
            "-s",
            "-o",
            served_path.to_str().unwrap(),
            "-u",
            "admin:admin",
            &format!("http://localhost:{port}/content/interop/files/file/jcr:content"),
        ])
        .status()
        .expect("curl the binary");
    assert!(status.success(), "{phase}: curl failed fetching the binary");
    let served = std::fs::read(&served_path).expect("read the served binary");
    assert_eq!(
        served.len(),
        expected.len(),
        "{phase}: Oak served {} bytes of the binary, not the {} that were uploaded",
        served.len(),
        expected.len()
    );
    if served != expected {
        let first_difference = served
            .iter()
            .zip(&expected)
            .position(|(left, right)| left != right)
            .unwrap_or(0);
        panic!(
            "{phase}: the binary Oak serves is the right length but differs from what was \
             uploaded, first at byte {first_difference}"
        );
    }
    let _ = std::fs::remove_file(&served_path);
    eprintln!(
        "  binary round-tripped byte-for-byte ({} bytes)",
        served.len()
    );
}

pub(crate) fn content_baseline_path() -> PathBuf {
    work_root().join("content-fingerprint-baseline.txt")
}

/// Record the fingerprint of the pristine Oak-written content, before froe
/// has touched anything.
pub(crate) fn save_content_baseline(port: u16) {
    let fingerprint = content_fingerprint(port);
    std::fs::write(content_baseline_path(), fingerprint.join("\n"))
        .expect("write the content baseline");
    eprintln!("  recorded content baseline: {} entries", fingerprint.len());
}

/// Assert the content Oak serves is exactly what it served before froe ran —
/// no node lost, none altered, none added.
pub(crate) fn assert_content_matches_baseline(port: u16, phase: &str) {
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
