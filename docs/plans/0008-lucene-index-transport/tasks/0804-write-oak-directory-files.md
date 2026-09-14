---
id: write-oak-directory-files
title: Write Lucene Index Files Into The Repository
workstream: "0008"
kind: task
depends_on: [write-the-transport-safety-case]
gated: false
touches:
  - crates/froe/src/writer/index/lucene_directory.rs
  - crates/froe/src/writer/record_writer/values.rs
  - crates/froe/tests/oak_directory_writer_tests.rs
  - crates/froe/src/writer/index/mod.rs
  - crates/froe/src/writer/identifier_generator.rs
status: done
merged_as: "0a52bb988e86d971c895f3c4c11f988d4e7bf9cc"
---
# Write Lucene Index Files Into The Repository

Implement `OakDirectoryWriter`: given a `RecordWriter`, a set of named files each supplied as a `Read`, and the definition's `blobSize` and `saveDirectoryListing` settings, write the `:data`-shaped subtree Oak 1.90.0 itself writes — one child per file with `uniqueKey` (sixteen bytes from froe's entropy source as lower-case hexadecimal), `blobSize`, `jcr:lastModified`, and `jcr:data` as a single `Binary` whose bytes are the file followed by the sixteen key bytes, appended once, plus `dirListing` on the directory node when enabled; no `unsafeForActiveDeletion` flag, which Oak sets only under a blob-deletion callback that marks active deletion unsafe and whose callback is a no-op when the store has no external blob store. Oak stores `dirListing` in the iteration order of a concurrent hash set and reads it back as a set, so froe writes it in name order — a deviation recorded at the site — and every comparison against an Oak-written listing is a set comparison. The single-blob encoding is chosen because it is what Oak's own buffered directory produces by default in the consumer build (`oak.lucene.enableSingleBlobIndexFiles` defaults to true); the buffered encoding, which oak-run's Lucene importer writes when it copies a directory, is read, never written — a departure from the importer froe replaces, recorded at the site, and the reason the 0809 round trip compares the reader's view rather than the blobs. Binaries are written through a new streaming `write_binary_stream(Read)` on the record writer, so an index file of any size is chunked into blocks without being held in memory. This task changes a storage serializer, so it follows the safety case.

**Steps:**

1. `record_writer/values.rs`: `write_binary_stream(&mut self, reader: impl Read) -> Result<RecordIdentifier>` sharing the block-list machinery with `write_binary_content`, `pub(crate)` because only the directory writer calls it, with hand-crafted byte tests in the module (the independent-encoder round trip lives in `crates/froe/tests/oak_directory_writer_tests.rs`, since only integration tests reach `tests/support/`), so the feature map's blob-creation row is unchanged.
2. `lucene_directory.rs`: `OakDirectoryWriter::new(writer, blob_size, listing: DirectoryListing::{Saved, Omitted})`, `add_file(name, reader) -> Result<()>`, `finish() -> RecordIdentifier` of the directory node; a duplicate name is a typed error.
3. Tests: files of 0, 1, `blobSize - 1`, `blobSize`, `blobSize + 1` and several megabytes read back byte-identically through plan 0006's `OakDirectory`; the listing property; the metadata properties' types (`Long` for `blobSize` and `jcr:lastModified`, `String` for `uniqueKey`). The subtree's property set and types are read through the lines of `tooling::digest::digest_repository_excluding`.

- **Done when:** every file round-trips through the reader with the correct length and bytes, the subtree's lines from `tooling::digest::digest_repository_excluding` show the same property set and types as the real fixture's `:data` children (minus values that are random by design and the listing's order), and the stable host gate passes.
