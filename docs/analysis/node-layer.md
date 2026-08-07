# Oak Segment TAR — Node, Template, and Property Record Specification

Subsystem: the record-level grammar for NODE, TEMPLATE, property VALUE/LIST records,
child-node lookup through MAP records, and the repository super-root structure
(`root` / `checkpoints`). This is the byte-exact contract for a read-only Rust
implementation. All facts are cited to the Java sources under
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment` (referred to below
by bare file name) and to `oak-doc/src/site/markdown/nodestore/segment/records.md`.

---

## 0. Conventions and prerequisites

* **Endianness: every multi-byte field in a segment is BIG-ENDIAN.** All reads go
  through `org.apache.jackrabbit.oak.commons.Buffer`, a wrapper over `java.nio.ByteBuffer`,
  which is big-endian by default; the code never changes the byte order
  (`data/SegmentDataV12.java` uses `buffer.getInt`, `buffer.getShort`, `buffer.getLong` directly).
* **Java int arithmetic wraps at 32 bits** (two's complement). Every hash computation
  below must be performed with wrapping 32-bit signed semantics (`wrapping_mul`,
  `wrapping_add` in Rust on `i32`).
* **Record addressing.** A record is addressed by a *record number*, translated to a
  byte offset by the segment's record-reference table (`Segment.java`,
  `recordNumbers.getOffset(recordNumber)`). Offsets in the table are *virtual*
  offsets in a virtual 256 KiB (`MAX_SEGMENT_SIZE = 1 << 18`, `Segment.java`) segment.
  The actual buffer index is:

  ```
  index = buffer.limit - (0x40000 - virtualOffset)      // data/SegmentDataUtils.index()
  ```

  i.e. records are packed at the *end* of the segment buffer. The record-number table
  itself is specified in the segment-layer document; this document assumes a function
  `offset_of(record_number) -> buffer index`.
* All "read at (recordNumber, off)" operations below mean: read at
  `offset_of(recordNumber) + off` in the segment data.
* **Record alignment:** records are aligned to 4-byte boundaries
  (`RECORD_ALIGN_BITS = 2`, `Segment.java`). A reader does not need this fact except
  to sanity-check offsets.

### 0.1 In-segment record ID: 6 bytes

`Segment.RECORD_ID_BYTES = 2 + 4 = 6` (`Segment.java`). Wherever a record ID is
embedded in another record it occupies exactly 6 bytes:

| bytes | field | type | meaning |
|---|---|---|---|
| 0–1 | segment reference | u16 BE | `0` = the current segment; otherwise 1-based index into this segment's segment-reference table |
| 2–5 | record number | i32 BE | record number within the referenced segment |

From `SegmentData.readRecordId` (`data/SegmentData.java`):

```java
int segmentReference = readShort(off) & 0xffff;
int recordNumber = readInt(off + 2);
```

and `Segment.dereferenceSegmentId` (`Segment.java`): reference `0` maps to the
segment itself; a nonzero reference is looked up via
`segmentReferences.getSegmentId(reference)` (1-based); a missing entry throws
`IllegalStateException("Referenced segment not found")` — **fatal** for the reader.

`Segment.readRecordId(recordNumber, rawOffset, recordIdOffset)` reads the record ID at
`offset_of(recordNumber) + rawOffset + recordIdOffset * 6` (`Segment.java`). The
grammar below uses the notation `rid(recordNumber, rawOffset, idx)` for this.

### 0.2 Serialized (out-of-line) record ID: 20 bytes

`RecordId.SERIALIZED_RECORD_ID_BYTES = 20` (`RecordId.java`). Layout
(`RecordId.getBytes()`):

| bytes | field | type |
|---|---|---|
| 0–7 | segment ID most-significant 64 bits (msb) | i64 BE |
| 8–15 | segment ID least-significant 64 bits (lsb) | i64 BE |
| 16–19 | record number | i32 BE |

> Note: the Javadoc of `RecordId.getBytes()` says "`(msb, lsb, offset >> 2)`" but the
> code writes the record number **unshifted**: `BinaryUtils.writeInt(buffer, 16, offset)`.
> The comment is stale (a leftover from the pre-record-number format); trust the code.

The string form is `"<uuid>:<recordNumber>"` — `new UUID(msb, lsb) + ":" + offset`
(`SegmentNodeState.getStableId(Buffer)`).

### 0.3 Value/string length encoding (needed to read names and values)

From `SegmentData.readLength` (`data/SegmentData.java`) and constants
`SMALL_LIMIT = 1 << 7 = 128`, `MEDIUM_LIMIT = (1 << 14) + 128 = 16512` (`Segment.java`):

```
head = byte at offset 0 (as unsigned u8)
if (head & 0x80) == 0:            # 0xxxxxxx  small
    length = head                                  # 0..127; data at offset 1
