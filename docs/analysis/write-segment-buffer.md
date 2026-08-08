# Writer Specification: Building Segment Buffers

Subsystem: `SegmentBufferWriter` and its supporting mutable tables. This document
specifies exactly how oak-segment-tar **assembles the bytes of a new data
segment in memory** before handing it to the store, byte for byte. It builds on
the reader-side layout in [segment-layer.md](segment-layer.md) (header layout,
offset addressing, reference tables) and does not repeat it; anything cited
there is referenced by section.

Primary sources (all under
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/`):

| File | Role |
|---|---|
| `SegmentBufferWriter.java` | the buffer state machine: `newSegment`, `prepare`, `writeXxx`, `flush` |
| `SegmentBufferWriterPool.java` | pooling of writers per (thread, generation); writer-id numbering |
| `MutableRecordNumbers.java` | record-number → (offset, type) table under construction |
| `MutableSegmentReferences.java` | segment-reference table under construction |
| `Segment.java` | constants (`HEADER_SIZE` etc.), `align`, writer-facing constructor |
| `SegmentTracker.java` | fresh segment UUID generation |
| `SegmentVersion.java` | version byte |
| `BinaryUtils.java` | big-endian primitive serialization |
| `RecordWriters.java` | the `RecordWriter.write` protocol and the VALUE writer used for the meta record |

---

## 1. Writer state

`SegmentBufferWriter` (fields, `SegmentBufferWriter.java` lines 96–138) is a
single-segment builder. State:

- `buffer: byte[262144]` — always allocated at full `MAX_SEGMENT_SIZE = 1 << 18`
  (`Segment.MAX_SEGMENT_SIZE`, `Segment.java` line 90). Records are written
  **from the end toward the beginning** (OAK-629); the 32-byte header sits at
  `buffer[0..31]` during construction.
- `length: int` — bytes already allocated for records, counted **from the end**
  of the buffer.
- `position: int` — current raw-write cursor. `prepare` sets it to
  `buffer.length - length`; the `writeXxx` methods advance it upward
  (left-to-right within a record).
- `recordNumbers: MutableRecordNumbers`, `segmentReferences: MutableSegmentReferences`
  — reset per segment.
- `dirty: boolean` — set by every `writeByte/Short/Int/Long/RecordId/Bytes`;
  cleared at the end of `newSegment`. `flush` is a no-op when `dirty == false`
  (`flush`, line 312: `if (dirty) { ... }`). Consequence: **a segment
  containing only the meta-info record is never written to disk.**
- `gcGeneration: GCGeneration` — fixed at construction. `execute()` asserts the
  requested generation equals the writer's
  (`Validate.checkState(gcGeneration.equals(this.gcGeneration))`, line 156). One
  writer instance never mixes generations.
- `wid: String` — writer id; if `null` was passed, `"w-" + identityHashCode(this)`
  (constructor, lines 144–146).

Instances are explicitly **not thread safe** (class javadoc); a single-threaded
port needs exactly one live writer per GC generation.

## 2. `newSegment(store)` — buffer initialization

`SegmentBufferWriter.newSegment` (lines 178–221). Executed lazily on the first
`prepare` (line 388–391) and again at the end of every `flush`.

Pseudocode (every byte):

```
buffer = new byte[262144]            // zero-filled (Java array init)
buffer[0] = 0x30                     // '0'
buffer[1] = 0x61                     // 'a'
buffer[2] = 0x4B                     // 'K'
buffer[3] = 13                       // SegmentVersion.asByte(LATEST_VERSION) = V_13
buffer[4] = 0                        // "reserved" — immediately overwritten below
buffer[5] = 0                        // "reserved" — immediately overwritten below

g  = gcGeneration.generation         // signed 32-bit
buffer[10..13] = big_endian_i32(g)   // GC_GENERATION_OFFSET = 10

fg = gcGeneration.fullGeneration     // signed 32-bit
if gcGeneration.isCompacted:
    fg = fg | 0x80000000             // Java int wrapping: sets the sign bit
buffer[4..7] = big_endian_i32(fg)    // GC_FULL_GENERATION_OFFSET = 4

// bytes 8..9 stay 0x0000; bytes 14..21 (the two counts) stay 0 until flush;
// bytes 22..31 stay 0.

length = 0
position = 262144
recordNumbers = empty MutableRecordNumbers
segmentReferences = empty MutableSegmentReferences

metaInfo = '{"wid":"' + wid + '","sno":' + idProvider.getSegmentIdCount()
         + ',"t":' + currentTimeMillis() + '}'
segment = Segment(idProvider.newDataSegmentId(), buffer,
                  recordNumbers, segmentReferences, metaInfo)

data = utf8(metaInfo)
RecordWriters.newValueWriter(data.length, data).write(this, store)  // record #0

dirty = false
```

Notes:

- The comment "reserved" on bytes 4–5 is a historical artifact: `newSegment`
  immediately overwrites bytes 4–7 with the full generation
  (`GC_FULL_GENERATION_OFFSET = 4`, `Segment.java` line 117). Only bytes 8–9
  (plus the unused header tail 22–31) are actually reserved-zero in a V13
  segment.
- **Version byte**: `SegmentVersion.LATEST_VERSION` is `V_13` (byte value 13),
  computed as the max of the enum (`SegmentVersion.java` lines 49–56). A writer
  port must always emit 13. Oak can read 12, but never writes it anymore.
- **Order matters for `sno`**: the meta-info string is built with
  `idProvider.getSegmentIdCount()` *before* `idProvider.newDataSegmentId()` is
  evaluated as the constructor argument on the next statement. `SegmentTracker`
  increments its counter inside `newSegmentId(long type)`
  (`SegmentTracker.java` line 138), so `sno` is the number of fresh segment ids
  (data **and** bulk) generated by this tracker **before** this segment's own
  id. First segment of a process: `sno = 0`. The counter is per-process
  (`AtomicInteger`, starts at 0 on every store open), not persistent.

### 2.1 The segment-info meta record (record number 0)

Written through the normal record pipeline (`RecordWriters.ArrayValueWriter`,
`RecordWriters.java` lines 343–376):

- `prepare(VALUE, len + 1, [], store)` — `len < 128` always in practice
  (`getSizeDelta` = 1 for small values). If the wid were pathologically long
  (total metaInfo ≥ 128 bytes = `Segment.SMALL_LIMIT`; the small encoding
  applies iff `len < 128`), delta 2 and the medium-length encoding would be
  used; keep wids short.
- Content bytes, written left-to-right at the record position:
  `byte(len)` followed by the UTF-8 bytes (small-value encoding, see
  segment-layer.md §10).
- Because the buffer is empty, `prepare` gives it `position =
  262144 - align(len + 1, 4)`: the meta record sits at the **very end** of the
  virtual 256 KiB segment, record number **0**, type `VALUE` (ordinal 4).

Exact string format (`newSegment`, lines 208–211) — note it is real JSON with
quoted keys, although the javadoc (line 168) shows the older `{wid=W,...}`
notation; the code is authoritative:

```
{"wid":"<wid>","sno":<decimal int>,"t":<decimal millis>}
```

- `<wid>`: the writer id string (see §7 for the pooled format like
  `"sys.0000"`; FileStore uses base names `"sys"` (normal writes), `"c"`
  (compaction), `"init"` (store initialization) — `FileStore.java` lines 146,
  202, 262).
- `<sno>`: `SegmentTracker.getSegmentIdCount()` as above.
- `<t>`: `System.currentTimeMillis()`.

Nothing parses this string on the read path for correctness; it is diagnostic.
**Required**: record 0 must exist and be a valid small VALUE record (tools like
`SegmentDump` and `RecordUsageAnalyser` read it; the guarantee documented in
the javadoc is "first string record in a segment"). The exact field values are
free, but a conforming port should emit the same shape.

## 3. Segment UUID generation

`SegmentTracker.newSegmentId(long type)` (`SegmentTracker.java` lines 136–142):

```
MSB_MASK = ~(0xfL << 12)        // clears bits 12..15 of msb
VERSION  = 0x4L << 12           // UUID "version 4" nibble
LSB_MASK = ~(0xfL << 60)        // clears top nibble of lsb
DATA     = 0xAL << 60
BULK     = 0xBL << 60

msb = (secureRandom.nextLong() & MSB_MASK) | VERSION
lsb = (secureRandom.nextLong() & LSB_MASK) | type    // type = DATA for newDataSegmentId
```

- Random source: one `java.security.SecureRandom` per tracker (line 51).
- The msb nibble at bit 12 is forced to `4` (looks like a random UUID version
  field). **The RFC-4122 variant bits are NOT set**: the entire top nibble of
  the lsb is replaced by `0xA` (data) or `0xB` (bulk) —
  `SegmentId.isDataSegmentId(lsb)` is `(lsb >>> 60) == 0xAL`
  (`SegmentId.java` lines 54–56). All other **120** bits are random (60 in the
  msb — bits 12–15 are forced — and 60 in the lsb — bits 60–63 are forced).
- `SegmentBufferWriter` only ever asks for **data** segment ids
  (`newSegment` line 212). Bulk ids (`0xB`) are allocated by
  `DefaultSegmentWriter` when externalizing large binaries.
- The Rust port must generate ids the same way (any CSPRNG is fine; nibble
  placement is the contract), and must go through a dedup table equivalent to
  `SegmentIdTable` only if it needs identity semantics in memory — the on-disk
  bytes only need msb/lsb.

## 4. `prepare(type, size, ids, store)` — space allocation

`SegmentBufferWriter.prepare` (lines 384–460). Called by
`RecordWriters.RecordWriter.write` (RecordWriters.java lines 69–72) **before**
any content byte of a record is written. `size` excludes the bytes for the
record ids in `ids`; `ids` is the exact collection of record-ids the record
content will contain (possibly with duplicates).

Exact arithmetic (all Java signed 32-bit int; `align(a, b) = (a + b - 1) & ~(b - 1)`,
`Segment.java` line 154):

```
assert size >= 0
if segment == null: newSegment(store)          // first use only

idCount    = ids.size()                        // duplicates counted
recordSize = align(size + idCount * 6, 4)      // RECORD_ID_BYTES = 6; RECORD_ALIGN_BITS = 2

// Pessimistic estimate: every id references a segment not yet in the table
recordNumbersCount = recordNumbers.size() + 1
referencedIdCount  = segmentReferences.size() + idCount
headerSize  = 32 + referencedIdCount * 16 + recordNumbersCount * 9
              // HEADER_SIZE + refs * SEGMENT_REFERENCE_SIZE + records * RECORD_SIZE
segmentSize = align(headerSize + recordSize + length, 16)

if segmentSize > 262144:
    // Refine: count only distinct segment ids not already referenced.
    newIds = { rid.segmentId : rid in ids, !segmentReferences.contains(rid.segmentId) }
    referencedIdCount = segmentReferences.size() + newIds.size()
    headerSize  = 32 + referencedIdCount * 16 + recordNumbersCount * 9
    segmentSize = align(headerSize + recordSize + length, 16)

if segmentSize > 262144:
    if dirty:
        flush(store)                            // writes current segment, starts fresh one
        return prepare(type, size, ids, store)  // recurses exactly once
    throw IllegalArgumentException("Record too big: ...")

length  += recordSize
position = 262144 - length
recordNumber = recordNumbers.addRecord(type, position)   // consecutive: 0, 1, 2, ...
return RecordId(segment.segmentId, recordNumber)
```

Facts that matter for a port:

- **The refined estimate still over-counts self-references.** An id pointing
  into the *current* segment is never added to `segmentReferences`
  (`writeSegmentIdReference` returns 0 for it, lines 265–271) and is never in
  `contains()`, yet `prepare` counts it in `newIds`. This is a deliberate
  conservative over-estimate; it can trigger a flush slightly earlier than
  strictly needed. Reproducing it is not required for correctness (segment
  boundaries are not observable invariants), but reproducing it keeps segment
  sizes statistically identical to Oak's.
- **Recursion depth is exactly ≤ 2** (comment, lines 427–435): after `flush`,
  the fresh segment has `dirty == false`, so a second failure throws. The hard
  failure bound for a single record: with the fresh segment already containing
  the meta record (`recordNumbers.size() == 1`, `length == align(metaLen+1,4)`),
  the record must satisfy
  `align(32 + refs*16 + 2*9 + recordSize + metaAligned, 16) <= 262144`.
- **Record numbers are dense**: `MutableRecordNumbers.addRecord` returns
  `size++` (`MutableRecordNumbers.java` lines 112–120), so numbers are
  `0, 1, 2, …` in allocation order, and the record table written at flush lists
  them ascending with strictly descending offsets. (Readers tolerate arbitrary
  numbers — see segment-layer.md §7 — but the writer always produces dense
  consecutive ones. Keep this; `SegmentParser`-based tooling and the
  binary-search reader both rely on ascending order in the table.)
- **The offset recorded is the raw buffer position** (`position =
  262144 - length`), i.e. relative to the *virtual 256 KiB segment end* — it is
  **not** rewritten when the segment is trimmed at flush. This is exactly the
  addressing scheme in segment-layer.md §8.
- `prepare` only allocates. If the caller then writes fewer bytes than
  reserved, the gap is whatever the zero-filled buffer holds (alignment padding
  is always zero bytes for the same reason). Overrunning the reservation is
  undefined behavior (class javadoc).

## 5. Writing record content

All primitive writers are big-endian and advance `position`
(`BinaryUtils.java`; `SegmentBufferWriter.writeByte/Short/Int/Long/Bytes`,
lines 223–285). Each sets `dirty = true`.

`writeRecordId(recordId)` (lines 248–259):

```
require segmentReferences.size() + 1 < 0xffff     // max 65533 existing refs
ref = (recordId.segmentId == currentSegmentId) ? 0
      : segmentReferences.addOrReference(recordId.segmentId)
writeShort(u16(ref))            // 2 bytes BE; 0 = self-reference
writeInt(recordId.recordNumber) // 4 bytes BE
```

- `MutableSegmentReferences.addOrReference` (`MutableSegmentReferences.java`
  lines 60–79) dedups by segment id and numbers references **from 1** in first-use
  order (`ids.add(id); number = ids.size()`).
- The limit check throws `IllegalStateException` *before* writing when the
  table already holds 0xFFFE entries; combined with `prepare`'s size math the
  reference count in practice stays far below this (a full table alone would
  be 65534 × 16 bytes ≈ 1 MiB > segment size). The check exists for the
  pathological many-duplicate-ids case.
- Self-reference **0 is never stored in the table** and does not consume a
  table slot; segment-layer.md §6 describes the reader mapping
  (`reference r → table entry r - 1`).

## 6. `flush(store)` — finalizing the segment

`SegmentBufferWriter.flush` (lines 310–364). No-op if `!dirty`. Otherwise:

```
refCount = segmentReferences.size()
buffer[14..17] = big_endian_i32(refCount)        // REFERENCED_SEGMENT_ID_COUNT_OFFSET
recCount = recordNumbers.size()
buffer[18..21] = big_endian_i32(recCount)        // RECORD_NUMBER_COUNT_OFFSET

totalLength = align(32 + refCount*16 + recCount*9 + length, 16)
if totalLength > 262144: throw IllegalStateException("Too much data for a segment ...")
      // cannot happen if prepare's accounting was honored; indicates buffer corruption

pos = 32                                          // HEADER_SIZE
if pos + totalLength <= 262144:
    // trim: move the 32-byte header to the start of the tail window
    memmove(buffer[262144 - totalLength .. 262144 - totalLength + 32], buffer[0..32])
    pos = 32 + (262144 - totalLength)
else:
    // segment nearly full (totalLength > 262112): keep in place, ship all 256 KiB.
    // May leave a zero gap between the tables and the record area.
    totalLength = 262144
    // pos stays 32; header remains at buffer[0..31]

for id in segmentReferences (insertion order, ref 1 first):
    pos = writeLong(buffer, pos, id.msb)          // 8 bytes BE
    pos = writeLong(buffer, pos, id.lsb)          // 8 bytes BE

for entry in recordNumbers (insertion order == number order 0,1,2,...):
    pos = writeInt (buffer, pos, entry.recordNumber)   // 4 bytes BE
    pos = writeByte(buffer, pos, entry.type.ordinal)   // 1 byte
    pos = writeInt (buffer, pos, entry.offset)         // 4 bytes BE (raw virtual offset)

store.writeSegment(segment.segmentId, buffer, 262144 - totalLength, totalLength)
newSegment(store)                                 // immediately start a fresh segment
```

Byte-exact consequences:

- **What is handed to the store** is the *tail window* of the working buffer:
  `(buffer, offset = 262144 - totalLength, length = totalLength)`. Layout of
  that window: 32-byte header, segment-reference table, record table,
  zero padding up to the 16-alignment, then the record data ending exactly at
  the window's end. `totalLength` is always a multiple of 16 except in the
  keep-in-place branch where it is exactly 262144 (which is also a multiple of
  16, so: always a multiple of 16).
- **Record offsets are not rewritten.** They remain positions in the virtual
  256 KiB address space; readers subtract `MAX_SEGMENT_SIZE - totalLength`
  (`Segment.getAddress`: `data.size() - (MAX_SEGMENT_SIZE - offset)`,
  `Segment.java` line 425).
- The keep-in-place branch triggers when `32 + totalLength > 262144`, i.e.
  `totalLength ∈ (262112, 262144]`. In that branch the shipped length is
  forced to the full 262144 even if `totalLength` was, e.g., 262128 — this is
  the only case where a written data segment's length is not
  `align(header+tables+records, 16)` and where a zero gap can sit between the
  tables and the first (lowest-offset) record. The in-code comment claims
  ">252kB"; the precise condition is the one above.
- **RecordType ordinals** (write side of segment-layer.md §7): `LEAF=0,
  BRANCH=1, BUCKET=2, LIST=3, VALUE=4, BLOCK=5, TEMPLATE=6, NODE=7, BLOB_ID=8`
  (`RecordType.java` declaration order).
- After the store write, `newSegment(store)` runs unconditionally — it
  allocates a **new segment id** and writes a new meta record into the fresh
  buffer. A port that flushes once at the end of a job will therefore burn one
  extra segment id and increment `sno`; the fresh buffer is clean
  (`dirty=false`) and is simply dropped, never written. Harmless, but expected.
- Durability is **not** this layer's job: `store.writeSegment` lands in the
  `TarWriter`/`SegmentWriteQueue`; fsync ordering, journal interplay and crash
  recovery are specified in filestore-layer.md and the write-path tar/journal
  documents. Within this layer the only ordering guarantee is: a segment is
  complete and internally consistent at the moment `writeSegment` is called,
  and every record id it ever returned for this segment refers to data fully
  inside the shipped window.

### 6.1 Error behavior

- `prepare` throwing `IllegalArgumentException` ("Record too big"): when the
  failure is hit on the post-flush retry, it leaves the freshly flushed
  previous segment on disk and a clean empty buffer in memory — nothing to
  roll back. When the writer was already clean (`dirty == false`, e.g. the
  very first record is oversized), it throws without flushing anything.
- `flush` throwing `IllegalStateException` ("Too much data") indicates
  writer-internal corruption (cannot occur with correct `prepare` accounting).
  The buffer is not shipped; the store is untouched by this segment.
- A crash between `store.writeSegment` and journal update: the segment is in a
  tar file but unreferenced by the journal head — Oak startup tolerates
  unreferenced segments (they are garbage-collected later); see
  filestore-layer.md recovery semantics.
- Nothing in this layer writes to disk directly; cancellation mid-record simply
  abandons the in-memory buffer. Abandoned buffers must never be shipped — a
  partially written record with a dangling allocated record number would be
  structurally invalid only if some *other* shipped record referenced it, which
  cannot happen because record ids escape the writer only after the content
  write completes.

## 7. GCGeneration binding and the writer pool

`SegmentBufferWriterPool` (`SegmentBufferWriterPool.java`) exists to give each
(thread, GCGeneration) pair its own `SegmentBufferWriter` and to make
`flush()` flush all of them. What a single-threaded port needs:

- A writer is created per generation via
  `new SegmentBufferWriter(idProvider, getWriterId(), gcGeneration)`
  (`newWriter`, lines 313–316). The generation given to the new writer is the
  one **requested by the `execute(gcGeneration, op)` call**, not the supplier's
  current value — the pool's `Supplier<GCGeneration>` is consulted only by
  `getGCGeneration()`, which callers (e.g. `DefaultSegmentWriter`) use to pick
  the generation they then pass to `execute`.
- **Writer id format** (`getWriterId`, lines 318–333): `wid + "." + NNNN`,
  where `NNNN` is a 4-digit zero-padded counter starting at `0000` (field
  `writerId` starts at −1, pre-incremented) and wrapping to `0000` after
  `9999`. So the first pooled writer for base name `"sys"` stamps
  `{"wid":"sys.0000",...}` into its segments' meta records.
- Both pool flavors (`GLOBAL` with borrow/return + disposed-set, and
  `THREAD_SPECIFIC` with a read-write lock) reduce, for one thread and one
  generation, to: reuse the same `SegmentBufferWriter` for all writes; on
  `flush(store)` flush it and **discard it from the pool** (`writers.clear()`,
  lines 156 / 227), so the next write after a pool flush gets a brand-new
  writer (new wid suffix, new segment). A single-threaded port can model this
  as: one current writer per generation, dropped on flush.
- `execute(gcGeneration, op)` routes each operation to the writer of exactly
  that generation; the plain `SegmentBufferWriter.execute` instead *asserts*
  the generation matches (line 156). Compaction relies on this to write old-
  and new-generation records concurrently — a port doing compaction needs one
  writer per target generation, nothing more.

Generation numbers themselves (how they advance for full/tail compaction) are
out of scope here; this layer only serializes the three fields it is given:
`generation` → bytes 10–13, `fullGeneration | (compacted ? 0x80000000 : 0)` →
bytes 4–7 (§2).

## 8. What is required vs. optimization

| Behavior | Status |
|---|---|
| Header magic/version/generation bytes exactly as §2 | **Required** — readers validate magic and version (`SegmentVersion.isValid`) and GC uses the generation fields |
| Meta VALUE record as record 0 at segment end | **Required in practice** (tooling assumes it; `Segment.getSegmentInfo` blindly reads the *first record-table entry* as a string — record 0 in every writer-produced segment) |
| Meta record field values (`wid`, `sno`, `t`) | Informational — any well-formed values acceptable |
| Dense ascending record numbers, descending offsets | **Keep** — matches every segment Oak ever wrote; table must be ascending by number for the reader's binary search |
| Reference numbering from 1 in first-use order, 0 = self | **Required** — encoded in every record-id short |
| Offsets relative to virtual 256 KiB end | **Required** — reader addressing depends on it |
| `totalLength` 16-aligned; full-262144 keep-in-place branch | **Required** for the size arithmetic readers perform; the keep-in-place branch specifically must be reproduced or a >262112 segment becomes unaddressable |
| Exact flush-trigger arithmetic incl. self-reference over-count | Optimization/parity — segment boundaries are free, but staying within the same bounds guarantees no oversize segment |
| One writer per generation, generation stamped once | **Required** — mixing generations inside a segment breaks cleanup |
| UUID nibbles: msb version nibble 4, lsb top nibble A (data)/B (bulk) | **Required** — `isDataSegmentId` gates header parsing everywhere |
| SecureRandom for the other 120 bits | Required in spirit: ids must be globally unique; collisions corrupt the store |

## AEM safety invariants

The Rust segment-buffer builder must guarantee, for every data segment it
ships to the store:

1. Bytes 0–3 are exactly `30 61 4B 0D` (`"0aK"` + version 13).
2. Bytes 4–7 hold the full generation with bit 31 set iff the segment was
   produced by a compactor writer; bytes 10–13 hold the generation; both
   big-endian signed 32-bit; bytes 8–9 and 22–31 are zero.
3. Bytes 14–17 and 18–21 hold the exact reference-table and record-table entry
   counts, and the tables that follow the header have exactly
   `refCount*16 + recCount*9` bytes in the layout of segment-layer.md §6–7.
4. Every segment id generated has msb bits 12–15 = `0100` and lsb bits 60–63 =
   `1010` (data) or `1011` (bulk), all other bits from a cryptographic RNG,
   and is never reused.
5. Record numbers in the table are `0..recCount-1` ascending; entry offsets are
   the original virtual-256 KiB positions, 4-byte aligned, strictly decreasing;
   record 0 is the segment-info VALUE record ending at virtual offset 262144.
6. Shipped length is `align(32 + refCount*16 + recCount*9 + recordBytes, 16)`,
   except when that exceeds 262112, in which case it is exactly 262144 — and
   the shipped window always ends at the virtual segment end (trim from the
   front only, header memmoved to the window start).
7. Every record-id written into record content is `u16 reference` (0 = this
   segment; else 1-based first-use index into the reference table) followed by
   `u32 record number`, big-endian, and the referenced segment id is present
   in the table for every nonzero reference.
8. All record content for every allocated record number was fully written
   before the segment is shipped; no record id minted from this segment ever
   escapes to a caller before its content bytes are complete.
9. Every record, including alignment padding, lies entirely within
   `[headerEnd(tables) , 262144)` of the virtual segment — enforced by the
   `prepare` bound `align(header + tables + recordBytes + length, 16) ≤ 262144`.
10. All records in one segment carry the writer's single GCGeneration; a new
    generation always starts a new segment.
11. Never ship a segment whose only record is the meta record (Oak's
    `dirty`-flag behavior) — empty segments waste ids but, more importantly,
    shipping must happen through the same tar/journal path so that AEM's
    recovery sees only complete segments.
12. *(added by verification)* The reference table contains no duplicates and
    never the current segment's own id, and holds at most 0xFFFE entries —
    every reference must fit an unsigned 16-bit value with 0 reserved for
    self-reference (`writeRecordId` enforces
    `segmentReferences.size() + 1 < 0xffff` before writing).
13. *(added by verification)* Every record-table type byte is a valid
    `RecordType` ordinal in `0..8` — readers evaluate
    `RecordType.values()[type]` unchecked, so any other value crashes every
    read of the segment.
14. *(added by verification)* Every segment shipped by this builder carries a
    **data** segment id (lsb top nibble `0xA`) — header/table parsing is gated
    on `isDataSegmentId` throughout Oak, so shipping this layout under a bulk
    (`0xB`) id would make readers treat the bytes as raw binary content.
