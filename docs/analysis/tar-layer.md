# Oak Segment-Tar: TAR Archive Layer — Byte-Exact Specification

Scope: the on-disk layout of a TarMK `data?????*.tar` file — file naming, segment
entries, the three trailer entries (`.brf`, `.gph`, `.idx`), checksums, and the exact
open/recovery procedures. All facts are cited to the Java sources under
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/file/tar/` (referred to
below by bare file name) and the official doc `oak-doc/.../nodestore/segment/tar.md`.

## 0. Global conventions

* **Endianness**: every multi-byte binary integer in the index, graph and binary
  references structures is **big-endian**. All reads go through
  `org.apache.jackrabbit.oak.commons.Buffer`, a thin wrapper over `java.nio.ByteBuffer`;
  no code in this layer ever changes the byte order, so the `ByteBuffer` default
  (BIG_ENDIAN) applies. The official doc (`tar.md`, "serialized as a big endian
  integer") confirms this.
* **TAR header numeric fields** (size, mtime, mode, uid, gid, checksum) are ASCII
  **octal** strings, per classic tar (`SegmentTarWriter.newEntryHeader`).
* **CRC32** everywhere means `java.util.zip.CRC32`, i.e. the standard zlib/IEEE 802.3
  CRC-32 (polynomial 0x04C11DB7 reflected = 0xEDB88320, init 0xFFFFFFFF, final XOR
  0xFFFFFFFF, reflected in/out). Java stores the result as an unsigned value in a
  `long`; every on-disk comparison truncates it to 32 bits: `crcField == (int) crc.getValue()`.
* **Java int arithmetic wraps at 32 bits** (two's complement). Where noted below,
  faithfully reproduce that (Rust: use `i32`/`wrapping_*` or `u32 as i32` casts).
* **Block size**: `TarConstants.BLOCK_SIZE = 512` (decimal).
* A "UUID" is stored as two big-endian 64-bit values: `msb` then `lsb` (16 bytes),
  matching `java.util.UUID(msb, lsb)`.

## 1. File naming and generations

* Writer file-name format: `FILE_NAME_FORMAT = "data%05d%s.tar"`
  (`TarConstants.FILE_NAME_FORMAT`); the writer always creates generation `"a"`:
  `format(FILE_NAME_FORMAT, writeIndex, "a")` (`TarWriter` constructor). So the first
  file is `data00000a.tar`, then `data00001a.tar`, ... `%05d` zero-pads to *at least*
  5 digits (indexes ≥ 100000 produce 6 digits).
* Recognition pattern when scanning a directory
  (`TarFiles.FILE_NAME_PATTERN`):

  ```
  (data)((0|[1-9][0-9]*)[0-9]{4})([a-z])?.tar
  ```

  * group 2 (the full digit run, minimum 5 digits, no redundant leading zeros before
    the last 4) is parsed with `Integer.parseInt` → the **file index**.
  * group 4 is the optional **generation letter** `a`–`z`; when absent it defaults to
    `'a'` (`TarFiles.collectFiles`).
  * Note the `.` before `tar` is **not escaped** in the Java regex, so e.g.
    `data00000aXtar` would technically match. Reproduce or not at your discretion;
    real stores only contain `.tar` names.
  * Files are grouped `index -> {generation letter -> file name}`; two files with the
    same index and letter are a fatal state error (`Validate.checkState` in
    `collectFiles`).
* Only files ending in `.tar` are listed at all (`SegmentTarManager.listArchives`,
  `SuffixFileFilter(".tar")`).
* Generation bump on compaction/cleanup: `TarReader.sweep` takes the char at position
  `name.length() - "a.tar".length()` and writes `name[0..pos] + (char)(generation+1) + ".tar"`;
  generation `'z'` is never rewritten.
* Reader ordering: readers are kept in **descending index order**; a segment lookup
  probes the current writer first, then readers from newest index to oldest
  (`TarFiles.init`, `TarFiles.readSegment`).
* The next writer index after opening = (maximum existing index) + 1, or **0** when no
  matching files exist yet — so an empty store starts with `data00000a.tar`
  (`TarFiles.init`). *(Addition: empty-store case made explicit.)*

## 2. TAR container layout

A tar file is a sequence of 512-byte blocks. Each logical entry is:

```
[ 1 header block (512 B) ] [ ceil(dataSize/512) data blocks ]
```

The file is terminated by **two 512-byte zero blocks** (`SegmentTarWriter.close`).

Entry order as written (`TarWriter.close` → `writeBinaryReferences`, `writeGraph`;
`SegmentTarWriter.close` → `writeIndex`):

```
segment entry, segment entry, ..., <name>.brf, <name>.gph, <name>.idx, 0-block, 0-block
```

where `<name>` is the tar file's own name (e.g. `data00000a.tar.idx`).

### 2.1 Header block fields actually written (`SegmentTarWriter.newEntryHeader`)

All unlisted bytes are 0x00.

| Offset | Len | Field | Value written |
|-------:|----:|-------|---------------|
| 0 | 100 | name | UTF-8 entry name, NUL-padded; truncated to 100 bytes |
| 100 | 7 | mode | `"0000400"` (`%07o` of 0400); byte 107 stays 0x00 |
| 108 | 7 | uid | `"0000000"`; byte 115 stays 0x00 |
| 116 | 7 | gid | `"0000000"`; byte 123 stays 0x00 |
| 124 | 11 | size | `%011o` of data size (decimal size of entry payload); byte 135 stays 0x00 |
| 136 | 11 | mtime | `%011o` of `System.currentTimeMillis()/1000`; byte 147 stays 0x00 |
| 148 | 8 | checksum | see below |
| 156 | 1 | typeflag | ASCII `'0'` (0x30) |

No ustar magic, no link name, no prefix — everything past offset 157 is zero.

**Header checksum**: first fill bytes 148–155 with eight ASCII spaces (0x20), then
`checksum = Σ (header[i] & 0xff) for i in 0..511` (Java `int` addition — cannot
overflow: max 512*255), then store `String.format("%06o\0 ", checksum)` at offset 148:
six octal digits, one NUL (0x00), one space (0x20).

### 2.2 Padding rule

`SegmentTarWriter.getPaddingSize(size)`:

```
remainder = size % 512
padding   = remainder > 0 ? 512 - remainder : 0
```

Total on-disk footprint of an entry of payload `size`:
`getEntrySize(size) = 512 + size + getPaddingSize(size)` (`SegmentTarReader.getEntrySize`).

**Padding position differs by entry kind** (critical):

| Entry | Header `size` field | Padding location |
|-------|--------------------|------------------|
| segment | exact segment size | **after** the data (`SegmentTarWriter.writeSegment`) |
| `.brf` | `data.length + padding` | **before** the data (`writeBinaryReferences`) |
| `.gph` | `data.length + padding` | **before** the data (`writeGraph`) |
| `.idx` | exact serialized size (already 512-aligned; padding is *inside* the payload at its front, see §4) | none extra (`writeIndex`) |

Because `.brf`/`.gph` data is written after its padding, and `.idx` data is internally
front-padded to a 512 multiple, **all three trailer structures end exactly on a block
boundary**, which is what makes the backwards-reading scheme in §3 work.

### 2.3 Segment entries

`SegmentTarWriter.writeSegment(msb, lsb, data, offset, size, generation, fullGeneration, compacted)`:

* Entry name: `String.format("%s.%08x", uuid, crc32(data[offset..offset+size]))` —
  the lowercase canonical UUID string (36 chars, `8-4-4-4-12` hex) followed by `.` and
  **exactly 8 lowercase hex digits** of the CRC32 of the raw segment bytes. Total 45
  bytes. Example: `5e2edd9f-d27d-42fa-a349-955ba36cfd09.9f8b41e2`.
* The index (§4) records `position` = file offset of the **first data byte** (i.e.
  header offset + 512), which is always a multiple of 512, and `length` = exact
  segment size.
* Name pattern accepted by the recovery scanner (`SegmentTarManager.NAME_PATTERN`):

  ```
  ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(\.([0-9a-f]{8}))?(\..*)?
  ```

  i.e. the checksum suffix is optional on read, and a further dotted suffix is
  tolerated "for compatibility with possible future extensions".

## 3. Locating the trailer structures (read path)

All three trailer structures are read **backwards from the end of the file** through a
`ReaderAtEnd` abstraction (`org.apache.jackrabbit.oak.segment.util.ReaderAtEnd`):
`readAtEnd(whence, amount)` reads `amount` bytes starting `whence` bytes before the
anchor point.

Anchors (`SegmentTarReader`):

* **Index**: anchor = `fileLength - 2*512` (skip the two zero blocks);
  `loadAndValidateIndex` builds `ReaderAtEnd` as
  `file.seek(length - 2*BLOCK_SIZE - whence)`.
* **Graph**: anchor = `fileLength - 2*512 - indexEntrySize` where
  `indexEntrySize = getEntrySize(index.size())` and `index.size()` =
  `count * ENTRY_SIZE + 16` (the *unpadded* index payload; `getEntrySize` re-adds the
  padding and the header block) (`SegmentTarReader.getGraph`, `getIndexEntrySize`,
  `IndexV1.size`/`IndexV2.size`).
* **Binary references**: anchor = `fileLength - 2*512 - indexEntrySize - graphEntrySize`
  where `graphEntrySize = getEntrySize(graph.size())`, `graph.size()` being the
  unpadded graph payload size recomputed from the parsed graph
  (`SegmentTarReader.getBinaryReferences`, `getGraphEntrySize`, `SegmentGraph.size`).
  For a well-formed file this recomputed size equals the graph footer's `bytes` field
  (16 + Σ per source-UUID (20 + 16·outDegree)).
  **Caveat**: if the pre-compiled graph entry is absent/invalid, `getGraph()` silently
  *recomputes* the graph from segment contents, and `getGraphEntrySize()` then returns
  the size of that recomputed graph — which generally does **not** match the on-disk
  entry, so the `.brf` anchor would be wrong. A robust reimplementation should locate
  the `.brf` using the *on-disk* graph footer's `bytes` field (or fail), not the
  recomputed graph. (This is a faithful description of the Java behavior; the Java
  code has this latent inconsistency.)

All three structures share the same 16-byte footer shape, laid out at the very end of
their payload (read via `readAtEnd(16, 16)` then four consecutive big-endian ints):

| Byte offset in footer | Len | Field |
|---:|---:|-------|
| 0 | 4 | `crc32` — CRC32 of the structure's data area (see each section) |
| 4 | 4 | `count` — number of top-level entries |
| 8 | 4 | `bytes` / `size` — total structure size in bytes (see each section for what it includes) |
| 12 | 4 | `magic` |

So the **last 4 bytes of each structure are its magic number**, and dispatching on
version is done by reading exactly those 4 bytes first
(`IndexLoader.readMagic`, `BinaryReferencesIndexLoader.readMagic`).

Magic numbers (all computed as `('\n'<<24) + (c1<<16) + (c2<<8) + '\n'`; shown as the
big-endian int value and the byte sequence as it appears in the file):

| Structure | Java constant | Int value | Bytes in file |
|-----------|---------------|-----------|---------------|
| Index V1 | `IndexLoaderV1.MAGIC` | `0x0A304B0A` | `0A 30 4B 0A` (`\n0K\n`) |
| Index V2 | `IndexLoaderV2.MAGIC` | `0x0A314B0A` | `0A 31 4B 0A` (`\n1K\n`) |
| Graph | `SegmentGraph.MAGIC` | `0x0A30470A` | `0A 30 47 0A` (`\n0G\n`) |
| Binary refs V1 | `BinaryReferencesIndexLoaderV1.MAGIC` | `0x0A30420A` | `0A 30 42 0A` (`\n0B\n`) |
| Binary refs V2 | `BinaryReferencesIndexLoaderV2.MAGIC` | `0x0A31420A` | `0A 31 42 0A` (`\n1B\n`) |

## 4. Index entry (`<tarname>.idx`)

Serialized by `index/IndexWriter.write()` (always **V2** for new files), loaded by
`index/IndexLoader` → `IndexLoaderV1`/`IndexLoaderV2`.

### 4.1 Payload layout (as produced by `IndexWriter.write`)

```
dataSize  = count * 33 + 16                     // V2 entry size + footer
totalSize = ceil(dataSize / 512) * 512          // ((dataSize + 511) / 512) * 512
```

| Region | Size | Content |
|--------|------|---------|
| front padding | `totalSize - dataSize` | zero bytes |
| entries | `count * ENTRY_SIZE` | sorted index entries |
| footer | 16 | `crc32, count, bytes=totalSize, magic` (§3) |

The tar header's size field for `.idx` is exactly `totalSize` (`SegmentTarWriter.writeIndex`).

### 4.2 Entry layouts

**V1** (`IndexEntryV1`, `SIZE = 28`), all big-endian:

| Offset | Len | Field |
|---:|---:|-------|
| 0 | 8 | `msb` |
| 8 | 8 | `lsb` |
| 16 | 4 | `position` — file offset of first byte of segment data (multiple of 512) |
| 20 | 4 | `size` — exact segment length in bytes |
| 24 | 4 | `generation` |

V1 semantics: `fullGeneration = generation`, `compacted = true`
(`IndexEntryV1.getFullGeneration/isCompacted`).

**V2** (`IndexEntryV2`, `SIZE = 33`), all big-endian:

| Offset | Len | Field |
|---:|---:|-------|
| 0 | 8 | `msb` |
| 8 | 8 | `lsb` |
| 16 | 4 | `position` |
| 20 | 4 | `size` |
| 24 | 4 | `generation` |
| 28 | 4 | `fullGeneration` |
| 32 | 1 | `compacted` — 0 = false, any non-zero = true |

### 4.3 Sort order

Entries are sorted ascending by `(msb, lsb)` using **signed 64-bit comparison**
(`IndexWriter.write` comparator uses Java `long` `<`/`>`; the loader's monotonicity
check likewise starts from `Long.MIN_VALUE` and uses signed compares). A Rust
implementation must compare as `i64`, **not** `u64` — UUIDs with the top bit set sort
*before* those without.

### 4.4 Load-and-validate algorithm (`IndexLoaderV1.loadIndex` / `IndexLoaderV2.loadIndex`)

Given `ReaderAtEnd r` anchored at `fileLength - 1024`:

```
magic = r.readAtEnd(4, 4).getInt()          // last 4 bytes before the zero blocks
dispatch on magic (0x0A304B0A -> V1, 0x0A314B0A -> V2, else InvalidIndexException "Unrecognized magic number")

