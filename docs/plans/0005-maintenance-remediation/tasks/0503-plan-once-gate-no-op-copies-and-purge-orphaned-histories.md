---
id: plan-once-gate-no-op-copies-and-purge-orphaned-histories
title: Plan Once, Gate No-Op Copies, And Purge Orphaned Histories
workstream: "0005"
kind: task
depends_on: [conclude-progress-steps-with-results]
gated: false
touches:
  - crates/froe-cli/src/command_line.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/mutation.rs
  - crates/froe-cli/src/tooling_display.rs
  - crates/froe-cli/tests/command_line_tests/compaction.rs
  - crates/froe-cli/tests/command_line_tests/diagnostics.rs
  - crates/froe-cli/tests/command_line_tests/export.rs
  - crates/froe-cli/tests/command_line_tests/support.rs
  - crates/froe/src/content/node.rs
  - crates/froe/src/lib.rs
  - crates/froe/src/tooling/check/path.rs
  - crates/froe/src/tooling/check/subtree.rs
  - crates/froe/src/tooling/digest.rs
  - crates/froe/src/tooling/mod.rs
  - crates/froe/src/writer/compaction/mod.rs
  - crates/froe/src/writer/compaction/walk.rs
  - crates/froe/src/writer/fault_injection/mod.rs
  - crates/froe/src/writer/fault_injection/publication.rs
  - crates/froe/src/writer/fault_injection/test_support.rs
  - crates/froe/src/writer/maintenance/apply/compaction_phase.rs
  - crates/froe/src/writer/maintenance/apply/journal_phase.rs
  - crates/froe/src/writer/maintenance/mod.rs
  - crates/froe/src/writer/maintenance/options.rs
  - crates/froe/src/writer/maintenance/plan.rs
  - crates/froe/src/writer/maintenance/planning/content_census.rs
  - crates/froe/src/writer/maintenance/planning/listing.rs
  - crates/froe/src/writer/maintenance/planning/mod.rs
  - crates/froe/src/writer/maintenance/planning/segments.rs
  - crates/froe/src/writer/maintenance/planning/shape.rs
  - crates/froe/src/writer/maintenance/planning/version_storage.rs
  - crates/froe/src/writer/maintenance/prepared.rs
  - crates/froe/src/writer/maintenance/reclamation.rs
  - crates/froe/src/writer/mod.rs
  - crates/froe/tests/orphaned_version_history_tests.rs
  - crates/froe/tests/plan_reporting_tests.rs
status: done
merged_as: "cbd6a2eba73ba4e50e120c5de9c2960c968b484e"
---
# Plan Once, Gate No-Op Copies, And Purge Orphaned Histories

Phases 1 through 4 in one breaking commit: the three-line estimate and the blob-store line; apply acquiring the lock first and planning once with the copy verified before the head swap and publication-boundary fault probes; `compaction_disposition` with `--always-copy`; the two-pass orphaned-history census, the always-on report, `--purge-orphaned-version-histories` with the age bound, the inbound-reference demotion and the per-scope memo for context-dependent ancestors; and `froe digest --exclude-subtree` with the exclusions stamped in the header. Landed in `cbd6a2e` (2026-08-19).

**Steps:**

1. Replace the preview-and-replan flow with `PreparedCompaction::prepare` running repairs and building the one plan under the lock; `--dry-run` keeps the lockless read-only pass.
2. Move the full walk of the fresh copy before `compare_and_set_head` and arm `publication.rs` cutpoints for an error and a process death between them.
3. Compute the disposition from triple equality, checkpoint and history selection and the journal line count; route the no-copy case through the standalone sweep.
4. Run the content census first, the version-storage pre-scan second, resolve matches after both, and realize a confirmed purge by copy-time omission with childless intermediate nodes pruned.
5. Add the subtree exclusion to the digest and derive the interop exclusion set from the confirmed plan, never by hand.

- **Done when:** on the reproduction store an apply transcript shows three `opening archives`, two `verifying the current head` and one `verifying the compacted copy`; a second run reports `nothing to do` without mutation; the orphan set equals the independent oracle; and the fault probes leave the store at its old head. Met at `cbd6a2e`.
