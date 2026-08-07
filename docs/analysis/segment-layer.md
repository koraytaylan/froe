# TarMK Segment Binary Layout Specification

Byte-exact specification of the Apache Jackrabbit Oak `oak-segment-tar` segment
format, extracted from the Java sources at
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment` (referenced
below by file name only) and the official documentation
(`oak-doc/src/site/markdown/nodestore/segment/records.md`). Scope: the raw
segment buffer as it appears inside a TAR entry — header, segment reference
table, record reference table, offset addressing, record-id serialization, and
segment-UUID semantics. Record *content* layouts (nodes, maps, templates, ...)
are out of scope except for the value-length prefix, which is defined at this
layer (`SegmentData.readLength`).

Every fact cites the Java file and constant/method it comes from.

---

## 1. Endianness

**Every multi-byte integer in the segment format is big-endian.**

Evidence:

* `BinaryUtils.java` (`writeShort`, `writeInt`, `writeLong`) writes the
  most-significant byte first (e.g. `writeShort` writes `(byte)(value >> 8)`
  then `(byte)(value)`).
* All reads go through `org.apache.jackrabbit.oak.commons.Buffer`, a thin
  wrapper over `java.nio.ByteBuffer`, whose byte order defaults to
  `ByteOrder.BIG_ENDIAN` and is never changed by the segment code
  (`SegmentDataV12.java` uses `buffer.getInt`, `buffer.getShort`,
  `buffer.getLong` directly).
* `records.md`: "All integers are stored in big endian format."

Java arithmetic note for the porter: all Java `int` arithmetic wraps modulo
2^32 (two's complement); `>>>` is an unsigned (logical) right shift, `>>` is
arithmetic. The formulas below are written with those semantics.

## 2. Segment kinds and UUID semantics

There are two kinds of segments — **data** and **bulk** — and the kind is
encoded *in the segment's 128-bit UUID itself*, not in the segment content.

A segment identifier is a UUID stored/handled as two 64-bit longs, `msb`
(most significant bits) and `lsb` (least significant bits), each big-endian
when serialized (16 bytes total: `msb` then `lsb`).

The discriminator is the most significant nibble of `lsb`
(`SegmentId.java`):

```
isDataSegmentId(lsb)  :=  (lsb >>> 60) == 0xA     // SegmentId.isDataSegmentId
isBulkSegmentId(lsb)  :=  (lsb >>> 60) == 0xB     // SegmentId.isBulkSegmentId
```

In the canonical UUID string `xxxxxxxx-xxxx-xxxx-Nxxx-xxxxxxxxxxxx`, `N` is
this nibble: `a` for data segments, `b` for bulk segments. (This nibble
occupies the position that RFC 4122 uses for the variant field.)

* **Data segments** have the header described in this document and may
  reference other segments.
* **Bulk segments** have **no header whatsoever**: the buffer is raw record
  data. `Segment.java` (constructor) wraps bulk buffers with
  `newRawSegmentData` (`SegmentDataRaw.java`), which only supports
  `readBytes`/`size` — every header accessor throws `IllegalStateException`
  ("invalid operation"). A bulk segment of `n` bytes consists of
  `n div 4096` block records of 4096 bytes each, followed by one block record
  of `n mod 4096` bytes if nonzero (`records.md`; block size constant
  `SegmentStream.BLOCK_SIZE = 1 << 12 = 4096`). Record numbers in a bulk
  segment are mapped through an identity table (`Segment.java`,
  `IdentityRecordNumbers`): record number == offset.
* Bulk segments have **no GC generation**: `Segment.getGcGeneration(data,
  segmentId)` returns the null generation (generation 0, full generation 0,
  not compacted) whenever `isDataSegmentId(lsb)` is false (`Segment.java`,
  `getGcGeneration`).

## 3. Global constants

All from `Segment.java` unless noted:

| Constant | Value | Meaning |
|---|---|---|
| `HEADER_SIZE` | 32 (0x20) | Fixed header size in bytes |
| `SEGMENT_REFERENCE_SIZE` | 16 (0x10) | Bytes per segment-reference entry |
| `RECORD_SIZE` | 9 | Bytes per record-reference entry |
| `RECORD_ID_BYTES` | 6 (= 2 + 4) | Serialized record id inside record data |
| `RECORD_ALIGN_BITS` | 2 | Records align to 4-byte (1 << 2) boundaries |
| `MAX_SEGMENT_SIZE` | 262144 (1 << 18, 256 KiB) | Virtual segment size |
| `SMALL_LIMIT` | 128 (1 << 7) | Small-value length limit (exclusive) |
| `MEDIUM_LIMIT` | 16512 ((1 << 14) + 128) | Medium-value length limit (exclusive) |
| `BLOB_ID_SMALL_LIMIT` | 4096 (1 << 12) | Max small blob-id length (record layer) |
| `GC_FULL_GENERATION_OFFSET` | 4 | Header offset of full generation (V13) |
| `GC_GENERATION_OFFSET` | 10 (0x0A) | Header offset of generation |
| `REFERENCED_SEGMENT_ID_COUNT_OFFSET` | 14 (0x0E) | Header offset of segref count |
| `RECORD_NUMBER_COUNT_OFFSET` | 18 (0x12) | Header offset of record count |

`SegmentData.java` duplicates the length limits as
`MAX_SMALL_LENGTH_VALUE = 1 << 7 = 128` and
`MAX_MEDIUM_LENGTH_VALUE = (1 << 14) + 128 = 16512` (identical values).

Alignment helper (`Segment.align`), used by the writer to pad the total
segment length to a 16-byte boundary and record sizes to 4 bytes:

```
align(address, boundary) := (address + boundary - 1) & ~(boundary - 1)
// boundary is a power of two; Java int arithmetic (wraps at 2^32)
```

## 4. Data segment overall structure

```
[fixed header: 32 bytes]
[segment reference table: segrefcount * 16 bytes]
[record reference table: reccount * 9 bytes]
[padding to align next record region]        (zero bytes)
[record data ... grows from the END of the buffer toward the header]
```

Records are written from the end of a 256 KiB virtual buffer toward the
beginning (`SegmentBufferWriter.java`, field `position`, comment "filled from
the end to the beginning, see OAK-629"). When flushed, the writer copies the
header + tables to `bufferLength - totalLength` and persists only the tail
window `[buffer.length - totalLength, buffer.length)`, where

```
totalLength = align(HEADER_SIZE
              + segrefcount * SEGMENT_REFERENCE_SIZE
              + reccount * RECORD_SIZE
              + length_of_record_data, 16)
