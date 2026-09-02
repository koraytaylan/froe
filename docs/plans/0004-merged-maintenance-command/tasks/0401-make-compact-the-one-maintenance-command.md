---
id: make-compact-the-one-maintenance-command
title: Make Compact The One Maintenance Command
workstream: "0004"
kind: task
depends_on: []
gated: false
touches:
  - README.md
  - crates/froe-cli/README.md
  - crates/froe-cli/src/inspection.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/mutation.rs
  - crates/froe-cli/tests/command_line_tests.rs
  - crates/froe-cli/tests/interop.rs
  - crates/froe/README.md
  - crates/froe/src/checksum.rs
  - crates/froe/src/lib.rs
  - crates/froe/src/packed_records.rs
  - crates/froe/src/tooling/check.rs
  - crates/froe/src/units.rs
  - crates/froe/src/writer/compaction.rs
  - crates/froe/src/writer/journal_maintenance.rs
  - crates/froe/src/writer/maintenance.rs
  - crates/froe/src/writer/maintenance_fault_injection.rs
  - crates/froe/src/writer/mod.rs
  - crates/froe/src/writer/record_writer.rs
  - crates/froe/src/writer/store_writer.rs
  - crates/froe/src/writer/tar_writer.rs
  - crates/froe/tests/progress_api_tests.rs
  - crates/froe/tests/reclamation_completeness_tests.rs
  - docs/analysis/write-cleanup.md
  - docs/analysis/write-filestore.md
  - docs/cli-output.md
  - docs/compact.md
  - docs/high-risk-changes.md
  - docs/interop.md
  - docs/oak-segment-tar-feature-map.md
  - docs/plans/0001-repair-archives/ARCHITECTURE.md
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
  - docs/plans/0003-exact-compaction-sharing-memo/ARCHITECTURE.md
  - docs/storage-format.md
  - scripts/interop-fixture.sh
status: done
merged_as: "55158095d31162d517244814d3d0ec18f54c68dc"
---
# Make Compact The One Maintenance Command

`froe cleanup` was removed and `froe compact` became the one maintenance command: reclaim sources certified in parallel before the copy appends anything, the head published exactly once, every reclaimable archive rewritten without Oak's savings gate, one generation retained under `validate_reclaim_reference_invariant`, the journal retired to the head line on every run, and `docs/compact.md` rewritten as the command's contract. The all-features rustdoc gate was restored beside it. Landed in `4798418` and `5515809` (2026-08-17).

**Steps:**

1. Replace the two commands and their option sets with one `CompactionOptions` flow: plan, certify, copy, publish, sweep, `gc.log`, journal, verify, retire residue.
2. Parallelize source certification over `std::thread::scope` with a shared position counter, and let the copy's certificate stand for the reclaim pass.
3. Write `segments_unreachable_from_the_journal` in `reclamation_completeness_tests.rs` from the storage format alone and bind the plan's prediction to it.
4. Rewrite `docs/compact.md`, the feature map, `docs/interop.md`, the analysis notes and the three earlier plan documents to describe the merged command.

- **Done when:** `compact_then_cleanup_leaves_nothing_unreachable_on_a_binary_heavy_store`, `repeated_compaction_and_cleanup_never_accumulates_unreachable_segments` and `one_merged_run_compacts_and_reclaims_in_a_single_pass` pass against the format-only oracle, and no `froe cleanup` remains. Met at `5515809`.