meta  = r.readAtEnd(16, 16)
crc32 = meta.getInt(); count = meta.getInt(); bytes = meta.getInt(); magic = meta.getInt()

fail "Magic number mismatch"     if magic != MAGIC
fail "Invalid entry count"       if count < 1
fail "Invalid size"              if bytes < count * 28 + 16     // see note below
fail "Invalid size alignment"    if bytes % 512 != 0

entries = r.readAtEnd(16 + count * ENTRY_SIZE, count * ENTRY_SIZE)   // ENTRY_SIZE: 28 (V1) or 33 (V2)

crc = CRC32(); crc.update(entries)          // over exactly count*ENTRY_SIZE bytes
fail "Invalid checksum" if crc32 != (int) crc.getValue()

lastMsb = lastLsb = Long.MIN_VALUE
for i in 0..count-1:
    read msb(8), lsb(8), offset(4), size(4) from entry i   // trailing fields not validated
    fail "Incorrect entry ordering"      if lastMsb > msb || (lastMsb == msb && lastLsb > lsb)   // signed
    fail "Duplicate entry"               if lastMsb == msb && lastLsb == lsb && i > 0
    fail "Invalid entry offset"          if offset < 0
    fail "Invalid entry offset alignment" if offset % 512 != 0
    fail "Invalid entry size"            if size < 1
    lastMsb = msb; lastLsb = lsb
