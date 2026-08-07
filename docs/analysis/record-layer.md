# Oak Segment TAR — Record Layer Specification (below the node level)

Byte-exact specification of the record encodings used inside Oak `oak-segment-tar`
segments: value records (strings/binaries), external blob IDs, block records,
list records, and map records (HAMT). Derived from the Java sources under
`org/apache/jackrabbit/oak/segment/` (files cited per fact). This is the layer a
read-only Rust port must implement to decode property values, child-node maps,
and long strings once it can locate a record inside a segment.

Scope boundary: node records and template records are only mentioned where their
javadoc constrains this layer. The segment header, record-references table and
TAR index are covered by other specifications; this document assumes you can map
a `(segment, record number)` pair to a byte position.

---

## 0. Conventions and addressing model

### 0.1 Endianness

**Every multi-byte integer in a segment is big-endian.** Segments are accessed
through `org.apache.jackrabbit.oak.commons.Buffer`, a wrapper over
`java.nio.ByteBuffer`, whose default byte order is `BIG_ENDIAN` and is never
changed. The official documentation confirms: "All integers are stored in big
endian format" (`oak-doc/.../records.md`, "Data segments").

### 0.2 Java integer semantics

All arithmetic written below as "wrapping" follows Java semantics:

- `int` is signed 32-bit two's complement; `+`, `*`, `^`, `<<` wrap modulo 2^32.
- `long` is signed 64-bit two's complement.
- `>>` is an **arithmetic** (sign-extending) shift; `>>>` is a logical shift.
- **Shift distances are taken modulo the operand width**: for `int`,
  `x >> n` uses `n & 31`; for `long`, `n & 63`. This matters for map branches
  at level 6 (see §7.5). Rust `>>` panics/UBs on out-of-range shifts, so a port
  must mask the shift distance explicitly.

### 0.3 Record addressing (recap)

A record is identified by a **record ID** = (segment, record number). Inside a
data segment, the record number is looked up in the segment's record-references
table, which yields a *record offset* (`Segment.readByte` et al. call
`recordNumbers.getOffset(recordNumber)`; `Segment.java`).

The offset is expressed in a **virtual 256 KiB segment**
(`Segment.MAX_SEGMENT_SIZE = 1 << 18 = 262144`, `Segment.java`). The physical
buffer index is:

```
index = segment_size - (262144 - offset)          // SegmentDataUtils.index
```

(`data/SegmentDataUtils.java`, method `index`; `Segment.getAddress` is the same
formula.) All the "offset N within the record" reads below are performed at
`index(record_offset + N)`.

**Bulk segments** have no header and no tables. They are read through
`SegmentDataRaw` with `RecordNumbers` replaced by an identity mapping
(`Segment` constructor: `new IdentityRecordNumbers()`; `Segment.java`), so for a
bulk segment the *record number is itself the virtual offset*, normalized with
the same formula above. Bulk segments are simply a sequence of 4 KiB blocks
(see §6).

### 0.4 Alignment

Records in data segments are written aligned to 4 bytes:
`Segment.RECORD_ALIGN_BITS = 2` and `SegmentBufferWriter.prepare` aligns each
record size with `align(size + idCount * 6, 1 << 2)` (`SegmentBufferWriter.java`).
A reader never needs to compute alignment (offsets come from the table), but
must not assume records are contiguous — padding bytes may separate them.

### 0.5 Record types

The record-references table stores a type byte per record. The enum ordinal
order in `RecordType.java` is:

| ordinal | type | used by this layer |
|--------:|------|--------------------|
| 0 | `LEAF` | map leaf (§7.4) |
| 1 | `BRANCH` | map branch and map diff (§7.5, §7.6) |
| 2 | `BUCKET` | list bucket (§5.3) |
| 3 | `LIST` | list head (§5.2) |
| 4 | `VALUE` | value record (§2) |
| 5 | `BLOCK` | block record (§6) |
| 6 | `TEMPLATE` | (node layer) |
| 7 | `NODE` | (node layer) |
| 8 | `BLOB_ID` | external blob ID (§4) |

The reading code never dispatches on the type byte — record kind is always
implied by context (which pointer you followed). The type byte is informational
for tooling.

---

## 1. Record ID encoding — 6 bytes

`Segment.RECORD_ID_BYTES = 2 + 4` (`Segment.java`); `RecordIdData.BYTES =
Short.BYTES + Integer.BYTES = 6` (`data/RecordIdData.java`).

| bytes | field | encoding |
|-------|-------|----------|
| 0–1 | segment reference | unsigned 16-bit big-endian (`readShort(...) & 0xffff`, `SegmentData.readRecordId`) |
| 2–5 | record number | signed 32-bit big-endian int |

