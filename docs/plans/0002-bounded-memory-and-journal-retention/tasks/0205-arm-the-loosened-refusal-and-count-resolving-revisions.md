---
id: arm-the-loosened-refusal-and-count-resolving-revisions
title: Arm The Loosened Refusal And Count Resolving Revisions
workstream: "0002"
kind: task
depends_on: [write-the-safety-case]
gated: false
touches:
  - crates/froe/src/writer/cleanup.rs
  - crates/froe/src/writer/compaction.rs
  - docs/cleanup.md
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
status: done
merged_as: "f29a9f135cdf0adc19a12147105eb9334e02135d"
---
# Arm The Loosened Refusal And Count Resolving Revisions

Writing the safety case exposed two defects: the loosened survivor refusal had no regression that failed when the stricter check returned, and the retention bound counted every journal line whose segment existed rather than every revision that resolved, so a readable revision could be retired to make room for an unreadable one. Both were fixed, and the case's independent-review gap was restated as the exception it is. Landed in `cca15be`, `d640efe` and `f29a9f1` (2026-08-14).

**Steps:**

1. Arm `a_dead_survivor_pointing_at_a_removed_segment_is_handled` so restoring `|| reclaimable.contains(&identifier)` fails it with the `InvalidFormat` the case quotes.
2. Make `journal_retention_boundary` count only revisions that resolve, pinned by `a_bound_counts_only_revisions_that_actually_resolve`.
3. Record the independent-review exception in the safety case rather than leaving it as a gap.

- **Done when:** both regressions fail under their neutralization and pass restored, and the case's review section states the exception. Met at `f29a9f1`.
