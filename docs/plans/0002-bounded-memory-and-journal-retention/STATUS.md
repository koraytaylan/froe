# Plan 0002 — Bounded Memory and Journal Retention — ✅ Done

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** ✅ Done. Landed between 2026-08-14 and 2026-08-14, before this directory adopted the Makina layout; reconstructed from history on 2026-09-02.
- **Goal:** bound every cache and memo by configuration, certify a write session by CRC, and add a policy-based journal retention bound with the evidence a destructive option needs.
- **Root cause:** caches, session state and walk state all grew with the repository, and the only reclamation of reachable history froe had was none at all.
- **Approach:** budgets first with tests that assert residency rather than results, then the session rewrite, then the retention bound and its refusals, then the safety case with disabled-guard runs, then the Oak phase for the one operation that destroys reachable history by policy.
- **Progress:** 9/9 tasks done; 0 blocked; 0 dropped.
- **Integration:** `done`; run —; base `develop` @ `bdcbe55b8dd9efd7e5fc158609a00ff3fd014873`; validation base —; mode `manual, before Makina`; final integration `7dbb1029a00096a64d55761ca7704f7aa193344a`.
- **Exceptions:** —.
- **Outcome:** every cache is a function of configuration, the session certificate proves payload identity by recorded CRC, `--retain-journal-revisions` shipped in `v0.8.0` with an Oak-proved phase, and the writer stopped rewriting identical records and re-copying binary content.

_Last updated: 2026-09-02, against `develop` @ `1a88a72`._
