# Oak Segment TAR — Writer Specification: Record Serialization (`RecordWriters` / `DefaultSegmentWriter`)

Status: normative for the Rust port's write path.
Source tree: `oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/` (Oak trunk, read 2026-08).
Builds on the reader-side specs in this directory — especially `record-layer.md` (byte layouts as seen by the reader), `segment-layer.md` (segment header, references, record table), `node-layer.md` (templates, node states). This document specifies what is NEW for the writer: the exact algorithms that *produce* those bytes, the deduplication machinery that decides *which* record ids appear, and the ordering/durability consequences.

Primary sources, cited throughout:

| File | Role |
|---|---|
| `RecordWriters.java` | Byte-exact serializers for each record kind |
| `DefaultSegmentWriter.java` | Orchestration: `SegmentWriteOperation`, value/map/list/template/node algorithms |
| `SegmentWriter.java`, `SegmentWriterFactory.java` | Public interface |
| `WriteOperationHandler.java` | `execute(gcGeneration, op)` bridge |
| `SegmentBufferWriter.java` | `prepare()` / `writeRecordId()` / `flush()` (full spec in the segment-buffer writer doc; cited here for reference forcing) |
| `WriterCacheManager.java`, `RecordCache.java`, `file/PriorityCache.java` | Deduplication caches |
| `MapRecord.java`, `MapEntry.java`, `ListRecord.java`, `Segment.java`, `SegmentStream.java`, `SegmentNodeState.java`, `Template.java`, `RecordId.java` | Constants and key derivations |

All multi-byte integers are big-endian (`record-layer.md` §0.1). All `int` arithmetic below is Java 32-bit two's-complement with silent wrap-around; `<<`/`>>` shift counts are taken **mod 32** for `int` and mod 64 for `long` (Java semantics — this matters, see §5.4).

---

## 1. Execution model

### 1.1 `SegmentWriteOperation` and the `execute` pattern

Every public `DefaultSegmentWriter` entry point (`writeNode`, `writeBlob`, `writeStream`, package-private `writeMap`/`writeList`/`writeString`/`writeBlock`/`writeProperty`) creates one `SegmentWriteOperation` bound to the store's *current* GC generation:

```java
// DefaultSegmentWriter.writeNode(...)
return new SegmentWriteOperation(writeOperationHandler.getGCGeneration())
        .writeNode(state, stableIdBytes);
```

`SegmentWriteOperation` is the recursion context ("a poor mans monad" per the class comment, `DefaultSegmentWriter.java` lines 205–210). It is **not thread safe**; thread safety comes from the `WriteOperationHandler`. On construction it resolves the three deduplication caches for `gcGeneration.getGeneration()` (the plain `int` generation only — see §8.3):

```java
SegmentWriteOperation(@NotNull GCGeneration gcGeneration) {
    int generation = gcGeneration.getGeneration();
    this.stringCache   = cacheManager.getStringCache(generation);
    this.templateCache = cacheManager.getTemplateCache(generation);
    this.nodeCache     = cacheManager.getNodeCache(generation);
}
```

Note on the task's "`with(writer)` pattern": older Oak versions had `SegmentWriteOperation.with(SegmentBufferWriter)`. In current trunk this has been replaced by `newWriteOperation(RecordWriter)` + `writeOperationHandler.execute(gcGeneration, op)` (`DefaultSegmentWriter.java` line 230). Each individual record write is an independent `execute` call:

```java
private WriteOperation newWriteOperation(RecordWriter recordWriter) {
    return writer -> recordWriter.write(writer, store);
}
```

The handler (a `SegmentBufferWriterPool` for pooled writers, or a raw `SegmentBufferWriter` for compaction — filestore layer doc) picks a `SegmentBufferWriter` whose generation equals `gcGeneration` and runs the operation on it. **Consequence for a port:** a logical write (one node) is a *sequence* of record writes that may be interleaved with other threads' records in the same segment (pooled case) or laid down back-to-back (single-writer case). Byte content of each record is fixed; segment packing is not, and readers do not care.

### 1.2 `RecordWriter.write` = `prepare` + content

Every serializer extends `RecordWriters.RecordWriter` (`RecordWriters.java` lines 49–76):

```java
public final RecordId write(SegmentBufferWriter writer, SegmentStore store) throws IOException {
    RecordId id = writer.prepare(type, size, ids, store);
    return writeRecordContent(id, writer);
}
```

- `type` — `RecordType` used only for the segment's record-table entry (root-record typing).
- `size` — number of content bytes **excluding** record ids that will be written via `writeRecordId`.
- `ids` — the collection of record ids the record will reference. `prepare` uses `size + ids.size() * RECORD_ID_BYTES` (6 bytes each, `Segment.java` line 79: `RECORD_ID_BYTES = 2 + 4`) aligned to 4 (`1 << Segment.RECORD_ALIGN_BITS`) to allocate space, and to pre-estimate how many *new segment references* the record could add (`SegmentBufferWriter.prepare`, lines 384–460). If the estimated segment size would exceed 256 KiB the current segment is flushed and `prepare` recurses once into a fresh segment; if a record cannot fit an empty segment, `IllegalArgumentException("Record too big: ...")` is thrown.

**Declared-vs-written discrepancy to replicate carefully:** `ids` is only used for *space estimation and reference pre-accounting*; content is whatever `writeRecordContent` emits. One writer deliberately under-declares: `NodeStateWriter` (§7.4) writes the stable-id record id in its `size` bytes but does **not** include it in `ids`, so `prepare` does not pre-account a possible new segment reference for it. This is safe in Oak only because `flush()` re-checks total length; a port must either replicate exactly or over-account (over-accounting only causes slightly earlier segment rollover, which is always legal).

