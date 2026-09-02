---
id: restore-the-windows-build
title: Restore The Windows Build
workstream: "0004"
kind: task
depends_on: [write-the-safety-case]
gated: false
touches:
  - crates/froe/src/writer/fault_injection/mod.rs
  - crates/froe/src/writer/fault_injection/test_support.rs
  - crates/froe/src/writer/maintenance/apply_identity.rs
  - crates/froe/src/writer/maintenance/journal/file_identity.rs
  - crates/froe/src/writer/maintenance/journal/mod.rs
  - crates/froe/src/writer/maintenance/journal/scan.rs
  - crates/froe/src/writer/maintenance/mod.rs
  - crates/froe/src/writer/maintenance/planning/shape.rs
  - crates/froe/src/writer/maintenance/prepared.rs
  - crates/froe/src/writer/repository_lock/identity.rs
  - crates/froe/src/writer/repository_lock/publication.rs
  - crates/froe/src/writer/store_writer/file_identity.rs
  - crates/froe-export/src/refresh/assessment.rs
  - crates/froe-export/src/sqlite/target.rs
  - docs/plans/0004-merged-maintenance-command/ARCHITECTURE.md
status: done
merged_as: "f0f2d7c7839eb677611487a6a6ee6c64ddedd557"
---
# Restore The Windows Build

The first CI run after the push failed the `windows-build` job with 31 errors, nine in the library: every module split had left a child importing through `super::` with a `cfg(unix)` that no longer agreed with the item it reached. The release workflow's Windows binary could not have built. Both repairs landed and the safety case recorded the lesson: a local gate covering two of three platforms cannot stand in for the matrix, and fifty commits is too long to leave unpushed. Landed in `400cb47`, `c108f1f` and `f0f2d7c` (2026-08-18).

**Steps:**

1. Align every `cfg(unix)` import with the item it reaches across the split modules of the library.
2. Gate the froe-export test imports the Windows job rejected.
3. Record the break, its cause and the procedural lesson in the safety case's verification report.

- **Done when:** the `windows-build` job passes on the range and the safety case names the break. Met at `f0f2d7c`.
