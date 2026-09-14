---
id: wire-the-property-reindex-phase-into-the-suite
title: Wire The Property Reindex Phase Into The Suite
workstream: "0007"
kind: task
depends_on: [prove-the-reindex-against-oak]
gated: false
touches:
  - docs/interop.md
  - crates/froe-cli/tests/interop/phase_recovery.rs
  - crates/froe-cli/tests/interop/main.rs
  - scripts/interop-fixture.sh
status: done
merged_as: "8f5faf3d7c90e425dc1b746ab937c43ef4b7a0fc"
---
# Wire The Property Reindex Phase Into The Suite

The command's own documentation landed with the command (task 0710). What remains is making the `property_reindex` phase part of the evidence anyone sees: the chain, the script's three phase lists, the run record and the suite's documentation.

**Steps:**

1. `interop_full` calls `property_reindex` after `index_inventory`; the run record gains lines stating what the phase proved — Oak's own rebuild as the oracle, the froe-side edits made to the copy's definitions before froe's rebuild (the bookkeeping reset), the canonical-index check and the attempt on which it passed, read from the `canonical-index-property.txt` task 0712's phase writes in the work root, as `oak-build.txt` is read, the sampled queries and plans, the convergence run — and names the pinned build.
2. `scripts/interop-fixture.sh` accepts the phase name in all three of its lists — the accept pattern, the error text and the header's `(phases: …)` usage block — as task 0612's invariant requires.
3. `docs/interop.md`: the `property_reindex` section describing the Oak-rebuild oracle, the probe, the judge's `CounterVectors` class task 0705 added, and what the phase does not prove (query semantics beyond the sampled statements; cost estimation, which the approximate counters influence), and the chain diagram updated, and `main.rs`'s numbered module-doc chain extended alongside them, so the script's three phase lists, the chain diagram, the module doc and `interop_full` name the same phases in the same order, as task 0612's invariant requires.

- **Done when:** `scripts/interop-fixture.sh property_reindex` runs the phase alone after `generate`, the whole chain reaches its completion sentinel with the phase included, the chain diagram, the script's three phase lists and `main.rs`'s module doc name it, and `git diff --check` is clean, and the stable host gate passes (`--all-features`, so the interop suite is compiled and linted).
