---
id: prove-writer-conformance-with-lucene
title: Prove Writer Conformance With Lucene's Own Reader
workstream: "0009"
kind: task
depends_on: [build-the-document-writer]
gated: false
touches:
  - crates/froe-cli/tests/interop/phase_lucene_writer.rs
  - crates/froe-cli/tests/interop/main.rs
  - crates/froe-cli/tests/interop/judge/Corpus.java
  - crates/froe/tests/fixtures/lucene-writer-corpus.jsonl
status: done
merged_as: "fc4c798"
---
# Prove Writer Conformance With Lucene's Own Reader

The `lucene_writer_conformance` phase is the plan's oracle. A committed JSON-lines corpus describes documents in the writer's own model (already tokenized, typed, with stored and doc values), chosen to exercise every capability: many terms per field, terms across block boundaries, a term with a document frequency of at least 8,193 (three skip levels under the skip multiplier of 8 and interval of 128), every index option, norms and omitted norms, every doc-value type, stored strings and binaries, a document with several analyzed fields of one name and one with several boosted, norms-bearing fields of one name (the composition rules of task 0910), two documents that disagree on one field's index options and one on `omitNorms` (the field-infos downgrade), a term above the maximum term length (skipped, document kept), a norms field with overlapping tokens (position increment 0, never on a field's first token), a norms-bearing analyzed field whose every value yields no token (so it reaches the terms writer with no term and earns no `.tim` directory entry, task 0905's no-op rule), an empty document, and a document count above 8,192. froe writes the corpus to a directory; the judge builds the same corpus with Lucene — Lucene's own index writer under the `oakCodec` composition, a pre-tokenized token stream per field, the writer configuration Oak builds for a Lucene index, one commit — and compares. The phase also runs task 0903's FST corpus through `fst-check`. The phase lives in a new `phase_lucene_writer.rs`, registered in `main.rs`; one file per plan's phases keeps each under the thousand-line gate.

**Steps:**

1. Judge class `judge/Corpus.java` with `build-corpus <corpus> <directory>` (whose canned token stream sets the corpus's `final_position_increment` and `final_offset` in its end state, so the composition rules are proved, not assumed) and `enumerate <directory> <outputFile>`, the latter printing a canonical dump over *live* documents only — every field that at least one live document carries (a term, a stored value, a doc value or a norm), with its options, since a merge inside a lane cycle unions field infos of documents that were since deleted; every term with its document frequency and total frequency recomputed from live postings (Lucene's stored term statistics count deleted documents, which an incrementally maintained Oak index carries and a fresh segment cannot reproduce); every posting with frequency, positions and offsets; every stored field; every doc value beside the field's has-a-value bitset; every norm — the per-field body ordered deterministically and independent of segment layout and of deletions, preceded by a header line carrying the commit file's `counter`, which this task's phase compares across the two directories.
2. The phase: `checkindex` clean on froe's directory; `enumerate` over both directories byte-identical; `numdocs` equal; the commit file's `counter`, which `enumerate` prints, equal for both directories; every FST in task 0903's committed `crates/froe/tests/fixtures/lucene-fst-corpus.tsv` enumerated by `fst-check` back to its exact input map, asserted through its exit status — it is the judge's one verdict-only class, refusing on the first key or output that does not round-trip and naming it.
3. The corpus file under `crates/froe/tests/fixtures/`, with a README-style header comment on what each document exercises and the skip-list multiplier the long term targets.

- **Done when:** the phase passes against the pinned image with the enumerations identical and every FST enumerated back exactly, and flipping one posting's position in froe's output makes the comparison name the term, and the stable host gate passes (`--all-features`, so the interop suite is compiled and linted).
