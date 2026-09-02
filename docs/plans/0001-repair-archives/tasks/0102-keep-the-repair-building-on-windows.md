---
id: keep-the-repair-building-on-windows
title: Keep The Repair Building On Windows
workstream: "0001"
kind: task
depends_on: [rebuild-index-less-archives]
gated: false
touches:
  - crates/froe/src/writer/cleanup.rs
  - crates/froe/src/writer/store_writer.rs
status: done
merged_as: "08e22c82b160a2668e907f1237b44d2986a7f592"
---
# Keep The Repair Building On Windows

The repair-target ownership preflight and its test helper reached for Unix-only file identity, and the Windows CI leg refused to compile the crate. Landed in `08e22c8` (2026-08-13).

**Steps:**

1. Gate the identity preflight on `cfg(unix)` and give Windows a metadata comparison that refuses the same substitutions.
2. Give the test helper the same gating so the Unix tests are unchanged and the Windows build has no dead reference.

- **Done when:** the workspace compiles for `x86_64-pc-windows-msvc` in CI with the repair stage present, and every Unix repair test still passes. Met at `08e22c8`.
