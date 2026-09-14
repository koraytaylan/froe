---
id: implement-numeric-field-encodings
title: Implement Lucene's Numeric Field Encodings
workstream: "0010"
kind: task
depends_on: [specify-oaks-analysis-chain, implement-oaks-analyzers]
gated: false
touches:
  - crates/froe/src/index/lucene/analysis/numeric.rs
  - crates/froe/src/java/iso8601.rs
  - crates/froe/src/java/mod.rs
  - crates/froe/src/writer/maintenance/planning/version_storage.rs
  - crates/froe/tests/lucene_numeric_tests.rs
  - crates/froe/tests/fixtures/lucene-numeric-vectors.tsv
  - crates/froe-cli/tests/interop/judge/NumericVectors.java
status: planned
merged_as: ""
---
# Implement Lucene's Numeric Field Encodings

Implement Lucene's prefix coding and its numeric token streams: for a long, integer or double field value, the sequence of terms (one per shift level at the field's precision step, each opening with the shift byte offset by its type's shift-start marker, the value in 7-bit groups) all at the same position, and the epoch-millisecond conversion of a DATE (parsed as ISO-8601 to milliseconds, returning a typed error for a value that does not parse and absence only for a missing value — Oak's own conversion throws an unchecked exception there, which its editor does not catch, so the indexing commit fails rather than the document being skipped). Vectors from the judge pin the bytes.

**Steps:**

1. `numeric.rs` with `long_terms(value, precision_step)`, `integer_terms` for Lucene's integer field, `double_terms` through the double's sortable-long encoding, and `date_to_long` over the shared millisecond-precision ISO-8601 parser that reproduces Jackrabbit's own, which this task's first, separate `refactor:` commit moves from the private maintenance planning module (`parse_iso8601_epoch_seconds` in `version_storage.rs`, which drops the fraction) into a crate-root `java/iso8601.rs` beside `numbers.rs`, returning milliseconds, with `version_storage.rs` delegating through floor division (`div_euclid(1_000)`, since `/` truncates toward zero for a pre-epoch instant with a fraction) and `epochs_match_a_hand_computed_table` extended by one pre-epoch row with a non-zero fraction, passing.
2. Judge class `judge/NumericVectors.java` with `numeric-vectors <outputFile>` printing the terms Lucene produces for a table of values, beside a unit test that an unparseable DATE value yields the typed error and a missing one yields absence; commit the output as `crates/froe/tests/fixtures/lucene-numeric-vectors.tsv` with the generating command in its header, and replay it in `crates/froe/tests/lucene_numeric_tests.rs`.

- **Done when:** every vector matches and the stable host gate passes.
