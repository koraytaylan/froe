//! Moving a store between the work tree and a podman volume, and the
//! froe commands that report where its head is.

use super::*;

/// The integer immediately preceding `suffix` in froe's output.
///
/// froe reports its counts inside one formatted line, so each count is
/// identified by the text that follows it. Parsing the number is what makes an
/// assertion fail on a zero count — matching the surrounding phrase cannot,
/// because the phrase comes from an unconditional format template.
pub(crate) fn parse_count(output: &str, suffix: &str) -> u64 {
    let position = output
        .find(suffix)
        .unwrap_or_else(|| panic!("output contains {suffix:?}: {output}"));
    let reversed: String = output[..position]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit() || *character == ',')
        .collect();
    // froe groups long counts with thousands separators; scanning without
    // accepting them would silently read `18,796,598` as `598` and leave a
    // "greater than zero" assertion passing on a truncated number.
    let digits: String = reversed.chars().rev().filter(|c| *c != ',').collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("an integer precedes {suffix:?} in: {output}"))
}

/// The head revision froe reports, so a phase can prove which revision Oak
/// resolved rather than assuming it was the one froe wrote.
pub(crate) fn froe_head(store: &Path) -> String {
    let summary = froe(&["summary", store.to_str().unwrap()]);
    summary
        .lines()
        .find(|line| line.trim_start().starts_with("head"))
        .unwrap_or_else(|| panic!("summary reports a head line: {summary}"))
        .trim()
        .to_owned()
}

/// The revision on the last line of `journal.log` — the head froe actually
/// wrote, read from the file rather than from froe's own reporting.
pub(crate) fn journal_head_revision(store: &Path) -> String {
    let journal = std::fs::read_to_string(store.join("journal.log")).expect("read journal.log");
    journal
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_else(|| panic!("journal.log has no revision line:\n{journal}"))
        .to_owned()
}

/// `froe check` over the whole tree *including binary content*, asserting
/// it found a good revision **at the head froe wrote**.
///
/// Asserting the exit status alone proves almost nothing:
/// `ConsistencyReport::has_good_revision` is an `any` over the head paths
/// chained with every checkpoint's paths, so a store whose head is broken
/// still exits zero as long as one checkpoint resolves somewhere. The
/// revision is what carries the claim, and froe already prints it.
///
/// `--binaries` matters for the same reason: without it binary records are
/// resolved but never read, so a store whose blocks are unreachable passes.
pub(crate) fn assert_check_passes_at_head(store: &Path, phase: &str) {
    let report = froe(&[
        "check",
        store.to_str().unwrap(),
        "--path",
        "/",
        "--binaries",
    ]);
    let head = journal_head_revision(store);
    let expected = format!("latest good revision for path / is {head}");
    assert!(
        report.contains(&expected),
        "{phase}: froe check found a good revision, but not at the head froe wrote.\n\
         expected to find: {expected}\n\
         has_good_revision() is an `any` over head *and* checkpoint paths, so a zero exit \
         status alone would not have caught this.\nreport:\n{report}"
    );
}

// ---------------------------------------------------------------------------
// Store manipulation helpers
// ---------------------------------------------------------------------------

/// Copy a store directory, removing repo.lock. The target is cleared
/// first: a rerun's leftover output would otherwise merge with the fresh
/// copy, and a stale higher-letter archive from a previous compaction
/// shadows the fresh archive of the same number — a franken-store whose
/// head resolves against the wrong bytes.
pub(crate) fn copy_store(src: &Path, dst: &Path) {
    if dst.exists() {
        std::fs::remove_dir_all(dst).expect("clear a leftover store copy");
    }
    std::fs::create_dir_all(dst).expect("create dst");
    copy_dir_recursive(src, dst);
    let _ = std::fs::remove_file(dst.join("repo.lock"));
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) {
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

/// Copy a store from a podman volume to a host directory. Cleared first
/// for the same reason as [`copy_store`]: extraction into a rerun's
/// leftovers merges two stores into neither.
pub(crate) fn store_from_volume(volume: &str, dst: &Path) {
    if dst.exists() {
        std::fs::remove_dir_all(dst).expect("clear a leftover extraction");
    }
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
pub(crate) fn store_into_volume(src: &Path, volume: &str) {
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