```

**Verbatim quirk** — `IndexLoaderV2.loadIndex` line 52 validates size using the **V1**
constants:

```java
if (bytes < count * IndexEntryV1.SIZE + IndexV1.FOOTER_SIZE) {
    throw new InvalidIndexException("Invalid size");
}
```

i.e. the V2 loader checks `bytes < count*28 + 16`, not `count*33 + 16`. Files written
by `IndexWriter` always satisfy the stricter bound, but a byte-exact reimplementation
must use `28` here for V2 as well, or it may reject files Java accepts (a V2 index
whose `bytes` lies between `count*28+16` and `count*33+16` would be *accepted by the
size check* in Java and then fail later, or not at all). Note also the "Duplicate
entry" check's `i > 0` guard: a first entry equal to `(Long.MIN_VALUE, Long.MIN_VALUE)`
is not flagged as a duplicate of the sentinel.

Any `InvalidIndexException` is caught by `SegmentTarReader.loadAndValidateIndex`
(returns `null` → the file is treated as index-less; see §7/§8).

### 4.5 Lookup

`IndexV1/V2.findEntry(msb, lsb)` uses interpolation search over the sorted entries
(float interpolation on `msb` as Java `float`, `Math.round`); the result index is
order-of-entries-in-the-index. Any search that respects the signed `(msb, lsb)` order
(e.g. binary search) is behaviorally equivalent for valid indexes. `listSegments`
returns entries re-sorted by `position` (`IndexEntry.POSITION_ORDER`,
`SegmentTarReader.listSegments`).

## 5. Graph entry (`<tarname>.gph`)

Written by `SegmentGraph.write()` (via `TarWriter.writeGraph` →
`SegmentTarWriter.writeGraph`), loaded by `SegmentGraph.load(ReaderAtEnd)` anchored
per §3.

### 5.1 Payload layout (unpadded size = `graphSize`, stored in footer `bytes`)

```
for each (source UUID -> adjacency set) in the edge map:   // unordered (HashMap iteration)
    msb        int64 BE
    lsb        int64 BE
    nVertices  int32 BE
    for each target UUID:                                  // unordered (HashSet iteration)
        msb    int64 BE
        lsb    int64 BE
