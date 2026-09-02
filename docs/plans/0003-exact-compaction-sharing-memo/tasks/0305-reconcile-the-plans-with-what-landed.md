---
id: reconcile-the-plans-with-what-landed
title: Reconcile The Plans With What Landed
workstream: "0003"
kind: task
depends_on: [drop-every-depth-limit]
gated: false
touches:
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
  - docs/plans/0003-exact-compaction-sharing-memo/ARCHITECTURE.md
status: done
merged_as: "aabf152c3eb1b08d1aada0a67f8d5108c3143be3"
---
# Reconcile The Plans With What Landed

Both plan documents were corrected against the code: the design's earlier claim that `check.rs` satisfied the eviction rule was retracted with the 56,389,743-node evidence, the per-node estimate was restated as a range between growths, the bounded-memory safety case gained the two store-proportional memos, the depth-proportional walk state and its measurement, and the interop re-verification after the walk rewrites was recorded. Landed in `aabf152` (2026-08-15).

**Steps:**

1. Re-read every measured figure against the tests that pin it and correct the overstated ones.
2. Add the open items: the depth-proportional term without a ceiling and the oracles' shared primitive.
3. Record the `interop_full` run after the rewrites as the load-bearing evidence that Oak reads the compacted store as written.

- **Done when:** no sentence in either document claims more than a named test or a recorded run supports. Met at `aabf152`.