```

(`SegmentBufferWriter.flush`; only the 32 header bytes are block-copied, the
two tables are then serialized immediately after them at the destination).
Consequence: **a stored data segment is
usually smaller than 256 KiB, its header is at buffer offset 0, and record
offsets in the record table remain relative to the virtual 256 KiB segment**
(see §8). If `HEADER_SIZE + totalLength > buffer.length` — i.e.
`totalLength > 262112`, within 32 bytes of the maximum (the Java comment's
">252kB" is only a loose paraphrase) —
the writer instead keeps `totalLength = 262144`, which may leave a zero gap
between the tables and the record data — readers must not assume the record
area begins immediately after the tables.

## 5. Fixed header (32 bytes)

### 5.1 Version 12 (`SegmentDataV12.java`, `SegmentBufferWriter.newSegment`)

| Offset (dec/hex) | Size | Field | Encoding |
|---|---|---|---|
| 0 / 0x00 | 3 | Magic signature | ASCII bytes `'0' 'a' 'K'` = `0x30 0x61 0x4B` (`SIGNATURE_OFFSET=0`, `SIGNATURE_LENGTH=3`) |
| 3 / 0x03 | 1 | Version | `0x0C` (12) (`VERSION_OFFSET=3`) |
| 4 / 0x04 | 6 | Reserved | Never read by the V12 reader (bytes 4–9). *Correction: the current writer (`SegmentBufferWriter.newSegment`) always emits version 13 (`LATEST_VERSION`) and stores the full generation in bytes 4–7; V12 segments only come from older writers, which left 4–9 zero.* |
| 10 / 0x0A | 4 | Generation | Signed 32-bit big-endian int (`GENERATION_OFFSET=10`) |
| 14 / 0x0E | 4 | Referenced-segment count (`segrefcount`) | Signed 32-bit BE int (`SEGMENT_REFERENCES_COUNT_OFFSET=14`) |
| 18 / 0x12 | 4 | Record count (`reccount`) | Signed 32-bit BE int (`RECORD_REFERENCES_COUNT_OFFSET=18`) |
| 22 / 0x16 | 10 | Reserved | Zero |

In V12 there is **no full generation and no compacted flag on disk**. The
reader defines (`SegmentDataV12.java`):

```
getFullGeneration() := getGeneration()   // same 4 bytes at offset 10
isCompacted()       := true              // constant
```

### 5.2 Version 13 (`SegmentDataV13.java`)

Identical to V12 except the 4 bytes at offset 4 are meaningful:

| Offset | Size | Field | Encoding |
|---|---|---|---|
| 4 / 0x04 | 4 | Full generation + compacted flag | Signed 32-bit BE int `fg`; `FULL_GENERATION_OFFSET = 4` |

```
getFullGeneration() := fg & 0x7fffffff      // low 31 bits
isCompacted()       := fg < 0               // i.e. bit 31 (0x80000000) set
```

The writer (`SegmentBufferWriter.newSegment`) produces this with
`fullGeneration |= 0x80000000` when `gcGeneration.isCompacted()`.

Header bytes 8–9 remain reserved (zero) in V13; generation, segrefcount and
reccount are at the same offsets as V12.

### 5.3 Version dispatch and validation

* `SegmentDataLoader.newSegmentData` switches on the byte at offset 3:
  `12 -> SegmentDataV12`, `13 -> SegmentDataV13`, anything else →
  `IllegalArgumentException("invalid segment buffer")`. **Fatal.**
* `Segment.java` (constructor from `Buffer`) additionally checks
  `getSignature().equals("0aK")` and `SegmentVersion.isValid(version)`;
  failure is fatal (`IllegalStateException` with a hex dump).
* `SegmentVersion.java`: valid versions are exactly `V_12 = 12` and
  `V_13 = 13`; `LATEST_VERSION = V_13`. Versions 10/11 belong to the legacy
  `oak-segment` module and are invalid here (class comment).
* The version check is only applied to **data** segments; bulk segment
  buffers are accepted as-is.

## 6. Segment reference table

Starts immediately after the fixed header, i.e. at buffer offset
`HEADER_SIZE = 32` (`SegmentDataV12.getSegmentReferenceBase(i) = 32 + i*16`).
`segrefcount` entries, 16 bytes each:

| Rel. offset | Size | Field | Encoding |
|---|---|---|---|
| 0 | 8 | `msb` of referenced segment UUID | 64-bit BE (`SEGMENT_REFERENCE_MSB_OFFSET = 0`) |
| 8 | 8 | `lsb` of referenced segment UUID | 64-bit BE (`SEGMENT_REFERENCE_LSB_OFFSET = 8`) |

**Reference numbering is 1-based**: reference value `r` (from a serialized
record id, §9) maps to table entry index `r - 1`
(`SegmentReferences.fromSegmentData`: `getSegmentReferenceMsb(reference - 1)`).
Reference `0` never appears in this table — it means "the current segment".

Limit: `SegmentReferences.fromSegmentData` enforces
`segrefcount + 1 < 0xffff`, i.e. **max 65533 (0xFFFD) referenced segments**
(fatal `IllegalStateException` "Segment cannot have more than 0xffff
references" otherwise). The writer performs the check `size() + 1 < 0xffff`
*before* adding a reference (`SegmentBufferWriter.writeRecordId`), which in
principle allows one more entry (65534) than the reader accepts — unreachable
in practice, since 65534 × 16-byte entries alone exceed the 256 KiB segment
size; a reader should apply the reader-side bound (≤ 65533). The lookup
(`SegmentReferences.getSegmentId`) checks `reference <= segrefcount`
(fatal `IllegalArgumentException` "Segment reference out of bounds").

Duplicate-free: the writer reuses an existing entry for an already-referenced
segment (`MutableSegmentReferences.addOrReference` via
`SegmentBufferWriter.writeSegmentIdReference`), so each referenced UUID
appears once; a reader need not deduplicate.

## 7. Record reference table

Starts immediately after the segment reference table, at buffer offset
`32 + segrefcount * 16` (`SegmentDataV12.getRecordReferenceBase(i) =
HEADER_SIZE + segrefcount * SEGMENT_REFERENCE_LENGTH + i *
RECORD_REFERENCE_LENGTH`). `reccount` entries, 9 bytes each
(`RECORD_REFERENCE_LENGTH = 9`):

| Rel. offset | Size | Field | Encoding |
|---|---|---|---|
| 0 | 4 | Record number | Signed 32-bit BE int (`RECORD_REFERENCE_NUMBER_OFFSET = 0`) |
| 4 | 1 | Record type | 1 byte, ordinal of `RecordType` (`RECORD_REFERENCE_TYPE_OFFSET = 4`) |
| 5 | 4 | Record offset | Signed 32-bit BE int, virtual offset in a 256 KiB segment (`RECORD_REFERENCE_OFFSET_OFFSET = 5`) |

Record type byte values (`RecordType.java` declaration order; the writer
stores `entry.getType().ordinal()` in `SegmentBufferWriter.flush`, the reader
maps back with `RecordType.values()[type]` in `ImmutableRecordNumbers`):

| Value | Type |
|---|---|
| 0 | LEAF |
| 1 | BRANCH |
| 2 | BUCKET |
| 3 | LIST |
| 4 | VALUE |
| 5 | BLOCK |
| 6 | TEMPLATE |
| 7 | NODE |
| 8 | BLOB_ID |

Any other type byte is undefined; `RecordType.values()[b]` throws
`ArrayIndexOutOfBoundsException` in Java (fatal). Note the type is only
consulted when *iterating* records (e.g. for diagnostics/GC); plain record
lookup by number ignores it.

### 7.1 Sort order and lookup

* The writer assigns record numbers **sequentially from 0** in allocation
  order (`MutableRecordNumbers.addRecord` returns `size++`) and emits the
  table by iterating in that order (`SegmentBufferWriter.flush`). Hence in a
  freshly written segment the table is sorted ascending by record number and
  record numbers are exactly `0..reccount-1` with no gaps.
* The reader does **not** assume density, but it **does assume the table is
  sorted ascending by record number** in one place:
  `RecordNumbers.fromSegmentData` computes
  `maxIndex = getRecordReferenceNumber(reccount - 1)` — i.e. it takes the
  *last* entry's record number as the maximum — then builds dense arrays
  `offsets[maxIndex + 1]` (initialized to -1) and `types[maxIndex + 1]`, and
  fills `offsets[recordNumber] = offset`, `types[recordNumber] = type` for
  every entry. If `reccount == 0` an empty table is used
  (`EMPTY_RECORD_NUMBERS`, every lookup yields -1).
* Lookup (`ImmutableRecordNumbers.getOffset`): direct array index;
  `recordNumber >= offsets.length` or an unfilled slot yields **-1** ("no
  offset associated"). *Added note:* only the upper bound is guarded — a
  **negative** record number indexes the array directly and throws
  `ArrayIndexOutOfBoundsException` in Java; a port should return -1 / error
  for negative record numbers. There is no binary search in the current code; a Rust
  implementation may use a binary search over the sorted table or the same
  dense-array strategy — both are valid given the sortedness invariant above.
  Caution: an entry whose record number exceeds the last entry's record
  number makes the Java algorithm write past `maxIndex`, which throws
  `ArrayIndexOutOfBoundsException` (fatal); a negative record number in the
  table likewise throws. Such a table violates the sorted invariant and can
  be treated as corrupt.
* A lookup result of -1 is not checked in `Segment.java`; subsequent reads
  with a -1 offset produce out-of-range buffer accesses (fatal in Java). A
  defensive port should treat "record number not in table" as a corruption
  error.

### 7.2 First record convention

The first record of every data segment (record number 0, the first table
entry) is a VALUE record containing the segment meta-info string
`{"wid":"...","sno":...,"t":...}` (`SegmentBufferWriter.newSegment`;
`Segment.getSegmentInfo` reads it via
`recordNumbers.iterator().next().getRecordNumber()`). Readers should not rely
on this for correctness, but `getSegmentInfo` does.

## 8. Offset addressing: virtual 256 KiB segment

Record offsets stored in the record reference table (and any offset derived
from them by adding a positive displacement) are **positions within a virtual
segment of exactly `MAX_SEGMENT_SIZE` = 262144 bytes whose *end* coincides
with the end of the actual buffer**. Conversion to an index in the actual
buffer of `size` bytes (`SegmentDataUtils.index`, used by every
`SegmentDataV12.read*` method; equivalently `Segment.getAddress`):

```
index(offset) := size - (MAX_SEGMENT_SIZE - offset)
             ==  size - 262144 + offset
