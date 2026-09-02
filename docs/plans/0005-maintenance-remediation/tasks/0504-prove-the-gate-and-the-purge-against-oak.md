---
id: prove-the-gate-and-the-purge-against-oak
title: Prove The Gate And The Purge Against Oak
workstream: "0005"
kind: task
depends_on: [plan-once-gate-no-op-copies-and-purge-orphaned-histories]
gated: false
touches:
  - crates/froe-cli/tests/interop/digest.rs
  - crates/froe-cli/tests/interop/fixtures.rs
  - crates/froe-cli/tests/interop/phase_maintenance.rs
  - crates/froe-cli/tests/interop/phase_recovery.rs
  - crates/froe-cli/tests/interop/podman.rs
  - crates/froe-cli/tests/interop/sling.rs
  - crates/froe-cli/tests/interop/store.rs
status: done
merged_as: "bc913f2dbedfe1438000635eab4d6fdc3ad290f9"
---
# Prove The Gate And The Purge Against Oak

Oak 1.90.0 writes versionable content and deletes some versionables; froe purges and compacts; Oak boots the result, serves the surviving tree, resolves version history for surviving versionables, and the before-and-after digests match modulo exactly the confirmed exclusions. A second phase runs the convergence gate and requires `nothing to do` with no mutation. Landed in `bc913f2` (2026-08-19).

**Steps:**

1. Extend the fixture with versionables, deletions and large binaries so purged histories release shared bulk segments.
2. Add the purge and the convergence phases with plan-derived digest exclusions threaded through the harness.
3. Assert Oak resolves the surviving histories and logs no repair.

- **Done when:** `scripts/interop-fixture.sh` passes with both phases, the store measurably shrinks by at least the released-bulk figure, and Oak serves the purged store. Met at `bc913f2`.
