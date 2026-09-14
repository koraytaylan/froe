---
id: assemble-a-segment-and-commit
title: Assemble A Segment, Its Compound File And The Commit
workstream: "0009"
kind: task
depends_on: [write-blocktree-terms, write-stored-fields, write-doc-values, write-norms]
gated: false
touches:
  - crates/froe/src/index/lucene/codec/field_infos.rs
  - crates/froe/src/index/lucene/codec/segment_info.rs
  - crates/froe/src/index/lucene/codec/compound.rs
  - crates/froe/tests/lucene_segment_assembly_tests.rs
status: done
merged_as: "d5e2b7f"
---
# Assemble A Segment, Its Compound File And The Commit

Implement the remaining formats and the assembly order: `.fnm` at format version 46 from the field table, the compound file over the per-segment files, then `.si` at format version 46 whose file set is exactly `_0.cfs`, `_0.cfe` and `_0.si` — creating the compound file replaces the set with the two compound names and the segment-info writer adds its own; a `.si` naming the inner files would make Oak's next open delete the compound files as unreferenced and Lucene's own index checker fail on the missing names — then `segments_1` and `segments.gen` naming the segment `_0` with codec `oakCodec`, no deletions, no user data, and a `counter` equal to the number of segment names consumed — 1 for the single `_0` segment, 0 only for the zero-segment commit — because Oak's next segment name is derived from it (`_` plus the counter in radix 36, the counter then incremented) and no reader validates it: with a `counter` of 0, Oak's first lane flush would name its segment `_0` and creating that file in the repository directory would overwrite `_0.cfs`, `_0.cfe` and `_0.si`, silently losing every froe-built document. The result is a directory whose file set is exactly what a fresh single-commit `oakCodec` index carries — `_0.cfs`, `_0.cfe`, `_0.si`, `segments_1`, `segments.gen`, the set the committed sample index of task 0802 holds; the fixture's own `lucene` index has two segments, a `.del` file and generation 2 after several async cycles — and which plan 0008's readers parse.

**Steps:**

1. `field_infos.rs`, `segment_info.rs`, `compound.rs`, and an `assemble_segment(directory_sink, fields, outputs: SegmentOutputs { document_count, per_format_files })` — the document count reaching `.si`, task 0906's `finish` and tasks 0907's and 0908's consumers as one agreed value, since Lucene's own index checker recomputes each format's own document count and refuses a segment whose `.si` count differs (`segment_info.rs` is where this task writes `.si`; the codec module root belongs to task 0902).
2. Tests: assemble a segment with one field of each capability and verify with plan 0008's readers that the file set, the document count, the codec name and every header agree and that `segments_1`'s `counter` is 1; a commit with zero segments — `segments_1` listing none, `segments.gen`, no `_0` files — which is what Oak persists for an empty index, since closing its index writer forces a commit and a writer that received no document flushes no segment, so the commit writer accepts an empty segment list.

- **Done when:** plan 0008's readers accept every assembled segment, the `.si` document count equals the assembled count and `segments_1` carries a `counter` of 1 with the zero-segment commit accepted, the file set equals the expected set, and the stable host gate passes.
