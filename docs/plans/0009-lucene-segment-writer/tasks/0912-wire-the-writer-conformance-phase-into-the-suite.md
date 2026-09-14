---
id: wire-the-writer-conformance-phase-into-the-suite
title: Wire The Writer Conformance Phase Into The Suite
workstream: "0009"
kind: task
depends_on: [prove-writer-conformance-with-lucene]
gated: false
touches:
  - docs/interop.md
  - crates/froe-cli/tests/interop/phase_recovery.rs
  - crates/froe-cli/tests/interop/main.rs
  - scripts/interop-fixture.sh
status: planned
merged_as: ""
---
# Wire The Writer Conformance Phase Into The Suite

The writer's documentation landed with the writer (task 0910). Make the `lucene_writer_conformance` phase part of the evidence anyone sees.

**Steps:**

1. `interop_full` calls the phase after `lucene_import`; the run record gains lines stating what enumeration equality proves and does not prove (contents, not bytes; single segment; no merges).
2. `scripts/interop-fixture.sh` accepts the phase name in all three of its lists — the accept pattern, the error text and the header's `(phases: …)` usage block — as task 0612's invariant requires.
3. `docs/interop.md`: the phase section, the judge's new classes, and the chain diagram updated, and `main.rs`'s numbered module-doc chain extended alongside them, so the script's three phase lists, the chain diagram, the module doc and `interop_full` name the same phases in the same order, as task 0612's invariant requires.

- **Done when:** the phase runs alone through the script after `generate`, the whole chain reaches its completion sentinel with it included, the chain diagram, the script's three phase lists and `main.rs`'s module doc name it, and `git diff --check` is clean, and the stable host gate passes (`--all-features`, so the interop suite is compiled and linted).
