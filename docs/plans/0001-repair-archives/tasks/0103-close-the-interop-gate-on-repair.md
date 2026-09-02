---
id: close-the-interop-gate-on-repair
title: Close The Interop Gate On Repair
workstream: "0001"
kind: task
depends_on: [keep-the-repair-building-on-windows]
gated: false
touches:
  - README.md
  - crates/froe-cli/tests/interop.rs
  - docs/cleanup.md
  - docs/cli-output.md
  - docs/interop.md
  - docs/plans/0001-repair-archives/ARCHITECTURE.md
status: done
merged_as: "eaf3308c8e4dbd418b584c332267558019b1ae5d"
---
# Close The Interop Gate On Repair

The evidence the safety case rests on most: a `repair` phase of the interoperability suite that kills Oak's own JVM with `SIGKILL` while it holds an archive open, asserts the container exited 137, confirms exactly one archive has no index, repairs it with froe, and boots a real Oak against the result, which serves the byte-identical baseline tree and logs none of its own repair messages. The safety case itself was written in the same commit. Landed in `eaf3308` (2026-08-14).

**Steps:**

1. Add the `repair` phase to `crates/froe-cli/tests/interop.rs`: SIGKILL under load, exit-status assertion, the index-less archive count, `froe cleanup --repair-archive-indexes` beside the default tasks, the boot and the baseline comparison.
2. Write the safety case: scope and retention, authoritative state, the mutation and publication table with interruption prefixes, the guards table with a named regression per guard, the interoperability record, and the known gaps (no neutralization evidence, no abrupt-exit harness, the `InvalidFormat` collapse, redundant scanning, the whole-run refusal).
3. Document the stage in `docs/cleanup.md`, `docs/cli-output.md`, `docs/interop.md` and the README.

- **Done when:** `interop_full` passes with the `repair` phase included and the safety case records the run, the Oak build under test and its gaps. Met at `eaf3308`.
