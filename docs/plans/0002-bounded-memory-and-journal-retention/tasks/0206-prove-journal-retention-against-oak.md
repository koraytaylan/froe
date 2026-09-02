---
id: prove-journal-retention-against-oak
title: Prove Journal Retention Against Oak
workstream: "0002"
kind: task
depends_on: [arm-the-loosened-refusal-and-count-resolving-revisions]
gated: false
touches:
  - crates/froe-cli/tests/interop.rs
  - docs/interop.md
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
status: done
merged_as: "2e8179a884ed189db4e2a5de3423809fe10c0cb0"
---
# Prove Journal Retention Against Oak

`--retain-journal-revisions` is the one operation in the range that destroys reachable history by policy rather than by Oak's generation predicate, so froe agreeing with its own reachability rules proved nothing about it. An interop phase was added: Oak's own fixture carries three revisions, froe bounds the journal to one and sweeps the segments behind the other two, and Sling boots the result and serves the exact baseline tree from the single revision froe kept. Landed in `6af6116` and `2e8179a` (2026-08-14).

**Steps:**

1. Add the `journal_retention` phase to the interop suite with the revision count, the backup name and the served-tree assertion.
2. Document the phase in `docs/interop.md` and close the gap in the safety case's interoperability section.

- **Done when:** `interop_full` passes with the retention phase and the safety case's interoperability section names the run. Met at `2e8179a`.
