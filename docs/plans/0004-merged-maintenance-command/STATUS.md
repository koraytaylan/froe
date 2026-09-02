# Plan 0004 — Merged Maintenance Command — ✅ Done

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** ✅ Done. Landed between 2026-08-18 and 2026-08-18, before this directory adopted the Makina layout; reconstructed from history on 2026-09-02.
- **Goal:** make `froe compact` the one maintenance command whose every run leaves nothing reclaimable behind, and prove it with a content digest and an Oak that reads the result without repair.
- **Root cause:** the savings gate left partially dead archives unrewritten, two generations were retained where one plus an invariant suffices, and two write-path defects were invisible to structural checks.
- **Approach:** behaviour first (the merge, the fixes, the digest, the declared deltas), then the standards and the splits that keep the merged path readable, then the safety case with its adversarial review, then the repairs that review's CI run demanded.
- **Progress:** 9/9 tasks done; 0 blocked; 0 dropped.
- **Integration:** `done`; run —; base `develop` @ `52fa39deb314b8b3049f54f895f81ccec78751f6`; validation base —; mode `manual, before Makina`; final integration `f0f2d7c7839eb677611487a6a6ee6c64ddedd557`.
- **Exceptions:** —.
- **Outcome:** `v0.10.0` ships one maintenance command with unconditional reclamation, a content digest, a safety case reviewed adversarially, and an interop suite that holds every phase to a declared delta against a digest-pinned Sling image.

_Last updated: 2026-09-02, against `develop` @ `1a88a72`._