Segment-reference resolution (`Segment.dereferenceSegmentId`, `Segment.java`):

- `0` → the current segment (the one containing this record ID).
- `N > 0` → the **N-th entry, 1-based**, of the segment-references table in the
  current segment's header; i.e. table slot `N - 1`
  (`SegmentReferences.fromSegmentData`: `refIds[reference - 1]`,
  `SegmentReferences.java`).
- Unresolvable reference → fatal error. In the standard table implementation a
  reference greater than the table size fails
  `checkArgument(reference <= referencedSegmentIdCount, "Segment reference out
  of bounds")` (`IllegalArgumentException`, `SegmentReferences.fromSegmentData`);
  `Segment.dereferenceSegmentId` additionally throws
  `IllegalStateException("Referenced segment not found")` if a table
  implementation returns `null`. Either way: fatal, corrupt data.

A segment can have at most **65533** (`0xffff - 2`) references:
`SegmentReferences.fromSegmentData` enforces
`Validate.checkState(referencedSegmentIdCount + 1 < 0xffff, ...)`
(`SegmentReferences.java`), i.e. `count + 1 < 65535` ⇒ `count ≤ 65533`.

`Segment.readRecordId(recordNumber, rawOffset, recordIdOffset)` reads a record
ID at byte position `offset(recordNumber) + rawOffset + recordIdOffset * 6`
(`Segment.java`). The pattern `readRecordId(rn, base, k)` below always means
"the k-th 6-byte record ID in an array starting `base` bytes into the record".

---

## 2. Value records — length encodings

Constants (`Segment.java`):

```
SMALL_LIMIT        = 1 << 7                 = 128
MEDIUM_LIMIT       = (1 << (16 - 2)) + 128  = 16512
BLOB_ID_SMALL_LIMIT= 1 << 12                = 4096
```

(`data/SegmentData.java` repeats the first two as `MAX_SMALL_LENGTH_VALUE = 128`
and `MAX_MEDIUM_LENGTH_VALUE = (1 << 14) + 128 = 16512`.)

### 2.1 First-byte dispatch table

The high bits of the first byte of a VALUE/BLOB_ID record select the size class.
This is the exact dispatch performed by `SegmentBlob.getNewStream()` /
`SegmentBlob.length()` (`SegmentBlob.java`):

| test on `head` (u8) | byte pattern | class | head byte range |
|---|---|---|---|
| `(head & 0x80) == 0x00` | `0xxxxxxx` | small value, inline | `0x00`–`0x7F` |
| `(head & 0xC0) == 0x80` | `10xxxxxx` | medium value, inline | `0x80`–`0xBF` |
| `(head & 0xE0) == 0xC0` | `110xxxxx` | long value, block list | `0xC0`–`0xDF` |
| `(head & 0xF0) == 0xE0` | `1110xxxx` | external blob, short ID inline | `0xE0`–`0xEF` |
| `(head & 0xF8) == 0xF0` | `11110xxx` | external blob, long ID by reference | `0xF0`–`0xF7` |
| otherwise | `11111xxx` | **invalid** — `IllegalStateException("Unexpected value record type: %02x")` | `0xF8`–`0xFF` |

The writer only ever emits `0xF0` exactly for the long-blob-ID marker
(`RecordWriters.LargeBlobIdWriter.writeRecordContent`: `writer.writeByte((byte)
0xF0)`), but the reader tolerates `0xF0`–`0xF7`.

### 2.2 Small value — `0xxxxxxx`

Data length 0–127 bytes.

| bytes | content |
|-------|---------|
| 0 | `length` (u8, `0x00`–`0x7F`) |
| 1 .. 1+length | raw data bytes |

Writer: `ArrayValueWriter`: `writer.writeByte((byte) length)` when
`length < SMALL_LIMIT` (`RecordWriters.java`).
Reader: `length = head` (`SegmentBlob.length`), data at offset 1.

### 2.3 Medium value — `10xxxxxx xxxxxxxx`

Data length 128–16511 bytes.

| bytes | content |
|-------|---------|
| 0–1 | u16 big-endian `H`; stored value `H = 0x8000 \| (length - 128)` |
| 2 .. 2+length | raw data bytes |

Writer: `writer.writeShort((short) ((length - SMALL_LIMIT) | 0x8000))`
(`RecordWriters.ArrayValueWriter`).
Reader: `length = (readShort(rn) & 0x3fff) + 128` (`SegmentBlob.length`,
`SegmentData.readLength`). 14 payload bits ⇒ stored value 0–16383 maps to real
length 128–16511. `MEDIUM_LIMIT = 16512` is the first length that does **not**
fit this class.

