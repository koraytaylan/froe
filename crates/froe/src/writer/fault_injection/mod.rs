//! Process-crash probes for the write path's durability boundaries.
//!
//! A cutpoint sits at each point a run has committed to something on disk.
//! They span the write path rather than one module of it: the tar writer,
//! the archive sweep under a live session, the journal rewrite, and the
//! maintenance apply all carry them. A probe arms one, runs a child
//! process into it, and asserts the store the child left behind is exactly
//! one of the outcomes that boundary permits.
//!
//! This entire module exists only in unit-test builds. Production binaries do
//! not contain the environment checks or the cutpoint calls.

use std::ffi::OsStr;

// Every probe forks a child and ends it with a signal, so the whole
// harness is Unix-only. The `*_if_armed` helpers below stay available on
// every target, because production code calls them under `cfg(test)`.
#[cfg(unix)]
mod journal;
#[cfg(unix)]
mod manifest;
#[cfg(unix)]
mod session;
#[cfg(unix)]
mod sweep;
#[cfg(all(test, unix))]
mod test_support;

pub(crate) const CHILD_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_CHILD";

pub(crate) const CUTPOINT_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_CUTPOINT";

pub(crate) const MODE_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_MODE";

#[cfg(unix)]
pub(crate) const CRASH_EXIT_CODE: i32 = 86;

#[cfg(unix)]
pub(crate) const VERIFIED_EXIT_CODE: i32 = 87;

#[cfg(unix)]
pub(crate) const CRASH_MODE: &str = "crash";

pub(crate) const ERROR_MODE: &str = "error";

pub(crate) const SUBSTITUTE_MODE: &str = "substitute";

pub(crate) const ABSENCE_MODE: &str = "absence";

pub(crate) fn is_armed(cutpoint: &str, mode: &str) -> bool {
    std::env::var_os(CHILD_ENVIRONMENT).as_deref() == Some(OsStr::new("1"))
        && std::env::var_os(CUTPOINT_ENVIRONMENT).as_deref() == Some(OsStr::new(cutpoint))
        && std::env::var_os(MODE_ENVIRONMENT).as_deref() == Some(OsStr::new(mode))
}

/// Whether an in-memory consistency probe is explicitly armed in the
/// isolated fault-test child.
pub(super) fn is_substitution_armed(cutpoint: &str) -> bool {
    is_armed(cutpoint, SUBSTITUTE_MODE)
}

/// Terminates an explicitly armed child process at `cutpoint` without running
/// destructors. Ordinary unit-test processes never set the child marker.
pub(super) fn crash_if_armed(cutpoint: &str) {
    #[cfg(unix)]
    if is_armed(cutpoint, CRASH_MODE) {
        // SAFETY: `_exit` has no memory-safety preconditions. It is used only
        // in an isolated test child specifically to model abrupt process death
        // without Rust unwinding or guard cleanup.
        unsafe { libc::_exit(CRASH_EXIT_CODE) }
    }
    #[cfg(not(unix))]
    let _ = cutpoint;
}

/// Returns a deterministic synthetic I/O error from an explicitly armed test
/// child. Callers place this immediately before or after a real syscall to
/// exercise both old-state and completed-syscall error handling.
pub(super) fn fail_if_armed(cutpoint: &str) -> crate::error::Result<()> {
    if is_armed(cutpoint, ERROR_MODE) {
        return Err(
            std::io::Error::other(format!("injected cleanup I/O failure at {cutpoint}")).into(),
        );
    }
    Ok(())
}

/// Replaces an armed staging/source pathname exactly once while leaving its
/// previously validated inode under a diagnostic non-active name. Isolated
/// child-process tests use this to prove destructive syscalls are bound to the
/// descriptor that was certified, not merely to a reusable pathname.
pub(super) fn substitute_path_if_armed(
    cutpoint: &str,
    path: &std::path::Path,
) -> crate::error::Result<()> {
    if !is_armed(cutpoint, SUBSTITUTE_MODE) {
        return Ok(());
    }
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("staging");
    let displaced = path.with_file_name(format!("{file_name}.validated-inode"));
    std::fs::rename(path, displaced)?;
    std::fs::write(path, b"substituted pathname\n")?;
    Ok(())
}

/// Removes an armed pathname immediately before production retries the same
/// unlink, modelling an external actor winning that exact deletion race.
pub(super) fn remove_path_if_armed(
    cutpoint: &str,
    path: &std::path::Path,
) -> crate::error::Result<()> {
    if is_armed(cutpoint, ABSENCE_MODE) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Omits the final retained item from an explicitly armed in-memory
/// post-mutation analysis. This changes no repository byte; it makes the
/// production retained-root verification call load-bearing in tests.
pub(super) fn omit_last_if_armed<T>(cutpoint: &str, items: &mut Vec<T>) {
    if is_armed(cutpoint, SUBSTITUTE_MODE) {
        items.pop();
    }
}

/// Adds a physical line that cannot exist in the final journal to an armed
/// verifier's expected set. This changes no repository byte; it makes the
/// production byte-exact retained-line verification call load-bearing.
pub(super) fn append_missing_journal_line_if_armed(cutpoint: &str, expected: &mut Vec<Vec<u8>>) {
    if is_armed(cutpoint, SUBSTITUTE_MODE) {
        expected.push(b"froe injected missing retained journal line\n".to_vec());
    }
}
