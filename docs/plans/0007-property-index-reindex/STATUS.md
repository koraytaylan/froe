# Plan 0007 — Property Index Reindex — 🚧 In progress

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** 🚧 In progress.
- **Goal:** `froe index reindex` rebuilds property, unique, node-type, reference and counter indexes offline from the state Oak's own editors would index, in one head move under the lock, with a safety case, fault coverage and Oak-side proof.
- **Root cause:** Oak rebuilds a flagged property index synchronously inside the first commit after startup and has no offline path that writes the result back; on large stores that blocks AEM for hours.
- **Approach:** safety case first; bounded external sort and a streaming trie writer; Oak's exact key derivation and bookkeeping from plan 0006's specification; one compare-and-set; fresh-reopen verification; an interop phase whose oracle is Oak's own reindex of the same store, compared with only the randomized approximate counters excused; frozen adversarial review before the beta framing is lifted.
- **Progress:** 1/15 tasks done; 0 blocked; 0 dropped.
- **Integration:** `planned`; run —; base `develop` @ `314b9c704fef73636d40f3e7ec5ff2c839aa1870` plus plan 0006 merged; validation base —; mode —; final integration —.
- **Exceptions:** — (coordinator-owned blocked/dropped reasons are recorded here).
- **Outcome:** `froe index reindex` rebuilds property, unique, node-type, reference and counter indexes offline, from the state Oak's own editors would index, with one head move, a safety case, fault coverage, and an interop phase proving the rebuilt indexes agree with Oak's own reindex and serve Oak's queries.

_Last updated: 2026-09-14, against `develop` @ `bb940da`._