### 1.3 Cross-segment references

`SegmentBufferWriter.writeRecordId` (lines 248–271):

```java
writer.writeShort(toShort(writeSegmentIdReference(recordId.getSegmentId())));
writer.writeInt(recordId.getRecordNumber());
```

where `writeSegmentIdReference` returns `0` when the referenced segment **is the segment currently being written**, else `segmentReferences.addOrReference(id)` — a 1-based index into the segment's referenced-segment-ID table, appending the 16-byte UUID on first use. A hard limit is enforced *before* writing: `segmentReferences.size() + 1 < 0xffff` (`checkState`, line 250). So: **any record id produced by an earlier `execute` that landed in a different (possibly already flushed) segment forces a segment reference entry in the current segment's header.** This is the writer-side counterpart of `segment-layer.md`'s reference table and is what makes tar-level graph/cleanup work.

---

## 2. Constants (exact values)

| Constant | Value | Source |
|---|---|---|
| `Segment.RECORD_ID_BYTES` | 6 | `Segment.java:79` |
| `Segment.MAX_SEGMENT_SIZE` | `1 << 18` = 262144 | `Segment.java:90` |
| `Segment.SMALL_LIMIT` | `1 << 7` = 128 | `Segment.java:97` |
| `Segment.MEDIUM_LIMIT` | `(1 << 14) + 128` = 16512 | `Segment.java:106` |
| `Segment.BLOB_ID_SMALL_LIMIT` | `1 << 12` = 4096 | `Segment.java:115` |
| `RecordId.SERIALIZED_RECORD_ID_BYTES` | 20 | `RecordId.java:47` |
| `SegmentStream.BLOCK_SIZE` | `1 << 12` = 4096 | `SegmentStream.java:41` |
| `ListRecord.LEVEL_SIZE` | 255 | `ListRecord.java:35` |
| `MapRecord.BITS_PER_LEVEL` | 5 | `MapRecord.java:69` |
| `MapRecord.BUCKETS_PER_LEVEL` | 32 | `MapRecord.java:74` |
| `MapRecord.MAX_NUMBER_OF_LEVELS` | 7 | `MapRecord.java:79` |
| `MapRecord.LEVEL_BITS` / `SIZE_BITS` | **3 / 29** — computed as `numberOfTrailingZeros(highestOneBit(7) << 1)` = `numberOfTrailingZeros(8)` = 3, `SIZE_BITS = 32 − 3` = 29. The Javadoc comments in the file ("Currently 4" / "Currently 28") are stale and WRONG; trust the expression. | `MapRecord.java:86–93` |
| `MapRecord.MAX_SIZE` | `(1<<29)-1` = 536 870 911 (the stale `// ~268e6` comment is wrong; note `ERROR_SIZE_HARD_STOP` = 536 000 000 sits just under this) | `MapRecord.java:98` |
| `MapRecord.WARN_SIZE` / `ERROR_SIZE` / `ERROR_SIZE_DISCARD_WRITES` / `ERROR_SIZE_HARD_STOP` | 400 000 000 / 450 000 000 / 500 000 000 / 536 000 000 | `MapRecord.java:103–120` |
| `MapRecord.M` / `A` / `HASH_MASK` | `0xDEECE66D` / `0xB` / `0xFFFFFFFFL` | `MapRecord.java:51–53` |
| `DefaultSegmentWriter.CHILD_NODE_UPDATE_LIMIT` | 10000 (system property `child.node.update.limit`) | `DefaultSegmentWriter.java:89` |
| `binariesInlineThreshold` default | `Segment.MEDIUM_LIMIT` = 16512 (must be `0 ≤ t ≤ 16512`) | `FileStoreBuilder.java:100`, `DefaultSegmentWriter` ctor checks |
| Cache defaults: string / template / node | 15000 / 3000 / 1048576 entries | `WriterCacheManager.java:53–85` (overridable via `oak.tar.stringsCacheSize` etc.) |

---

## 3. Value records (strings and inline binaries)

Reader layout: `record-layer.md` §2–§3. Writer algorithms:

### 3.1 Small/medium inline value — `ArrayValueWriter` (`RecordWriters.java:344–376`)

Type `VALUE`, declared size = `length + (length < 128 ? 1 : 2)`, no ids.

```
if length < 128:                      # SMALL_LIMIT
    write_u8(length)                  # 0xxxxxxx
else:                                 # 128 <= length <= 16511
    write_u16_be((length - 128) | 0x8000)   # 10xxxxxx xxxxxxxx
write_bytes(data[0 .. length])
```

Caller-side guard: `writeValueRecord(int length, byte... data)` requires `length < MEDIUM_LIMIT` (`DefaultSegmentWriter.java:498–502`).

### 3.2 Long value head — `SingleValueWriter` (`RecordWriters.java:318–335`)

Type `VALUE`, size 8, ids = `{listRootId}`. Caller computes the length word (`DefaultSegmentWriter.writeValueRecord(long,RecordId)`, lines 492–496):

```
len_word = (length - 16512) | (0x3 << 62)     # 64-bit; top bits 11
write_u64_be(len_word)
write_record_id(list_root)                    # 6 bytes: ref index + record number
```

(The reader masks the top 3 bits, so `110xxxxx...`; the writer always produces exactly `0x3<<62`, i.e. first byte `0xC0 | high length bits`.)

### 3.3 Block record — `BlockWriter` (`RecordWriters.java:296–312`)

Type `BLOCK`, size = `length`, no ids, content = the raw bytes, **no length header**. Length is implied by the referencing structure (fixed 4096-byte blocks except the tail block).

### 3.4 `writeString` (`DefaultSegmentWriter.java:510–547`) — exact algorithm