### 2.4 Long value — `110xxxxx` + 7 more length bytes + list record ID

Data length ≥ 16512 bytes.

| bytes | content |
|-------|---------|
| 0–7 | u64 big-endian `H`; stored value `H = (length - 16512) \| (0x3 << 62)` |
| 8–13 | record ID of the **block list** (see §5.4 for what it points to) |

Writer: `DefaultSegmentWriter.SegmentWriteOperation.writeValueRecord(long, RecordId)`:

```java
long len = (length - Segment.MEDIUM_LIMIT) | (0x3L << 62);
// then RecordWriters.SingleValueWriter: writeLong(len); writeRecordId(rid);
```

Reader (`SegmentBlob.length`):
`length = (readLong(rn) & 0x1fffffffffffffffL) + 16512`. The number of 4 KiB
blocks in the list is `listSize = (length + 4095) / 4096` (integer division;
`SegmentBlob.getNewStream`), and the list record ID is read at raw offset 8.

Bit accounting: the writer sets bits 63–62 to `11`; bit 61 is bit 61 of
`length - 16512` and must be `0` for the head byte to match `110xxxxx`, so the
maximum representable length is `2^61 - 1 + 16512` bytes ("up to 2^61" per
`records.md`).

> **Mask discrepancy (quote, not speculation):** `SegmentData.readLength` uses
> `readLong(...) & 0x3fffffffffffffffL` (62-bit mask), while
> `SegmentBlob.length()` uses `& 0x1fffffffffffffffL` (61-bit mask). For any
> value the writer can produce (bit 61 = 0) both give identical results. A port
> may use either; the 61-bit mask matches the documented `110xxxxx` pattern.

### 2.5 `readLength` — canonical pseudocode

From `SegmentData.readLength` (`data/SegmentData.java`); used by
`Segment.readLength` and `SegmentData.readString`:

```
fn read_length(off) -> u64:
    head = u8_at(off)
    if head & 0x80 == 0:
        return head                                   # 0 .. 127
    if head & 0x40 == 0:
        return 128 + (u16_be_at(off) & 0x3FFF)        # 128 .. 16511
    return 16512 + (u64_be_at(off) & 0x3FFFFFFFFFFFFFFF)
```

**Warning:** this function does not distinguish blob-ID markers. Applied to a
record starting with `0xE0`–`0xF7` it happily takes the "long" branch and
returns garbage. `readLength`/`readString` must only be called on records known
from context to be plain values (e.g. string values); binaries must go through
the §2.1 dispatch (`SegmentBlob`), which is the only reader that handles all
five classes.

---

## 3. String storage (`Segment.readString` / `SegmentData.readString`)

Strings are value records containing UTF-8 bytes. The size class is chosen by
the **UTF-8 byte length** (`DefaultSegmentWriter.writeString`:
`string.getBytes(UTF_8)`).

Reader algorithm (`SegmentData.readString`, `data/SegmentData.java`, plus
`Segment.readString`, `Segment.java`):

```
fn read_string(off) -> String:
    length = read_length(off)
    if length >= 2147483647:            # Integer.MAX_VALUE
        fatal "String is too long"      # IllegalStateException
    if length >= 16512:                 # MEDIUM_LIMIT
        list_id = record_id_at(off + 8)               # 8 = Long.BYTES
        blocks  = ListRecord(list_id, ceil_div(length, 4096))
        return utf8(read_blocks(blocks, length))      # via SegmentStream
    data_off = off + (if length >= 128 { 2 } else { 1 })
    return utf8(bytes_at(data_off, length))
```

- Small strings: bytes at offset 1. Medium: bytes at offset 2. Long: **the
  record ID sits 8 bytes into the record** (after the 8-byte length), and
  points to the block list (§5.4). Block size is 4096
  (`SegmentStream.BLOCK_SIZE = 1 << 12`, `SegmentStream.java`).
- Number of blocks = `(length + 4095) / 4096` (`Segment.readString`:
  `(data.getLength() + BLOCK_SIZE - 1) / BLOCK_SIZE`).
- The final block holds `length mod 4096` bytes when not a multiple (readers
  compute per-block sizes from the remaining length; `SegmentStream.read`).
- UTF-8 decoding uses Java's lenient decoder (`Buffer.decode` /
  `new String(bytes, UTF_8)`): malformed sequences become U+FFFD, they are not
  an error.

The writer never emits a string longer than `Integer.MAX_VALUE` UTF-8 bytes
through `writeString`; huge streams only exist as binaries.