footer (16 bytes): crc32, count = number of source UUIDs, bytes = graphSize, magic = 0x0A30470A
```

`graphSize = 16 + Σ_sources (16 + 4 + 16 * outDegree)` (`SegmentGraph.size`).
The CRC32 covers **all adjacency data**, i.e. the `bytes - 16` bytes immediately
preceding the footer.

In the tar container, the `.gph` entry is `[header][padding][data]` with header size
field = `data.length + padding` (§2.2), so the footer ends exactly at the anchor.

### 5.2 Load algorithm (`SegmentGraph.load`, `SegmentGraph.parse`)

```
meta = readAtEnd(16, 16); crc32, count, bytes, magic = 4 BE ints
return null (log "Invalid graph magic number")  if magic != 0x0A30470A
return null (log "Invalid number of entries")   if count < 0            // count == 0 is legal
return null (log "Invalid entry size")          if bytes < 4 + count * 34
buffer = readAtEnd(bytes, bytes)                 // includes the footer
crc = CRC32 over buffer[0 .. bytes-16)
return null (log "Invalid graph checksum")      if crc32 != (int) crc.getValue()
nEntries = buffer.getInt(bytes - 12)             // the count field again
parse nEntries adjacency records sequentially from offset 0
```

**Verbatim quirk** — the size check is `if (bytes < 4 + count * 34)`. `34` does not
correspond to the actual record size (which is ≥ 20 per source); it appears to be a
holdover from an older index-based graph format. Reproduce as-is. Note also the
in-source javadoc of `SegmentGraph.MAGIC` still *describes* that older format ("The
index of the source segment UUID (in the above list, 4 bytes)... terminated by -1");
the actual code (`parse`/`write`) uses full 16-byte UUID adjacency lists as specified
above — trust the code, not that comment.

**Error handling**: `load` never throws for malformed content — it returns `null`, and
`SegmentTarReader.getGraph` then recomputes the graph by reading every **data**
segment (those with `(lsb >>> 60) == 0xA`, `SegmentId.isDataSegmentId`) and extracting
its segment-reference list (`SegmentGraph.compute`). For a read-only port, a missing
graph is non-fatal; it is only needed for GC and diagnostics.

## 6. Binary references entry (`<tarname>.brf`)

Written by `binaries/BinaryReferencesIndexWriter.write()` (always **V2** format),
loaded by `binaries/BinaryReferencesIndexLoader` (dispatches V1/V2 on the trailing
magic), anchored per §3.

### 6.1 V2 payload layout (unpadded size stored in footer `size`)

```
for each generation triple:                       // unordered (HashMap)
    generation      int32 BE
    fullGeneration  int32 BE
    compacted       byte (1 = true, 0 = false)
    segmentCount    int32 BE
    for each segment:                             // unordered
        msb             int64 BE
        lsb             int64 BE
        referenceCount  int32 BE
        for each reference:                       // unordered
            length  int32 BE
            bytes   UTF-8 string, `length` bytes (no terminator)
