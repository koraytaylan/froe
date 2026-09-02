---
id: size-the-sharing-memo-for-the-tree
title: Size The Sharing Memo For The Tree
workstream: "0002"
kind: task
depends_on: [reuse-records-and-share-bulk-segments]
gated: false
touches:
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/mutation.rs
  - crates/froe/src/writer/compaction.rs
  - crates/froe/src/writer/mod.rs
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
status: done
merged_as: "7dbb1029a00096a64d55761ca7704f7aa193344a"
---
# Size The Sharing Memo For The Tree

The evicting compaction memo's default budget starved on an 18.8M-node repository and the copied-node count climbed past the head's node count. As a stop-gap the memo became sizeable for the tree through `--memo-budget-mb` and `COMPACTION_MEMO_BYTES_PER_NODE`; plan 0003 replaced the budget with an exact memo the next day and removed both knobs. Landed in `7dbb102` (2026-08-14).

**Steps:**

1. Add `compact_with_memo_budget` and the CLI flag, and record the per-node cost in the safety case.
2. State in the safety case that the memo is the one deliberately store-proportional structure.

- **Done when:** a compaction with an explicit memo budget large enough for the tree copies each node once, and the safety case records the trade. Met at `7dbb102`; superseded by plan 0003.
