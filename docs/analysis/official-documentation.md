# Oak Segment Tar — Official Documentation, Distilled (Independent Witness)

**Scope and provenance.** This document distills ONLY the official Apache Jackrabbit Oak
documentation found at
`oak-doc/src/site/markdown/nodestore/segment/` (files: `overview.md`, `tar.md`,
`records.md`, `changes.md`, `onrc-memoirs.md`, `classes.md`). It deliberately does NOT
consult the Java sources, so it can serve as an independent cross-check against the
code-derived specifications. Every fact below cites the documentation file (and section)
it came from. Where the documentation is vague, self-contradictory, or suspected to be
stale relative to the code, that is flagged prominently in
[Section 4](#4-facts-the-code-may-contradict--must-be-cross-checked).

**Warning to the implementer.** The official documentation is known to be incomplete and
in places stale (see Section 4). The code-derived specifications are authoritative; this
document is a witness for cross-checking, not a implementation source of record.

---

## Table of contents

1. [The storage format as documented](#1-the-storage-format-as-documented)
   - 1.1 Repository anatomy
   - 1.2 TAR container structure
   - 1.3 The four entry kinds in an Oak TAR file
   - 1.4 Data segment layout
   - 1.5 Bulk segment layout
   - 1.6 Segment UUIDs
   - 1.7 Record addressing (record IDs, record numbers, offsets)
   - 1.8 Record types and their documented layouts
   - 1.9 Index file (.idx)
   - 1.10 Graph file (.gph)
   - 1.11 Binary references file (.brf)
   - 1.12 Journal and manifest
2. [Storage format version history (from changes.md)](#2-storage-format-version-history)
3. [Garbage collection model (as it affects a reader)](#3-garbage-collection-model)
4. [Facts the code may contradict — MUST be cross-checked](#4-facts-the-code-may-contradict--must-be-cross-checked)
5. [Operational knowledge for a reader implementation](#5-operational-knowledge-for-a-reader-implementation)

---

## 1. The storage format as documented

### 1.1 Repository anatomy

(Source: `overview.md`, "Overview".)

- Content is stored as immutable **records** inside **segments**; segments are collected
  in **tar files**; a **journal** file tracks the sequence of head states.
- Segments are immutable and identified by a UUID. A segment "typically contains a
  continuous subset of the content tree, for example a node with its properties and
  closest child nodes."
- **Segments can be up to 256 KiB in size** (`overview.md`, Overview; also implied by the
  offset normalization formula in `records.md`).
- Tar files contain, besides segments: an index of the segments, the segment-reference
  graph of all contained segments, and an index of all external binaries referenced from
  contained segments (`overview.md`, Overview).
- The journal is "a special, atomically updated file that records the state of the
  repository as a sequence of references to successive root node records." It is only
  appended once the referenced record has been flushed to disk (crash resiliency)
  (`overview.md`, Overview).
- Three design principles: immutability, compactness, locality (`overview.md`).

Class-level architecture (source: `classes.md`): `SegmentNodeStore` (NodeStore API
implementation) over a `SegmentStore`; `FileStore` is the tar-file implementation of
`SegmentStore`; `FileStore` depends on `TarFiles` = one `TarWriter` + zero or more
`TarReader` instances (append-only design: data is appended via one writer, archived in
many readers over time).

### 1.2 TAR container structure

(Source: `tar.md`, "Organization of a TAR file" and "The TAR file as used by Oak".)

- A TAR file is a linear sequence of **512-byte blocks**, terminated by **two blocks of
  zero bytes**.
- Each logical entry = 1 header block + content blocks.
- Documented TAR header fields (standard ustar-ish header; the docs list):

| Field | Size (bytes) | Used by Oak? |
|---|---|---|
| file name | 100 | YES — entry name |
| file mode | 8 (octal string) | no (uninteresting value) |
| owner uid | 8 (string) | no |
| group gid | 8 (string) | no |
| file size | 12 (octal string) | YES — entry payload size |
| last modification time | 12 (octal string) | YES — write timestamp |
| checksum | 8 | no ("uninteresting value" per docs — see Section 4, item C1) |
| file type | 1 | no |
| name of linked file | 1 (**sic** — see Section 4, item C2) | no |

- **TAR file naming convention** (`tar.md`, "Oak TAR file layout"): files start at
  `data00000a.tar`; new files increment the number (`data00001a.tar`, ...). After cleanup,
  tar files that would shrink **by at least 25%** are rewritten to a new "tar generation",
  incrementing the trailing letter: `data00000a.tar` → `data00000b.tar`.
- **Entry ordering / read strategy** (`tar.md`, "Oak TAR file layout"): the important
  metadata lives at the *bottom* (end) of the file. Reading entries from the bottom you
  encounter, in order: **index**, then **graph**, then **binary references**, then the
  segments (whose relative order is irrelevant). A reader should read the index first to
  locate everything else.

### 1.3 The four entry kinds in an Oak TAR file

(Source: `tar.md`, "The TAR file as used by Oak".)

| Kind | Entry name | Content |
|---|---|---|
| segment | `UUID.CRC2` — a 128-bit UUID as hex string, dot, zero-padded numeric string of the checksum of the raw segment data (docs literally say "CRC2"; see Section 4, item C3) | raw segment bytes |
| binary references | name ending in `.brf` | catalog of blobs (value records) referenced by segments in this tar, indexed by segment generation |
| graph | name ending in `.gph` | segment graph of all segments in this tar, as an adjacency list of UUIDs |
| index | name ending in `.idx` | sorted list of every segment in this tar |

- A **bulk segment** is stored as-is; a **data segment** is inspected on write to extract
  references to other segments and to binary content (`tar.md`, "Segment files").
- The list of segments referenced by a data segment "is maintained ordered" to speed up
  lookup (`tar.md`, "Segment files").

### 1.4 Data segment layout

(Source: `records.md`, "Data segments", including the ASCII diagram; corroborated by
`tar.md`, "Segment files".)

Overall structure: `[segment header] [record 1] [record 2] ... [record N]`.
"The segment header and each record is zero-padded to make their size a multiple of four
bytes and to align the next record at a four-byte boundary" (`records.md`).

**All integers are big endian** (`records.md`: "All integers are stored in big endian
format"; `tar.md` repeats "serialized as a big endian integer" for each field).

Fixed header part — 32 bytes (`tar.md`, "Segment files"; `records.md` diagram):

| Offset (bytes) | Size | Field | Value / notes |
|---|---|---|---|
| 0 | 3 | magic | ASCII `"0aK"` = bytes `0x30 0x61 0x4B` (`records.md`: "The first three bytes of a segment always contain the ASCII string \"0aK\"") |
| 3 | 1 | version | "currently set to 12" (`records.md`) — see Section 4, item C4 |
| 4 | 6 | reserved/empty | reserved for future use |
| 10 | 4 | generation | big-endian int32; GC generation of the segment. `changes.md` confirms: "the generation is saved at offsets 10 to 13 as a 4-byte integer value" |
| 14 | 4 | segrefcount (number of references) | big-endian int32; count of referenced external segments |
| 18 | 4 | reccount (number of records) | big-endian int32; count of records in this segment |
| 22 | 10 | reserved/empty | reserved for future use |

After the fixed part:

1. **Referenced segment identifiers**: `segrefcount × 16 bytes`, starting at offset 32
   (`records.md`: "The identifiers of those segments are listed starting at offset 32 of
   the segment header"). One 16-byte UUID per referenced segment.
2. **Record headers**: `reccount × 9 bytes` each:

| Field | Size | Notes |
|---|---|---|
| record number | 4 | big-endian int32; logical id of the record within this segment |
| record type | 1 | one of *LEAF*, *BRANCH*, *BUCKET*, *LIST*, *VALUE*, *BLOCK*, *TEMPLATE*, *NODE*, *BLOB_ID* (`tar.md`, "Segment files") |
| record offset | 4 | big-endian int32; "offset of the record counting from the end of the segment" (`tar.md`) — but see the normalization formula in 1.7 and contradiction C5 |

3. Padding set to 0 to reach the next 8-byte row boundary in the diagram / 4-byte
   alignment rule (`records.md` diagram shows "padding (set to 0)" after the record
   headers).

The header also conceptually "maintains a set of references to *root records*: those
records that are not referenced from any other records in the segment" (`records.md`,
"Data segments") — in the current format this is realized by the record-header table
(see `changes.md`, "Root record types").

### 1.5 Bulk segment layout

(Source: `records.md`, "Bulk segments".)

- A bulk segment is a raw sequence of block records with **no header or metadata**:
  `[block 1] [block 2] ... [block N]`.
- A bulk segment of length `n` bytes consists of `n div 4096` block records of 4 KiB each,
  followed (if `n mod 4096 != 0`) by one block record of `n mod 4096` bytes. Structure is
  fully determined by segment length.
- Because bulk segments have no header, they cannot record a GC generation; the cleanup
  phase determines their reclaimability by reachability through the segment graph instead
  (`onrc-memoirs.md`, Oak 1.6 section).

### 1.6 Segment UUIDs

(Source: `records.md`, "Segments".)

- Segment identifiers are 16-byte, randomly generated UUIDs that look like RFC 4122
  version-4 UUIDs. Oak reserves 4 bits to distinguish segment kinds:
  - `xxxxxxxx-xxxx-4xxx-axxx-xxxxxxxxxxxx` → **data** segment
  - `xxxxxxxx-xxxx-4xxx-bxxx-xxxxxxxxxxxx` → **bulk** segment
  - (version nibble = `4`; the "variant" nibble is `a` for data, `b` for bulk; all `x`
    positions are random.)

### 1.7 Record addressing (record IDs, record numbers, offsets)

(Source: `records.md`, "Record numbers and offsets"; `changes.md`, "Logic record IDs".)

- A **record identifier** = *segment field* + *record number field*.
- The **segment field** is a "two-bytes short integer": an index into the segment's array
  of referenced segment identifiers (the lookup table in the header). "The array can
  store up to `Integer.MAX_VALUE` entries, but two bytes are enough ... in practice."
  Special value: **segment field = 0 means the referenced record is in the current
  segment**. (Implication, not spelled out explicitly by the docs: index `k > 0` refers
  to the `k`-th entry of the reference table, i.e. the table is effectively 1-based from
  the record ID's perspective. Cross-check against code — Section 4, item C6.)
- The **record number field** is a logical identifier, looked up in the record-headers
  table ("record references table") of the target segment to obtain the record offset.
- **Offset normalization** (`records.md`): "The offset is relative to the beginning of a
  theoretical segment which is defined to be 256 KiB." Records grow from the bottom of the
  segment toward the top, and segments may be shorter than 256 KiB, so the physical
  position is:

  ```
  position = SIZE - 256KiB + OFFSET
  ```

  where `SIZE` is the actual segment length in bytes, 256 KiB = 262144, and `OFFSET` is
  the value from the record-headers table. (Note: `tar.md` instead states position =
  `segment size - offset` — an internal contradiction; see Section 4, item C5.)

### 1.8 Record types and their documented layouts

(Source: `records.md`, "Records".)

The documentation names record types: **block, list (bucket/list), map (leaf/branch),
value, template, node**, and the table-entry types LEAF, BRANCH, BUCKET, LIST, VALUE,
BLOCK, TEMPLATE, NODE, BLOB_ID (`tar.md`). The docs give conceptual layouts only; no
byte-level layout is given for maps, templates, or nodes.

#### 1.8.1 Block records

- Plain byte sequence up to 4 kB. **No length field** — the writer stores the length
  elsewhere. Stored 4-byte-aligned. The only record type that cannot reference other
  records. Typically live in bulk segments.

#### 1.8.2 Value records

- Store data with a length and optional references. Four representations, discriminated by
  the high-order bits of the first byte:

| First-byte pattern | Kind | Length encoding | Documented range |
|---|---|---|---|
| `0xxxxxxx` | small value | length in 7 bits | 0 – 127 bytes, inline |
| `10xxxxxx` | medium value | length in 6 + 8 = 14 bits | 128 – 16511 bytes, inline |
| `110xxxxx` | long value | length in 5 + 7×8 = 61 bits | up to 2^61 bytes, stored as a list of block records (list record ID follows) |
| `1110xxxx` | external value | reference-string length in 4 + 8 = 12 bits | external blob: record holds length of value + a string reference (up to 4 kB) to an external storage location |

  (The docs describe the long value as "up to two exabytes (2^61)".)
- Value records are used for storing all names and values (all values reduced to binary
  UTF-8 string form).

#### 1.8.3 List records

- Logical list of record IDs built from two physical record types:
  - **bucket record**: recursive; a list of **at most 255 references**; contains nothing
    but record IDs (either child buckets or the element IDs).
  - **list record** (top level): an integer size field + one record ID pointing to a
    bucket.
- Access is O(log N); immutable.
- Empty/one-element special cases are not documented (cross-check with code).

#### 1.8.4 Map records

- General-purpose unordered map string → record ID, stored as a **HAMT** (hash array
  mapped trie):
  - The hash code of each key is split into **5-bit** pieces; keys are sorted into
    **32 (2^5) buckets** by the first 5 bits.
  - If a bucket contains **fewer than 32 entries**, it is stored directly as a list of
    key–value pairs (**leaf record**); otherwise it is split again on the next 5 bits
    (**branch record**, recursive).
  - "When all buckets are stored, the list of top-level bucket references gets stored
    along with the total number of entries in the map."
- The hash function itself is **not documented** — must come from the code.
- **Map diffs**: "if only one element of a previously stored map is modified, and the map
  is stored again, only a 'diff' of the map is stored." (A reader MUST therefore be
  prepared for a diff representation of branch records; its byte layout is not
  documented.)
- **Hard limits** (verbatim from `records.md` warning): a map record can store up to
  **2^29 − 1 = 536,870,911 entries**. Log messages are printed after 400,000,000 entries;
  writing beyond 500,000,000 entries is not allowed unless the boolean system property
  `oak.segmentNodeStore.allowWritesOnHugeMapRecord` is set; the segment store does not
  allow writing map records with more than 536,000,000 entries.
  - The 2^29 − 1 limit implies the entry count occupies at most 29 bits of a word,
    suggesting the top bits of the size word carry other information (level/type) — not
    documented; must be confirmed from code.

#### 1.8.5 Template records

- Store the slow-changing structural metadata of a node: primary type, mixin types,
  property names, property types, and child-node arity information (zero / one / many
  children; if exactly one, the child's name is stored in the template).
- "A template record consists of a set of up to N (exact size TBD, N ~ 256) property name
  and type pairs" — **the docs explicitly do not commit to the limit** ("exact size
  TBD").
- Names in templates are stored as separate value records and referenced by ID.
- No byte-level layout is documented.

#### 1.8.6 Node records

- A node record always references a **template record**.
- Variable part: a **list of property values** (record IDs of the value records, packed
  in a list record whose ID is stored in the node record) and a **map of child nodes**
  (child name → child node record ID, stored as a map record whose ID is stored in the
  node record).
- Since Oak Segment Tar 0.0.2, every node record also carries a **stable identifier**
  (`changes.md`, "Stable identifiers"): when a node record is first serialized, the
  address it is serialized to becomes its stable ID; the stable ID is "serialized as a
  18-bytes-long string record" and "referenced from the node record by adding an
  additional 3-bytes-long reference field to it" (worst-case overhead 21 bytes per node
  record). **The "3-bytes-long reference" claim is suspect** — see Section 4, item C7.
  Stable IDs exist so two copies of a node in different GC generations can be compared
  cheaply (`changes.md`; `onrc-memoirs.md`, Oak 1.6).
- Record-ID rendering used in logs/tools: `UUID.hex-record-number`, e.g.
  `3e3b35d3-2a15-43bc-a422-7bd4741d97a5.0000002a` (`overview.md`, compaction log
  examples), and colon form `UUID:12345` for the `debug` tool (`overview.md`, Debug).

### 1.9 Index file (.idx)

(Source: `tar.md`, "Index files".)

- An ordered list of references to the segment entries in the tar file, **ordered by
  UUID**. "Like the graph file, the index file is also stored backwards" (i.e., placed so
  it can be read from the end of the tar file; the docs do not precisely define what
  "stored backwards" means byte-wise — see Section 4, item C8).
- Header fields (each 4 bytes):

| Field | Size | Notes |
|---|---|---|
| magic number | 4 | value NOT given by the docs |
| size of the index | 4 | number of bytes occupied by index data, **including padding added to align with the TAR 512-byte block boundary** |
| number of entries | 4 | |
| checksum | 4 | **CRC32** of the content of the index file |

- Per-entry fields:

| Field | Size | Notes |
|---|---|---|
| UUID most significant bits | 8 | |
| UUID least significant bits | 8 | |
| offset in tar file of the entry | not stated (docs omit width) | position of the TAR entry containing the segment |
| size of the entry | not stated | |
| generation of the entry | not stated | |

  The docs do NOT give the widths of offset/size/generation, do NOT mention an index
  format version, and do NOT mention full-generation/compacted fields (see Section 4,
  items C9 and C10).
- Because entries are sorted by UUID and UUIDs are uniformly distributed, the documented
  lookup algorithm is **interpolation search** (`tar.md` closes with a long explanation of
  it). Binary search is of course also correct.

### 1.10 Graph file (.gph)

(Source: `tar.md`, "Graph files".)

- Represents relationships between segments (inside or outside the tar file) as an
  adjacency list of UUIDs. "Stored backwards" like the binary references file.
- Header fields (each 4 bytes): magic number (value not given), size of the adjacency
  list in bytes, number of entries (adjacency lists), checksum ("CRC2" per docs — read
  CRC32, see C3).
- Data, per adjacency list: UUID of the source segment; size (count) of its adjacency
  list; an unordered enumeration of target-segment UUIDs.
- Field widths for the counts and the exact serialization order relative to the header
  are NOT documented.

### 1.11 Binary references file (.brf)

(Source: `tar.md`, "Binary references files"; `changes.md`, "Binary references index".)

- Purpose: per-tar index of external binary references (blob IDs), so Blob Store GC does
  not need to read every segment (`changes.md`). Introduced in Oak Segment Tar 0.0.4.
- Groups references **by generation first, segment ID next**. "Stored in reverse order to
  maintain the most important information at the end of the file."
- Header fields (each 4 bytes): magic number (value not given); size in bytes of the
  whole mapping structure; number of generations; checksum ("CRC2" per docs — read CRC32).
- Data layout, per generation: generation number; count of segment→references mappings;
  then per mapping: UUID of the referencing segment; number of referenced blobs; an
  unordered enumeration of blob IDs.
- The docs do NOT document: field widths of the counts, how a blob ID string is encoded
  (length prefix?), whether the generation entry contains full-generation/compacted
  fields (a later format adds these — not mentioned in the docs at all; see C10).
- Note the docs' copy/paste error: "Immediately after the graph header, the index data is
  stored" — in the *binary references* section; it means "after the binary references
  header".

### 1.12 Journal and manifest

- **Journal** (`overview.md`): a file recording successive root node record references;
  appended atomically, only after the referenced record is flushed. The most recent entry
  is the head state and the GC root. The docs do not specify the journal's line format
  (the log examples imply entries look like `UUID.hex-record-number`; the `recover-journal`
  tool backs up `journal.log` to `journal.log.bak.XXX` with a three-digit
  monotonically-increasing counter — `overview.md`, "Recover journal").
- **Manifest** (`changes.md`, "Storage format versioning"): every data folder created by
  Oak Segment Tar contains a manifest file, "a source of metadata for the whole
  repository", checked every time a data folder is opened. Fail-fast behavior:
  - Old implementation (oak-segment) opening a folder WITH a manifest → fails ("data
    format too new").
  - Oak Segment Tar opening a NON-EMPTY folder WITHOUT a manifest → fails ("data format
    too old").
  - The manifest's file name, format, and keys are NOT documented.

---

## 2. Storage format version history

(Source: `changes.md` — presented chronologically there; `onrc-memoirs.md` maps module
versions to Oak releases: Oak Segment Tar 0.0.x versions culminated in the module shipped
with **Oak 1.6**, the first release with working online revision GC.)

| Change | Jira | Since (module version) | On-disk effect |
|---|---|---|---|
| **Generation in segment headers** | OAK-3348 | Oak Segment Tar 0.0.2 | GC generation stored per segment at header offsets 10–13 as a 4-byte (big-endian) integer, in space that was "reserved" in the old oak-segment format. |
| **Stable identifiers** | OAK-3348 | 0.0.2 | Every node record gains a stable ID: an 18-byte string record referenced from the node record via an additional reference field (docs say 3 bytes; suspect — see C7). Worst-case +21 bytes per node record. |
| **Binary references index** | OAK-4201 | 0.0.4 | New `.brf` entry per tar file aggregating external binary references of all contained segments, keyed by generation then segment. |
| **Simplified segment/record format** | OAK-4631 | 0.0.10 | The hard limit on the number of segment references per segment was relaxed "to the point that it can now be considered irrelevant" (old format's limit caused premature segment flushes). NOTE: `changes.md` explicitly says most of the other changes proposed in OAK-4631 were reverted or never merged. |
| **Storage format versioning** | OAK-4295 | 0.0.10 | Segment header version byte incremented **11 (oak-segment) → 12 (oak-segment-tar)**. Manifest file introduced in the data folder; mutual fail-fast between old and new implementations. |
| **Logic record IDs** | OAK-4659 | 0.0.14 | Record offsets in references replaced with logical record numbers. New translation table in the segment header: **9 bytes per record = 4 (record number) + 1 (type) + 4 (offset)**, plus a new 4-byte header field for the table's entry count. Records become movable within a segment without breaking references. |
| **Root record types** | OAK-2498 | 0.0.16 | Enriched the 1-byte type field of the record table; notably added a record type for records pointing to **external binary data (BLOB_ID)** so the `.brf` index can be rebuilt from a single segment during recovery, without whole-repository context. |

Segment header **versions the documentation knows about**: 11 (legacy oak-segment,
unreadable by oak-segment-tar) and 12 ("currently set to 12", `records.md`).
**The documentation never mentions segment version 13 or any index/binary-reference
format v2** — this is a known documentation gap; the code is authoritative (see C4, C10).

GC-relevant release history (source: `onrc-memoirs.md`):

- **Oak 1.0–1.4** (module `oak-segment`, format version 11): online revision GC
  effectively never reclaimed anything (dense segment graph + OAK-3348 bug). Cleanup was
  reachability-based over the segment graph, with cycles possible (OAK-1828 / OAK-3864).
- **Oak 1.6** (first `oak-segment-tar`, format version 12): generation-based GC. GC
  generation is an integer starting at 0, incremented per OnRC run; stored in the segment
  header. Default cleanup retains current + previous generation (2 generations ⇒ ~24h
  minimal retention with daily GC). Bulk segments (no header ⇒ no generation) are still
  collected by graph reachability. Cross-generation record references are prevented at
  write time by rewriting stale records (deduplication caches keyed by stable ID for
  nodes). Existing customers had to **migrate** their repositories to the new format.
- **Oak 1.8**: checkpoints compacted by sequential rebasing; **tail compaction**
  introduced. The GC generation was generalized from a simple integer to a
  **`GCGeneration` triple: (generation, full generation, compacted flag)**:
  - `generation`: incremented on every GC cycle, full or tail;
  - `fullGeneration`: incremented only on full GC cycles;
  - `isCompacted`: set only on segments created by compaction, never by normal writes;
    normal writes inherit generation/fullGeneration of the previous compaction with the
    flag cleared.
  - Reclaimability rule (with `H` = segment of current head, `n` = retained
    generations): `S` is old iff `H.generation − S.generation >= n`; `S` is in the same
    compaction tail as `H` iff `S.isCompacted && S.fullGeneration == H.fullGeneration`;
    `S` is reclaimable iff old and not in the same tail. (Oak 1.8.0 had bug OAK-7132 in
    this logic; fixed in 1.8.1.)
  - **The docs do not say where fullGeneration/compacted are persisted** (they must be —
    per segment and per index entry — since reclaimability is decided from them). This is
    exactly what segment version 13 / index v2 / brf v2 encode in the code; the
    documentation simply never describes it. Cross-check required (C4, C10).
  - As of Oak 1.8, `RetainedGenerations` is fixed to 2 and cannot be modified
    (`overview.md`, SegmentRevisionGarbageCollection MBean).
- **Oak 1.10**: cleanup reordered to run *before* compaction (OAK-7445, default via
  OAK-7550). Purely behavioral; no documented format change.

---

## 3. Garbage collection model

(As it affects a reader; sources: `overview.md`, `onrc-memoirs.md`.)

- GC = estimation → compaction → cleanup (Oak ≥1.10: cleanup before compaction).
- Compaction copies the reachable head state (and checkpoints) into segments of a new
  generation; cleanup deletes segments of reclaimable generations and rewrites tar files
  (generation-letter bump) when they shrink ≥25% (`tar.md`).
- Consequences for a reader:
  - Multiple generations of the same logical content can coexist; the journal's latest
    entry defines the head.
  - Two node records with different record IDs may still be the same logical node —
    equality must be decided by **stable ID**, not address (`changes.md`,
    `onrc-memoirs.md`).
  - Checkpoints are ordinary children under the `checkpoints` node of the *super root*
    (`onrc-memoirs.md`, Oak 1.8: checkpoints are "links to (previous) root node states
    from a child node under the `checkpoints` node of the super root"). The path shown in
    compaction logs is `checkpoints/<uuid>/root`, and the actual content root is the
    `root` child of the super root (log line "compacting root.").
  - A `SegmentNotFoundException` is the canonical failure when a referenced segment has
    been reclaimed or lost (`onrc-memoirs.md`).
  - Emptied tar files are first *marked* for deletion and physically removed later by a
    background task (`overview.md`, cleanup logging).

---

## 4. Facts the code may contradict — MUST be cross-checked

Listed prominently, as required. "Docs say X" below always means the official markdown;
each item states why it is suspect.

- **C1 — TAR header checksum.** `tar.md` claims Oak sets the TAR header *checksum* field
  to an "uninteresting value". Standard TAR requires a valid header checksum, and the Oak
  code is known to compute one; a reader that validates TAR checksums should not assume
  they are garbage, and a witness comparison against the code (TarWriter) is required.
- **C2 — "name of linked file (1 byte)".** `tar.md` gives the linkname field as 1 byte.
  In the POSIX/ustar TAR header the linkname field is **100 bytes** (and the docs' own
  field-size arithmetic doesn't reach 512). Documentation error; trust the TAR standard
  and the code.
- **C3 — "CRC2".** `tar.md` says segment entry names are `UUID.CRC2` and calls the `.brf`
  and `.gph` checksums "CRC2 checksums", while the `.idx` section says **CRC32**. "CRC2"
  is a typo for CRC32 throughout. The exact rendering of the checksum in the entry name
  (decimal vs hex, padding width, signedness) is NOT documented and must come from the
  code.
- **C4 — Segment format version.** `records.md` states the version byte is "currently set
  to 12". The docs never mention **version 13**, which the code introduced for the Oak
  1.8 `GCGeneration` (full generation + compacted flag) persistence. The documented
  32-byte header layout (generation at 10–13, segrefcount at 14–17, reccount at 18–21)
  describes version 12; version 13's use of the reserved bytes is undocumented here.
  Trust the code.
- **C5 — Record offset semantics: internal contradiction.** `tar.md` ("Segment files")
  says the record offset is "counting from the end of the segment. The actual position
  ... can be obtained by computing `(segment size - offset)`." `records.md` ("Record
  numbers and offsets") instead says the offset is relative to a theoretical 256 KiB
  segment and the position is `SIZE - 256KiB + OFFSET`. These formulas agree **only** for
  a full 256 KiB segment (if `tar.md` meant `size - (256K - offset)`), and as literally
  written they disagree even then. The `records.md` formula (`SIZE − 262144 + OFFSET`) is
  the one consistent with the code's addressing; treat `tar.md`'s phrasing as wrong.
- **C6 — Meaning of segment-reference index 0.** `records.md` says segment field 0 = the
  current segment, and the header stores `segrefcount` UUIDs. The docs never state
  explicitly that reference index `k` (k ≥ 1) maps to the (k−1)-th UUID in the table —
  i.e., that the table is 1-based from the record-ID side. An implementer must confirm
  the exact indexing convention from the code.
- **C7 — Stable-ID reference "3 bytes" and "18-byte string".** `changes.md` says the
  stable ID is an 18-byte string record referenced by "an additional 3-bytes-long
  reference field". A record ID in the current (post-0.0.14) format is segment field
  (2 bytes) + record number (4 bytes) = 6 bytes, not 3; the "3-bytes" figure appears to
  predate the logic-record-ID change (when references were 3-byte offsets) and the
  "18-byte" content layout (msb 8 + lsb 8 + 2-byte offset?) may likewise be stale.
  Cross-check the stable-ID serialization (length and content) in the code
  (SegmentNodeState / DefaultSegmentWriter).
- **C8 — "Stored backwards" is undefined.** For `.idx`, `.gph`, `.brf` the docs say the
  file is "stored backwards"/"in reverse order" with the header listed magic-first, but
  never define the actual byte order on disk (is the magic the FIRST or the LAST 4 bytes
  of the entry? is the header before or after the data?). The code writes these entries
  with the data first and a **footer** (checksum, count, size, magic) at the end so a
  reader can parse from the tail; the docs' "header ... magic number ... size ..." order
  must not be taken as the physical byte order. Resolve exact order and field positions
  from the code (IndexWriter/IndexLoader, GraphLoader, BinaryReferencesIndexWriter/Loader).
- **C9 — Index entry field widths omitted.** `tar.md` gives 8+8 bytes for the UUID halves
  but omits the widths of offset, size, and generation (code: 4+4+4 in V1; V2 entries are
  wider). Docs also never mention that entry `offset` points at the TAR *header block* vs
  the payload — code must decide.
- **C10 — No mention of index V2 / binary-references v2 / graph magic values.** The docs
  describe a single format for `.idx`, `.gph`, `.brf` with unspecified magic values. The
  code distinguishes **index format V1 vs V2** (V2 adds full generation + compacted flag
  per entry, mirroring segment version 12 vs 13) and correspondingly versioned `.brf`
  formats. Everything about V2, and every magic constant, must come from the code.
- **C11 — Bucket fan-out 255.** `records.md` says a bucket holds "at most 255
  references" while the list diagram shows `record ID 255` as the last of what could be
  read as 255 or 256 slots. Confirm the exact fan-out constant from the code
  (ListRecord).
- **C12 — Template property-pair limit.** `records.md`: "up to N (exact size TBD,
  N ~ 256)" — explicitly not committed. Get the real limit and the template's byte layout
  from the code (docs provide none).
- **C13 — Map leaf threshold phrasing.** "If a bucket contains less than 32 entries, then
  it is stored directly" — boundary condition (< 32 vs ≤ 32) and the top-level
  small-map/leaf threshold must be confirmed from code (MapRecord constants), as must the
  key hash function, which the docs never specify.
- **C14 — Value-record boundaries.** The medium-value upper bound 16511 (= 128 + 2^14 −
  1) and the "long value up to 2^61" phrasing come with no statement of how the length
  bias is encoded (value stored = length − 128 for medium, length − 16512 for long?).
  The bias/encoding must be taken from the code; the docs give only ranges.
- **C15 — External value record contents.** `tar.md`/`records.md` say an external value
  record "contains the length of the value and a string reference (up to 4kB in length)"
  — but the bit pattern `1110xxxx` allocates only 12 bits to the *reference string
  length*, and nothing is said about where the value's own length lives, nor about the
  BLOB_ID record type's layout. Code required.
- **C16 — `.brf` grouping key.** Docs say the `.brf` catalog is "indexed by the
  generation of the segments"; post-1.8 the key in code is the full GCGeneration triple.
  Cross-check.

---

## 5. Operational knowledge for a reader implementation

(Sources: `overview.md` Tools section, `tar.md`, `onrc-memoirs.md`.)

**File access / memory mapping**

- Memory-mapped access is the default on 64-bit systems; plain file access on 32-bit
  systems. On Windows, regular file access is always enforced and the `--mmap` option is
  ignored (`overview.md`, Compact). Tools accept `--mmap [Boolean]` (check/compact/
  iotrace; default `true` for check and iotrace).

**Caching**

- Segment cache default size: **256 MB** (`overview.md`, IOTrace `--segment-cache`
  default).
- For remote segment stores, an optional persistent disk cache exists, default cap
  **50 GB** (`overview.md`, Check/Compact `--persistent-cache-size-gb`).
- IO trace sample shows single-segment reads of ~181 KB served from a tar file,
  confirming segment-sized read granularity (`overview.md`, IOTrace example CSV).

**Typical sizes / limits recap**

- TAR block: 512 bytes; tar terminated by 2 zero blocks (`tar.md`).
- Max segment size: 256 KiB = 262,144 bytes (`overview.md`, `records.md`).
- Block record: up to 4 kB; bulk segments are 4096-byte blocks + remainder
  (`records.md`).
- Map record: ≤ 2^29 − 1 entries; writer soft/hard limits 400M / 500M / 536M
  (`records.md`).
- Tar files: `dataNNNNNx.tar`, 5-digit sequence number + generation letter; rewritten
  (letter bump) when cleanup would shrink them ≥ 25% (`tar.md`).
- Repository sizes in GC log examples run to tens of GB across thousands of tar files
  (e.g. `data01415a.tar`), so a reader must handle large tar sets.

**Read path (documented strategy)**

1. Open the data folder; expect a **manifest** (fail if a non-empty folder lacks one —
   `changes.md`).
2. Read `journal.log` (name per `overview.md`, Recover journal) and take the most recent
   record reference as head; entries are references to root node records, most recent
   last.
3. For each tar file, parse from the **end**: index first (locate segments), then graph,
   then binary references (`tar.md`). Use interpolation (or binary) search over the
   UUID-sorted index (`tar.md`).
4. Resolve the head record ID: UUID → tar file via index → segment payload → parse
   version-12/13 header → record table → offset normalization
   `pos = size − 262144 + offset` (`records.md`).
5. Distinguish bulk vs data segments by the UUID nibble (`a`/`b`) (`records.md`).

**Error/recovery behavior (documented)**

- Missing segments surface as `SegmentNotFoundException`; the `check` tool walks journal
  revisions newest→oldest until the first fully consistent one; `--fail-fast` stops at
  the first inconsistency (`overview.md`, Check).
- `recover-journal` rebuilds a journal by scanning all segments for potential head
  states, sorting old→new, and consistency-checking until the first consistent head;
  checkpoints are NOT checked; older recovered revisions may still be inconsistent;
  the old journal is backed up as `journal.log.bak.XXX` before replacement
  (`overview.md`, Recover journal).
- A reader should tolerate: tar files with reclaimed ("missing") segments referenced from
  older revisions, multiple journal entries of which only the newest is guaranteed
  consistent, and tar files marked-for-deletion still present on disk.
- The `compact --force` flag "upgrades the Segment Store to the latest version, which is
  incompatible with older versions. There is no way to downgrade" (`overview.md`) —
  further evidence that more than one on-disk version exists beyond documented v12.
