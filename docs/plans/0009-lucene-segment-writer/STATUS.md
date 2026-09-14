# Plan 0009 — Lucene Segment Writer — 📋 Planned

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** 📋 Planned.
- **Goal:** write Lucene 4.7.2 indexes in the `oakCodec` composition from typed, pre-tokenized documents, single segment, bounded memory, accepted by Lucene's own index checker and read back identically to Lucene's own output.
- **Root cause:** every earlier plan moves index data; none creates it, and creating it is what offline reindexing means for the Lucene indexes that dominate AEM.
- **Approach:** specify each file format from the vendored Lucene sources, closing with a go/no-go feasibility verdict as the last section of `docs/analysis/lucene-4-7-codec.md` on which the first implementation task is gated; implement the primitives first and pin them with vectors from the image's Lucene; build the formats bottom-up; assemble a segment; compare contents with a Lucene-built index through the judge.
- **Progress:** 0/13 tasks done; 0 blocked; 0 dropped.
- **Integration:** `planned`; run —; base `develop` @ `314b9c704fef73636d40f3e7ec5ff2c839aa1870` plus plans 0006–0008 merged; validation base —; mode —; final integration —.
- **Exceptions:** — (coordinator-owned blocked/dropped reasons are recorded here).
- **Outcome:** A dependency-free Rust writer for the Lucene 4.7.2 index format Oak selects as `oakCodec`, producing single-segment indexes that Lucene's own index checker accepts and reads back identically to indexes Lucene builds from the same documents.

_Last updated: 2026-09-12, against `develop` @ `314b9c7`._