elif (head & 0x40) == 0:          # 10xxxxxx  medium
    length = (u16BE at offset 0 & 0x3FFF) + 128    # 128..16511; data at offset 2
else:                             # 11xxxxxx  long
    length = (u64BE at offset 0 & 0x3FFFFFFFFFFFFFFF) + 16512
    # followed at offset 8 by one 6-byte record id of a LIST of BLOCK records
```

Note the mask in `readLength` is `0x3fffffffffffffffL` (62 bits); the writer applies
**no** mask — it writes `(length - 16512) | (0x3 << 62)` as one i64 BE
(`DefaultSegmentWriter`: `long len = (length - Segment.MEDIUM_LIMIT) | (0x3L << 62)`),
so all 62 non-tag bits carry length. **Beware a reader-side inconsistency in the Java
code:** the *blob* read path (`SegmentBlob.length()`/`getNewStream()` and
`SegmentParser.parseBlob`) masks only 61 bits (`& 0x1fffffffffffffffL`), while the
*string* path (`SegmentData.readLength`) masks 62 bits. The two agree for every length
< 2^61, which is always true in practice; a Rust reader may use the 62-bit mask
everywhere.

External-blob patterns also start value records, but **never occur for strings**
(names, string values); they occur only for BINARY property values
(corrected/expanded from `SegmentBlob.java`, `RecordWriters.java`):

* **Small blob ID** — first byte `1110 xxxx` (`(head & 0xF0) == 0xE0`). Two-byte
  header `u16BE = length | 0xE000` (`RecordWriters.SmallBlobIdWriter`); the blob-ID
  length is **12 bits**: `length = (head & 0x0F) << 8 | (u8 at offset 1)`, `0..4095`
  (`BLOB_ID_SMALL_LIMIT = 1 << 12`, `Segment.java`; read in
  `SegmentBlob.readShortBlobId`). The blob ID itself is `length` UTF-8 bytes at
  offset 2. The blob ID is a reference into an external `BlobStore`, not inline data.
* **Large blob ID** — first byte written as exactly `0xF0`
  (`RecordWriters.LargeBlobIdWriter`), but readers accept `(head & 0xF8) == 0xF0`
  (i.e. `0xF0..0xF7`, `SegmentBlob.readBlobId`). At offset 1 follows one 6-byte
  record ID of a string (VALUE) record containing the blob ID
  (`SegmentBlob.readLongBlobId`: `readRecordId(recordNumber, 1)` then `readString`).

`SegmentParser.parseBlob` treats any first byte matching `(head & 0xf8) == 0xf8`
(i.e. `0xF8..0xFF`) as **fatal**: `IllegalStateException("Unexpected value record type")`.

Reading a string (`SegmentData.readString`):

* `length >= Integer.MAX_VALUE` → fatal `IllegalStateException` ("String is too long …
  possibly trying to read a BLOB using getString").
* `length >= 16512` → the 8-byte length header is followed by a 6-byte record ID of a
  LIST record of `(length + 4095) / 4096` BLOCK records (`BLOCK_SIZE = 4096`, cited in
  `Segment.readString` via `SegmentStream.BLOCK_SIZE`); concatenate the blocks and
  decode UTF-8. Each block is raw bytes; all blocks are 4096 bytes except possibly the last.
* `128 <= length < 16512` → UTF-8 bytes at offset 2.
* `length < 128` → UTF-8 bytes at offset 1.

### 0.4 LIST records (used by templates, nodes, and multi-valued properties)

`ListRecord.java`. A *logical* list of `size` record IDs is stored as a tree of
BUCKET records with fan-out `LEVEL_SIZE = 255`; `MAX_ELEMENTS = 255^3 = 16_581_375`.
Crucially, **the size is not stored with the list**; it is always known from context
(property count in the template head, array length in the property record, block
count computed from the value length).

Given a list root record ID `id` and known `size`:

```
bucketSize = 1
while bucketSize * 255 < size: bucketSize *= 255

