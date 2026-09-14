---
id: write-stored-fields
title: Write Lucene40 Stored Fields
workstream: "0009"
kind: task
depends_on: [implement-codec-primitives]
gated: false
touches:
  - crates/froe/src/index/lucene/codec/stored_fields.rs
  - crates/froe/tests/lucene_stored_fields_tests.rs
status: done
merged_as: "c8529cd"
---
# Write Lucene40 Stored Fields

Implement the `Lucene40` stored-fields writer: `.fdx` with one 8-byte pointer per document after its `Lucene40StoredFieldsIndex` header (version 0), the matching `Lucene40StoredFieldsData` header on `.fdt`, and the size invariant the format asserts when it finishes — the `.fdx` length is its own header length plus eight bytes per document — `.fdt` with per-document field count and per-field number, type bits and value — strings in the output string encoding, binaries as a vint length plus bytes, numerics fixed-width big-endian (four bytes for an int, eight for a long, and the raw bits of the floating types in four and eight), and the type bits `FIELD_IS_BINARY = 1 << 1` with the numeric code in three bits at shift 3 (int 1, long 2, float 3, double 4) — uncompressed, which is the whole point of `oakCodec`. Oak stores `:path` on every document, for `useInExcerpt` properties the property text, and for binaries the extracted text (or plan 0010's marker) as a stored `:fulltext` or `fullnode:` value.

**Steps:**

1. `stored_fields.rs`: `StoredFieldsWriter::start_document(count)`, `write_field(number, value)`, `finish_document()` — which writes no bytes, the format's own per-document terminator being empty, and exists only to close the per-document scope, unlike task 0904's terminator, which latches skip state — `finish(document_count)`.
2. Unit tests with hand-computed bytes: a document with no stored fields, string, binary and long values, and a thousand documents to pin the index pointers.

- **Done when:** the vectors match and the stable host gate passes.