Segment-info string: the first record of every data segment (lowest record
number in table order) is a small/medium string value with segment metadata
`"{wid=W,sno=S,gc=G,t=T}"` (`Segment.getSegmentInfo`).

---

## 4. External blob IDs (record type `BLOB_ID`)

A blob stored in an external blob store is represented by its *blob ID string*
(UTF-8). Two encodings, selected by the UTF-8 byte length of the blob ID
against `BLOB_ID_SMALL_LIMIT = 4096` (`DefaultSegmentWriter.writeBlobId`).

### 4.1 Short blob ID — `1110xxxx` (`0xE0`), ID length 0–4095

Writer: `RecordWriters.SmallBlobIdWriter`:
`writer.writeShort((short) (length | 0xE000)); writer.writeBytes(blobId, 0, length)`
with `checkArgument(blobId.length < 4096)`.

| bytes | content |
|-------|---------|
| 0–1 | u16 big-endian = `0xE000 \| id_length` (4 marker bits `1110`, 12 length bits) |
| 2 .. 2+id_length | blob ID, UTF-8 |

Reader (`SegmentBlob.readShortBlobId`):
`length = (head & 0x0f) << 8 | (byte_at(rn, 1) & 0xff)`; bytes at offset 2.

### 4.2 Long blob ID — `11110xxx` (writer emits exactly `0xF0`), ID length ≥ 4096

Writer: `RecordWriters.LargeBlobIdWriter`: one byte `0xF0` then the record ID
of a **string value record** (§3) holding the blob ID
(`DefaultSegmentWriter.writeBlobId`: `writeString(blobId)` first).

| bytes | content |
|-------|---------|
| 0 | `0xF0` (reader accepts `0xF0`–`0xF7`) |
| 1–6 | record ID of string value record containing the blob ID |

**The record ID is at raw offset 1 — deliberately unaligned**
(`SegmentBlob.readLongBlobId`: `segment.readRecordId(recordNumber, 1)`).

### 4.3 External-blob semantics for a reader

`SegmentBlob.isExternal()` is `(head & 0xf0) == 0xe0 || (head & 0xf8) == 0xf0`.
Length and content of an external blob are **not** in the segment store; they
require the external blob store. A read-only port without a blob store should
surface the blob ID (`SegmentBlob.getBlobId`) and treat content/length requests
as errors (Java throws `IllegalStateException("Attempt to read external blob
... without specifying BlobStore")`).

---

## 5. List records

### 5.1 Constants

```
ListRecord.LEVEL_SIZE   = 255            // max record IDs per bucket
ListRecord.MAX_ELEMENTS = 255^3 = 16_581_375
SegmentStream.BLOCK_SIZE = 4096          // only for block lists, not general lists
```

(`ListRecord.java`, `SegmentStream.java`.)

### 5.2 LIST head record (used for multi-valued properties and template prop-name lists)

Writer `RecordWriters.ListWriter`:

| bytes | content |
|-------|---------|
| 0–3 | `count` (s32 big-endian) |
| 4–9 | record ID of the list body (present **only if** `count > 0`) |

- Empty list: just the int `0`, no record ID (`ListWriter()` no-arg;
  `DefaultSegmentWriter.writeProperty` for `count == 0`).
- Non-empty: `count` followed by the ID returned by `writeList` (§5.4).

### 5.3 BUCKET record