```
id = stringCache.get(string)                       # key: the full Java String
if id != null: return id
data = utf8(string)
if data.length < 16512:
    id = writeValueRecord(data.length, data)       # §3.1
    stringCache.put(string, id)                    # ONLY short strings are cached
    return id

# long string: bulk-segment strategy
pos = 0; blockIds = []
while pos + 262144 <= data.length:                 # as many FULL bulk segments as possible
    bulkId = idProvider.newBulkSegmentId()
    store.writeSegment(bulkId, data, pos, 262144)  # raw payload, no segment header
    for i in 0, 4096, 8192, ... < 262144:
        blockIds.append(RecordId(bulkId, i))
    pos += 262144
while pos < data.length:                           # remainder inlined as BLOCK records
    len = min(4096, data.length - pos)
    blockIds.append(writeBlock(data, pos, len))    # §3.3, goes into the current DATA segment
    pos += len
return writeValueRecord(data.length, writeList(blockIds))   # §4 then §3.2 — NOT cached
```

Key facts: (a) bulk segments are written to the store *immediately and unbuffered* via `store.writeSegment`, before the value record that references them (§9.1); (b) block ids inside a **full** bulk segment use offsets `0, 4096, …, 258048` directly as record numbers (bulk segments have no record table; the "record number" is the byte offset in the 256 KiB address space, `record-layer.md` §0.3); (c) long strings are never cached and never deduplicated.

### 3.5 `internalWriteStream` (`DefaultSegmentWriter.java:657–706`) — long binaries

```
data = new byte[binariesInlineThreshold]
n = read_fully(stream, data, 0, data.length)
if n < binariesInlineThreshold: return writeValueRecord(n, data)      # inline small/medium

if blobStore != null:
    blobId = blobStore.writeBlob(prefix_bytes(data, n) ++ rest_of_stream)
    return writeBlobId(blobId)                                        # §3.6 — external

# no blob store: inline path continues
grow data to 16512; n += read_fully(...)
if n < 16512: return writeValueRecord(n, data)

grow data to 262144; n += read_fully(...)
length = n; blockIds = []
while n != 0:
    bulkId = idProvider.newBulkSegmentId()
    store.writeSegment(bulkId, data, 0, n)                            # exactly n payload bytes
    for i in 0, 4096, ... < n:
        blockIds.append(RecordId(bulkId, data.length - n + i))        # NOTE the offset!
    n = read_fully(stream, data, 0, data.length); length += n
return writeValueRecord(length, writeList(blockIds))
```

**Offset subtlety (must-copy):** block record numbers are `data.length - n + i` = `262144 - n + i`. For every full chunk `n == 262144` so offsets are `0, 4096, …`; but for the **final partial chunk** the offsets start at `262144 - n`. This works because bulk-segment payloads are addressed from the *end* of the 256 KiB address space (`Segment.java:425`: `return data.size() - (MAX_SEGMENT_SIZE - offset)`). A port that naively emitted offsets from 0 for a partial trailing bulk segment would produce unreadable streams.

Difference from `writeString`: the binary path writes the final partial chunk as a (short) bulk segment; the string path inlines the remainder as BLOCK records in the data segment. Both are read back identically through the block list.

`writeStream` wrapper (lines 624–655): if the stream is a `SegmentStream` of this store (`getRecordIdIfAvailable`) and its id is **not** old-generation, return the id unchanged; if old-generation and the stream has `blockIds` (i.e. is a block-list stream, not an inline value), re-emit only the head: `writeValueRecord(segmentStream.getLength(), writeList(blockIds))` — **bulk segments are re-linked, never rewritten, across compaction**. Otherwise fully serialize. The stream is always closed; a close failure after success is only logged.

> **The "of this store" qualifier is load-bearing, not incidental.** Both this wrapper and `writeBlob` (§3.7) gate re-linking on the stream belonging to the *same* store — `getRecordIdIfAvailable` returns nothing for a `SegmentStream` from another store, so a cross-store copy falls through to full serialization. Re-linking is a statement about reachability *within one store*: a reference from the new generation is what keeps a bulk segment alive there, and there alone.
>
> froe's port originally reproduced the generation half of that rule and dropped the store half, keying only on whether the block already lived in a bulk segment. Compaction was unaffected — it is same-store by construction — but `backup` and `restore` share the copier, and a reference into the *source's* bulk segments resolves to nothing in the target. The result was a backup holding the whole content tree and none of the binary content. `BulkBlockSharing` now makes the store boundary explicit at the call site rather than implicit in the segment kind; see the `backup` phase in [`interop.md`](../interop.md) for why every other check passed.

### 3.6 External blob ids — `writeBlobId` (`DefaultSegmentWriter.java:603–614`) + `SmallBlobIdWriter`/`LargeBlobIdWriter` (`RecordWriters.java:386–430`)

```
data = utf8(blobId)
if data.length < 4096:                       # BLOB_ID_SMALL_LIMIT; checkArgument in writer
    # record type BLOB_ID, size 2 + data.length, no ids
    write_u16_be(data.length | 0xE000)       # 1110xxxx xxxxxxxx
    write_bytes(data)
else:
    refId = writeString(blobId)              # ordinary string record (§3.4)
    # record type BLOB_ID, size 1, ids = {refId}
    write_u8(0xF0)                           # fixed marker byte
    write_record_id(refId)
```

`writeBlob` (lines 563–592) resolution order: same-store `SegmentBlob` and not old generation → return existing id; same-store external old-generation → `writeBlobId(segmentBlob.getBlobId())`; `BlobStoreBlob` with id → `writeBlobId`; `blob.getReference()` resolvable through the blob store → `writeBlobId(blobStore.getBlobId(reference))`; else `writeStream(blob.getNewStream())` — which goes through the §3.5 *wrapper*, so a same-store old-generation **inline** `SegmentBlob` still gets its block list re-linked (bulk segments reused) rather than fully re-serialized.

