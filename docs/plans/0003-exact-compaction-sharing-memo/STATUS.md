# Plan 0003 — Exact Compaction Sharing Memo — ✅ Done

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** ✅ Done. Landed between 2026-08-14 and 2026-08-15, before this directory adopted the Makina layout; reconstructed from history on 2026-09-02.
- **Goal:** make `compacted_nodes` equal the number of distinct node records reachable from the head at any tree shape, and make every walk refuse a cycle exactly instead of capping depth.
- **Root cause:** an evicting memo carried an invariant, and depth caps stood in for cycle detection they could not perform while failing to bound the stack they claimed to bound.
- **Approach:** design first with the rule stated, then the interned packed table, then tests that pin exactness and cost, then the six walk rewrites landed together because compaction's bound and the verifier's were coupled through the head publication, then reconciliation of both plan documents.
- **Progress:** 5/5 tasks done; 0 blocked; 0 dropped.
- **Integration:** `done`; run —; base `develop` @ `7dbb1029a00096a64d55761ca7704f7aa193344a`; validation base —; mode `manual, before Makina`; final integration `aabf152c3eb1b08d1aada0a67f8d5108c3143be3`.
- **Exceptions:** —.
- **Outcome:** compaction copies each distinct node once with a memo costing 16 bytes a slot, every walk carries its own stack and refuses a self-referential graph at the closing record, and the verifier's count equals its certificate count by construction.

_Last updated: 2026-09-02, against `develop` @ `1a88a72`._
