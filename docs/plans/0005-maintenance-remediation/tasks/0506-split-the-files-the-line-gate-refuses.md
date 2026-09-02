---
id: split-the-files-the-line-gate-refuses
title: Split The Files The Line Gate Refuses
workstream: "0005"
kind: task
depends_on: [state-the-as-built-contracts]
gated: false
touches:
  - crates/froe/src/content/map.rs
  - crates/froe/src/content/map/mod.rs
  - crates/froe/src/content/map/tests.rs
  - crates/froe/src/writer/compaction/mod.rs
  - crates/froe/src/writer/compaction/tests.rs
  - crates/froe/src/writer/maintenance/planning/mod.rs
  - crates/froe/src/writer/maintenance/planning/version_history_plan.rs
  - crates/froe/tests/orphaned_version_history_tests.rs
  - crates/froe/tests/orphaned_version_history_tests/detection.rs
  - crates/froe/tests/orphaned_version_history_tests/main.rs
  - crates/froe/tests/orphaned_version_history_tests/purge.rs
  - crates/froe/tests/orphaned_version_history_tests/support.rs
status: done
merged_as: "597c9e0bb981dce375c6186226e6dda7b73043e2"
---
# Split The Files The Line Gate Refuses

The purge and the census pushed four files past the thousand-line gate; they were split by responsibility with their tests moved beside the code they cover. Landed in `597c9e0` (2026-08-19).

**Steps:**

1. Split `content/map.rs`, `writer/compaction/mod.rs` and `planning/mod.rs` into module directories.
2. Split the orphaned-version-history tests into detection, purge and support.

- **Done when:** `scripts/oversized-files.sh` passes and every moved test still runs. Met at `597c9e0`.