---

## 4. Lists — `writeList`, `ListBucketWriter`, `ListWriter`

Reader layout: `record-layer.md` §5. Writer (`DefaultSegmentWriter.java:450–472`):

```
writeList(list):                       # list non-empty (checkArgument)
    thisLevel = list
    while thisLevel.size > 1:
        nextLevel = []
        for bucket in partition(thisLevel, 255):        # ListRecord.LEVEL_SIZE
            if bucket.size > 1: nextLevel.append(writeListBucket(bucket))
            else:               nextLevel.append(bucket[0])   # single id passes through unchanged
        thisLevel = nextLevel
    return thisLevel[0]
```

`writeListBucket` → `ListBucketWriter` (`RecordWriters.java:276–290`): type `BUCKET`, size 0, ids = the bucket; content is exactly the record ids back to back (6 bytes each), 2–255 of them. **A singleton partition never gets a bucket record — its element id is reused verbatim at the next level.** Consequently the "list body" pointer for a 1-element list is the element itself; readers disambiguate purely via the externally known `count`.

`writeList` returns a *body root*, not a LIST record. A `LIST` head record (`ListWriter`, `RecordWriters.java:244–269`; `write_i32_be(count)` then optionally the body root id) is written only where the reader expects a counted list:
- multi-valued properties: `writeProperty` (lines 738–747) — empty array → `ListWriter()` (just `int 0`, size 4, no id); non-empty → `writeList(valueIds)` then `ListWriter(count, root)`.
- Everything else consumes the *body root* directly with an out-of-band count: template property-name list (count = property count in the template head), node property list (count = template property count), block lists (count derivable from the value length).

---

## 5. Maps (HAMT) — `writeMap`, leaf/branch writers, buckets, diffs

Reader layout and hash trie semantics: `record-layer.md` §7. Everything here is `DefaultSegmentWriter.java` unless noted.

### 5.1 Hash and entry ordering

`MapEntry.getHash()` → `MapRecord.getHash(name)` (`MapRecord.java:62–64`), with Java 32-bit wrapping:

```
hash(name) = (java_string_hashCode(name) XOR 0xDEECE66D) *wrap32 0xDEECE66D +wrap32 0xB
```

`java_string_hashCode` = `s[0]*31^(n-1) + s[1]*31^(n-2) + … + s[n-1]` over UTF-16 code units, 32-bit wrapping.

Leaf entries are sorted with `MapEntry.compareTo` (`MapEntry.java:150–155`):

```java
Comparator.comparingLong((MapEntry me) -> me.getHash() & HASH_MASK)   // hash as UNSIGNED 32-bit, ascending
    .thenComparing(MapEntry::getName)                                  // Java String order (UTF-16 code units)
    .thenComparing(MapEntry::getValue, Comparator.nullsLast(naturalOrder()))  // RecordId order; never null in practice (§5.6)
```

`RecordId` natural order compares segment id then record number (`RecordId.java:114+`).

### 5.2 Leaf — `MapLeafWriter` (`RecordWriters.java:150–199`)

Non-empty: type `LEAF`, size `4 + 4*size`, ids = `[k0,v0,k1,v1,…]` (interleaved, `extractIds`). Content:

```
write_i32_be((level << 29) | size)        # SIZE_BITS = 29 (level occupies the top 3 bits — see §2)
sort entries per §5.1
for e in sorted: write_i32_be(e.hash)     # signed 32-bit hash values, sorted by UNSIGNED value
for e in sorted: write_record_id(e.key); write_record_id(e.value)
```

Empty map (no-arg `MapLeafWriter`): type `LEAF`, size 4, content a single `int 0` (level and size both 0).

Guards in `writeMapLeaf` (lines 330–338): `0 ≤ size < MAX_SIZE` (= `(1<<29)-1`, via `checkIndex(size, MAX_SIZE)`), `level ≤ 7`, and `size != 0 || level == 7` (the empty-leaf-at-level-0 case goes through the dedicated no-arg writer in `writeMapBucket`).

### 5.3 Branch — `MapBranchWriter` (`RecordWriters.java:205–238`)

Type `BRANCH`, size 8, ids = present bucket ids in ascending bucket-index order. Content:

```
write_i32_be((level << 29) | entryCount)  # entryCount = total entries in subtree; SIZE_BITS = 29
write_i32_be(bitmap)                      # bit i set <=> bucket i present
for id in bucketIds: write_record_id(id)
```

Built by `writeMapBranch` (lines 340–352): `bitmap |= 1L << i` for each non-null bucket (as `int`; i < 32 so identical to `1 << i`).

**Diff branch** (`newMapBranchWriter(bitmap, ids)`): `level = 0`, `entryCount = -1` → head word is `(0 << 29) | -1` = `0xFFFFFFFF`; "bitmap" field carries the entry's full 32-bit hash; ids = `[keyId, valueId, baseMapId]`. Layout: head(4) bitmap/hash(4) key(6) value(6) base(6).

### 5.4 `splitToBuckets` (lines 474–490) — exact, including the mod-32 shift

```
mask  = 31                              # (1 << 5) - 1
shift = 32 - (level + 1) * 5            # level 0..6  → shift 27,22,17,12,7,2,-3
for e in entries:
    index = (e.hash >> shift) & mask    # Java: ARITHMETIC shift, count taken MOD 32
    buckets[index].append(e)
```

