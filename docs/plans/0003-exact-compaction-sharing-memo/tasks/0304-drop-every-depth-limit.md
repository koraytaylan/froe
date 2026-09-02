---
id: drop-every-depth-limit
title: Drop Every Depth Limit
workstream: "0003"
kind: task
depends_on: [pin-the-copy-once-invariant]
gated: false
touches:
  - crates/froe/src/writer/compaction.rs
  - crates/froe/src/tooling/check.rs
  - crates/froe/src/writer/backup.rs
  - crates/froe/src/content/traversal.rs
  - crates/froe/src/content/map.rs
  - crates/froe/src/tooling/diff.rs
status: done
merged_as: "cc06760a794d09b3a6cd58f6813f4fdc85b5ee39"
---
# Drop Every Depth Limit

All six walks over records were rewritten to carry their own stack on the heap with exact cycle detection, and `MAXIMUM_COMPACTION_DEPTH`, `MAXIMUM_CHECK_DEPTH`, `MAXIMUM_RECOVERY_DEPTH`, `MAXIMUM_TRAVERSAL_DEPTH`, `MAXIMUM_DIFF_DEPTH` and the map walk's `depth >= 64` were removed. Two fixes fell out: `backup.rs` inserted into its memo on entry rather than completion, and `check.rs` carried a subtree height that existed only to gate a memo hit against the cap. The per-level cost was measured. Landed in `08d717b`, `05b268b`, `2d48be3`, `c1bbb37`, `8566a44` and `cc06760` (2026-08-15).

**Steps:**

1. Rewrite compaction, the verifier and the recovery walk as iterative walks with exact `ancestors` or `nodes_on_path` sets.
2. Replace the traversal's depth cap with an exact path set; give the diff an exact set over the record pair; let the map walk rely on `visited` alone.
3. Keep `MAXIMUM_DIFF_VISITS` and `MAXIMUM_TRAVERSAL_NODES`, which charge against work done, and the caller-requested `descent_limit` and `--depth`.
4. Add `a_tree_deeper_than_any_call_stack_copies_whole` on a 2 MiB stack and `measure_deep_chain_walk_footprint`.

- **Done when:** a 100,000-level tree copies and verifies on a 2 MiB stack, every walk refuses a self-referential graph at the closing record, and no depth constant remains. Met at `cc06760`.
