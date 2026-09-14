---
id: write-lucene41-postings
title: Write Lucene41 Postings With Skip Data
workstream: "0009"
kind: task
depends_on: [implement-codec-primitives]
gated: false
touches:
  - crates/froe/src/index/lucene/codec/postings.rs
  - crates/froe/tests/lucene_postings_tests.rs
status: done
merged_as: "c06a822"
---
# Write Lucene41 Postings With Skip Data

Implement the `Lucene41` postings writer: for each term, documents in increasing order with frequencies and, per index option, positions, offsets and payload lengths, packed into 128-entry frame-of-reference blocks (an all-equal block through the `ALL_VALUES_EQUAL` escape, as Lucene's own writer emits it) with a `vint` tail; skip data written every 128 documents over at most 10 skip levels; the per-term metadata the terms writer will embed; and singleton pulsing — for every term with a document frequency of 1, whatever the field's index options, finishing the term writes the document id into the term metadata as `singletonDocID`, which Lucene's own postings reader then requires, the frequency being implied by `totalTermFreq`. The writer streams from a sorted postings run and never holds more than one block per file in memory.

**Steps:**

1. `postings.rs`, whose skip data is framed by the multi-level skip-list rules (levels highest first, a vlong length before each level above 0 and only when non-empty, level 0 last and unprefixed, a vlong child pointer after each skip entry at every level above 0 and never at level 0, holding the offset of that entry within the next lower level's buffer, which the reader rebases onto that level's region start) around the skip payload: `PostingsWriter::set_field(field_info) -> LongCount` — the per-field entry point, which fixes whether the field has frequencies, positions, offsets and payloads, configures the skip writer with the last three, and returns how many file pointers — 1, 2 or 3 — each term's metadata will carry, which is the `longsSize` task 0905 writes into the field directory and which Lucene's own postings reader reads positionally — then `start_term`, `start_document(document, frequency)`, `add_position(position, start_offset, end_offset)` — two offset inputs because the format buffers two values derived from them, the start delta against the previous position's start and the length `end - start`, and no payload parameter because payloads are out of scope and no caller could fill one — then `finish_document()`, which is not a formality: finishing a document is where a filled block latches `lastBlockDocID`, the `.pos` and `.pay` file pointers and their buffer offsets for the *next* document's skip entry and only then clears the document buffer, which starting a document deliberately leaves to it — without that clear, a term whose document count is a multiple of 128 emits its last 128 deltas twice in the vint tail, the 128, 1,024 and 8,192 cases this task's tests pin — then `finish_term() -> TermMetadata` and `finish()`; three output files created up front from the completed segment-wide field infos, as Lucene creates them before the first term and opens them again to read: `.doc` always, `.pos` when the field has positions, `.pay` when it has payloads or offsets, which for Oak's fields means offsets alone, since no Oak field carries payloads — each existing whenever its predicate holds, even if no posting is ever written into it.
2. Unit tests against hand-computed bytes from the specification for terms with 1, 127, 128, 129, 1023, 1024, 1025, 8192 and 8193 documents (the skip-level boundaries under the skip multiplier of 8 and interval of 128), for every index option, and for a single-document term in a positions-and-offsets field.

- **Done when:** the hand-computed vectors match, the block boundaries and skip levels follow the specification, every single-document term is pulsed, and the stable host gate passes; Lucene's acceptance is task 0911's.