At `level == 6`, `shift = -3`, and Java computes `hash >> ((-3) & 31)` = `hash >> 29` — an *arithmetic* shift leaving the top 3 bits sign-extended (a 3-bit signed value −4..3); after `& 31` indexes are in `{0..3} ∪ {28..31}`. A port must reproduce exactly this (shift count mod 32, arithmetic shift), or level-6/7 tries diverge from what Oak's reader (`MapRecord.getEntry`, same formula) expects.

### 5.5 `writeMapBucket(base, entries, level)` (lines 354–438) — the recursion

```
if entries empty:
    if base != null: return base.id
    if level == 0:   return write empty leaf (§5.2)
    return null                                   # absent bucket

if base == null:                                  # fresh subtree
    if entries.size <= 32 or level == 7: return writeMapLeaf(level, entries)
    changes = splitToBuckets(entries, level)
    for i in 0..31: buckets[i] = writeMapBucket(null, changes[i], level+1)
    return writeMapBranch(level, entries.size, buckets)

if base.isLeaf():                                 # small base: merge in memory
    map = {e.name: e for e in base.entries}
    for e in entries: if e.deleted: map.remove(e.name) else map[e.name] = e
    return writeMapBucket(null, map.values, level) # rewritten as fresh

# large base: per-bucket update
buckets = base.getBuckets()                       # 32 slots, null where absent
changes = splitToBuckets(entries, level)
newSize = 0; newCount = 0
for i in 0..31:
    buckets[i] = writeMapBucket(buckets[i], changes[i], level+1)
    if buckets[i] != null: newSize += buckets[i].size; newCount++
if newSize > 32:      return writeMapBranch(level, newSize, buckets)
elif newCount <= 1:   return the single bucket's id, or (if none) writeMapBucket(null, null, level)  # OAK-654 collapse
else:                 return writeMapLeaf(level, concat(entries of all buckets))                     # collapse to leaf
```

Untouched buckets keep their **existing record ids** — incremental map update reuses base subtrees byte-for-byte (this is required behavior for structural sharing but any structurally-equivalent rewrite would also be readable).

### 5.6 `writeMap(base, changes)` top level (lines 244–328)

1. **Huge-map guards** when `base.size() ≥ 400_000_000`: record high-water in system property `oak.segmentNodeStore.maxMapRecordSize`; ≥ 536 000 000 → always throw `UnsupportedOperationException`; ≥ 500 000 000 → throw unless `oak.segmentNodeStore.allowWritesOnHugeMapRecord`; ≥ 450 000 000 → log error only.
2. **Diff unfolding**: if `base.isDiff()`, read from the diff record (offsets after the 8-byte head+hash): `key = readRecordId(base, 8)`, and unless `changes` already contains that key's name, `changes[name] = readRecordId(base, 8, skip 1 id)`; then `base = MapRecord(readRecordId(base, 8, skip 2 ids))`. I.e. a diff base is folded into the change set against its underlying full map.
3. **Single-change diff optimization**: if `base != null` and exactly one change with non-null value and the key exists in `base`: equal value → return `base` id unchanged (no write at all); different value → write a **diff branch** (§5.3) `[existing keyId, new valueId, base.id]` with the entry's hash. This is how Oak keeps O(1) updates on huge child maps; it is an optimization — writing the full updated map is equally valid output.
4. **General path**: for each change, `keyId` = the base entry's existing key-string id if present, else `writeString(key)` (only when value non-null); entries with `keyId == null` (deletion of a non-existent key) are dropped; then `writeMapBucket(base, entries, 0)`.

Deleted entries (value = null) never reach leaf serialization: fresh writes drop them (no keyId when `base == null`), and base-leaf merges remove them; only the per-bucket recursion carries them downward.

---

## 6. Templates — `writeTemplate` (`DefaultSegmentWriter.java:750–823`) + `TemplateWriter` (`RecordWriters.java:436–481`)

Reader layout: `node-layer.md`. Writer algorithm:

```
id = templateCache.get(template); if id != null: return id
ids = []; head = 0
if primaryType != null: head |= 1<<31; primaryId = writeString(primaryName); ids.add(primaryId)
if mixinTypes != null:
    head |= 1<<30
    mixinIds = [writeString(m) for m in mixinNames]      # in property-value order
    ids.addAll(mixinIds); require mixinIds.size < 1024; head |= mixinIds.size << 18
if childName is ZERO_CHILD_NODES:  head |= 1<<29
elif childName is MANY_CHILD_NODES: head |= 1<<28
else: childNameId = writeString(childName); ids.add(childNameId)
for i, pt in enumerate(template.propertyTemplates):      # template's canonical order
    propertyNames[i] = writeString(pt.name)
    propertyTypes[i] = pt.type.isArray() ? (byte) -tag : (byte) tag   # JCR PropertyType tag
if propertyNames.length > 0: propNamesId = writeList(propertyNames); ids.add(propNamesId)
require propertyNames.length < (1<<18); head |= propertyNames.length

# TemplateWriter: type TEMPLATE, size = 4 + propertyCount, ids = ids (NOT the propertyNames themselves)
write_i32_be(head)
if primaryId: write_record_id(primaryId)
for m in mixinIds: write_record_id(m)                    # count known from head bits 18..27
if childNameId: write_record_id(childNameId)
if propNamesId: write_record_id(propNamesId)             # list BODY root (§4), count = head & 0x3FFFF
for i: write_u8(propertyTypes[i])                        # signed byte; negative = array

templateCache.put(template, tid)
```

Head bit layout (all facts from the code above): bit31 has-primary, bit30 has-mixins, bit29 zero-children, bit28 many-children, bits 27–18 mixin count, bits 17–0 property count. The property-name record ids are *not* referenced by the template record itself but by the propNames list buckets (hence the code comment "if the property names are stored in more than 255 separate segments, this will not work" — a single BUCKET can add at most 255 distinct segment references).

