---
id: wire-the-index-phases-into-the-suite
title: Wire The Index Phases Into The Suite, Workflow And Run Record
workstream: "0006"
kind: task
depends_on: [add-the-index-inventory-phase]
gated: false
touches:
  - crates/froe-cli/tests/interop/phase_recovery.rs
  - crates/froe-cli/tests/interop/main.rs
  - scripts/interop-fixture.sh
  - .github/workflows/interop.yml
  - docs/interop.md
status: planned
merged_as: ""
---
# Wire The Index Phases Into The Suite, Workflow And Run Record

A phase that exists but is not in the chain, the script's phase list, the run record and the workflow's path filter is not evidence anyone will see. Wire `judge_smoke` and `index_inventory` in, and repair two latent defects found while planning: `.github/workflows/interop.yml` still filters pushes on `crates/froe-cli/tests/interop.rs` and `crates/froe/src/store.rs`, paths that stopped existing when the suite and the store module were split into directories, so a change to the interop suite itself no longer triggers the push-filtered run; and the script's phase lists and the chain diagram in `docs/interop.md` have already drifted from `interop_full` (the script holds three lists: its accept pattern and its error text each lack `compact_convergence` and `version_history_purge`, while its header's usage block lacks those two plus `journal_retention` and `repair`; the diagram lacks `compact_tail`, `checkpoint_removal` and `cleanup`), so adding two names to each would not make them true.

**Steps:**

1. `interop_full` calls the two phases after `read` and before `commit` — load-bearing, because froe's direct commits run none of Oak's index editors, so after `commit` the fixture's synchronous `jcr:title` index legitimately lacks an entry and `froe index check` would report it missing; the phase's own position assertion belongs to task 0615, which registers the phase; the run record gains lines describing what each proved and names the judge as an Oak-side oracle running on the pinned build.
2. `scripts/interop-fixture.sh` accepts every phase `interop_full` runs, the two new ones included, and nothing it does not, with its error text and its header's `(phases: …)` usage block reconciled against the same list — all three, since the header's is shorter than the accept pattern — and its header's stale `crates/froe-cli/tests/interop.rs` path corrected.
3. `interop.yml` path filter: `crates/froe/src/**`, `crates/froe-cli/**`, `scripts/interop-fixture.sh`, the workflow itself — and the workflow's header comment, which still names `crates/froe-cli/tests/interop.rs` and describes the trigger as the write path, the suite or the workflow, reworded to the broadened scope (the library, the command-line crate, the script and the workflow).
4. `docs/interop.md`: the dependency chain diagram reconciled against `interop_full`, a section per phase, the `generate` section updated for the shapes 0616 adds, the numbered dependency chain in `crates/froe-cli/tests/interop/main.rs`'s module doc reconciled against `interop_full` (it still lists eight phases where the suite runs fourteen), a section on the judge (what it is, why it is not a second image, which Oak modules the image ships and which it does not, what it can and cannot stand in for), and the stale `crates/froe-cli/tests/interop.rs` references replaced by the directory. The CI section's push bullet is reworded to the broadened path scope.

- **Done when:** `scripts/interop-fixture.sh` runs the whole chain to its completion sentinel with the two phases included, `scripts/interop-fixture.sh index_inventory` runs the phase alone after `generate`, the script's three phase lists, the chain diagram, `main.rs`'s module doc and `interop_full` name the same phases in the same order, the workflow's `paths` list contains no path absent from the tree, and `docs/interop.md` names every phase the chain runs, and the stable host gate passes (`--all-features`, so the interop suite is compiled and linted).
