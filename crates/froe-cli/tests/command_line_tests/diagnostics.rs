//! What the command line writes and where: help and version on standard
//! output, errors with their escaping and layout intact.

use super::*;

#[test]
pub(crate) fn interactive_cleanup_prompt_frames_do_not_reuse_earlier_stderr() {
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

#[test]
pub(crate) fn clap_help_and_version_remain_successful_stdout_diagnostics() {
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
pub(crate) fn clap_errors_escape_bidi_values_and_keep_their_multiline_layout() {
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "compact",
            "/not-consulted",
            "--archive-rewrite-policy",
            "bad\u{202e}value",
        ])
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
pub(crate) fn destructive_prompt_escapes_hostile_repository_path() {
    // A real store under a hostile name, with a removable journal line so
    // the plan is never empty: the prompt must appear, carry the escaped
    // path, and be cancelled by end-of-file on standard input.
    let parent = TestDirectory::new("hostile-prompt");
    let hostile = parent.path.join("store-\u{1b}]8;;https:-bidi-\u{202e}");
    std::fs::create_dir_all(&hostile).expect("create hostile store directory");
    populate(&hostile);
    std::fs::OpenOptions::new()
        .append(true)
        .open(hostile.join("journal.log"))
        .expect("open journal")
        .write_all(b"parser-skipped-line\n")
        .expect("append removable line");

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .arg("compact")
        .arg(&hostile)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run cancelled compaction prompt");

    assert!(!run.status.success(), "EOF must cancel compaction");
    assert!(!run.stderr.contains(&0x1b), "raw ESC reached stderr");
    let stderr = String::from_utf8(run.stderr).expect("prompt stays UTF-8");
    assert!(!stderr.contains('\u{202e}'), "raw bidi reached stderr");
    assert!(stderr.contains(r"\u{1b}"), "{stderr}");
    assert!(stderr.contains(r"\u{202e}"), "{stderr}");
    assert!(
        stderr.contains("compaction cancelled"),
        "end-of-file must cancel, not error: {stderr}"
    );
}

#[test]
pub(crate) fn checkpoint_prompt_distinguishes_controls_from_literal_escape_text() {
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

/// `digest --exclude-subtree` names its exclusions in a header and leaves
/// everything else byte-identical to the unexcluded digest — the property
/// the purge's before/after comparison rests on.
#[test]
pub(crate) fn digest_excludes_exactly_the_named_subtree() {
    let directory = TestDirectory::new("digest-exclusion");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate_with_orphaned_history(&store);

    let full = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["digest", store.to_str().expect("path")])
        .output()
        .expect("run the full digest");
    assert!(full.status.success(), "the full digest must succeed");
    let full = String::from_utf8_lossy(&full.stdout).into_owned();

    let excluded_path = "/jcr:system/jcr:versionStorage";
    let excluded = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "digest",
            store.to_str().expect("path"),
            "--exclude-subtree",
            excluded_path,
        ])
        .output()
        .expect("run the excluding digest");
    assert!(
        excluded.status.success(),
        "the excluding digest must succeed"
    );
    let excluded = String::from_utf8_lossy(&excluded.stdout).into_owned();

    assert!(
        excluded.starts_with(&format!("#excluded\t{excluded_path}\n")),
        "the exclusion is declared in the output itself:\n{excluded}"
    );
    assert!(
        full.lines().any(|line| line.starts_with(excluded_path)),
        "the fixture puts content under the excluded path:\n{full}"
    );
    assert!(
        !excluded.lines().any(|line| line.starts_with(excluded_path)),
        "no line under the exclusion survives:\n{excluded}"
    );
    let full_outside: Vec<&str> = full
        .lines()
        .filter(|line| !line.starts_with(excluded_path))
        .collect();
    let excluded_outside: Vec<&str> = excluded
        .lines()
        .filter(|line| !line.starts_with("#excluded"))
        .collect();
    assert_eq!(
        full_outside, excluded_outside,
        "outside the exclusion, the digests are byte-identical"
    );
}