---

## 7. Nodes — `writeNode` / `writeNodeUncached` / `ChildNodeCollectorDiff`

### 7.1 `writeNode(state, stableIdBytes)` (lines 825–845)

```
compactedId = deduplicateNode(state); if compactedId != null: return compactedId
if state is SegmentNodeState and stableIdBytes == null:
    stableIdBytes = state.getStableIdBytes()
recordId = writeNodeUncached(state, stableIdBytes)
if stableIdBytes != null:
    nodeCache.put(stableIdString(stableIdBytes), recordId, cost(state))
return recordId
```

- `deduplicateNode` (lines 971–995): only for `SegmentNodeState` of the **same store**. If the node's segment is *not* old-generation → **return its existing record id** (this is what makes ordinary commits incremental: unchanged subtrees are linked, not rewritten). If old-generation → look up `nodeCache.get(sns.getStableId())`; hit → reuse the already-compacted copy.
- `isOldGeneration(id)` (lines 1016–1038), the generation gate:

```java
if (thatGen.isCompacted()) {
    return thatGen.getFullGeneration() < thisGen.getFullGeneration();   // same tail ⇒ safe to reference
} else {
    return thatGen.compareWith(thisGen) < 0;
}
```

  A `SegmentNotFoundException` while reading the generation is rethrown as `SegmentNotFoundException("Cannot copy record from a generation that has been gc'ed already", …)`.
- `cost(state)` (lines 847–850): `(byte)(Byte.MIN_VALUE + 64 - numberOfLeadingZeros(childCount))` — −128 for 0 children, −127 for 1, growing with log2(childCount). Used as the PriorityCache priority.
- `stableIdString` = `SegmentNodeState.getStableId(Buffer)` (`SegmentNodeState.java:136–142`): parse msb(8)+lsb(8)+offset(4) big-endian and format `"<uuid>:<offset>"` — the *node-cache key format*.

Note: this Oak version has no `nodeWriteStats`; the old stats-suppression survives only as a comment above `deduplicateNode(((ModifiedNodeState) state).getBaseState())` (line 857).

### 7.2 `writeNodeUncached(state, stableIdBytes)` (lines 852–946)

```
beforeId = state is ModifiedNodeState ? deduplicateNode(state.getBaseState()) : null
before / beforeTemplate = read from beforeId if non-null       # base state, already current-generation

ids = []
template = new Template(reader, state)
ids.add(template == beforeTemplate ? before.templateId : writeTemplate(template))   # §6

if template.childName == MANY_CHILD_NODES: ids.add(writeChildNodes(before, state))  # §7.3
elif template.childName != ZERO_CHILD_NODES: ids.add(writeNode(state.getChildNode(childName), null))

pIds = []
for pt in template.propertyTemplates:
    property = state.getProperty(pt.name)
    if before != null and property.equals(before.getProperty(name)): property = beforeProperty
    if property is same-store SegmentPropertyState:
        pIds.add(isOldGeneration(pid) ? writeProperty(property) : pid)              # link if current gen
    elif before == null or before not same-store:
        pIds.add(writeProperty(property))
    else:
        bt = beforeTemplate.getPropertyTemplate(name)
        if bt == null: pIds.add(writeProperty(property))                            # new property
        else:
            bp = beforeTemplate.getProperty(before.id, bt.index)
            if property.equals(bp): pIds.add(bp.recordId)                           # unchanged: link
            elif bp.isArray() and bp.type != BINARIES:
                pIds.add(writeProperty(property, bp.getValueRecords()))             # reuse per-value ids
            else: pIds.add(writeProperty(property))
if pIds non-empty: ids.add(writeList(pIds))                    # list BODY root, count = template prop count

stableId = stableIdBytes != null ? writeBlock(copyOf(stableIdBytes), 0, len) : null  # a 20-byte BLOCK record
return execute(NodeStateWriter(stableId, ids))
```

`writeProperty(state, previousValues)` (lines 713–748): binary values via `writeBlob`; string-typed values reuse `previousValues[stringValue]` when present else `writeString`; single value → the value id itself is the property record id; array → LIST head (§4).

### 7.3 Child map — `writeChildNodes` + `ChildNodeCollectorDiff` (lines 948–960, 1040–1122)

```
if before != null and before.childCount(2) > 1 and after.childCount(2) > 1:
    diff against before.getChildNodeMap() (incremental)        # after.compareAgainstBaseState(before, diff)
else:
    diff against EMPTY (full build)                            # compareAgainstEmptyState
```