```

where `size` is the actual segment buffer length (`buffer.limit()` /
`data.size()`). For a full 256 KiB segment this is the identity. Valid data
offsets therefore satisfy `262144 - size <= offset < 262144`; anything
producing a negative index is corrupt (Java would throw
`IndexOutOfBoundsException`). The same rule applies to bulk segments
(`SegmentDataRaw` uses the same `index` function for `readBytes`).

Record data alignment: every record's virtual offset is 4-byte aligned
(`RECORD_ALIGN_BITS = 2`; the writer allocates
`align(size + idCount * RECORD_ID_BYTES, 4)` bytes per record,
`SegmentBufferWriter.prepare`). Do not *rely* on alignment for parsing —
offsets come from the table — but it holds for well-formed segments.

Note: header/table fields (§5–§7) are addressed from the **start** of the
buffer with plain offsets; only record data uses the virtual-offset rule.

## 9. Record-id serialization inside record data

A record id occupies `RecordIdData.BYTES = 6` bytes inside record data
(`Segment.RECORD_ID_BYTES = 2 + 4`):

| Rel. offset | Size | Field | Encoding |
|---|---|---|---|
| 0 | 2 | Segment reference | Unsigned 16-bit BE (read as `readShort(...) & 0xffff`, `SegmentData.readRecordId`) |
| 2 | 4 | Record number | Signed 32-bit BE int |

Semantics of the segment reference value `r`
(`Segment.dereferenceSegmentId`):

* `r == 0`: the record lives in the **current segment** (the one containing
  this record id).
* `r > 0`: the record lives in the segment whose UUID is entry `r - 1` of
  this segment's segment reference table (§6). A reference greater than
  `segrefcount` is fatal ("Segment reference out of bounds" /
  "Referenced segment not found").

The record number is then resolved through the *target* segment's record
reference table (§7.1).

Reading a record id embedded at displacement `d` (in units of record-id
slots) after `rawOffset` bytes into record `n`
(`Segment.readRecordId(recordNumber, rawOffset, recordIdOffset)`):

```
virtualOffset = recordTable.getOffset(n) + rawOffset + recordIdOffset * 6
```

Unrelated wider format: `RecordId.getBytes()` serializes a record id as 20
bytes (`SERIALIZED_RECORD_ID_BYTES = 20`: msb 8 BE, lsb 8 BE, recordNumber 4
BE). That form is used outside segment data (e.g. journal/checkpoint
plumbing), never inside a segment.

## 10. Value length encoding (`SegmentData.readLength`)

Record data at this layer begins, for VALUE records, with a variable-length
big-endian length prefix. Exact algorithm (`SegmentData.java`,
`readLength(offset)`; all reads via the virtual-offset rule of §8):

```
head = readByte(offset) & 0xff                     // unsigned first byte
if (head & 0x80) == 0:                             // 0xxxxxxx  → 1 byte
    return head                                    // 0 <= len < 128
