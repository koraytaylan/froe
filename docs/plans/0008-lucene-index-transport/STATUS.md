# Plan 0008 — Lucene Index Transport — 📋 Planned

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** 📋 Planned.
- **Goal:** `froe index dump` and `froe index import` move Lucene index data between the repository and oak-run's filesystem layouts without a JVM, `froe index check` reads segment-level structure, and an index built out of band by Oak's own editors installs into a stopped store that Oak then boots and queries through.
- **Root cause:** oak-run's `--index-import` is the step that touches the production store, and it is pure segment-store work that today still needs the whole Oak runtime; froe has no Lucene file readers at all.
- **Approach:** readers for `segments.gen`, `segments_N`, `.si` and compound files from the vendored Lucene sources; a streaming `:data` writer in the encoding Oak 1.90.0 writes; a state-identity rule in place of oak-run's live bring-up-to-date; the compaction discipline for lock, plan, verify and publish; interop phases whose oracles are Oak's own dumper (byte identity), Lucene's own index checker, Oak's own index consistency check at level 2 and a live Oak answering fulltext queries through the imported index.
- **Progress:** 0/11 tasks done; 0 blocked; 0 dropped.
- **Integration:** `planned`; run —; base `develop` @ `314b9c704fef73636d40f3e7ec5ff2c839aa1870` plus plans 0006 and 0007 merged; validation base —; mode —; final integration —.
- **Exceptions:** — (coordinator-owned blocked/dropped reasons are recorded here).
- **Outcome:** froe reads Lucene index data out of `:data` (`froe index dump`, byte-identical to Oak's dumper), checks it, and imports an out-of-band index built by Oak's own editors back into a stopped store (`froe index import`) so that Oak boots, queries through it, and logs no reindex.

_Last updated: 2026-09-12, against `develop` @ `314b9c7`._
