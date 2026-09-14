---
id: write-norms
title: Write Lucene42 Norms From The Default Similarity
workstream: "0009"
kind: task
depends_on: [implement-codec-primitives]
gated: false
touches:
  - crates/froe/src/index/lucene/codec/norms.rs
  - crates/froe/tests/lucene_norms_tests.rs
  - crates/froe/tests/fixtures/lucene-norm-vectors.tsv
status: planned
merged_as: ""
---
# Write Lucene42 Norms From The Default Similarity

Fields with norms — the analyzed fields Oak builds with norms: `:fulltext`, the relative-node `fullnode:<path>` fields aggregates produce, `:ancestors`, and the out-of-scope `sim:*` and `simtags`; not the norm-omitting analyzed fields Oak builds for `full:<name>` and `:spellcheck` — carry one byte per document: the field's boost on that document multiplied by the reciprocal square root of its term count, quantized to a byte by the small-float encoding, where the reciprocal square root is computed in double and narrowed to float once before the float boost multiplies it, and overlapping positions are discounted, so they do not count toward the term count. Implement the computation with exactly that mix of double and float steps and the `Lucene42` norms encoding into `.nvd`/`.nvm`.

**Steps:**

1. `norms.rs`: `norm_byte(boost: f32, term_count: u32) -> u8` with the double reciprocal square root narrowed once, as the Java does it, and `NormsConsumer::new(directory_sink, document_count)` taking the segment's document count as task 0907's consumer does, because the norms writer asserts its input is exactly that long and the reader reads exactly that many bytes back: the stream spans every document of the segment with the byte `0` for one that does not carry the field, which is the value Lucene's own norms accumulation pads a missing document with, so a stream holding only the documents that carried the field would read garbage from the following bytes. The consumer works over a `SortedPasses` from task 0702's `into_sorted_passes()` for a field whose norms spilled and from its `from_sorted_records` in the vector tests, writing the `Lucene42` `UNCOMPRESSED` format — the only one a norms field reaches, since that format asks packed integers for the fastest decode and a norm byte needs all eight bits — at norms version 1 and closing `.nvm` with the `-1` vint end-of-fields marker, the vectors pinning that branch.
2. Vectors: a `jshell` session on the pinned image computes the norm Lucene's default similarity produces for a table of boosts and lengths against the image's own `oak-lucene` jar, including lengths at the quantization boundaries where a pure-float port would differ and a length of 0 with and without a boost (a norms-bearing field whose analyzed value yields no token: `1.0 / sqrt(0)` is infinite and the small-float encoding returns `-1`, byte `0xFF`, rather than saturating); commit them as `crates/froe/tests/fixtures/lucene-norm-vectors.tsv` with the generating command in the header and replay them. Beside them, unit tests from hand-computed `.nvd` and `.nvm` bytes, as task 0907's do for doc values: a field every document carries and one several documents lack, each asserting a `.nvd` stream exactly `document_count` bytes long with `0` for every absent document, the `UNCOMPRESSED` format byte at norms version 1, and the `-1` vint end-of-fields marker.

- **Done when:** every vector matches, the hand-computed `.nvd` and `.nvm` bytes match including the `document_count`-long stream with `0` for an absent document and the `-1` marker, and the stable host gate passes.