if (head & 0x40) == 0:                             // 10xxxxxx  → 2 bytes
    return 128 + (readShort(offset) & 0x3fff)      // 16-bit BE, low 14 bits
                                                   // 128 <= len < 16512
// 11xxxxxx → 8 bytes
return 16512 + (readLong(offset) & 0x3fffffffffffffffL)
                                                   // 64-bit BE, low 62 bits
```

The prefix bytes are *included* in the reads: the 2-byte form re-reads the
head byte as the high byte of the short; the 8-byte form re-reads it as the
top byte of the long. Threshold constants: `MAX_SMALL_LENGTH_VALUE = 128`,
`MAX_MEDIUM_LENGTH_VALUE = 16512`.

String reading (`SegmentData.readString`) at this layer:

* `length >= 2147483647` (`Integer.MAX_VALUE`): fatal ("String is too long").
* `length >= 16512`: the 8-byte length prefix is followed immediately by a
  6-byte record id (at `offset + 8`) pointing to a LIST record of
  `ceil(length / 4096)` BLOCK records (`Segment.readString`,
  `SegmentStream.BLOCK_SIZE = 4096`).
* `128 <= length < 16512`: UTF-8 bytes start at `offset + 2`.
* `length < 128`: UTF-8 bytes start at `offset + 1`.

(The header bit pattern `1110` marking small blob IDs, limit
`BLOB_ID_SMALL_LIMIT = 4096`, belongs to the record layer; noted here because
its constants live in `Segment.java`.)

## 11. Maximum counts and limits

| Quantity | Limit | Source |
|---|---|---|
| Segment buffer size | ≤ 262144 bytes (256 KiB) | `Segment.MAX_SEGMENT_SIZE` |
| Referenced segments per segment | ≤ 65533 (`count + 1 < 0xffff`) | `SegmentReferences.fromSegmentData`, `SegmentBufferWriter.writeRecordId` |
| Segment-reference value in a record id | 16-bit unsigned, `0..65535`; must be ≤ segrefcount | `SegmentData.readRecordId`, `SegmentReferences.getSegmentId` |
| Records per segment | no explicit format limit; bounded in practice by segment size (each record normally ≥ 4 bytes data + 9 bytes table entry ⇒ ≤ (262144 − 32)/13 ≈ 20162; note `prepare` aligns `size + 6*idCount` to 4, so a degenerate 0-byte record is representable, raising the theoretical cap to (262144 − 32)/9 ≈ 29123). Field is a 4-byte int; docs say the table "can store up to Integer.MAX_VALUE entries" (`records.md`) | `RECORD_NUMBER_COUNT_OFFSET` (int), `MutableRecordNumbers` (unbounded) |
| Record number | signed 32-bit int; writer only produces `0..reccount-1` | `MutableRecordNumbers.addRecord` |
| Total segment length after flush | `align(32 + 16*segrefs + 9*recs + data, 16)`, capped at 262144 | `SegmentBufferWriter.flush` |

## 12. Error / recovery behavior summary for readers

Fatal (exception in Java, unrecoverable for that segment):

* Signature ≠ `"0aK"` or version byte ∉ {12, 13} on a data segment
  (`Segment.java` ctor, `SegmentDataLoader`).
* Record-id segment reference > segrefcount, or unresolvable
  (`SegmentReferences.getSegmentId` bounds check;
  `Segment.dereferenceSegmentId` → "Referenced segment not found").
* segrefcount + 1 ≥ 0xffff (`SegmentReferences.fromSegmentData`).
* String length ≥ 2^31 - 1 (`SegmentData.readString`).
* Out-of-range buffer access resulting from a corrupt offset/record number
  (surfaces as `IndexOutOfBoundsException` /
  `ArrayIndexOutOfBoundsException`; a port should turn these into explicit
  corruption errors).

Tolerated / silent:

* Record numbers with no table entry: `getOffset` returns -1 (only
  meaningful to callers that check; `Segment.java` itself does not).
* `reccount == 0`: empty record table, all lookups -1
  (`RecordNumbers.fromSegmentData`).
* Gap of zero bytes between tables and record data in segments > ~252 KiB
  (§4).
* Bulk segments: header accessors must never be called; the buffer is raw
  data (`SegmentDataRaw` throws on all of them).
* Reserved header bytes are ignored on read (readers never look at offsets
  4–9 in V12, 8–9 in V13, or 22–31).

## 13. V12 vs V13 differences (complete list)

| Aspect | V12 | V13 |
|---|---|---|
| Version byte at offset 3 | `0x0C` | `0x0D` |
| Bytes 4–7 | reserved (zero) | full generation int, bit 31 = compacted flag |
| `getFullGeneration()` | = generation (offset 10) | `int32_be(4) & 0x7fffffff` |
| `isCompacted()` | always `true` | `int32_be(4) < 0` |
| Everything else (tables, offsets, record ids, lengths) | identical | identical |

(`SegmentDataV12.java`, `SegmentDataV13.java`, `SegmentDataLoader.java`.)

## 14. Worked byte map (data segment, V13)

```
offset    size  content
0x00      3     "0aK"                       30 61 4B
0x03      1     version                     0D
0x04      4     fullGeneration|compacted    BE int, bit 31 = compacted
0x08      2     reserved                    00 00
0x0A      4     generation                  BE int
0x0E      4     segrefcount = S             BE int
0x12      4     reccount    = R             BE int
0x16      10    reserved                    zeros
0x20      16*S  segment refs                per entry: msb (8, BE), lsb (8, BE)
0x20+16S  9*R   record refs                 per entry: number (4, BE),
                                            type (1), offset (4, BE, virtual)
...       —     zero padding (if any)
...       —     record data; a virtual offset v maps to buffer index
                size - 262144 + v
```