footer (16 bytes): crc32, count = number of generation triples, size, magic = 0x0A31420A
```

`size` = full structure including the 16-byte footer, excluding tar padding.
CRC32 covers the `size - 16` data bytes preceding the footer.

### 6.2 V1 payload differences (`BinaryReferencesIndexLoaderV1`)

Per generation there is only a single `generation int32 BE` field (no
`fullGeneration`, no `compacted` byte); on load it is expanded to
`Generation(generation, generation, true)` (`parseGeneration`). Magic `0x0A30420A`.
Everything else is identical.

### 6.3 Load/validate (`loadBinaryReferencesIndex` + `parseBinaryReferencesIndex`, both versions)

```
magic = readAtEnd(4, 4)                            // dispatch; unknown -> InvalidBinaryReferencesIndexException
meta  = readAtEnd(16, 16): crc32, count, size, magic
fail "Invalid magic number" if magic != MAGIC
fail "Invalid count"        if count < 0
fail "Invalid size"         if size < count * 22 + 16      // 22 in BOTH versions
buffer = readAtEnd(size, size)                     // includes footer
-- then, in parseBinaryReferencesIndex (re-run on the returned buffer):
re-read footer from buffer[size-16..], re-run the same three checks
crc = CRC32 over buffer[0 .. size-16)
fail "Invalid checksum"     if crc32 != (int) crc.getValue()
parse `count` generation records sequentially from offset 0
```

The `count * 22` lower bound is a heuristic minimum (22 bytes does not equal any exact
record size); reproduce as-is.

**Error handling**: `TarReader.getBinaryReferences` catches
`InvalidBinaryReferencesIndexException`/`IOException`, logs a warning, and returns
`null` — a broken/missing `.brf` is non-fatal (blob references simply cannot be
collected).

## 7. Opening a TAR file — index-first path

`SegmentTarManager.open(name)` → `SegmentTarReader.loadAndValidateIndex(file, name)`:

1. `length = file.length()`.
2. Reject (return `null`, log "Invalid alignment") if `length % 512 != 0`.
3. Reject ("File too short") if `length < 6 * 512` (= 3072).
4. Reject ("File too long") if `length > Integer.MAX_VALUE` (0x7FFFFFFF) — the whole
   file must be addressable with a signed 32-bit offset; index `position` fields are
   `int32`.
5. Build `ReaderAtEnd` anchored at `length - 1024` and run the index load/validate of
   §4.4. Any failure → `null`.
6. `null` index ⇒ `open` returns `null` ("No index found in tar file, skipping").
7. Otherwise construct the reader (mmap or pread access; on mmap failure fall back to
   pread with a warning — `SegmentTarManager.open`).

Segment reads then go straight to `access.read(entry.position, entry.length)`
(`SegmentTarReader.readSegment`); nothing re-validates the per-entry tar headers or
the name CRC on this path.

## 8. Opening with multiple generations, and recovery

### 8.1 Read-write store (`TarReader.open(Map<Character,String>, ...)`)

1. Sort the `{generation letter -> file}` map ascending, then iterate the file list
   **in reverse** (highest letter first).
2. For each candidate, attempt §7. The first file with a valid index wins; **all other
   generations of that index are deleted** (`openFirstFileWithValidIndex`). Per-file
   `IOException` is logged and the next candidate is tried.
3. If none has a valid index: recovery. For each generation in **ascending** letter
   order, run the full scan of §8.3 collecting `UUID -> raw segment bytes` into one
   insertion-ordered map (later files overwrite same-UUID entries), then rename each
   scanned file to a backup name (`file + ".bak"`, or `file + "." + i + ".bak"` for
   the first free `i ≥ 2` — `backupSafely`, `findAvailGen`; rename falls back to
   copy+delete). Then rewrite the *lowest* generation file from the recovered entries
   via the segment-level `TarRecovery` callback (which re-parses each segment and
   re-adds graph edges / binary references), and open it per §7. Failure to open the
   regenerated file is fatal (`IOException "Failed to open recovered tar file"`).

### 8.2 Read-only store (`TarReader.openRO`)

Only the **highest** generation letter is considered. Strategies in order:
`open`, `forceOpen` (identical for local tar files), then recover-without-touching:
scan per §8.3 (no backup/rename), write the recovered entries to a fresh file named
`file + ".ro.bak"` (or `file + "." + i + ".ro.bak"`), and open that. All three
failing is fatal.

### 8.3 Full-scan recovery of one file (`SegmentTarManager.recoverEntries`)

Iterate from offset 0 while `pos + 512 <= length`:

```
read 512-byte header block
sum = Σ (header[i] & 0xff), i = 0..511                        // Java int, no overflow possible

