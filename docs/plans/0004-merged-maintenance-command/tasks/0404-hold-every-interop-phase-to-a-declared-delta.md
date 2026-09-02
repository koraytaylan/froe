---
id: hold-every-interop-phase-to-a-declared-delta
title: Hold Every Interop Phase To A Declared Delta
workstream: "0004"
kind: task
depends_on: [add-froe-digest]
gated: false
touches:
  - README.md
  - crates/froe-cli/tests/interop.rs
  - docs/compact.md
  - docs/interop.md
status: done
merged_as: "3bcf26c8f0b96824bf0b9cac1e1835ec518bbb1a"
---
# Hold Every Interop Phase To A Declared Delta

Every mutating interop phase takes a `froe digest` before and after, and each line that differs must fall inside the delta the phase declares; the `reclaim` phase asserts a partially dead archive rewritten to its next letter with reconstructed trailers that Oak reads without repair, and the `journal_retention` phase runs a plain `froe compact` and requires every revision but the head gone. Landed in `3bcf26c` (2026-08-17).

**Steps:**

1. Add the declared-delta type and the before-and-after digest comparison to every mutating phase.
2. Rewrite `reclaim` for the savings-gate removal and `journal_retention` for the flag-less retirement.
3. State the discipline in `docs/interop.md` and `docs/compact.md`.

- **Done when:** `scripts/interop-fixture.sh` passes with every phase digest-held, and a phase whose delta exceeds its declaration fails. Met at `3bcf26c`.
