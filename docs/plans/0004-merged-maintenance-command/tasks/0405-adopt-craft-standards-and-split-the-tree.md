---
id: adopt-craft-standards-and-split-the-tree
title: Adopt Craft Standards And Split The Tree
workstream: "0004"
kind: task
depends_on: [hold-every-interop-phase-to-a-declared-delta]
gated: false
touches:
  - CONTRIBUTING.md
  - Cargo.toml
  - clippy.toml
  - .github/workflows/ci.yml
  - scripts/oversized-files.sh
  - README.md
  - docs/compact.md
  - docs/interop.md
  - docs/oak-segment-tar-feature-map.md
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
  - crates/froe/src
  - crates/froe/tests
  - crates/froe-cli/src
  - crates/froe-cli/tests
  - crates/froe-export/src
  - crates/froe-export/tests/export_tests.rs
status: done
merged_as: "5904c9ca16b0671c58caa14ae15c9791dc853362"
---
# Adopt Craft Standards And Split The Tree

`CONTRIBUTING.md` adopted the clean-code craft standards, `clippy.toml` bounded a function's branching and nesting, `allow(clippy::too_many_lines)` was forbidden and `scripts/oversized-files.sh` became a CI gate; then about forty-five refactor commits split every file over a thousand lines and every function over a hundred into module directories (`maintenance/`, `store_writer/`, `compaction/`, `fault_injection/`, `content/`, `tooling/`, the interop suite into its phases, the export crate into its stages), gave Java's semantics one home under `crates/froe/src/java/`, and reused bounded caches to skip store-proportional rework. Landed as `4e354b9..5904c9c` (2026-08-18); `merged_as` names the last commit.

**Steps:**

1. Write the standards and the gates first so every split is judged by them.
2. Split by responsibility, moving each test beside the stage it exercises, one commit per file or path so the history stays readable.
3. Close the last duplications and make the file limit a gate in CI (`2d683ec`), then fix the MSRV clippy rejection (`d3991b2`) and the stale command description (`5904c9c`).

- **Done when:** `scripts/oversized-files.sh` and both clippy legs pass on the whole workspace, and every test that moved still runs. Met at `5904c9c`.