A bucket is a bare array of 2–255 record IDs, 6 bytes each, no header, no count
(`RecordWriters.ListBucketWriter`; `RecordType.BUCKET` javadoc: "It always
includes at least 2 elements, up to 255 entries ... The size of the list is not
stored"). The element count is always known from the parent context.

### 5.4 What a "list body" pointer points to — writer algorithm

`DefaultSegmentWriter.SegmentWriteOperation.writeList` (faithful pseudocode):

```
fn write_list(ids):                # ids non-empty
    level = ids
    while level.len() > 1:
        next = []
        for chunk in level.partition(255):     # consecutive chunks
            if chunk.len() > 1: next.push(write_bucket(chunk))   # BUCKET record
            else:               next.push(chunk[0])              # pass-through!
        level = next
    return level[0]
```

Consequences a reader must honor:

- **There is no wrapper record.** The returned ID is the top BUCKET — or, for a
  1-element list, **the element itself** (a block, a value, a node ID...).
- Single-element chunks are *passed through unwrapped*, so an entry inside a
  bucket may point either to a sub-bucket or directly to an element; which one
  is determined purely by arithmetic on the known size (below), never by tags.

### 5.5 Reading: `ListRecord` — exact algorithm

`ListRecord(id, size)` (`ListRecord.java`), where `size` comes from the parent
(LIST head count, or `ceil(length/4096)` for block lists):

```
constraints: 0 <= size <= 16_581_375     # else IllegalArgumentException

bucket_size = 1
while bucket_size * 255 < size:
    bucket_size *= 255                   # 1, 255, 65025 (255^2)

fn get_entry(list_id, size, bucket_size, index):     # 0 <= index < size
    if size == 1:
        return list_id                   # the "list" IS the single element
    bucket_index  = index / bucket_size
    bucket_offset = index % bucket_size
    sub_id   = record_id_at(list_id, offset = 6 * bucket_index)
    sub_size = min(bucket_size, size - bucket_index * bucket_size)
    return get_entry(sub_id, sub_size, parent_bucket_size(sub_size), bucket_offset)
    # i.e. recurse with a fresh ListRecord(sub_id, sub_size)
```

The recursion recomputes `bucket_size` from `sub_size` at each step
(`ListRecord.getEntry` constructs `new ListRecord(id, min(bucketSize, size -
bucketIndex * bucketSize))`). Depth ≤ 3 (255³ max elements). `getEntries(from,
count)` walks the same structure sequentially, clamping `count` to
`size - from` (`ListRecord.getEntries`).

### 5.6 Block lists for long values

For a long value of `length` bytes, the list has `ceil(length/4096)` entries;
entry *i* is the record ID of the block covering bytes
`[i*4096, min((i+1)*4096, length))`.

Writer detail — the two producers of block lists emit blocks differently
(**corrected**, both verified in `DefaultSegmentWriter.java`):

- **Long strings** (`writeString`): full 256 KiB chunks are written as **bulk
  segments**, contributing 64 block IDs `RecordId(bulkId, i)` with
  `i = 0, 4096, 8192, ... 258048` — the record number is the raw virtual
  offset (§0.3). Trailing data (< 256 KiB) is written as BLOCK records in
  **data segments** (4 KiB each, last one possibly short), addressed via the
  record table like any record (`writeBlock`).
- **Long binaries** (`internalWriteStream`): **all** data goes to bulk
  segments, including the trailing partial chunk. A bulk segment holding `n`
  bytes (`n ≤ 262144`) contributes block IDs
  `RecordId(bulkId, 262144 - n + i)` for `i = 0, 4096, ... < n`
  (`blockIds.add(new RecordId(bulkId, data.length - n + i))` where
  `data.length = MAX_SEGMENT_SIZE`) — record numbers are *virtual offsets*
  that the §0.3 formula normalizes to physical positions `0, 4096, ...` in
  the bulk segment. For a full chunk (`n = 262144`) this reduces to the same
  `0 ... 258048` sequence as the string case.

A reader must therefore handle block IDs pointing at partial (short) bulk
segments as well as BLOCK records in data segments. `SegmentStream.read`
additionally coalesces adjacent reads when consecutive block IDs are in the
same segment with record numbers exactly `prev + k*4096` — an optimization
only, not a format guarantee.

---

## 6. Block records

A BLOCK record is a raw byte sequence, up to 4096 bytes, with **no header and
no length** (`BlockRecord.java`; `records.md` "Block records"). The length is
supplied by the owner (block list arithmetic, §5.6; or the stable-ID block of a
node record, which is 20 bytes — `RecordId.SERIALIZED_RECORD_ID_BYTES = 20`:
msb u64 + lsb u64 + record number s32, node layer). Blocks are aligned to 4 bytes in
data segments; a bulk segment of `n` bytes is `n div 4096` full blocks plus one
`n mod 4096`-byte block (`records.md` "Bulk segments").

---

## 7. Map records (HAMT)

Maps store `String → RecordId` (child-node name → node record). Top-level
record is `LEAF` or `BRANCH` (or a diff, encoded as `BRANCH`).

### 7.1 Constants (`MapRecord.java`)

```
M                    = 0xDEECE66D          // hash multiplier ("magic constant from a RNG")
A                    = 0xB                 // hash addend
HASH_MASK            = 0xFFFFFFFFL         // for unsigned comparison of hashes
BITS_PER_LEVEL       = 5
BUCKETS_PER_LEVEL    = 1 << 5      = 32
MAX_NUMBER_OF_LEVELS = ceil(32/5)  = 7
LEVEL_BITS           = 3           // numberOfTrailingZeros(highestOneBit(7) << 1) = 3
SIZE_BITS            = 32 - 3      = 29
MAX_SIZE             = (1 << 29) - 1 = 536_870_911
WARN_SIZE            = 400_000_000         // log warning only
ERROR_SIZE           = 450_000_000         // log error only
ERROR_SIZE_DISCARD_WRITES = 500_000_000    // writes refused unless system property set
ERROR_SIZE_HARD_STOP = 536_000_000         // writes always refused
```

> **Correction note:** the Java source comments next to `LEVEL_BITS` ("// 4")
> and `MAX_SIZE` ("// ~268e6") are stale. The *computed* values are
> `LEVEL_BITS = numberOfTrailingZeros(highestOneBit(7) << 1) = 3` and
> `SIZE_BITS = 29`, so `MAX_SIZE = 2^29 - 1 = 536_870_911` — which matches
> `records.md` ("up to 2^29 - 1 entries") and makes the 536_000_000 hard-stop
> write limit meaningful. The `*_SIZE` limits only affect writer logging — a
> reader can ignore them.

### 7.2 The hash function — exact

`MapRecord.getHash` (`MapRecord.java`), all 32-bit wrapping:

```java
static int getHash(String name) {
    return (name.hashCode() ^ M) * M + A;    // M = 0xDEECE66D, A = 0xB
}
```

`String.hashCode()` is Java-specified: over the string's **UTF-16 code units**
`c[0..n)`:

```
h = 0
for each UTF-16 code unit c:      # supplementary chars contribute 2 surrogates
    h = wrapping_i32(31 * h + c)
```

Pseudocode for the full hash:

```
fn map_hash(name) -> i32:
    h: i32 = 0
    for cu in utf16_code_units(name):
        h = h.wrapping_mul(31).wrapping_add(cu as i32)
    h ^= 0xDEECE66Du32 as i32
    h = h.wrapping_mul(0xDEECE66Du32 as i32)
    h = h.wrapping_add(0xB)
    return h
```

Hashes are *compared and sorted as unsigned 32-bit* values via
`hash & 0xFFFFFFFFL` (`MapRecord.HASH_MASK`).

### 7.3 The head word and record classification

Every map record starts with a 4-byte big-endian int `head`:

```
getSize(head)  = head & 0x1FFFFFFF        // low 29 bits  (MapRecord.getSize)
getLevel(head) = head >>> 29              // high 3 bits, unsigned (MapRecord.getLevel)
isDiff(head)   = head == -1               // head == 0xFFFFFFFF (MapRecord.isDiff)
isBranch(size, level) = size > 32 && level < 7    // MapRecord.isBranch
```

Classification order matters: **check `head == 0xFFFFFFFF` (diff) first**, then
branch vs leaf. `size` is the total number of entries in the whole subtree
rooted at this record (for both leaves and branches:
`RecordWriters.MapBranchWriter` writes the subtree entry count;
`writeMapBranch(level, size, ...)` passes `entries.size()` of the whole map,
`DefaultSegmentWriter.java`).

- `size <= 32` → always a leaf.
- `size > 32 && level == 7` → an *overflow leaf* at the deepest level (hash
  collisions across all 32 bits; `writeMapBucket`: `entries.size() <=
  BUCKETS_PER_LEVEL || level == MAX_NUMBER_OF_LEVELS` → leaf).
- Empty map: a leaf with `head == 0` (writer `MapLeafWriter()` no-arg writes a
  single int `0`; `RecordWriters.java`).

### 7.4 Leaf layout (`RecordType.LEAF`)

Writer `RecordWriters.MapLeafWriter.writeRecordContent`; reader
`MapRecord.getEntry` / `getEntries`. For a leaf with `size = N` entries at trie
level `L`:

| byte offset | width | content |
|---|---|---|
| 0 | 4 | `head = (L << 29) \| N` (s32 BE) |
| 4 + 4*i (i = 0..N-1) | 4 | `hash[i]` — `map_hash(key_i)` (s32 BE), **sorted ascending by unsigned value**, ties by key |
| 4 + 4*N + 12*i | 6 | record ID of key *i* (string value record, §3) |
| 4 + 4*N + 12*i + 6 | 6 | record ID of value *i* |

**Key and value IDs are interleaved as (key, value) pairs, not grouped.**
Verified on both sides: writer emits `writeRecordId(entry.getKey());
writeRecordId(entry.getValue())` per entry (`MapLeafWriter`); reader fetches
`keyId = readRecordId(rn, 4 + size*4, i*2)` and `valueId = readRecordId(rn,
4 + size*4, i*2 + 1)` (`MapRecord.getEntry`).

Sort order of entries (writer `sort(array)` with `MapEntry.compareTo`,
`MapEntry.java`): primary key `hash & 0xFFFFFFFFL` ascending (unsigned);
secondary `getName()` via Java `String.compareTo` — **lexicographic by UTF-16
code unit**, which differs from UTF-8 byte order for supplementary characters;
a Rust port must compare `str::encode_utf16()` sequences, not bytes. (The
tertiary value-ID comparator never fires for distinct map keys.)

Lookup by name in a leaf: compute `h = map_hash(name)`; find entries with equal
unsigned hash (Java uses interpolation search over the sorted hash array —
`MapRecord.getEntry`; binary or linear search is equivalent for a port since
the array is sorted); for each hash match, load the key string and compare for
equality; on match return the value ID. Note the interpolation search resolves
hash ties by comparing the **key string** (`reader.readString(keyId).compareTo(name)`)
and continues bisecting on that ordering, which is why ties must be
name-sorted.

### 7.5 Branch layout (`RecordType.BRANCH`, `head != -1`)

Writer `RecordWriters.MapBranchWriter`; reader `MapRecord.getEntry` /
`getBuckets`. For a branch at level `L` covering `N` total entries with `k`
non-empty child buckets:

| byte offset | width | content |
|---|---|---|
| 0 | 4 | `head = (L << 29) \| N` (s32 BE) |
| 4 | 4 | `bitmap` (s32 BE): bit `i` (i.e. `1 << i`, LSB = bucket 0) set iff child bucket `i` exists |
| 8 + 6*j (j = 0..k-1) | 6 | record ID of the j-th present child, **in increasing bucket-index order** (bit 0 → bit 31) |

Child records are themselves map records (leaf, branch, or — never in
practice — diff) at level `L + 1`.

**Bucket selection at level `L`** (both `MapRecord.getEntry` and the writer's
`splitToBuckets`, `DefaultSegmentWriter.java` — identical expressions):

```
mask  = (1 << 5) - 1 = 31
shift = 32 - (L + 1) * 5          # L=0 → 27, L=1 → 22, ... L=5 → 2, L=6 → -3
index = (hash >> shift) & mask    # Java arithmetic shift, distance taken mod 32
bit   = 1 << index
if bitmap & bit == 0: not present
j     = popcount(bitmap & (bit - 1))          # Integer.bitCount
child = record_id_at(offset 8 + 6*j)
```

**Level-6 quirk (mandatory to replicate):** at `L = 6`, `shift = -3`; Java
computes `hash >> (-3 & 31)` = `hash >> 29` (arithmetic). So level-6 branches
select buckets from the **top 3 bits of the hash** (indices land in
`{0,1,2,3, 28,29,30,31}` because the shift is sign-extending), re-using bits
already consumed at level 0. Reader and writer agree, so data is consistent;
a Rust port must write `index = (hash >> (((32 - (L+1)*5) as u32) & 31)) & 31`
with `hash` as `i32` and arithmetic shift. Level 7 records are always leaves,
so no shift is ever computed for `L = 7`.

### 7.6 Diff record — `head == -1` (`0xFFFFFFFF`)

Encoded as a BRANCH record (`RecordType.BRANCH` javadoc; writer
`RecordWriters.newMapBranchWriter(bitmap, ids)` with `level = 0, entryCount =
-1`, so the head int is `(0 << 29) | -1 = 0xFFFFFFFF`, and the "bitmap" slot
carries the key hash — `DefaultSegmentWriter.writeMap` passes
`entry.getHash()` as the `bitmap` argument).

| byte offset | width | content |
|---|---|---|
| 0 | 4 | `0xFFFFFFFF` |
| 4 | 4 | `hash` = `map_hash(modified_key)` (s32 BE) |
| 8 | 6 | record ID of the key (string value record) |
| 14 | 6 | record ID of the new value (node record) |
| 20 | 6 | record ID of the **base map** record |

(Reader offsets: `readInt(rn, 4)`, `readRecordId(rn, 8, 0)`,
`readRecordId(rn, 8, 1)`, `readRecordId(rn, 8, 2)` — `MapRecord.getEntry`,
`getEntries`, `isLeaf`, `size`.)

Semantics — the diff overlays exactly one changed/added entry on the base map:

```
fn get_entry(map, name):
    head = i32_at(map, 0)
    if head == -1:
        if map_hash(name) == i32_at(map, 4)
           and read_string(record_id_at(map, 8)) == name:
            return record_id_at(map, 14)          # overlaid value
        return get_entry(base = record_id_at(map, 20), name)
    ... branch/leaf logic (§7.4, §7.5)
```

- `size()` and `isLeaf()` of a diff delegate to the base map.
- Enumerating entries: enumerate the base map, substituting the diff's value
  wherever the entry's key record ID equals the diff's key record ID
  (`MapRecord.getEntries(diffKey, diffValue)`: comparison is by **key record
  ID equality**, not string equality).
- "There is only ever one single diff record for a map"
  (`RecordType.BRANCH` javadoc) — the writer collapses an existing diff before
  layering a new one (`DefaultSegmentWriter.writeMap`) — but the read path is
  fully recursive and tolerates chains; a port should recurse likewise.
- The diff never removes an entry: deletions force a full rewrite.

### 7.7 Full lookup algorithm (assembled)

```
fn map_get(map_id, name) -> Option<RecordId>:
    hash = map_hash(name)
    loop:
        head = i32_at(map_id, 0)
        if head == 0xFFFFFFFF:                       # diff
            if i32_at(map_id, 4) == hash
               and read_string(rid_at(map_id, 8)) == name:
                return Some(rid_at(map_id, 14))
            map_id = rid_at(map_id, 20); continue
        size  = head & 0x1FFFFFFF
        level = (head as u32) >> 29
        if size == 0: return None
        if size > 32 and level < 7:                  # branch
            bitmap = i32_at(map_id, 4)
            shift  = ((32 - (level+1)*5) as u32) & 31
            index  = ((hash >> shift) & 31) as u32   # arithmetic shift on i32
            bit    = 1i32 << index
            if bitmap & bit == 0: return None
            j = popcount(bitmap & (bit - 1))
            map_id = rid_at(map_id, 8 + 6*j); continue
        # leaf: hashes sorted unsigned-ascending at offset 4, N entries
        for i in matching_hash_range(map_id, size, hash):   # binary search ok
            key_id = rid_at(map_id, 4 + 4*size + 12*i)
            if read_string(key_id) == name:
                return Some(rid_at(map_id, 4 + 4*size + 12*i + 6))
        return None
```

---

## 8. Where these records are referenced from (context recap)

- Property value (single): record ID of a VALUE record (§2) — or BLOB_ID (§4)
  for external binaries. All non-binary types (long, boolean, date...) are
  stored as their string representations in VALUE records (`records.md`).
- Property value (multi): record ID of a LIST head (§5.2); `count` is the
  number of values; the body resolves per §5.5 to per-value VALUE record IDs
  (`DefaultSegmentWriter.writeProperty`).
- Template records reference a LIST body (not a LIST head — `writeList` output
  directly) of property-name VALUE IDs; count comes from the template head's
  low 18 bits (`RecordWriters.TemplateWriter`, node-layer spec).
- Child-node map of a node: record ID of a map record (§7); values are node
  record IDs.
- Long string/binary VALUE records reference block lists (§5.4/§5.6).

## 9. Error tolerance summary for a reader

Fatal (corrupt data / programming error — Java throws unchecked exceptions):

- Head byte `0xF8`–`0xFF` in a value record (`SegmentBlob`).
- Segment reference not resolvable (`Segment.dereferenceSegmentId`).
- String length ≥ `Integer.MAX_VALUE` = 2147483647 (`SegmentData.readString`).
- List size negative or > 16 581 375 (`ListRecord` constructor).
- Segment signature ≠ `"0aK"` or version ∉ {12, 13} (`Segment` constructor,
  `SegmentVersion.java`, `SegmentDataLoader.java`).

Tolerated / by design:

- Malformed UTF-8 in strings (replacement characters, no error).
- Record IDs pointing into other segments anywhere, including bulk segments.
- Map diff chains (recursion handles them).
- `head` values `0xF1`–`0xF7` treated as long-blob-ID markers.
- Missing external blobs are *not* detectable from the segment store; only the
  blob ID string is stored.

## 10. Version notes

- **Segment format 12 vs 13** (`SegmentVersion.java`,
  `data/SegmentDataV12.java`, `data/SegmentDataV13.java`): the difference is
  confined to the segment **header** — V13 adds the full-generation word at
  header offset 4 (`getFullGeneration() = getInt(4) & 0x7fffffff`,
  `isCompacted() = getInt(4) < 0`), whereas V12 reports
  `fullGeneration = generation` and `isCompacted() = true`. **All record
  encodings in this document are byte-identical in versions 12 and 13** (both
  versions share `SegmentDataV12`'s record-access code; `SegmentDataV13
  extends SegmentDataV12` overrides only those two methods).
- Versions 10/11 belong to the legacy `oak-segment` format and are invalid for
  `oak-segment-tar` (`SegmentVersion.java` comment).
- TAR **index format V1 vs V2** does not exist at this layer; it affects only
  the TAR-file index entries (see the tar-layer specification).
