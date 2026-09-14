# Architecture — Plan 0009

## 0009 — Lucene Segment Writer

Requires plans 0006 through 0008 merged (the directory writer, the segment readers, the judge) and plan 0007's bounded external sort with its public `RunLocation`, `SortBudget`, `SortedPasses`, `SortedPass` and the `SpillRecord` bound, plus the `SortedPasses::from_sorted_records` constructor, which plan 0007's own module and integration tests exercise inside the range that freezes it and which this plan's doc-value and norms tests also call, and the `#[cfg(test)] pub(crate)` open-file accounting and merge-pass counter that plan's task 0702 exposes, which task 0910's in-crate tests in `inverted.rs` assert.

### Ground truth

Task 0901 writes `docs/analysis/lucene-4-7-codec.md`, and that document is where the Java lives: every statement in it names the vendored file and method it was read from, in the Lucene 4.7.2 sources `oak-lucene` vendors at the pinned Oak commit and exports as `4.7.2-oak2` (`oak-lucene/src/main/java/org/apache/lucene/{codecs,index,store,util,document}/**`). This plan and its tasks state the format instead, and point at that document where a reader needs the source.

The `oakCodec` composition is field infos and segment info at format version 46, uncompressed `Lucene40` stored fields, the `Lucene41` postings format, the `Lucene45` doc-values format and the `Lucene42` norms format, with a `Lucene42` term-vectors format and a `Lucene40` live-documents format declared but unused. The codec name written into `segments_N` is `oakCodec`, which the consumer resolves through the `META-INF/services/org.apache.lucene.codecs.Codec` registration the image's jar carries. Oak selects that composition only for a fulltext-enabled definition — a rule with node aggregates or a fulltext-enabled property — or for an explicit `codec` property; otherwise the configuration Oak builds for a Lucene index writer leaves Lucene's default `Lucene46` codec in place, which this plan does not write and plan 0010 refuses by name. The index-time defaults come from that same configuration: Lucene version 4.7, compound files left at Lucene's default of enabled — the real fixture holds `_0.cfs` — and Lucene's default similarity for norms.

### Shape of the writer

```
documents (fields: name, index options, tokens with position/offset, stored value, doc value)
        │
        ▼
inverted index accumulation per field: term ─► postings (doc, freq, positions, offsets)
        │  bounded: postings spilled as sorted runs keyed by (field, term, doc)
        ▼
segment assembly, one field at a time (Lucene's own writer sorts fields by name and its
readers key fields by name, so the order is a choice, not a requirement):
   .fdt/.fdx   written first, per document, as documents stream in
   .doc/.pos/.pay  from the merged postings run, 128-document FOR blocks, skip data
   .tim/.tip   terms in unsigned byte order, blocks of 25..48, floor blocks, transducer index
   .dvd/.dvm   numeric / sorted / sorted-set values, from re-readable spilled runs
               (two per SORTED/SORTED_SET field: the dictionary and the per-document
               ordinals), charged to the budget
   .nvd/.nvm   norms from the default similarity for fields with norms (one byte per document
               per field with norms, computed from each document's token count less overlapping
               tokens as it arrives and kept, or spilled, until the field is written; charged
               to the budget)
   .fnm        field infos
   .cfs/.cfe   compound file over every per-segment file except .si
   .si         segment info whose file set is _0.cfs, _0.cfe, _0.si
   segments_N + segments.gen
```

Documents are numbered in arrival order; several fields of one name in one document compose as Lucene's own inverter composes them — after each value's tokens, the token stream's end state is consulted and its position increment and end offset are added to the running position and offset (a tokenizer's end state is its own: the standard tokenizer reports the scanned length, not the last token's end), then the analyzer's position-increment gap of 0 and offset gap of 1 for analyzed fields; boosts multiply, and the norm's term count sums over the values — so every token stream the writer consumes carries a `final_position_increment` and a `final_offset` beside its tokens, because Oak adds one `:fulltext` and one `full:<name>` field per value; terms per field are sorted in unsigned byte order; the transducer index is built from the block prefixes in that order, which makes a single-pass, non-packed transducer sufficient. Nothing here depends on a Lucene byte-for-byte match: two valid indexes may differ in block boundaries, transducer packing and diagnostics, and the judge compares *contents* — fields, terms, postings, positions, offsets, stored values, doc values, norms — not files.

### Modules

```
crates/froe/src/index/lucene/codec/
├── mod.rs
├── data_output.rs      vint, vlong, string, string map, string set, codec header
├── packed.rs           packed-integer writer (PACKED format), block-packed, monotonic block-packed
├── fst.rs              minimal byte-keyed transducer builder and serializer
├── postings.rs         Lucene41 .doc/.pos/.pay writer, FOR block coder, skip writer, term metadata
├── terms.rs            BlockTree .tim/.tip writer
├── stored_fields.rs    Lucene40 .fdt/.fdx writer
├── doc_values.rs       Lucene45 numeric, sorted, sorted-set consumer
├── norms.rs            Lucene42 norms consumer, the default similarity's norm, small-float encoding
├── field_infos.rs      Lucene46 .fnm writer
├── segment_info.rs     Lucene46 .si writer, segments_N, segments.gen
└── compound.rs         .cfs/.cfe writer
crates/froe/src/progress.rs        WorkUnit::IndexDocuments (0910)
crates/froe/src/index/lucene/writer/
├── mod.rs              Document, Field, LuceneIndexWriter: add documents, finish into a directory
└── inverted.rs         per-field inversion with spilled postings runs
```

The capability the feature map inventories is the writer (`LuceneIndexWriter`); the codec modules are its components. Every writer targets a `Write + Seek` sink and fills a filesystem directory; plan 0010 then copies the finished files into the repository through plan 0008's `OakDirectoryWriter`, which takes a `Read` per file, so no adapter between the two is needed.

### Feasibility gate

Task 0901's specification ends with a go/no-go: it must find no file or feature the consumer requires that the plan cannot write with the primitives above (the known risks are the transducer serializer and the packed-integer block formats), and it must estimate the size of each module against the thousand-line file limit. The verdict is the closing section of `docs/analysis/lucene-4-7-codec.md`; the coordinator summarizes it in `STATUS.md`; task 0902 is gated on it.

### Task graph

```
0901 codec specification and verdict ─► 0902 primitives (gated)
0902 ─► 0903 FST
0902 ─► 0904 postings
0903, 0904 ─► 0905 BlockTree terms
0902 ─► 0906 stored fields
0902 ─► 0907 doc values
0902 ─► 0908 norms
0905, 0906, 0907, 0908 ─► 0909 segment assembly (field infos, .si, compound, commit)
0909 ─► 0910 document model and inversion
0910 ─► 0911 conformance judge and phase
0911 ─► 0912 suite wiring and run record
0912 ─► 0913 review (gated)
```

The writer's feature-map row and the storage-format pointer land with the writer (0910); the interop phase's wiring follows the phase (0912). The judge grows by one class per task (`CodecVectors`, `FstCheck`, `Corpus`), never by editing a shared file.
