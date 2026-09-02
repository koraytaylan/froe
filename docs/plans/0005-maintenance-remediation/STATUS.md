# Plan 0005 — Maintenance Remediation — ✅ Done

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** ✅ Done. Landed between 2026-08-19 and 2026-09-01, before this directory adopted the Makina layout; reconstructed from history on 2026-09-02.
- **Goal:** turn each of the ten field observations from explained into fixed, with acceptance criteria phrased as observable behaviour against a fixture that models the field store's shape.
- **Root cause:** the merged command reported nothing it computed, planned twice, verified where it could not protect, copied when there was nothing to copy, and had no notion of the semantic garbage the operator cared about.
- **Approach:** cheap observability first, then the pipeline simplification, then the convergence gate, then the one content-mutating feature on top of the cleaned-up structure, then Oak evidence and a runbook.
- **Progress:** 8/8 tasks done; 0 blocked; 0 dropped.
- **Integration:** `done`; run —; base `develop` @ `85edf836b9db15c5713392896db858f4909e2eeb`; validation base —; mode `manual, before Makina`; final integration `1a88a72b019e5370514026c4412bbf09edcb5287`.
- **Exceptions:** —.
- **Outcome:** `v0.11.0` says what it knows, plans once under the lock, verifies the copy before publishing it, reports `nothing to do` on an already-compact store, purges orphaned version histories as a confirmed default-on question, and proves the gate and the purge against a real Oak with digest exclusions derived from the confirmed plan.

_Last updated: 2026-09-02, against `develop` @ `1a88a72`._
