---
id: read-lucene-segment-descriptors
title: Read Lucene Segment Descriptors And Compound Files
workstream: "0008"
kind: task
depends_on: [write-the-transport-safety-case]
gated: false
touches:
  - docs/analysis/index-lucene-storage.md
  - crates/froe/src/index/lucene/codec_header.rs
  - crates/froe/src/index/lucene/segments.rs
  - crates/froe/src/index/lucene/compound.rs
  - crates/froe/src/index/lucene/check.rs
  - crates/froe/tests/lucene_segments_tests.rs
  - crates/froe/tests/fixtures/lucene-4-7-sample-index/README.md
  - crates/froe/tests/fixtures/lucene-4-7-sample-index/_0.cfs
  - crates/froe/tests/fixtures/lucene-4-7-sample-index/_0.cfe
  - crates/froe/tests/fixtures/lucene-4-7-sample-index/_0.si
  - crates/froe/tests/fixtures/lucene-4-7-sample-index/segments_1
  - crates/froe/tests/fixtures/lucene-4-7-sample-index/segments.gen
  - crates/froe/src/index/lucene/mod.rs
  - crates/froe/src/index/inventory.rs
  - crates/froe-cli/tests/interop/phase_index_inventory.rs
  - docs/index.md
  - docs/oak-segment-tar-feature-map.md
status: planned
merged_as: ""
---
# Read Lucene Segment Descriptors And Compound Files

Specify, then implement, from the Lucene 4.7.2 formats `oak-lucene` vendors at the pinned commit and exports as `4.7.2-oak2`, the readers for the index's table of contents: the codec header every one of these files opens with (magic `0x3fd76c17`, codec name, version range); `segments.gen` (its own format constant, then the commit generation written twice; the reading rule is that the commit generation is the maximum of the one derived from the directory listing — the highest `segments_N` suffix, `segments.gen` itself skipped — and the one in the file, which counts only when its two copies agree, the file being an optional best-effort fallback Oak deletes on any failure while writing it, so its absence is never a finding); `segments_N` (version, counter, the segment count as a 32-bit integer, then per segment the name, codec name, deletion generation, deletion count and — only from format version 46 upward — field-infos generation and update files, then user data and the trailing checksum); each segment's `.si` (Lucene version string, document count, compound flag, diagnostics, file set); and the compound file (the `.cfe`'s own header, naming `CompoundFileWriterEntries`, a vint count, then per entry the segment-stripped name — `.fdt`, `.tim`, never `_0.fdt` — an 8-byte offset and an 8-byte length, in unspecified order, so a lookup strips the segment name first and never mixes that namespace with the full names `segments_N` and `.si` carry), so the per-segment codec files inside the compound file (`.fnm`, `.fdx`, …) are readable; the `.si` always sits beside the `.cfs`, never inside it. The codec name is whatever the definition selected: `oakCodec` for a fulltext-enabled definition or an explicit `codec = oakCodec`, `Lucene46` for every other definition, `compressingCodec` under the `oak.lucene.compressing-codec` system property, or any other name the `META-INF/services` codec registration carries when `codec` names it; the reader accepts every registered name and reports it, and only the writers of plans 0009 and 0010 restrict themselves to `oakCodec`. On top of these, `check.rs` performs the structural check between oak-run's levels: every file the segments name exists in the directory listing and vice versa, allowing by name the `.del` and generation files the commit file itself lists and `segments.gen`, which the commit's own aggregate file set never names and which may legitimately be absent, every header validates, document and deletion counts are consistent, and the live document count is computed as Oak's own document count over a directory computes it. The prime directive puts the specification first, so the task opens by extending plan 0006's `index-lucene-storage.md` with a "Table of contents formats" section rather than reading bytes from memory.

**Steps:**

1. Extend `docs/analysis/index-lucene-storage.md` with the read-side specification of the five structures above, every field cited to the vendored file and method, the reader-side validation that rejects a mistake, and the codec-name rule; plan 0009's codec specification later cites this section for the write side rather than restating it.
2. `codec_header.rs`, `segments.rs`, `compound.rs` with typed errors naming file and offset (`check.rs` already exists, created by task 0611 for the blob pass; this task adds the structural report beside it); every size read from a file is validated before allocation; a hostile fixture whose `.cfe` entry name carries the segment prefix is refused; the readers work over any `Read + Seek`, so they serve both `OakDirectory` files and filesystem files.
3. Commit `crates/froe/tests/fixtures/lucene-4-7-sample-index/`: the five files of the judge's `sample-index` output (task 0614; a real single-segment `oakCodec` index of a few kilobytes — the fixture's own `_0.cfs` is 1.9 MB and is not committed) plus a README recording the generating command and the judge's `numdocs` figure; tests parse the directory, assert its codec name is `oakCodec`, and pin that document count.
4. Hand-crafted byte tests: a truncated header, a wrong magic, a `.cfe` entry past the end of `.cfs`, a `segments_N` naming a missing file, a checksum mismatch, a `segments_N` naming a codec outside the registered set (reported, not refused, since Oak would fail on it at open and the check exists to say so).
5. `check.rs`: `LuceneStructuralReport` with the findings above and the codec name, plus the document count for the inventory of plan 0006 (fill the `None` left there), and in the same commit the `index_inventory` phase's assertion that froe's Lucene entry count is absent (task 0615) turned into the equality assertion against the judge's `info` entry count, so the interop chain never goes red between this task and the phase tasks; `docs/index.md`'s `list` section says in the same commit that the Lucene document count is now shown and the feature map's `--index-info` row drops its qualifier, since a capability's documentation moves with it.

- **Done when:** the specification section exists and every reader statement in it cites a vendored file and method, the committed sample index parses to the document count the judge reported when it was captured, every hostile fixture yields its typed error without panic or unbounded allocation, and the stable host gate passes.
