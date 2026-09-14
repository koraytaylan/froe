---
id: build-the-fst-index-writer
title: Build A Minimal FST Writer For The Terms Index
workstream: "0009"
kind: task
depends_on: [implement-codec-primitives]
gated: false
touches:
  - crates/froe/src/index/lucene/codec/fst.rs
  - crates/froe/tests/lucene_fst_tests.rs
  - crates/froe-cli/tests/interop/judge/FstCheck.java
  - crates/froe/tests/fixtures/lucene-fst-corpus.tsv
status: planned
merged_as: ""
---
# Build A Minimal FST Writer For The Terms Index

The `.tip` file is a transducer from term-block prefixes to byte-string outputs. Lucene's own builder produces a minimized, optionally packed automaton; froe needs only an automaton Lucene reads correctly, and Lucene's own terms writer itself builds unpacked. Implement a builder over sorted input keys that shares suffixes as Lucene's does for the unpacked case under the terms writer's own settings (suffix sharing on, non-singleton node sharing off, so only single-arc tails are deduplicated — not a fully minimized automaton) and emits linear arcs only, never the fixed-array form Lucene allows itself to choose, which the reader dispatches on per node (recorded in task 0901's quirks register) — enough to keep the index small — and a serializer matching the form Lucene reads back for single-byte input labels and byte-string outputs, unpacked, including the reversed byte store, the arc flag bits, the empty output and the start-node pointer.

**Steps:**

1. `fst.rs`: `FstBuilder::add(key, output)`, `finish() -> Vec<u8>`; the builder refuses out-of-order keys.
2. Judge class `judge/FstCheck.java` with `fst-check <corpus>`: for every line of `crates/froe/tests/fixtures/lucene-fst-corpus.tsv` (the FST bytes froe wrote in hexadecimal, then the input key/output pairs), loads the bytes with Lucene's own transducer reader and enumerates every key/output pair; task 0911's conformance phase runs it and compares, since the judge only runs inside the interop suite.
3. Unit tests against hand-computed bytes from the specification: a transducer carrying only the empty-string key, which is the minimal serializable shape (Lucene's builder forces the start node to 0 for it over an empty byte store, while a builder given no key at all produces nothing a serializer will accept, so an empty transducer has no serialized form to hand-compute and none can reach `.tip`, where the terms writer saves one only under a positive term count), a single key, an empty-string key carrying a multi-byte output both alone and beside ordinary keys (the shape every `.tip` takes, since the terms writer stores the root block's code as the transducer's empty output and the unpacked form is saved with its byte store reversed), shared prefixes and suffixes, keys that are prefixes of other keys, outputs that share prefixes (the common-prefix rule byte-string outputs follow); the unit tests also regenerate `lucene-fst-corpus.tsv` from the same cases and assert it is unchanged, so the committed corpus is the corpus the judge checks.

- **Done when:** every hand-computed vector matches byte for byte, out-of-order input is refused, and the stable host gate passes; the judge's enumeration of the same corpus is task 0911's acceptance.
