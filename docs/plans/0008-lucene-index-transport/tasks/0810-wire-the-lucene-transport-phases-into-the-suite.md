---
id: wire-the-lucene-transport-phases-into-the-suite
title: Wire The Lucene Transport Phases Into The Suite
workstream: "0008"
kind: task
depends_on: [prove-the-import-against-oak]
gated: false
touches:
  - docs/interop.md
  - crates/froe-cli/tests/interop/phase_recovery.rs
  - crates/froe-cli/tests/interop/main.rs
  - scripts/interop-fixture.sh
status: done
merged_as: "b80db37, 44d6a61, 3de6338"
---
# Wire The Lucene Transport Phases Into The Suite

The commands' documentation landed with the commands (tasks 0803 and 0807). Make the `lucene_dump` and `lucene_import` phases part of the evidence anyone sees.

**Steps:**

1. `interop_full` calls the two phases after `property_reindex`; the run record gains lines stating what each proved, the froe-side edits made to the copies before the imports under test (hidden children removed, `corrupt` forged as a `DATE`, `async` removed), and names the judge's new subcommands as Oak-side oracles on the pinned build.
2. `scripts/interop-fixture.sh` accepts the two phase names in all three of its lists — the accept pattern, the error text and the header's `(phases: …)` usage block.
3. `docs/interop.md`: the two phase sections, the judge's new subcommands, and the chain diagram updated, and `main.rs`'s numbered module-doc chain extended alongside them, so the script's three phase lists, the chain diagram, the module doc and `interop_full` name the same phases in the same order, as task 0612's invariant requires.

- **Done when:** each phase runs alone through the script after `generate`, the whole chain reaches its completion sentinel with both included, the chain diagram, the script's three phase lists and `main.rs`'s module doc name them, and `git diff --check` is clean, and the stable host gate passes (`--all-features`, so the interop suite is compiled and linted).
