---
id: arm-reindex-fault-cutpoints
title: Arm Fault And Process-Death Cutpoints Around Publication
workstream: "0007"
kind: task
depends_on: [plan-prepare-and-apply-the-reindex]
gated: false
touches:
  - crates/froe/src/writer/fault_injection/index_reindex.rs
  - crates/froe/src/writer/fault_injection/mod.rs
  - crates/froe/src/writer/fault_injection/test_support.rs
  - crates/froe/src/writer/index/apply.rs
  - docs/plans/0007-property-index-reindex/ARCHITECTURE.md
status: done
merged_as: "d2862f487137427e6a41c61c8d810027bcde9a77"
---
# Arm Fault And Process-Death Cutpoints Around Publication

Place cutpoints at the meaningful boundaries of the mutation table and arm each with a named test, in the existing `fault_injection` framework: `index-reindex.before-head-publish`, `index-reindex.after-head-publish-before-flush`, `index-reindex.before-spill-cleanup`, and `index-reindex.before-applied-verification`. Cover both a returned error and abrupt `_exit` death where the states differ, through the framework's child harness (a full exact test name; the crash child exits with `CRASH_EXIT_CODE` at the armed cutpoint and the error child with `VERIFIED_EXIT_CODE` after its assertions, as `fault_injection::test_support` does, and the parent's fresh reopen proves the prefix). The harness in `test_support.rs` today dispatches compaction scenarios only and panics on any other, so this task adds a reindex child entry point and scenario dispatch beside them.

**Steps:**

1. Add the cutpoints in `apply.rs` at the boundaries — `index-reindex.before-spill-cleanup` fired once the collector's sort has returned its iterator and before the first index record is appended, so every spill file is on disk and the store is untouched; the sort takes no cutpoint of its own, which keeps its crate-root independence from the segment-store write path, and the rest around publication and applied verification — with the `#[cfg(test)]` plumbing the other cutpoints use.
2. `fault_injection/index_reindex.rs`: one test per cutpoint per fault model, each freshly reopening the store and asserting the exact prefix: before publication, the old head resolves, every original `:index` subtree is intact, and the new archives hold only records unreachable from the head — stamped at the head's generation, so not what the planner calls interrupted-run residue — and a subsequent `froe compact` reclaims them, which the test runs and asserts; after publication but before flush, the journal still names the old head and the new records are unreachable, the same on-disk prefix as before publication, which the pair of tests states explicitly; before applied verification, the store is already final and the outcome is reported. At `index-reindex.before-spill-cleanup` the store is unchanged in both models; the run's subdirectory is removed on a returned error and left with its spill files on death. After every death variant the parent reacquires `repo.lock` and reruns the operation, asserting the retry's observable behaviour against the left-behind run subdirectory — refused in an operator-named work directory, warned about and proceeded with under the default — and that the retry publishes the same post-state.
3. Register the module in `fault_injection/mod.rs`, extend `test_support.rs` with the reindex child entry point, and record every cutpoint name with its test in the safety case's fault table in `ARCHITECTURE.md`.

- **Done when:** every cutpoint is armed by a named test that fails when the cutpoint is removed, each test asserts the prefix state from a fresh reopen, the process-death tests prove the scenario ran through the harness's scenario-specific exit codes (`CRASH_EXIT_CODE` at the cutpoint, `VERIFIED_EXIT_CODE` after the assertions) and the parent's fresh reopen, and the fault table names every cutpoint and its test, the lock is reacquired and the retry asserted after every death variant, and the stable host gate passes.
