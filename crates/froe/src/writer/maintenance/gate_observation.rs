//! A test-only record of which open-protocol gates ran, and on what.
//!
//! The gates of `apply_identity.rs` and `planning/shape.rs` are what make an
//! apply safe, and an operation that forgot to call one would still pass
//! every test about what it *wrote* — the failure is the absence of a
//! refusal, which nothing downstream can observe. So each gate records its
//! own call here, and a test of a `prepare` can assert the wiring directly
//! rather than by inference.
//!
//! `#[cfg(test)]` throughout: the recorder does not exist in a release
//! build, and the call sites compile to nothing.

#[cfg(test)]
std::thread_local! {
    static CALLS: std::cell::RefCell<Vec<(&'static str, std::path::PathBuf)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Records that `gate` ran against `directory`.
#[cfg(test)]
pub(crate) fn record(gate: &'static str, directory: &std::path::Path) {
    CALLS.with(|calls| {
        calls.borrow_mut().push((gate, directory.to_path_buf()));
    });
}

/// Does nothing outside a test build.
#[cfg(not(test))]
#[inline]
pub(crate) fn record(gate: &'static str, directory: &std::path::Path) {
    let _ = (gate, directory);
}

/// Forgets every recorded call.
#[cfg(test)]
pub(crate) fn reset() {
    CALLS.with(|calls| calls.borrow_mut().clear());
}

/// Every gate recorded since the last reset, in order.
#[cfg(test)]
pub(crate) fn recorded() -> Vec<(&'static str, std::path::PathBuf)> {
    CALLS.with(|calls| calls.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::{recorded, reset};
    use crate::writer::maintenance::test_support::TestDirectory;
    use crate::writer::maintenance::{CompactionOptions, PreparedCompaction};

    /// Compaction's own `prepare` calls every gate the open protocol is made
    /// of, against the canonicalized repository directory.
    ///
    /// The first user of the seam, and here rather than in `prepared.rs` so
    /// that module stays untouched by this refactor. What it pins is the
    /// *wiring*: a `prepare` that dropped a gate would still produce a
    /// correct plan and a correct apply on a healthy store, and only fail to
    /// refuse an unhealthy one.
    #[test]
    fn a_prepare_calls_every_open_protocol_gate() {
        let directory = TestDirectory::repository("gate-observation-prepare");
        reset();
        let prepared =
            PreparedCompaction::prepare(&directory.path, CompactionOptions::new().with_tasks([]))
                .expect("prepare a healthy store");

        let calls = recorded();
        let gates: Vec<&str> = calls.iter().map(|(gate, _)| *gate).collect();
        for expected in [
            "validate_repository_shape",
            "validate_apply_environment",
            "validate_apply_identity",
        ] {
            assert!(
                gates.contains(&expected),
                "{expected} was not called during prepare: {gates:?}"
            );
        }

        // Before *and* again after the lock: a check that ran only before it
        // proves nothing about the state the apply will act on.
        for repeated in ["validate_apply_environment", "validate_apply_identity"] {
            assert!(
                gates.iter().filter(|gate| **gate == repeated).count() >= 2,
                "{repeated} must run before and again after the lock: {gates:?}"
            );
        }

        let canonical =
            std::fs::canonicalize(&directory.path).expect("canonicalize the repository");
        for (gate, seen) in &calls {
            assert_eq!(
                seen,
                &canonical,
                "{gate} received {} rather than the canonicalized directory",
                seen.display()
            );
        }
        drop(prepared);
    }
}
