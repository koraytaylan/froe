---
id: expose-the-maintenance-gates
title: Expose The Maintenance Gates To The Index Module
workstream: "0007"
kind: task
depends_on: []
gated: false
touches:
  - crates/froe/src/writer/maintenance/mod.rs
  - crates/froe/src/writer/maintenance/planning/mod.rs
  - crates/froe/src/writer/maintenance/planning/shape.rs
  - crates/froe/src/writer/maintenance/apply_identity.rs
  - crates/froe/src/writer/maintenance/gate_observation.rs
  - crates/froe/tests/support/mod.rs
  - crates/froe/tests/support/observation_log.rs
  - crates/froe/tests/progress_api_tests.rs
status: done
merged_as: "eef46e5fd444431f582cf215c2774b56c94a49fa"
---
# Expose The Maintenance Gates To The Index Module

`CONTRIBUTING.md` keeps refactors of format-facing code in a `refactor:` commit apart from the behaviour change they serve, so that a high-risk diff shows only what changed on disk. The reindex of task 0707 must follow compaction's open protocol step for step, and every function that protocol is made of lives behind the private `mod maintenance;` and `mod planning;` boundaries with `pub(in crate::writer::maintenance)` or `pub(super)` visibility. This task widens and re-exports them, factors the one gate compaction only has in plan-bound form, adds the observation seam the later guard evidence needs, and moves the test-side recording observer into shared test support — with no change in behaviour, which compaction's existing guard tests prove.

**Steps:**

1. Widen to `pub(crate)` at their definitions and re-export through `planning/mod.rs` and `maintenance/mod.rs`: `directory_fingerprint`, `DirectoryFingerprint` and its parts, `canonical_repository_directory`, `validate_repository_shape`, `validate_apply_environment`, `validate_apply_identity`, and `available_filesystem_bytes` with its `#[cfg(not(unix))]` arm that answers `None` — the last because tasks 0707 and 1007 compare a work-directory estimate against reported free space and must not carry a second copy of its platform-sensitive `unsafe` block.
2. Factor `validate_metadata_source_apply_identity(directory)` in `apply_identity.rs` from `planned_metadata_sources` and `metadata_source_apply_identity_issue` — the newest active archive, the one `open_prepared` takes its metadata from, checked without a `CompactionPlan` — beside a `_for_credentials` twin with a module test modelled on `authoritative_plan_rejects_a_foreign_owned_archive_rewrite_before_mutation`; `validate_plan_apply_identity` keeps its behaviour and calls the shared helper (`apply_identity.rs` is 804 lines; new tests stay in-module only while the file remains under the thousand-line gate).
3. Add a `#[cfg(test)]` observation seam — its recorder in a new `maintenance/gate_observation.rs`, one call at each gate definition in `apply_identity.rs` and `shape.rs` — that records which gates were called and the directory each received, so an in-crate unit test of a `prepare` can assert the wiring; compaction's own `prepare` is asserted through it once, as the first user, by a unit test in `gate_observation.rs`'s own test module, so `maintenance/prepared.rs` stays untouched.
4. Move the private `ObservationLog` of `crates/froe/tests/progress_api_tests.rs` into `crates/froe/tests/support/observation_log.rs`, registered in `tests/support/mod.rs` and opening with the same `#![allow(dead_code, reason = …)]` as `filesystem_snapshot.rs` (every binary declaring `support` compiles it), so the reindex tests of task 0707 can assert step counts through it; `progress_api_tests.rs` includes only that file by `#[path = "support/observation_log.rs"]`, as `filesystem_snapshot_tests.rs` does for its helper.
5. Run compaction's existing guard and fault tests and the progress tests unchanged; the workspace gate.

- **Done when:** every existing compaction test and every progress test passes without modification beyond the observer's import, the new twin's module test refuses a foreign-owned newest archive with the observed failure quoted in the test, the seam records compaction's `prepare` calling every gate with the canonical directory, and the stable host gate passes.