getEntry(id, size, index):                       # ListRecord.getEntry
    require 0 <= index < size
    if size == 1: return id                      # the "list" IS the single element
    bucketIndex  = index / bucketSize
    bucketOffset = index % bucketSize
    bucketId = rid(id.recordNumber, 0, bucketIndex)   # 6-byte ids packed back-to-back
    childSize = min(bucketSize, size - bucketIndex * bucketSize)
    return getEntry(bucketId, childSize, bucketOffset)
```

So: if `size == 1` the record ID stored by the parent points **directly at the element**
(no bucket record exists at all); if `2 <= size <= 255` it points at a BUCKET record
containing `size` consecutive 6-byte record IDs; for larger sizes it points at a bucket
of up to 255 sub-bucket IDs, recursively. A `size == 0` list stores no pointer at all
(callers skip the field or, for LIST-typed records with an explicit count field, only the
count is present — see §4.2).

There is also a *counted* LIST record type (used for multi-valued properties, and by
long values internally): `int count` followed, **only if `count > 0`**, by the 6-byte
root ID described above (`RecordWriters.ListWriter.writeRecordContent`).

---

## 1. Template record (RecordType.TEMPLATE)

Authoritative grammar: `SegmentParser.parseTemplate` and
`CachingSegmentReader.readTemplate`; write side `DefaultSegmentWriter.writeTemplate` +
`RecordWriters.TemplateWriter`.

### 1.1 Byte layout

| offset | size | field |
|---|---|---|
| 0 | 4 | `head` (i32 BE, bit-packed, see below) |
| 4 | 6 | record ID of primary-type name string — **present only if bit 31 set** |
| … | 6 × mixinCount | record IDs of mixin-type name strings, in order — **present only if bit 30 set** |
| … | 6 | record ID of the single child node's *name* string — **present only if bit 29 clear AND bit 28 clear** |
| … | 6 | record ID of the property-name list (a LIST of `propertyCount` string record IDs, uncounted, size known from head) — **present only if propertyCount > 0** |
| … | 1 × propertyCount | property type bytes, one signed byte per property, same order as the name list — **present only if propertyCount > 0** |

Total size = `4 + 6*(hasPrimary?1:0) + 6*mixinCount + 6*(singleChild?1:0) + 6*(propertyCount>0?1:0) + propertyCount`.

### 1.2 The 32-bit head

From `CachingSegmentReader.readTemplate` (identical in `SegmentParser.parseTemplate`):

```java
boolean hasPrimaryType = (head & (1 << 31)) != 0;
boolean hasMixinTypes  = (head & (1 << 30)) != 0;
boolean zeroChildNodes = (head & (1 << 29)) != 0;
boolean manyChildNodes = (head & (1 << 28)) != 0;
int mixinCount    = (head >> 18) & ((1 << 10) - 1);   // bits 18..27, 10 bits, 0..1023
int propertyCount =  head        & ((1 << 18) - 1);   // bits 0..17, 18 bits, 0..262143
```

| bit(s) | meaning |
|---|---|
| 31 (0x8000_0000) | node has a single-valued `jcr:primaryType` NAME property |
| 30 (0x4000_0000) | node has a multi-valued `jcr:mixinTypes` NAMES property |
| 29 (0x2000_0000) | node has **zero** child nodes |
| 28 (0x1000_0000) | node has **more than one** child node |
| 27–18 | mixin count (10 bits); meaningful only when bit 30 set, otherwise written 0 |
| 17–0 | property count (18 bits), excluding jcr:primaryType and jcr:mixinTypes |

Child-node arity decoding (`CachingSegmentReader.readTemplate`):
* bit 28 set → MANY child nodes (node record holds a child *map*).
* bit 29 set (bit 28 clear) → ZERO child nodes.
* both clear → exactly ONE child node; its name string's record ID is stored in the
  template (see layout), and the child node's record ID is stored in the node record.
* Writer never sets both bits 29 and 28 (`DefaultSegmentWriter.writeTemplate` uses
  if/else-if/else). The readers check **bit 28 first** (`if (manyChildNodes) … else if
  (!zeroChildNodes) …` in `CachingSegmentReader.readTemplate`), so the impossible
  both-set combination would decode as MANY in Java; a Rust reader may instead treat
  both-set as corrupt.

Limits enforced by the writer (`DefaultSegmentWriter.writeTemplate`):
`mixinCount < 1024` (`Validate.checkState(mixinIds.size() < (1 << 10))`),
`propertyCount < 262144` (`checkState(propertyNames.length < (1 << 18))`).

### 1.3 Property name list and type bytes

* The property-name list ID points to an *uncounted* LIST (§0.4) of `propertyCount`
  record IDs, each the ID of a string (VALUE) record holding the property name in UTF-8.
* Immediately after that 6-byte list ID come `propertyCount` **signed** type bytes,
  one per property, in the same index order as the name list
  (`CachingSegmentReader.readProps`: `byte type = segment.readByte(recordNumber, offset++)`).

Type byte encoding (`DefaultSegmentWriter.writeTemplate`):

```java
if (type.isArray()) propertyTypes[i] = (byte) -type.tag();
else                propertyTypes[i] = (byte)  type.tag();
```

Decoding (`CachingSegmentReader.readProps`):

```java
Type.fromTag(Math.abs(type), type < 0)   // negative byte => multi-valued (array)
```

JCR property type codes (`javax.jcr.PropertyType`, used via `Type.tag()`):

| tag | JCR type | tag | JCR type |
|---|---|---|---|
| 1 | STRING | 7 | NAME |
| 2 | BINARY | 8 | PATH |
| 3 | LONG | 9 | REFERENCE |
| 4 | DOUBLE | 10 | WEAKREFERENCE |
| 5 | DATE | 11 | URI |
| 6 | BOOLEAN | 12 | DECIMAL |

So byte `0x03` = single-valued LONG, byte `0xFD` (= −3) = multi-valued LONGs.
Tag 0 (UNDEFINED) is not written by templates. Values outside ±1..±12 are corrupt
(Java's `Type.fromTag` throws `IllegalArgumentException` — fatal).

All value types are stored as UTF-8 strings of their Oak string representation
(longs as decimal, doubles via Java `Double.toString`, booleans as `"true"`/`"false"`,
decimals via `BigDecimal.toString`, dates as ISO-8601 strings) except BINARY, which is
a blob record (`SegmentPropertyState.getValue(RecordId, Type)`;
`records.md` "all JCR and Oak values … strings encoded in UTF-8").

### 1.4 Property ordering inside a template — REQUIRED for lookup

`Template`'s constructor sorts `PropertyTemplate[]` with `Arrays.sort`
(`Template.java`) using `PropertyTemplate.compareTo`:

```java
Comparator.comparingInt(PropertyTemplate::hashCode)   // == name.hashCode() (Java String)
          .thenComparing(PropertyTemplate::getName)   // lexicographic by UTF-16 code units
          .thenComparing(PropertyTemplate::getType)
