---
id: write-the-safety-case
title: Write The Safety Case
workstream: "0002"
kind: task
depends_on: [let-reclamation-proceed-past-dead-survivors]
gated: false
touches:
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
  - crates/froe/src/writer/store_writer.rs
  - docs/interop.md
  - crates/froe-export/src/refresh.rs
status: done
merged_as: "fe84b38e70a49174df1e07a3ff58fad41808cc91"
---
# Write The Safety Case

The safety case for the range under `docs/high-risk-changes.md`: scope and retention, the mutation table with the new reopen boundary, the guards table with disabled-guard runs and their observed failing results, the resources section listing every budget, and the verification report whose rows are commands and exit statuses. Two documentation defects the report found on the way (`484c893`, `83a3ad0`) landed before it. Landed in `fe84b38` (2026-08-14).

**Steps:**

1. Neutralize each guard serially against an isolated target, record the observed failing result verbatim, restore the pristine source and rerun the original test.
2. Run the host gate, MSRV gate, i686 width sentinel and `interop_full`, and bind each claim to a command and its exit status.
3. Name the known gaps: no neutralization evidence for record reuse and value sharing, the loosened check reaching the default path, no RSS measurement, macOS untested, the reopen boundary without a cutpoint, the source-shape guard's blind spots.

- **Done when:** the document carries every section the guide names, a neutralization row per guard with its failing output, and a verification table whose every row was executed. Met at `fe84b38`.
