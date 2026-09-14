---
id: implement-codec-primitives
title: Implement Lucene's Output Primitives And Packed Integers
workstream: "0009"
kind: task
depends_on: [specify-the-lucene-4-7-codec]
gated: true
touches:
  - crates/froe/src/index/lucene/codec/mod.rs
  - crates/froe/src/index/lucene/codec/data_output.rs
  - crates/froe/src/index/lucene/codec/packed.rs
  - crates/froe/tests/lucene_codec_primitive_tests.rs
  - crates/froe/tests/fixtures/lucene-4-7-primitive-vectors.tsv
  - crates/froe-cli/tests/interop/judge/CodecVectors.java
  - crates/froe/src/index/lucene/mod.rs
  - crates/froe/src/index/lucene/codec/fst.rs
  - crates/froe/src/index/lucene/codec/postings.rs
  - crates/froe/src/index/lucene/codec/terms.rs
  - crates/froe/src/index/lucene/codec/stored_fields.rs
  - crates/froe/src/index/lucene/codec/doc_values.rs
  - crates/froe/src/index/lucene/codec/norms.rs
  - crates/froe/src/index/lucene/codec/field_infos.rs
  - crates/froe/src/index/lucene/codec/segment_info.rs
  - crates/froe/src/index/lucene/codec/compound.rs
status: planned
merged_as: ""
---
# Implement Lucene's Output Primitives And Packed Integers

Gated on the maintainer's acceptance of the feasibility verdict. Implement the output encodings (`vint`, `vlong`, `string`, string map, string set), the codec header, and the packed-integer family (a `PACKED` writer for any bits-per-value, the block-packed writer, the monotonic block-packed writer), each pinned by vectors the judge produces with the image's own Lucene.

**Steps:**

1. Add `judge/CodecVectors.java` with `vectors` (no argument: it prints, the fixture header recording the redirection, as `counter-vectors` does): writes, through an in-memory Lucene directory and its own output stream, the encodings of a fixed input set for every primitive (boundary values for `vint`/`vlong`, strings with multi-byte characters, packed blocks at every bits-per-value from 1 to 64, block-packed blocks exercising the header a block-packed stream carries — the `bitsRequired << 1` token with its `MIN_VALUE_EQUALS_0` bit and the optional vlong holding the zigzag-encoded minimum less one — with a large positive minimum (epoch-millisecond dates), a negative minimum, an all-equal block (`bitsRequired == 0`) and a 64-bit delta that forces the minimum to 0, monotonic blocks: a perfectly linear one whose zigzag width is 0 and which therefore carries no packed data at all — the common case for an address stream — one whose deltas need bits, and a single-value block whose average is `0f`), one case per line as `<primitive>\t<input>\t<expected bytes in hexadecimal>`; commit the output as `crates/froe/tests/fixtures/lucene-4-7-primitive-vectors.tsv` with the generating command in its header comment.
2. `codec/mod.rs` registered as `pub mod codec;` in `lucene/mod.rs` — public because this plan's tests are separate crates, the reason plan 0007 gives for `writer/index` — with documented stubs for every codec file the later tasks own (`fst`, `postings`, `terms`, `stored_fields`, `doc_values`, `norms`, `field_infos`, `segment_info`, `compound`); then `data_output.rs` and `packed.rs` over `Write`, with the module comments citing the Java.
3. Tests replaying every vector byte for byte.

- **Done when:** every committed vector is reproduced byte-identically, and the stable host gate passes.