if sum == 0 and pos_after_header + 2*512 == length: return    // zero block with EXACTLY 2 blocks after it
   // note: this fires only for a zero block starting at length - 3*512 (e.g. a run of
   // >= 3 trailing zero blocks). For the standard Oak layout with exactly TWO trailing
   // zero blocks it NEVER fires: when the first trailing zero block is read,
   // pos_after_header + 2*512 == length + 512 != length, so both zero blocks fall
   // through below (checksum-mismatch warning, empty name -> "Unexpected entry,
   // skipping", size 0) and the scan ends via the loop condition at EOF.
   // A zero block anywhere else is likewise NOT a terminator and falls through.

// verify header checksum: substitute the checksum field with spaces
for i in 148..155: sum -= header[i] & 0xff; sum += 0x20
expected = bytes of String.format("%06o\0 ", sum)             // 8 bytes
for i in 0..7:
    if expected[i] != header[148 + i]: log warn "Invalid entry checksum ... skipping..."
    // *** despite the log text, processing CONTINUES — the entry is NOT skipped ***

name = NUL-terminated UTF-8 string from header[0..100)
size = octal number from header[124..136):                    // readNumber:
    number = 0
    for each byte b: if '0' <= b <= '7': number = number*8 + (b - '0')   // Java int, wraps at 2^31
                     else: break

