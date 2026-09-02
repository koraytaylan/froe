---
id: make-the-memo-exact
title: Make The Memo Exact
workstream: "0003"
kind: task
depends_on: [design-the-exact-memo]
gated: false
touches:
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/mutation.rs
  - crates/froe/src/writer/compaction.rs
  - crates/froe/src/writer/mod.rs
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
  - docs/plans/0003-exact-compaction-sharing-memo/ARCHITECTURE.md
status: done
merged_as: "4d29d99ee9905be445a55c608fba1110f6504df0"
---
# Make The Memo Exact

`RewrittenNodes` and `SegmentInterner` replaced the `BoundedCache`; `nodes_on_path` refuses a repeat as `node record {…} is contained in its own subtree` at the closing record; `--memo-budget-mb`, `compact_with_memo_budget` and `COMPACTION_MEMO_BYTES_PER_NODE` were removed as a compile break rather than a silent behavioural one. Landed in `4d29d99` (2026-08-15).

**Steps:**

1. Implement the interner (index 0 never issued) and the open-addressed table with power-of-two capacity, growth at 70 percent load and no eviction.
2. Test the path set before the memo probe and clear it before the memo insert.
3. Remove the budget knob from the library and the CLI, and record the trade in both plan documents.

- **Done when:** `compacted_nodes` equals the number of distinct node records reachable from the head on the nested-diamond fixtures, and the knobs no longer compile. Met at `4d29d99`.
