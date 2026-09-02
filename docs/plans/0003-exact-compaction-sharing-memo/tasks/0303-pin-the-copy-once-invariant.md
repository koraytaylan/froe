---
id: pin-the-copy-once-invariant
title: Pin The Copy-Once Invariant
workstream: "0003"
kind: task
depends_on: [make-the-memo-exact]
gated: false
touches:
  - crates/froe/src/writer/compaction.rs
  - docs/plans/0003-exact-compaction-sharing-memo/ARCHITECTURE.md
status: done
merged_as: "4d25e4744e06faa9efe325135d124430883806c5"
---
# Pin The Copy-Once Invariant

The invariant was made checked rather than argued: six named tests including 200 generated DAGs cross-checked against an independent walk, a footprint test pinned as table occupancy rather than RSS, and two guards on every real copy, a duplicate-key assertion and a postcondition that recounts occupancy (comparing against `len` had compared a counter with itself). Copy throughput was measured, and a stack defect found on the way was recorded for the next task. Landed in `cc52c72`, `41b14e6` and `4d25e47` (2026-08-15).

**Steps:**

1. Add `a_deep_copy_copies_each_distinct_node_exactly_once`, `a_shared_subtree_is_copied_once_however_deep_the_sharing_nests`, `the_exact_memo_costs_a_bounded_number_of_bytes_a_node`, `a_cyclic_source_is_refused_at_the_record_that_closes_the_cycle`, `every_random_shape_copies_each_distinct_node_exactly_once` and `the_memo_and_the_interner_hold_their_own_invariants`.
2. Verify both runtime guards by mutation: dropping one entry on rehash and making the memo miss one key in three each fail the suite.
3. Record the stack defect: 4000 levels of the compaction walk need about 2.8 MiB against the 2 MiB a spawned thread receives.

- **Done when:** every listed test passes, both mutations fail the suite with the duplicate-key assertion firing first, and the design document records the measured cost table. Met at `4d25e47`.
