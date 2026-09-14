---
id: read-oak-directory
title: Read Lucene Index Files From The Repository
workstream: "0006"
kind: task
depends_on: [model-index-definitions, specify-lucene-index-storage]
gated: false
touches:
  - crates/froe/src/index/lucene/mod.rs
  - crates/froe/src/index/lucene/directory.rs
  - crates/froe/src/content/value/stream.rs
  - crates/froe/src/index/lucene/layout.rs
  - crates/froe/tests/oak_directory_tests.rs
status: planned
merged_as: ""
---
# Read Lucene Index Files From The Repository

Implement `OakDirectory` reading: list a directory node (`dirListing` when the definition's `saveDirectoryListing` is not `false`, child names otherwise), open a file as a `Read` over both encodings — a single `Binary` streamed once with the trailing unique key withheld, or a `Binary[]` of chunks each shortened by the key and the last one shortened to the file length — and report the file length Oak computes. The streams sit on `froe::content::read_binary_stream`, so a file of any size costs constant memory. `layout.rs` carries the pure functions oak-run uses for filesystem names: the index folder base name, the colon-stripped subdirectory name, and the `index-details.txt` and `indexer-info.properties` formats. The chunk arithmetic is width-sensitive, so this task runs the i686 width sentinel beside the host gate.

**Steps:**

1. `directory.rs`: `OakDirectory::open(provider: &dyn SegmentProvider, definition, data_child_name)`, `file_names()`, `file(name) -> OakIndexFile` with `length()` and `reader()`, whose stream implements `io::Read + io::Seek`: the `Seek` half is added to `content::value::BinaryStream` here, positioning within one binary value's blocks — which that type already tracks — and `OakIndexFile::reader` implements `Seek` over the chunk sequence above it, choosing the chunk from the file offset and seeking inside it, since the buffered encoding's `jcr:data` is a `Binary[]` whose chunks are separate values; a seek past the end is a typed refusal, and the module and type documentation gain the `Seek` half including how it reports a non-I/O error. This task owns the only footprint that can add either, and plan 0008's descriptor readers are bound on `Read + Seek` so they reach the per-segment files inside a compound file at their recorded offsets without buffering the whole `.cfs`. `directory.rs` also specifies: `blobSize` read from the file node with 32,768 as the fallback — the buffered reader's own constant, not the definition's default — exactly as Oak resolves it; an absent `uniqueKey` treated as no trailing key, as both Oak readers treat it; a typed error for a missing file, a `jcr:data` of any other type (a deliberate, stricter departure from Oak, which reads such a file as buffered and zero-length; recorded at the site), a chunk shorter than the key, and a length that does not fit the chunk arithmetic.
2. `layout.rs`: `index_folder_base_name(index_path)` as `docs/analysis/index-lucene-storage.md` records it — three trailing elements with `oak:index` dropped, non-word characters stripped, `_`-joined, 127 characters at most — `filesystem_directory_name(jcr_name)` with its colon stripping, and parsers plus writers for `index-details.txt` (`metaFormatVersion`, `indexPath`, `creationTime`, `dir.*`) and `indexer-info.properties` (`checkpoint`) in Java properties syntax — the writer escapes exactly what Java's properties writer escapes, the reader accepts what Java's properties reader accepts within a declared size bound (these files arrive from an import directory, so an oversized or malformed file is a typed error naming the file and line, never a panic or an unbounded allocation).
3. Tests with hand-authored stores: a streaming file, a buffered file with three chunks, a zero-length file, a file without `uniqueKey`, a file node without `blobSize`, a file with `dirListing` disagreeing with the children under `saveDirectoryListing` on (the listing wins, the disagreement is reported) and off (the children win, the property is ignored), an end-relative seek on both encodings (the streaming one's last block shortened by the sixteen withheld key bytes), a seek back across a chunk boundary, a seek past the end refused with its typed error, a negative seek refused, property-file round trips including a `#` comment line and a `\:` escape, a properties file above the size bound, and the layout names for `/oak:index/lucene` (`lucene`, `data`, `suggest-data`).

- **Done when:** every seek case holds — end-relative on both encodings, back across a chunk boundary, past the end and negative both refused by name — both encodings read back byte-identically to what the test helper stored before appending unique keys, the length rules reproduce the specification's worked example, the layout functions reproduce oak-run's names for `/oak:index/lucene` (`lucene`, `data`, `suggest-data`), every hostile properties fixture yields its typed error, the four i686 sentinel commands `docs/high-risk-changes.md` gives pass, and the stable host gate passes.
