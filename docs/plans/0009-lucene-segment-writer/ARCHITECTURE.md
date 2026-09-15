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

### Verification report

**The frozen range** is `668b6f5..d195c40` — 26 commits, the whole of plan
0009. `git diff --check 668b6f5..d195c40` is clean and `git status
--porcelain` was empty when the review began, so there was no untracked
candidate file. The findings below were answered in **follow-up commits
after the range**, as `high-risk-changes.md` asks, and the cumulative
result was re-verified: `ffbff61`, `de08c5d`, `20c7159`, `301ead7`,
`abb01fb`, `09094ad`.

**The stable host gate**, executed on `x86_64-unknown-linux-gnu`, each
command's own exit status recorded rather than a pipeline's:

| Command | Exit |
| --- | --- |
| `cargo +stable fmt --all -- --check` | 0 |
| `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings` | 0 |
| `cargo +stable test --workspace --all-features --no-fail-fast` | 0 |
| `cargo +stable test --workspace --all-features --release --no-fail-fast` | 0 |
| `RUSTDOCFLAGS="-D warnings" cargo +stable doc --workspace --all-features --no-deps` | 0 |
| `scripts/oversized-files.sh` | 0 |

**The MSRV gate**, which plans 0007 and 0008 recorded as a gap for want of
a toolchain, ran here: `1.89.0-x86_64-unknown-linux-gnu` is installed on
this host now, and `fmt`, `clippy`, `test`, `test --release` and `doc`
each exited 0 under `cargo +1.89`.

**The i686 width sentinel**, likewise a recorded gap, ran for the `froe`
package — the one this plan's code is in — on both toolchains:
`RUSTFLAGS="-D warnings" cargo +stable check -p froe --all-targets
--all-features --target i686-unknown-linux-gnu` and its clippy twin, then
both again under `+1.89`, all four exit 0. This is **compilation for a
32-bit target, not execution on one**; it is the width sentinel the guide
asks for and nothing more. The workspace-wide attempt plan 0008 recorded
still fails in `zstd-sys`, which needs a 32-bit C toolchain this host does
not have, so `froe-export` and `froe-cli` remain uncompiled for i686.

**The five separations this report is held to.**

*Execution from cross-compilation.* Every test result above was executed
on `x86_64-unknown-linux-gnu`. The i686 results are compilation only and
are labelled so in the paragraph that states them.

*Synthetic credentials from execution as root.* This range has no
credential-facing code: the writer takes a directory and a budget, and no
test in it depends on the runner's uid.

*Process-exit or syscall injection from true power-loss ordering.* **Not
applicable to this range.** It arms no cutpoint and publishes no
repository bytes; the segment it writes becomes reachable only when plan
0010 copies it into a store, and that plan's own fault table covers the
boundary.

*File existence from durability.* The conformance phase asserts that
Lucene opens and reads what froe wrote, through a real filesystem. That
the bytes reached the medium is not asserted and is not this range's
claim.

*froe-to-froe round trips from real Oak interoperability.* The unit
suites read froe's own bytes back with independent decoders written to the
reader's rules — necessary, never sufficient. The claim that the segment
is *Lucene's* format rests on the interoperability run below and on the
committed vector fixtures, which are Lucene's own output from the pinned
image.

**Interoperability: the `lucene_writer_conformance` run.**

* **Oak build:** `docker.io/apache/sling@sha256:8722cd66ae0758e50784ac21df836c8f8d9e443d105e1a4292a4cb7f810a8cc9`, whose `oak-lucene-1.90.0.jar` carries the Lucene 4.7.2 classes the judge compiles against.
* **Direction:** froe-to-Oak. Lucene, inside the image, reads what froe wrote.
* **Operation:** froe writes the committed corpus through
  `LuceneIndexWriter`; the judge builds the same corpus with Lucene's own
  `IndexWriter` under the `oakCodec` composition; `CheckIndex` runs over
  froe's index; both directories are enumerated by the same Java and
  compared line for line; and every transducer of the FST corpus is
  enumerated by Lucene's own reader and compared against the pairs froe
  put in.
* **Verified post-state, 2026-09-15:** `CheckIndex` clean over froe's
  index; **8,312 documents and 157,944 dump lines, identical**; **9
  transducers enumerated back to their own pairs**. The last of those is
  a claim this phase could not make before `de08c5d`: it ran `FstCheck`
  and discarded what it printed.

### Known gaps

**What the equivalence oracle does not cover.** The judge's enumeration
prints, per field, `indexed/options/norms/docvalues`; per term, `docFreq`
and `totalTermFreq` recomputed from postings, then each posting's
document, frequency and — where positions exist — position and offsets;
per live document, each stored field; per doc-values field, the value; per
norms-bearing field, the norm masked to eight bits. Not compared, and not
otherwise checked: **payloads** (froe cannot emit one), **term vectors**
(nothing writes them), **deleted documents** (nothing deletes), the
**segment metadata** beyond the commit's `counter` — segment count, name,
`SegmentInfos.version`, `userData`, diagnostics, the `.si` file list —
and **field numbers**, since the judge keys fields by name. `CheckIndex`
covers more than the dump does: in 4.7 it reads every posting, checks
position monotonicity and offset bounds, cross-checks the dictionary's
statistics against recounted postings, enforces term order and exercises
skip data.