The diff callbacks recursively `writeNode(child, null)` for added/changed children, record `null` for deleted ones, accumulating `Map<String, RecordId>` and calling `flush()` — `mapId = writeMap(base, childNodes); base = reader.readMap(mapId); childNodes.clear()` — whenever more than `CHILD_NODE_UPDATE_LIMIT` (10000) updates accumulate, and once at the end. So one huge commit produces a *chain* of intermediate map records (each written against the previous flush's map as base); the intermediate maps become garbage. `IOException`s inside the diff are stashed and rethrown wrapped (`throw new IOException(exception)`).

### 7.4 `NodeStateWriter` (`RecordWriters.java:487–516`) — the stable-id rule

Type `NODE`, declared size `RECORD_ID_BYTES` (6), ids = `[templateId, (childMap|childNode)?, propListRoot?]` — **the stable-id block id is intentionally not in `ids`** (§1.2). Content:

```
if stableId == null: write_record_id(id)      # SELF-REFERENCE: this record's own freshly assigned id
else:                write_record_id(stableId) # id of the 20-byte BLOCK holding the original
                                              # (msb, lsb, recordNumber) of the first incarnation
for rid in ids: write_record_id(rid)
```

So the first 6 bytes of every node record are either (a) `ref=0, recordNumber=own` — a marker meaning "my stable id is my own address" (a brand-new node), or (b) a pointer to a 20-byte BLOCK (`RecordId.SERIALIZED_RECORD_ID_BYTES`) containing the original address, written in §7.2. Readers reconstruct via `SegmentNodeState.getStableIdBytes` (`SegmentNodeState.java:163+`): equal-to-self ⇒ stable id = own serialized id; else read 20 bytes from the referenced block. **Compaction correctness depends on this**: a compacted node must carry the *original* stable id bytes so `SegmentNodeState.equals` (stable-id fast path, `SegmentNodeState.java:687`) and the node dedup cache keep working across generations.

---

## 8. Deduplication caches — what exists, keys, generations, required vs optional

### 8.1 Inventory

| Cache | Key | Value | Impl | Default capacity | Populated by | Consulted by |
|---|---|---|---|---|---|---|
| String | the Java `String` (full value; only strings with UTF-8 length < 16512) | `RecordId` of the VALUE record | `RecordCache` LRU (`maximumSize = size*4/3`, `RecordCache.java:163`) per generation | 15000 | `writeString` short path | `writeString` |
| Template | the `Template` object; equality = `primaryType, mixinTypes, properties[], childName` (`Template.java:293–307`), hashCode also mixes template type | `RecordId` of TEMPLATE record | `RecordCache` LRU per generation | 3000 | `writeTemplate` | `writeTemplate` |
| Node | stable-id string `"<uuid>:<recordNumber>"` (§7.1) **plus the generation int** (a `PriorityCache` field, not part of the string) | `RecordId` of the rewritten NODE record | single shared `PriorityCache` | 1048576 slots (power of two) | `writeNode` when `stableIdBytes != null` (i.e. rewrites/compaction) | `deduplicateNode` for old-generation nodes |

### 8.2 Generation partitioning

`WriterCacheManager.Default` keeps a `ConcurrentMap<Integer, cache>` per generation for strings/templates (`Generations`, `WriterCacheManager.java:264–295`) — lookups in generation *g* only ever see entries written in generation *g*, so a post-compaction writer can never resurrect a record id from a segment that cleanup may delete. The node cache is one `PriorityCache` whose entries carry an `int generation`; `get(key, generation)` matches only exact generation (`PriorityCache.java:332`), and `put` lets same-or-newer generations overwrite older entries regardless of cost (`PriorityCache.java:261–274`). `evictCaches(predicate)` / `purgeGenerations` drop whole generations after GC.

### 8.3 The generation used is `gcGeneration.getGeneration()` **only**

(§1.1 code.) Tail and full compaction bump `generation`, so each compaction gets fresh string/template partitions and node entries; the `fullGeneration`/`isCompacted` components do not partition caches — they only gate `isOldGeneration` (§7.1).

### 8.4 `PriorityCache` semantics (`file/PriorityCache.java`)

Open-addressed array of `size` (power of 2) entries, probe sequence `index_k = (hashCode(key) >> k) & (size-1)` for `k = 0..rehash` (default `rehash = 31 - trailingZeros(size)`), striped locks (1024 segments). `put(key, value, generation, cost)` scans the probe sequence and stops at the first of: empty slot, same-key slot with generation ≤ the new one (reuses that slot and boosts the entry's cost by 1, saturating at 127), or any older-generation slot; failing those it remembers the *cheapest* slot seen with `cost < initialCost` (generation is **not** checked here — even newer-generation entries can be cost-evicted) and keeps scanning for cheaper ones; if no slot qualifies, the put **fails silently** (`false`). `get(key, generation)` requires exact generation match and increments the entry's cost on hit. Key hash is Java `String.hashCode` of the stable-id string.

### 8.5 Required vs optimization — the port ruling

- **None of the three caches affects readability of the output.** A writer with all caches empty (Oak ships exactly this: `WriterCacheManager.Empty`, used e.g. by some tooling) produces duplicated-but-valid records; every record is self-consistent and AEM reads it fine.
- **String/template caches: pure size optimization.** Skip initially if desired; expect larger segments.
- **Node cache: required *in practice* for compaction, for size not correctness.** Without it, `deduplicateNode` never dedups old-generation nodes, so every *shared* subtree (checkpoints sharing structure with head, or the same subtree reachable twice) is compacted once per path. Content equality still holds (stable ids are preserved, and `SegmentNodeState.equals` falls back to content comparison), so the result is correct — but a repository with checkpoints can blow up multiplicatively and, worse, subsequent `compare`-based diffs in AEM lose the cheap stable-id equality between the checkpoint and head copies only when stable ids were *not* preserved. Since the writer always preserves stable ids (§7.4), semantic behavior is safe; treat the node cache as a strongly-recommended optimization, mandatory for production-size compactions.
- **What IS required for correctness** (not a cache, but adjacent): the *linking* rules — `deduplicateNode` returning existing ids for current-generation same-store nodes, and `isOldGeneration`'s exact comparison — because they decide whether the writer references existing segments (fine) or copies from generations that cleanup will delete (fatal: dangling `SegmentNotFoundException` after cleanup). A port must implement §7.1's generation gate exactly.

---

## 9. Durability, ordering, errors

### 9.1 Order of writes

Within this subsystem the only store-visible writes are:

1. `store.writeSegment(bulkId, …)` — bulk segments, written **synchronously at the point of value serialization** (§3.4/§3.5), before the block-list/value records referencing them are even buffered.
2. `SegmentBufferWriter.flush(store)` → `store.writeSegment(dataSegmentId, …)` — the buffered data segment, written when full (from `prepare`) or on explicit `SegmentWriter.flush()`.

Because every record id written into the current data segment was returned by a *completed* earlier write (same buffer, or an already-`writeSegment`-ed segment), the store-level append order always satisfies: **referenced segment is persisted no later than the referencing segment.** Journal advancement, fsync and tar mechanics are the filestore layer's job (`filestore-layer.md`); nothing in `RecordWriters`/`DefaultSegmentWriter` forces or fsyncs.

### 9.2 Failure behavior

- Any `IOException` aborts the current logical write; **nothing is rolled back**. Records already buffered stay in the segment buffer and may be flushed later; bulk segments already written stay in the tar file. All such records are unreachable garbage — Oak tolerates unreachable records/segments indefinitely (they are only removed by cleanup), so partial writes are safe as long as the journal head is never advanced to a record id that wasn't fully written and flushed.
- `ChildNodeCollectorDiff` converts callback exceptions into a wrapped `IOException` (§7.3).
- *(Added during verification)* Binary property values: `writeProperty` wraps any `IOException` thrown by `writeBlob` in `IllegalStateException("Unexpected IOException", e)` (`DefaultSegmentWriter.java:722–727`), so a blob failure inside property serialization surfaces as a runtime exception, not an `IOException`. A port should not assume all write failures propagate as I/O errors.
- Huge-map limits throw `UnsupportedOperationException` (§5.6) — the commit fails, store unharmed.
- `PriorityCache.put` failure is silent; `writeStream` close-failure after success is logged only (§3.5).

---

## 10. AEM safety invariants (record-serialization subset)

A Rust writer must guarantee all of the following or a subsequent AEM start can fail or corrupt:

1. **Big-endian everywhere**; record ids serialized as `u16 segment-ref-index + u32 record-number`, with index 0 = current segment and indices ≥ 1 matching the segment's reference table in order of first use; never exceed 0xfffe references per segment (§1.3).
2. **Length encodings byte-exact**: small `len<128` 1-byte; medium `(len-128)|0x8000` 2-byte; long `(len-16512)|(0x3<<62)` 8-byte + list id; small blob id `(len)|0xE000` 2-byte (len < 4096); large blob id marker byte exactly `0xF0` + string record id (§3). *(Added during verification)* All string and blob-id payloads are the UTF-8 encoding of the Java string; lengths in the headers count UTF-8 bytes, while dedup/cache keys and map hashes operate on the UTF-16 `String` (§3.4, §5.1).
3. **Bulk segment offsets**: block record numbers inside a bulk segment of payload `n` bytes are `262144 − n + i` for `i = 0, 4096, …` (end-aligned address space); full 256 KiB chunks therefore start at 0. Bulk segments are raw payload with a bulk-type segment id (`…AVAV` per `segment-layer.md`) and must reach the store before any data segment that references them (§3.5, §9.1).
4. **List chunking**: fan-out 255, singleton partitions pass their element id through unchanged, BUCKET records are bare id arrays, LIST heads (`count` + optional body root) only where §4 says so.
5. **Map records**: hash = `((jhash ^ 0xDEECE66D) * 0xDEECE66D + 0xB)` with 32-bit wrap; leaf head `(level<<29)|size` (level = top **3** bits, size = low **29** bits — the "4/28" comments in `MapRecord.java` are stale); leaf entries sorted by unsigned hash, then name, then value id; hashes array then key/value id pairs; branch head `(level<<29)|entryCount` + presence bitmap + present buckets ascending; diff record head `0xFFFFFFFF` + hash + key/value/base ids; bucket index `(hash >> ((32-(level+1)*5) mod 32)) & 31` with *arithmetic* shift; maps deeper than level 7 collapse into (possibly >32-entry) leaves (§5).
6. **Template head bits** exactly as §6, property tags as signed bytes (negative = array), record ids in the order: primary, mixins, childName, propNames-root; property-name ids referenced only via the list buckets.
7. **Node stable ids and field order**: first record id of a NODE record is either a self-reference (new node) or a pointer to a 20-byte BLOCK holding the original `(msb, lsb, recordNumber)`; when rewriting any existing node (compaction, checkpoint copy) the original stable-id bytes MUST be propagated, never regenerated — AEM's node equality, checkpoint diffing and the dedup caches all key on it (§7.4). *(Added during verification)* The remaining record ids follow in exactly this order: template id, then the child-map id (`MANY_CHILD_NODES`) or single-child node id (one named child) if any, then the property-list body root if the template has properties (§7.2).
8. **Generation gate**: never write a record that references a record id from an *older* generation as defined by §7.1's `isOldGeneration` (compacted → compare `fullGeneration`; non-compacted → full `compareWith`); always reference (not copy) same-generation same-store records where Oak would. Violations become `SegmentNotFoundException`s after the next cleanup.
9. **Dedup caches must be generation-partitioned** if implemented: a cache hit may only ever return a record id whose segment generation equals the current write generation (string/template) or that was written by this process in the current generation (node cache with exact-generation `get`). Serving a stale-generation id is the one way a cache can corrupt the store.
10. **String cache only below 16512 UTF-8 bytes**; long strings/binaries always re-emit head records (bodies may be re-linked per §3.5) — never cache or dedup long-value head ids across generations.
11. **Failure hygiene**: on any error, leave partial records/bulk segments in place (they are tolerated garbage) but never publish (journal/checkpoint) a record id that was not completely written and whose segment was not durably flushed.
12. **Record alignment**: every record occupies `align(size + 6*|ids|, 4)` bytes allocated back-to-front in the segment; content written front-to-back within the allocation (`SegmentBufferWriter.prepare`, §1.2); a record plus header overhead must fit 256 KiB or the write must be split/refused exactly as Oak does.