```

The **on-disk order equals this sorted order** (the writer builds templates from
already-sorted `Template` objects). `Template.getPropertyTemplate(name)` performs a
linear scan keyed on `name.hashCode()` — the Java `String.hashCode`:

```
h = 0            (i32, wrapping)
for each UTF-16 code unit c of the string: h = 31*h + c
```

A Rust reader that only iterates properties may ignore the ordering; a reader that
implements by-name lookup by binary/linear search over hash must reproduce Java
`String.hashCode` over **UTF-16 code units** with wrapping i32 arithmetic. The
property's *index* (position in this order) is what selects its value from the node's
property-value list (`PropertyTemplate.getIndex`, `SegmentNodeState.getRecordId`).

`jcr:primaryType` (single-valued NAME) and `jcr:mixinTypes` (multi-valued NAME) are
**not** in the property list — they live in the head/name-ID section of the template
and have **no** entry in the node's property-value list (`Template.java` field docs).
A property literally named `jcr:primaryType` but of a different type/arity than
(NAME, single) would be stored as an ordinary property (`Template(reader, NodeState)`
constructor checks both name and type).

---

## 2. Node record (RecordType.NODE)

Authoritative grammar: `SegmentParser.parseNode`; write side
`DefaultSegmentWriter.writeNodeUncached` + `RecordWriters.NodeStateWriter`.

### 2.1 Byte layout

A node record is a fixed sequence of 6-byte record IDs (no other fields):

| record-ID slot | rawOffset | field | presence |
|---|---|---|---|
| 0 | 0 | **stable-ID record ID** | always |
| 1 | 6 | template record ID | always |
| 2 | 12 | child map record ID (MANY) **or** single child node record ID (ONE) | only if template has ≥1 child |
| 2 or 3 | 12 or 18 | property-value list record ID | only if `propertyCount > 0` |

Slot arithmetic exactly as in the Java:

* Template ID: `rid(node, 0, 1)` (`SegmentNodeState.getTemplateId`,
  `SegmentParser.parseNode`).
* If template is MANY_CHILD_NODES: child map ID = `rid(node, 0, 2)`
  (`SegmentNodeState.getChildNodeMap`).
* If template is SINGLE child: child node ID = `rid(node, 0, 2)`
  (`SegmentNodeState.getChildNode`, `Template.getChildNode` reads at rawOffset
  `2 * RECORD_ID_BYTES` = 12, which is slot 2).
* Property-value list ID = `rid(node, 0, ids)` where `ids = 2` if ZERO children else
  `3` (`SegmentNodeState.getRecordId(segment, template, propertyTemplate)`:
  `int ids = 2; if (childName != ZERO_CHILD_NODES) ids++;`). Present **only** when the
  template's `propertyCount > 0` (`RecordWriters.NodeStateWriter` writes ids in order;
  `DefaultSegmentWriter.writeNodeUncached` adds the list only `if (!pIds.isEmpty())`).

The property-value list is an **uncounted** LIST (§0.4) of `propertyCount` record IDs;
entry *i* is the value record of the property with template index *i*
(`ListRecord pIds = new ListRecord(rid, propertyTemplates.length); pIds.getEntry(index)`).

### 2.2 The stable ID (slot 0)

`SegmentNodeState.getStableIdBytes()`:

* Read `id0 = rid(node, 0, 0)`.
* **If `id0 == the node's own record ID`** (same segment ID and record number): the
  stable ID is *implicit* — it is the 20-byte serialization (§0.2) of the node's own
  record ID (`RecordId.getBytes()`). In this case slot 0 is a self-reference marker,
  not a pointer (`RecordWriters.NodeStateWriter`: "If no stable ID exists … it is
  generated from the current record ID … only a marker and is not a reference to
  another record").
* **Otherwise** `id0` points to a BLOCK record of exactly 20 raw bytes:
  `id0.getSegment().readBytes(id0.recordNumber, 0, 20)` — the serialized
  (msb, lsb, recordNumber) of the *original* node record before it was rewritten by
  compaction (`DefaultSegmentWriter.writeNodeUncached` writes it with
  `writeBlock(bytes, 0, 20)`).

String form of a stable ID: `uuid(msb,lsb) + ":" + recordNumber`
(`SegmentNodeState.getStableId(Buffer)`). Stable IDs give compaction-invariant node
identity; `SegmentNodeState.fastEquals` compares them. A read-only exporter needs them
only if it wants identity/equality semantics matching Oak.

### 2.3 Reading a property value

Given node `n`, template `t`, property template `p` at index `i`
(`SegmentNodeState.getProperty` → `getRecordId` → `reader.readProperty`):

```
ids = 2 + (t has any child ? 1 : 0)
listId = rid(n, 0, ids)
valueId = list_get_entry(listId, size = t.propertyCount, index = i)
```

**Single-valued property** (`type byte > 0`): `valueId` points directly at a VALUE
record — a string (§0.3) for every type except BINARY, a blob for BINARY
(`SegmentParser.parseProperty` else-branch, `SegmentPropertyState.getValue`).

**Multi-valued property** (`type byte < 0`): `valueId` points at a **counted LIST**
(`SegmentParser.parseProperty`, `SegmentPropertyState.getValueList`):

| offset | size | field |
|---|---|---|
| 0 | 4 | `count` (i32 BE) — number of values |
| 4 | 6 | record ID of the value list root (uncounted LIST of `count` value-record IDs) — **present only if `count > 0`** |

Each element is then read as a single value of the base type. An empty multi-valued
property is just the 4-byte `count = 0` (`DefaultSegmentWriter.writeProperty`:
`RecordWriters.newListWriter()` with count 0 and no id). A multi-valued property with
one element has `count = 1` and the "list root" points directly at the value (§0.4
size-1 rule).

`SegmentPropertyState.count()` returns the i32 at offset 0 for arrays, 1 otherwise.
`SegmentPropertyState.size(index)` for non-binary values returns
`Segment.readLength(valueId)` — i.e. the *byte* length of the stored UTF-8 string.

---

## 3. Child-node lookup through the MAP record

(Full HAMT byte layouts belong to the map-record subsystem spec; this section gives
what node traversal needs, from `MapRecord.java`.)

A node with MANY children stores at slot 2 the ID of a MAP record mapping child name →
child node record ID. Three physical forms, discriminated by the first i32 `head`:

* **diff** if `head == -1` (`MapRecord.isDiff`): layout
  `[-1:i32][hash:i32][keyId:6][valueId:6][baseMapId:6]` (rawOffsets 0,4,8,14,20;
  `SegmentParser.parseMapDiff`, `MapRecord.getEntry` reads key at `(8,0)`, value at
  `(8,1)`, base at `(8,2)`). Lookup: if `hash(name) == stored hash` and the key string
  equals `name`, answer `valueId`; else recurse into `baseMapId`. Only one diff level
  is ever written per map ("There is only ever one single diff record for a map",
  `RecordType.java`), but readers recurse unconditionally.
* Otherwise decode `head`: `size = head & 0x1FFFFFFF` (29 bits), `level = head >>> 29`
  (3 bits). **Beware:** `MapRecord.SIZE_BITS = 32 - LEVEL_BITS` where
  `LEVEL_BITS = numberOfTrailingZeros(highestOneBit(7) << 1) = 3`; the code comments
  claiming "Currently 4"/"Currently 28" are stale — the computed values are
  LEVEL_BITS = 3, SIZE_BITS = 29, `MAX_SIZE = 2^29 - 1 = 536_870_911`
  (matches `records.md`'s "up to 2^29 − 1 entries" warning).
* **branch** if `size > 32 && level < 7` (`MapRecord.isBranch`;
  `BUCKETS_PER_LEVEL = 32`, `MAX_NUMBER_OF_LEVELS = ceil(32/5) = 7`): layout
  `[head:i32][bitmap:i32][bucketIds:6 each]`, one ID per set bit of `bitmap`, in
  ascending bit order. Lookup (`MapRecord.getEntry`):

  ```
  index = (hash >> (32 - (level+1)*5)) & 0x1F      # Java >> is arithmetic; hash is i32
  bit = 1 << index
  if bitmap & bit == 0: absent
  ids = popcount(bitmap & (bit - 1))
  recurse into rid(map, 8, ids)                    # child map has level+1 in its head
  ```
* **leaf** otherwise: layout `[head:i32][hash_i:i32 × size][keyId,valueId : 6+6 × size]`,
  entries sorted by hash treated as **unsigned** 32-bit (`HASH_MASK = 0xFFFFFFFFL`),
  ties broken by key string order. Key ID *i* is at `rid(map, 4 + size*4, 2*i)`,
  value ID at `rid(map, 4 + size*4, 2*i + 1)`. `MapRecord.getEntry` uses interpolation
  search; a Rust port may use plain binary search on `(hash as u32)` followed by
  string comparison among equal hashes (Java compares `readString(keyId).compareTo(name)`
  and steers the search with that result, so equal-hash entries are ordered by
  Java `String.compareTo`, i.e. lexicographic by UTF-16 code unit).

The hash function (`MapRecord.getHash`, all i32 wrapping):

```
M = 0xDEECE66D; A = 0xB
hash(name) = (javaStringHashCode(name) ^ M) * M + A
```

Map size for `getChildNodeCount`: `size` from the head (diff: size of the base map).
The map's *values* are node record IDs; its *keys* are string records with the child
names.

---

## 4. Repository super-root, journal head, and checkpoints

The record ID found in the journal (`journal.log`, covered by the store-layer spec)
points to a NODE record called the **super-root**. It is *not* the JCR root. Structure
(`SegmentNodeStore.java`: "The root node of the JCR content tree is actually stored in
the node `/root`, and checkpoints are stored under `/checkpoints`"; constants
`ROOT = "root"`, `CHECKPOINTS = "checkpoints"`):

```
(super-root)
 ├─ root            ← the actual content root (SegmentNodeStore.getRoot():
 │                     scheduler.getHeadNodeState().getChildNode("root"))
 └─ checkpoints     ← optional; absent until the first checkpoint is created
     └─ <checkpoint-name>            (name is a random UUID string, one child per checkpoint)
         ├─ timestamp : LONG         expiry time = creationMillis + lifetime,
         │                           or Long.MAX_VALUE on overflow
         ├─ created   : LONG         creation time, System.currentTimeMillis()
         ├─ properties                child node holding the user-supplied STRING
         │    └─ (arbitrary props)    metadata of the checkpoint
         └─ root                      full snapshot of the content root at checkpoint time
```

Source: `scheduler/LockBasedScheduler.CPCreator.call()` (`cp.setProperty("timestamp", …)`,
`cp.setProperty("created", now)`, `props = cp.setChildNode("properties")`,
`cp.setChildNode(ROOT, state.getChildNode(ROOT))`) and
`SegmentNodeStore.checkpointInfo` / `retrieve` which read
`head.getChildNode("checkpoints").getChildNode(name).getChildNode("properties" | "root")`.

Reader guidance:
* Resolve head → super-root node → child `"root"` for content. Missing `"root"` on an
  initialized repository is corruption; on a brand-new store the super-root may have
  zero children.
* Checkpoints whose `timestamp` is missing, non-LONG, or `< now` are considered expired
  (`CPCreator` removes them lazily on the next checkpoint creation:
  `if (ts == null || ts.getType() != LONG || now > ts.getValue(LONG)) cp.remove()`);
  a read-only tool should still list/export them as stored.
* Checkpoint `root` children share records with the head tree (hard links via record
  IDs) — traversal must tolerate diamond sharing (it is a DAG, not a tree).

---

## 5. Version differences

* **Segment format versions 12 (`0x0C`) and 13 (`0x0D`)** — byte 3 of the segment
  header (`SegmentVersion.java`; only these two are valid, versions 10/11 are the
  legacy oak-segment format and are rejected). **The record grammar in this document
  is byte-identical in both versions.** The versions differ only in segment-header
  fields: V13 adds a *full generation* + *compacted flag* at header offset 4
  (`data/SegmentDataV13.java`: `fullGeneration = i32@4 & 0x7fffffff`,
  `isCompacted = i32@4 < 0`), whereas V12 reports `fullGeneration = generation` and
  `isCompacted = true` (`data/SegmentDataV12.java`). Generation is i32 at offset 10 in
  both. None of this affects node/template/property parsing.
* **TAR index V1 vs V2** — out of scope here (tar layer); has no effect on record
  layouts.
* Bulk segments (segment ID with the "bulk" type nibble) contain only raw BLOCK data
  addressed by identity record numbers (`Segment` constructor:
  `IdentityRecordNumbers`, `newRawSegmentData`); node/template/property records occur
  only in data segments.

---

## 6. Error and recovery behavior summary

Fatal (Java throws; a Rust reader should return hard errors):
* Unresolvable segment reference in a record ID (`Segment.dereferenceSegmentId`).
* Segment signature ≠ `"0aK"` or version ∉ {12, 13} (`Segment` constructor,
  `SegmentVersion.isValid`).
* Value-record head byte in `0xF8..0xFF` (`SegmentParser.parseBlob`).
* String length ≥ 2^31−1, or reading a BLOB via the string path
  (`SegmentData.readString`).
* Property type byte with |value| ∉ 1..12 (`Type.fromTag`).
* List size < 0 or > 255³ (`ListRecord` constructor `checkArgument`).

Tolerated / non-fatal:
* Missing child in a map (returns "missing node", `Template.getChildNode` →
  `MISSING_NODE`); name lookup against a ZERO-child or wrong-single-name template
  likewise returns missing rather than erroring.
* Expired checkpoints remain readable.
* `head == -1` map-diff records may chain onto any map form; readers recurse.
* Both-clear child bits with an unreadable child-name string cannot be distinguished
  from corruption by structure alone; the Java simply attempts the string read and
  propagates any failure.

## 7. End-to-end read pseudocode

```
read_node(nodeId):
    t_id   = rid(nodeId, 0, 1)
    t      = read_template(t_id)                    # §1
    child  = match t.arity:
        ZERO   -> none
        ONE    -> node at rid(nodeId, 0, 2), name = t.childName
        MANY   -> map at rid(nodeId, 0, 2)          # §3
    props  = if t.propertyCount > 0:
        ids    = 2 + (t.arity != ZERO ? 1 : 0)
        listId = rid(nodeId, 0, ids)
        for i in 0..t.propertyCount:
            vId = list_get_entry(listId, t.propertyCount, i)
            if t.prop[i].multi: read count + value list at vId    # §2.3
            else:               read single value at vId
    synthesize jcr:primaryType / jcr:mixinTypes from t's head section  # §1.4
```
