# Scope — Plan 0003

> Replace the evicting compaction sharing memo with an exact one, so that each distinct node is copied exactly once at any tree shape, and remove every depth limit from every walk over records, replacing them with exact cycle detection and stacks held on the heap.

## Why this plan

Under the default 256 MiB memo, compacting an 18.8M-node repository copied more nodes than the head contained: a miss did not cost one duplicated subtree, it re-walked the subtree, and misses nested. The verifier had the same shape of defect and announced 56,389,743 nodes for a head holding 18,796,598. Copying each distinct node once is an invariant, and it was being enforced by a byte budget an operator had to guess. The six depth caps were a second instance of the same confusion: three stood in for cycle detection that did not exist, and none could bound the stack, so a legitimate 3000-deep tree aborted the process with `SIGABRT` through public API.

The plan was written as a design first (`01132c8`), stated the rule every walk must satisfy, and landed over 2026-08-15 with the memo, its tests, the depth-limit removal across six walks, and a reconciliation of both plan documents with what landed.

## In scope

- **The rule.** Three guarantees, three instruments: an exact set of records on the path for termination, a budget charged against the resource actually consumed for corrupt-but-acyclic graphs, and an explicit heap stack for the call stack. A memo may be evicted only when a miss changes running time and nothing else.
- **The exact memo.** `SegmentInterner` (segment identifier to `u32`) and `RewrittenNodes` (open addressing over two `Vec<u64>`, keys packed as segment index and record number, growth at 70 percent load, no eviction), 16 bytes a slot.
- **Exactness pinned.** Six named tests, a duplicate-key assertion and a postcondition that recounts occupancy, both verified by mutation; a five-angle adversarial exercise up to 4,020,101 distinct nodes.
- **No depth limits anywhere.** `compaction.rs`, `check.rs`, `backup.rs`, `traversal.rs`, `diff.rs` and `map.rs` all iterative with exact cycle detection; `a_tree_deeper_than_any_call_stack_copies_whole` on a 2 MiB stack.
- **Knob removal.** `--memo-budget-mb`, `compact_with_memo_budget` and `COMPACTION_MEMO_BYTES_PER_NODE` gone, with the reason no ceiling replaced them.

## Out of scope

- A ceiling on the depth-proportional walk state the removal left behind (about 175 bytes a level), and a residency-charged budget for `check.rs` and `backup.rs`; recorded as open.
- Whether Oak's compactor bounds recursion depth; unsettled and recorded as such.
- The shared-primitive weakness of every exactness oracle (`NodeState::child_node_entries`); recorded, not resolved.