**What the corpus does not reach.** No **non-ASCII term**, so unsigned
byte ordering in the terms dictionary is never put to Lucene. No numeric
doc value reaching **`GCD_COMPRESSED` or `TABLE_COMPRESSED`** — the one
numeric field lands in `DELTA_COMPRESSED`. No **single-valued sorted-set**
field. No **float or double stored value**. No **posting list whose
document frequency is an exact multiple of 128**, where the final `VInt`
block is empty. No **negative double doc value**, which is the live path
for `AbstractBlockPackedWriter.writeVLong`'s nine-byte capped form.

**Byte-level departures from what Lucene's own writer would produce**, each
documented at its site and in §10.3 of the specification: `PACKED_SINGLE_BLOCK`
is never chosen, FST arcs are linear only (a valid subset — the reader
dispatches per node), the `TABLE_COMPRESSED` table is ordered ascending,
and the `.si` file set is in froe's own order. A "byte-identical to Oak"
claim needs §10.3 read first.

**The memory bound is narrower than "bounded memory" suggests**, and the
writer's own module doc now states which structures are charged against
the budget and which are not. The three that are not — one document's
inverted form, the terms index of the field being written, and the field
table — are each what Lucene's own writer holds as well, but none has a
limit-plus-one regression, and only the sort's own budget does.

**Two fixture provenance gaps.** `lucene-norm-vectors.tsv` names a
generator, `NormVectors.java`, that is not in the tree, so unlike every
other vector fixture here it cannot be regenerated from the repository;
its header says so now. And the five generated Unicode tables plan 0010
consumes are checked against digests recorded in their own headers at
generation time, not against the UCD, whose files are deliberately not
committed.

**Standing environment axes**, inherited from plans 0006 through 0008: no
AEM build (the loop is Apache Sling with Oak), no external blob store, no
local macOS execution — CI's `macos-latest` job is the authority — and no
native Windows execution.

### Review

Four adversarial lenses were run over the frozen range by passes that did
not author it, each briefed on a distinct question. **The pass is
recorded as an automated one, not a second person**: it was briefed by the
author and inherits the author's framing of what the range is for.

**Lens 1 — encoding boundaries.** Every place the codec writes a size, a
length, a count or a pointer, against the specification and against
Lucene's own writer/reader pairs. **No critical or major defect.** The
boundaries it checked and found correct are the ones a port gets wrong:
the five-byte negative `VInt`, the nine-byte capped signed `VLong`, the
exact-128-document block with no tail and no skip list, `lastPosBlockOffset`
at exactly 128, the block-tree `25`/`48` constants and the `subBytes[0] ==
-1` quirk, the doc-values format decision's three traps, and
`MAXIMUM_TERM_LENGTH` as `BYTE_BLOCK_SIZE - 2`. It found one byte that is
not Oak's — the `OMIT_NORMS` bit on a non-indexed field — which the
specification could not settle because it quoted the writer and not
`FieldInfo`'s constructor; `javap -c` in the pinned image settled it and
`20c7159` fixed it. Two minor observations were recorded rather than
fixed: position and offset arithmetic is checked in `i64` and then
narrowed with `as`, which cannot produce wrong bytes because the next
call's own guard refuses the wrapped value, and the doc-values stream
closures latch a write error rather than short-circuiting.

**Lens 2 — the equivalence oracle's blind spots.** **One real defect, and
it was in the oracle itself**: the conformance phase ran `FstCheck` and
threw away what it printed, asserting only that the class exited zero — so
a transducer whose bytes Lucene parses while yielding the wrong keys, or
none, passed the phase that exists to catch exactly that, and
`docs/interop.md`'s "every transducer enumerated back" was not what the
wiring did. `de08c5d` makes the phase compare against the corpus's own
expected column, and the run above reports nine transducers checked. Two
smaller holes in the same oracle were closed with it: `Corpus` fell off
the end of its doc-values type chain in silence, and `compare_dumps`
reported `<end of dump>` against `<end of dump>` for two dumps that
differed in trailing whitespace. The lens's inventory of what the oracle
does not prove is the first half of the known gaps above.

**Lens 3 — the memory bound.** **One real defect**: `reduce_to_fan_in`
collected each fan-in group into a `Vec` before writing it, so the pass
that exists to keep memory off the input size held sixty-four runs' worth
of records at once — the declared budget's own multiple, at exactly the
scale the budget exists for. `ffbff61` streams it, with a regression that
counts live records rather than bytes and reports 512 against a ceiling of
74 when neutralized. The lens also established what the budget does and
does not charge, which is now the writer's own module doc and the known
gap above.

**Lens 4 — the wording of the evidence.** The lens `high-risk-changes.md`
says never to skip. **No accept-condition-cited-as-failure** of the kind
it caught on the v0.8.0 range: every named test exists, and each asserts
what its row claims. It found eight wording defects, all corrected in
`301ead7`, `abb01fb` and `09094ad`. Two were substantive: the `oakCodec`
composition was described three mutually exclusive ways and the wrong one
had reached two user-facing documents and a public doc comment — settled
against `OakCodec`'s own bytecode — and "every doc-value type" described a
corpus that crossed several-fields-of-one-name with analyzed and boosted
fields but not with doc values, which is the gap that hid a shipped defect
until `9b1e451`.
