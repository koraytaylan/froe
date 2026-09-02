# Plan 0001 — Archive Index Repair — ✅ Done

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** ✅ Done. Landed between 2026-08-14 and 2026-08-14, before this directory adopted the Makina layout; reconstructed from history on 2026-09-02.
- **Goal:** let `froe cleanup --repair-archive-indexes` rebuild index-less active archives under the lock with every original letter retained under `.bak`, and prove against Oak that Oak consumes the rebuilt index rather than reconstructing one.
- **Root cause:** a writer killed mid-archive leaves a `.tar` without an index trailer; froe treated that as a refusal, so reclamation could never run on such a store.
- **Approach:** survey under the lock with the predicate the preview shares, stage, validate, install by hard link with `.bak` retirement, upgrade the manifest last; then the `repair` interop phase with Oak's JVM killed by `SIGKILL`, and the safety case.
- **Progress:** 3/3 tasks done; 0 blocked; 0 dropped.
- **Integration:** `done`; run —; base `develop` @ `21d310441345665a9e5870986d796c4acd631702`; validation base —; mode `manual, before Makina`; final integration `eaf3308c8e4dbd418b584c332267558019b1ae5d`.
- **Exceptions:** —.
- **Outcome:** the repair stage landed with its safety case and the `repair` interop phase; plan 0005 later made it a default-on question of `froe compact` with the same mechanism, ordering and guarantees.

_Last updated: 2026-09-02, against `develop` @ `1a88a72`._
