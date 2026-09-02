---
id: write-the-safety-case
title: Write The Safety Case
workstream: "0004"
kind: task
depends_on: [close-the-sigpipe-fixture-race]
gated: false
touches:
  - docs/compact.md
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
  - docs/plans/0004-merged-maintenance-command/ARCHITECTURE.md
status: done
merged_as: "7b491decd63f4020452231233a1250465781eebd"
---
# Write The Safety Case

The safety case for the range: scope and retention with the journal inversion stated, authoritative state, the nine-row mutation table, the guards with neutralization results, the fault cutpoints, resources with the headroom requirement, the interoperability record against the digest-pinned image, a verification report of twelve executed commands, nine known gaps, and the adversarial review with its four lenses and its stated non-coverage. The bounded-memory case gained its superseded-note. Landed in `7b491de` (2026-08-18).

**Steps:**

1. Neutralize the reclamation-completeness guard, the bulk-sharing rule and the arity computation and quote each failing result.
2. Run the full matrix on stable and the MSRV, the i686 width sentinel, the file-size gate and `scripts/interop-fixture.sh`, and bind every claim to an exit status.
3. Perform the adversarial pass over the range with a clean worktree and record what it did and did not cover.

- **Done when:** the document carries every section the guide names, the review section names its lenses and its weakness, and every verification row was executed. Met at `7b491de`.
