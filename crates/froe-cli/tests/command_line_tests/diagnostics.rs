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
