---
id: design-the-exact-memo
title: Design The Exact Memo
workstream: "0003"
kind: task
depends_on: []
gated: false
touches:
  - docs/plans/0003-exact-compaction-sharing-memo/ARCHITECTURE.md
status: done
merged_as: "4f89a9cc5115cf9c11a0d694018fb5201f09f2bc"
---
# Design The Exact Memo

The design was written before the code: what the evicting memo carried, the measured cliff (557,024 copies at three memo entries against 464 at four), the interned and packed table, the measured rejection of a plain `HashMap`, and the walk-guard rule every walk over records has to satisfy. Landed in `01132c8` and `4f89a9c` (2026-08-14 and 2026-08-15).

**Steps:**

1. Measure the blowup on a 14-level diamond chain and record where the cliff sits and why filler nodes do not move it.
2. State the rule: three guarantees, three instruments, and the three conditions under which a memo may be evicted at all.
3. Record why no ceiling replaces the budget: the figure is a distinct-set cardinality unavailable before the walk, and a ceiling could only fire mid-copy.

- **Done when:** the design names the invariant, the data structure, the measured alternatives and the rule, so that the implementation task has nothing left to decide. Met at `4f89a9c`.