if pos_after_header + size > length: log "Partial entry, ignoring"; return  // ends the whole scan
    // (pos_after_header = file position right after the 512-byte header block;
    //  no data has been read yet at this point)

if name matches NAME_PATTERN (§2.3):
    id = UUID from group 1; checksumHex = group 3 (nullable)
    if checksumHex != null or id not already recovered:
        read `size` bytes of data; advance to next 512 boundary
        if checksumHex != null:
            if CRC32(data) != parseLong(checksumHex, 16): log "Checksum mismatch, skipping"; continue
        entries[id] = data                                     // overwrites earlier same-UUID entry
    // else: NOTE — data is neither read nor skipped; the next iteration
    //       reads the segment's first data block as a header (Java quirk;
    //       unreachable in practice because written entries always carry a checksum suffix)
else if name != tarFileName + ".idx":
    log "Unexpected entry, skipping"; seek past size, rounded up to 512
// else (the .idx entry): silently NOT skipped either — the scanner walks into the
// index payload and relies on the checksum/name filters to discard the garbage
```

Tolerated: bad header checksums (warn only), corrupt segments with mismatching name-CRC
(dropped), unknown entry names (skipped), truncated final entry (scan stops).
Fatal for the file (skip file, continue recovery with other generations): any
`IOException` while reading.

## 9. What the reader needs from each structure (read-only port summary)

* `.idx` — **required**. Sole means of locating segments; invalid index ⇒ recovery
  path. Provides per-segment `(uuid, position, size, generation, fullGeneration,
  compacted)`.
* `.gph` — optional; `null`/invalid tolerated (recomputable from data segments).
  Needed only for GC-style traversal, not for resolving the head state.
* `.brf` — optional; invalid tolerated (`null`). Needed only for blob GC.
* Trailing 2 zero blocks — required implicitly by the fixed `length - 1024` index
  anchor: a tar file lacking them will fail index validation.

## 10. Cross-checks with the official documentation

`tar.md` agrees with the code on: 512-byte blocks, two-zero-block termination, entry
order (segments, `.brf`, `.gph`, `.idx`), backwards layout rationale, UUID-adjacency
graph format, index sorted by UUID with interpolation search, `data00000a.tar` naming
and generation-letter bumping after ≥ 25% shrink. `tar.md` describes the *V1* index
entry (no fullGeneration/compacted fields) and calls CRC32 "CRC2" in places; the code
(§4.2) is authoritative. `tar.md`'s claim that the graph magic "identifies the
beginning" of the file is loose wording — all magics are at the **end** of their
structure (§3).
