# Oak Segment-Tar: Writing TAR Archives — Byte-Exact Writer Specification

Scope: how Oak **produces** a `data?????a.tar` file — the exact header bytes, entry
order, in-memory accumulation, close/flush/rotation protocol, and crash semantics.
The on-disk formats of the trailer structures and the read/validate path are already
specified in [tar-layer.md](tar-layer.md); this document specifies the *writer* side
and repeats layout facts only where the writer's construction order matters. All
citations are to `oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/`
(bare file names below live under `file/tar/` unless otherwise pathed).

Classes involved:

| Class | Role |
|---|---|
| `TarFiles` | Top-level read/write set of tar files; owns the single active `TarWriter`, rotation, locking |
| `TarWriter` | One in-flight tar file; accumulates graph + binary references; close protocol |
| `SegmentTarWriter` (implements `SegmentArchiveWriter`) | Actual byte emission: tar headers, entries, trailer serialization, fsync |
| `SegmentTarManager` (implements `SegmentArchiveManager`) | Creates/deletes/renames/copies archive files in the segmentstore directory |
| `index/IndexWriter`, `binaries/BinaryReferencesIndexWriter`, `SegmentGraph` | Serialize the three trailer payloads |

## 1. File creation and naming

* `TarFiles.init()` scans existing archives (`collectFiles`, pattern per
  tar-layer.md §1), sorts the indices ascending, and creates the writer with
  `writeNumber = maxIndex + 1` (0 if the directory has no data files)
  (`TarFiles.init`, lines 448–454). The writer's file name is
  `format("data%05d%s.tar", writeIndex, "a")` — a **new writer always has generation
  letter `a`** (`TarWriter(SegmentArchiveManager, int, CounterStats)` constructor;
  `TarConstants.FILE_NAME_FORMAT`).
* A second constructor `TarWriter(archiveManager, archiveName)` takes an explicit
  name and sets `writeIndex = -1`; it is used by recovery
  (`TarReader.generateTarFile`, line 181 — regenerates e.g. `data00012a.tar` after
  backup) and by cleanup's sweep (`TarReader.sweep`, line 516 — writes the
  next-generation-letter file, e.g. `data00012b.tar`). A `writeIndex == -1` writer
  cannot rotate (`createNextGeneration` has `Validate.checkState(writeIndex >= 0)`).
* **Lazy file creation**: `SegmentTarWriter` opens
  `new RandomAccessFile(file, "rw")` only on the first `writeSegment` call
  (`SegmentTarWriter.writeSegment`, lines 104–107). Until then `isCreated()` is
  false and: `flush()` is a no-op (`TarWriter.flush`, line 186), `close()` returns
  without writing anything (`TarWriter.close`, lines 213–215), and
  `createNextGeneration()` returns `this` (lines 242–246). **An untouched writer
  leaves zero bytes on disk** — a repository that was only read never gains an empty
  tar file.
* No file locking is done at this layer; the repository-wide `repo.lock` is handled
  by `TarPersistence` (see filestore-layer.md).

## 2. The tar entry header — exact 512 bytes

`SegmentTarWriter.newEntryHeader(String name, int size)` (lines 253–305) builds
every header the writer ever emits. It is a **pre-POSIX (v7-style) header: no
`ustar` magic, no version, no uname/gname, no devmajor/devminor, no prefix** —
those fields remain 0x00. Byte-exact algorithm:

```text
newEntryHeader(name: string, size: int32) -> byte[512]:
    header = [0x00; 512]
    nameBytes = utf8(name)
    header[0 .. min(len(nameBytes), 100)] = nameBytes[0 .. min(len,100)]   # truncated at 100, NOT NUL-terminated if exactly 100
    header[100..107] = ascii("%07o" of 0o400)      = "0000400"             # file mode; byte 107 stays 0x00
    header[108..115] = ascii("%07o" of 0)          = "0000000"             # uid;  byte 115 stays 0x00
    header[116..123] = ascii("%07o" of 0)          = "0000000"             # gid;  byte 123 stays 0x00
    header[124..135] = ascii("%011o" of size)                              # 11 octal digits; byte 135 stays 0x00
    header[136..147] = ascii("%011o" of (currentTimeMillis / 1000))        # mtime, seconds; byte 147 stays 0x00
    header[148..156] = "        " (8 x 0x20)                               # checksum placeholder
    header[156]      = '0' (0x30)                                          # typeflag: regular file
    checksum = sum over all 512 bytes of (byte & 0xff)                     # plain int sum; cannot overflow 32 bits
    header[148..156] = ascii("%06o" of checksum) + 0x00 + 0x20             # 6 octal digits, NUL, SPACE
    return header
```

Notes for the port:

* Each 7/11-character octal string is copied into an 8/12-byte field whose last
  byte is the 0x00 from zero-initialization — i.e. fields are effectively
  NUL-terminated, except the checksum field which ends `NUL SPACE`.
* `%06o` of the checksum: header sums are ≤ 512·255 = 130 560 < 8^6, so 6 digits
  always suffice.
* mtime is wall-clock at write time; nothing in Oak reads it back
  (`SegmentTarManager.recoverEntries` ignores it). The Rust port may write any
  plausible value but SHOULD write current time for fidelity.
* Oak's own recovery scanner re-derives the checksum the same way and only **warns**
  on mismatch (`SegmentTarManager.recoverEntries`, lines 210–234), and the index-based
  read path never looks at headers at all; but external `tar` tooling and the
  entry-name-driven recovery *do* read them, so emit them exactly.

## 3. Writing a segment entry

`SegmentTarWriter.writeSegment(msb, lsb, data, offset, size, generation, fullGeneration, compacted)`
(lines 95–132):

```text
writeSegment:
    uuid      = UUID(msb, lsb)                       # lowercase-hex canonical form
    crc       = CRC32(data[offset .. offset+size))   # standard zlib CRC32
    entryName = format("%s.%08x", uuid, crc)         # 36 + 1 + 8 = 45 chars, all lowercase hex
    header    = newEntryHeader(entryName, size)      # header size field = payload size, NOT padded size
    padding   = (size % 512 == 0) ? 0 : 512 - size % 512
    open RandomAccessFile "rw" if not yet open
    write header (512 bytes)
    dataOffset = filePointer                         # recorded BEFORE writing data
    write data[offset .. offset+size)
    write padding zero bytes                         # padding AFTER the payload
    length = filePointer                             # volatile field, exposed via getLength()
    index[uuid] = (msb, lsb, (int) dataOffset, size, generation, fullGeneration, compacted)
```

* The entry name checksum (`%08x`, 8 lowercase hex digits of the CRC32 value) is
  what full-scan recovery verifies per entry (`SegmentTarManager.recoverEntries`,
  lines 263–271). It MUST match the payload bytes.
* The in-memory index is a `Collections.synchronizedMap(new LinkedHashMap<>())` —
  **insertion-ordered**; it backs `containsSegment`/`readSegment` on the in-flight
  file and produces the `.idx` entries at close (`SegmentTarWriter`, lines 66–75).
  Re-writing an existing UUID would replace the map entry (never happens in practice:
  callers check `containsSegment` first).
* `dataOffset` is cast to `int`. `TarWriter.writeEntry` (lines 148–152) enforces
  `archive.getLength() <= Integer.MAX_VALUE` after every write — a tar file may
  never reach 2 GiB, since `.idx` offsets are signed 32-bit.
* `TarWriter.writeEntry` is `synchronized` and returns `getLength()` (current file
  pointer, i.e. all headers + payloads + paddings so far, excluding future trailers).

Generation values are passed through unmodified from
`GCGeneration.{getGeneration, getFullGeneration, isCompacted}`
(`TarWriter.writeEntry`, line 146); the tar layer performs no generation arithmetic.

## 4. In-memory accumulation of graph and binary references

Both structures live only in the `TarWriter` and hit the disk exclusively at
`close()`:

* **Graph**: `TarWriter.graph = new SegmentGraph()`; `addGraphEdge(from, to)` does
  `edgeMap.computeIfAbsent(from, HashSet::new).add(to)` (`SegmentGraph.addEdge`).
  `TarFiles.writeSegment` (lines 660–664) adds one edge per referenced segment UUID
  of the segment just written, under the `TarFiles` write lock.
* **Binary references**: `TarWriter.binaryReferences = newBinaryReferencesIndexWriter()`;
  `addEntry(generation, full, compacted, segmentUUID, referenceString)` stores into
  `Map<Generation, Map<UUID, Set<String>>>` (all `HashMap`/`HashSet`)
  (`BinaryReferencesIndexWriter`, lines 47–71). `TarFiles.writeSegment`
  (lines 665–669) adds each blob-reference string of the segment, keyed by that
  segment's own `GCGeneration` and UUID.
* Neither `addGraphEdge` nor `addBinaryReference` is internally synchronized; the
  `TarFiles.lock` write lock is the actual mutual exclusion. A port must keep
  segment-write + edge/binref registration atomic with respect to rotation, because
  rotation serializes and discards these accumulators.
* **Determinism**: because the containers are hash-based, the byte order of `.gph`
  and `.brf` payload records is arbitrary. Readers never rely on order (they parse
  into maps; tar-layer.md §5.2, §6.3). REQUIRED: every record present exactly once,
  correct counts, correct CRC. OPTIONAL: any particular ordering. A Rust port may
  emit deterministic (e.g. insertion or sorted) order — Oak tolerates it.
* Content rule (for equivalence, enforced by callers upstream): the graph must
  contain an edge for **every segment reference of every data segment in the file**
  and nothing else, because cleanup uses `.gph` instead of parsing segments
  (`SegmentGraph.compute` is the fallback that shows the expected content). The
  `.brf` must list every blob reference of every data segment in the file, under the
  segment's exact GC generation triple — the blob GC uses it to enumerate live blob
  IDs (`TarFiles.collectBlobReferences`).

## 5. The close() protocol — exact order

`TarWriter.close()` (lines 200–231). Locking is two-phase: step 1 runs under the
writer's own monitor (`synchronized (this)` — the same monitor `writeEntry` holds,
so no write can interleave with the flag flip); steps 3–5 run under the separate
`closeMonitor`, which is the monitor `flush()` takes — so trailer writing cannot
interleave with a concurrent fsync, but proceeds without the writer monitor:

1. mark closed (idempotent: second `close()` returns immediately);
2. if `!archive.isCreated()` → return (no file, nothing to finalize);
3. `writeBinaryReferences()` → `archive.writeBinaryReferences(binaryReferences.write())`;
4. `writeGraph()` → `archive.writeGraph(graph.write())`;
5. `archive.close()` → `SegmentTarWriter.close()` (lines 217–226):
   `writeIndex()`; write **two** 512-byte zero blocks; `access.close()`.
6. any `IOException` in 3–5 is rethrown wrapped as `UnrecoverableArchiveException`
   (`file/UnrecoverableArchiveException`) — the store treats this as fatal
   (`FileStore.tryFlush` shuts the store down on it).

So the final file layout is, in order: segment entries (write order) · `.brf` entry ·
`.gph` entry · `.idx` entry · 1024 zero bytes. **This order is mandatory** — the
reader locates the index at EOF−1024 and walks backwards `.idx` → `.gph` → `.brf`
(tar-layer.md §3, §7).

**There is no fsync in `close()`**. `SegmentTarWriter.close` writes and closes the
fd without `sync()`; durability of the trailer is left to the OS. Oak tolerates the
loss: a tar file whose index cannot be validated is fully recovered from entry
headers at next open (tar-layer.md §8). A Rust port SHOULD fsync before close for
robustness — that is strictly more durable than Oak and changes no bytes.

### 5.1 `.brf` entry (written first)

Payload from `BinaryReferencesIndexWriter.write()` (lines 79–170) — exact
serialization (all integers big-endian):

```text
size = 16                                       # footer: magic, crc, length, generation-count
for each (gen -> segmentMap):    size += 4+4+1+4
    for each (uuid -> refs):     size += 16+4
        for each ref:            size += 4 + len(utf8(ref))

buf = [0; size]
for each (gen, segmentMap) in entries:          # HashMap iteration order (arbitrary)
    putInt gen.generation; putInt gen.full; putByte (gen.compacted ? 1 : 0)
    putInt segmentMap.size
    for each (uuid, refs) in segmentMap:        # HashMap order
        putLong uuid.msb; putLong uuid.lsb
        putInt refs.size
        for each ref in refs:                   # HashSet order
            bytes = utf8(ref); putInt len(bytes); put bytes
crc = CRC32(buf[0 .. position))                 # everything before the footer
putInt (int) crc; putInt entries.size; putInt size; putInt 0x0A31420A   # MAGIC "\n1B\n"
```

Emission (`SegmentTarWriter.writeBinaryReferences`, lines 169–180): the tar header
is `newEntryHeader(fileName + ".brf", data.length + paddingSize)` and the
`paddingSize` zero bytes are written **before** the payload:

```text
padding = getPaddingSize(data.length)
write newEntryHeader(name + ".brf", len(data) + padding)   # header size = PADDED size
write zeros[0 .. padding)                                  # FRONT padding
write data
```

The payload therefore ends exactly at a 512-byte boundary; the footer's `size`
field holds the **unpadded** length. (Contrast with segment entries: padded after,
header size unpadded.)

### 5.2 `.gph` entry (written second)

Payload from `SegmentGraph.write()` (lines 181–207):

```text
graphSize = 16 + Σ over edges (16 + 4 + 16 * |adjacency|)
buf = [0; graphSize]
for each (from, adjacencySet) in edgeMap:       # HashMap order
    putLong from.msb; putLong from.lsb
    putInt |adjacencySet|
    for each to in adjacencySet:                # HashSet order
        putLong to.msb; putLong to.lsb
crc = CRC32(buf[0 .. position))
putInt (int) crc; putInt edgeMap.size; putInt graphSize; putInt 0x0A30470A   # MAGIC "\n0G\n"
```

(The long comment above `SegmentGraph.MAGIC` describing a UUID-table + index-list
format documents the **legacy pre-Oak-1.6 layout** — the code writes full UUID
adjacency lists as shown. Trust `write()`/`parse()`, not the comment.)

Emission (`SegmentTarWriter.writeGraph`, lines 155–166) is identical in shape to
`.brf`: header `newEntryHeader(fileName + ".gph", data.length + padding)`, then
front padding, then payload.

### 5.3 `.idx` entry (written last, inside `archive.close()`)

`SegmentTarWriter.writeIndex()` (lines 192–215) copies every in-memory index entry
into an `IndexWriter(512)` **in insertion order**, then serializes
(`IndexWriter.write`, lines 106–148):

```text
dataSize  = 33 * count + 16                     # IndexEntryV2.SIZE = 33, IndexV2.FOOTER_SIZE = 16
totalSize = ceil(dataSize / 512) * 512
buf = [0; totalSize]; position = totalSize - dataSize   # FRONT padding inside the payload
sort entries by (msb, lsb) ascending using SIGNED 64-bit comparison
for each entry:
    putLong msb; putLong lsb
    putInt offset                                # offset of the segment DATA (after its header)
    putInt size; putInt generation; putInt fullGeneration
    putByte (isCompacted ? 1 : 0)
crc = CRC32(buf[totalSize-dataSize .. totalSize-16))     # entries only; padding and footer excluded
putInt (int) crc; putInt count; putInt totalSize; putInt 0x0A314B0A   # MAGIC "\n1K\n"
```

Emission: header is `newEntryHeader(fileName + ".idx", data.length)` — here the
header's size field equals the (already 512-aligned) payload length and **no extra
padding bytes are written** (lines 209–214). Unlike `.brf`/`.gph`, the `.idx`
footer's third int is the **padded** `totalSize`, because the loader uses it to
seek over the whole aligned entry (tar-layer.md §4.4).

**The signed sort is load-bearing**: `IndexV2.findEntry` runs an interpolation
search using Java `long` (signed) comparisons on msb then lsb
(`index/IndexV2.findEntry`, lines 51–92; same for the binary search in `IndexV1`).
An index sorted by unsigned u64 would be mis-ordered for msb values with the top
bit set and lookups would fail. In Rust: sort by `(i64, i64)` (or XOR both with
`1 << 63` and sort unsigned).

After `writeIndex()`: `access.write(ZERO_BYTES); access.write(ZERO_BYTES);` — two
all-zero 512-byte blocks, the classic tar end-of-archive marker — then
`access.close()` (lines 221–223).

## 6. flush() semantics during normal operation

* `TarWriter.flush()` (lines 181–193): under `closeMonitor`, if the file was
  created and the writer is not closed, call `archive.flush()` →
  `SegmentTarWriter.flush()` = `access.getFD().sync()` (line 235) — a full
  `fsync(2)` of the data file. Deliberately **not** synchronized on `this`, so
  segment reads/writes proceed concurrently with the fsync
  (`TarFiles.flush` takes only the read lock, line 585–593).
* Callers: `FileStore.doFlush` (file/FileStore.java, lines 333–343) executes, inside
  `TarRevisions.flush`, the sequence
  `segmentWriter.flush(); tarFiles.flush(); stats.flushed();` and only after that
  callback returns does `TarRevisions` append the new head to `journal.log`. **The
  crash-consistency contract is exactly this ordering**: segment bytes are written
  to the tar and fsynced *before* the journal line that references them becomes
  visible. The tail of a tar file beyond the last fsync may be lost on crash; the
  journal never points into that tail. (Journal file format and its own sync are in
  filestore-layer.md; for cross-reference, the line appended here is
  `<segmentId>:<offset-decimal> root <currentTimeMillis>` — `RecordId.toString10`,
  `TarRevisions.doFlush` line 237 — and `doFlush` returns without calling the
  flusher or writing anything when the head equals the last persisted head.)
* Nothing else in the tar layer ever forces data: individual `writeSegment` calls
  are buffered by the OS only.
* *(Addition — verified gap.)* **Oak never fsyncs the segmentstore directory.**
  Neither lazy file creation (`new RandomAccessFile(file, "rw")`) nor rotation nor
  recovery's rename/delete (`SegmentTarManager.backup` uses plain `Files.move` /
  `Files.deleteIfExists`) is followed by a directory fsync, so after a crash a
  freshly created tar file may be missing from the directory even though its
  bytes were fsynced. Oak survives this because the journal is written *after*
  the data flush: a vanished whole file is equivalent to a lost unflushed tail.
  A Rust port MAY fsync the directory after create/rename — strictly more
  durable, no byte differences.

## 7. Rotation — `TarFiles.writeSegment` and `internalNewWriter`

`TarFiles.writeSegment` (lines 648–677), under the write lock:

1. `size = writer.writeEntry(...)` — post-write file length in bytes (headers +
   payloads + paddings; trailers not yet written).
2. register graph edges (one per entry of `references`) and binary references (one
   per string in `binaryReferences`) as in §4.
3. `if (size >= maxFileSize || entryCount >= writer.getMaxEntryCount()) internalNewWriter();`

Consequences:

* The check is **after** the write: a data file typically ends slightly above
  `maxFileSize` (default `FileStoreBuilder.DEFAULT_MAX_FILE_SIZE = 256` MB,
  interpreted as `maxFileSize * 1024 * 1024`), plus the trailer entries appended at
  close. There is no per-entry-overhead pre-calculation in this code path — the
  accounting is simply the real file pointer (`SegmentTarWriter.getLength()`).
* `SegmentTarWriter.getMaxEntryCount()` is `Integer.MAX_VALUE` (line 250), so the
  entry-count trigger never fires for local tar files (it exists for remote/Azure
  persistence).

`internalNewWriter` (lines 689–699), write lock held:

```text
newWriter = writer.createNextGeneration()      # close() current file (full §5 protocol),
                                               # then new TarWriter(archiveManager, writeIndex+1)  → "data%05(d+1)a.tar"
if newWriter == writer: return                 # current file was never created — keep it
reader = TarReader.open(writer.getFileName(), archiveManager)   # reopen finished file read-only
readers = Node(reader, readers)                # prepend: readers stay in descending index order
writer = newWriter
```

The finished file is immediately re-opened through the freshly written `.idx`
(`TarReader.open(String, ...)` fails hard with `IOException("Failed to open tar file ...")`
if the index does not validate) — so a writer bug surfaces at rotation, not at next
restart. `newWriter()`/rotation is also forced before `cleanup` and
`collectBlobReferences` so those only ever see closed files (lines 711–732, 863–874).

## 8. Reading back from the in-flight file

While a segment is only in the open writer, reads are served from it:
`TarFiles.readSegment`/`containsSegment` probe `writer` first under the read lock,
then the reader list (lines 595–646). `TarWriter.readEntry` checks `!closed`, then
`SegmentTarWriter.readSegment` (lines 135–147):

```text
entry = index.get(UUID(msb,lsb));  if none -> null
buf = allocate(entry.length)
positional-read from FileChannel at entry.position (loop until entry.length bytes; EOFException if short)
```

Positional channel reads do not move the `RandomAccessFile` pointer, so they are
safe against the appending writer; the in-memory index entry is only published
after its bytes are fully written (single-writer, `synchronized writeEntry`). A
Rust port needs the same regime: pread on the same file handle, index entry
published after the payload write completes, all guarded consistently with rotation
(Oak: `TarFiles.lock` read lock vs. write lock).

## 9. Errors, cancellation, and what may remain on disk

* **Crash / kill while writing segments**: the file ends mid-entry or after the
  last complete entry, with no trailers and no zero blocks. Next startup:
  `TarReader.open` finds no valid index → full-scan `recoverEntries` (header-walk,
  per-entry name-CRC check, "Partial entry … ignoring" for a torn tail), backup of
  the damaged file to `<name>.bak` — if that exists, `<name>.2.bak`, `<name>.3.bak`, …
  (`TarReader.findAvailGen`, lines 244–250); backup is a rename, falling back to
  copy + delete (`SegmentTarManager.backup`) — then regeneration of the
  recoverable entries into a fresh file, with graph and binary references rebuilt
  from the recovered segments (tar-layer.md §8; `TarReader.collectFileEntries`,
  `generateTarFile`, `backupSafely`). When several generation letters of the same
  index exist and none has a valid index, **all** of them are scanned in ascending
  letter order into one insertion-ordered map (later duplicates of a UUID replace
  earlier ones) and the regenerated file takes the **lowest** letter's name
  (`TarReader.open(Map, …)`, lines 98–114). A read-only store never modifies the
  originals: it regenerates into `<name>.ro.bak` instead (`TarReader.openRO`,
  lines 122–145). Segments not yet fsynced may vanish — safe,
  because the journal only references flushed state (§6).
* **Crash between `close()` and the reader reopen during rotation**: same as above;
  the trailer may be partially present. Loaders validate magic + CRC and fall back
  to recovery on any mismatch.
* **`IOException` during `close()`**: wrapped in `UnrecoverableArchiveException`;
  `FileStore` shuts down. The half-finalized file is left in place and handled by
  recovery at next open. Nothing is rolled back on disk — recovery is always
  forward (backup + regenerate), never in-place truncation.
* **Empty writer**: never creates a file; `close()`/rotation on it are no-ops (§1).
* **Cleanup interplay**: files replaced by sweep get the next generation letter
  (`data00012b.tar`); old generations are deleted by the `FileReaper` only after
  the new journal state is persisted (see filestore-layer.md / tooling docs). The
  writer itself always emits letter `a` except through the explicit-name
  constructor. Sweep guards (`TarReader.sweep`, lines 468–563): a read-only
  archive is returned unchanged; if **no** entry survives, sweep returns `null`
  and the whole file becomes removable; if the reclaimable space is **less than
  25%** of the entry region (`afterSize >= beforeSize * 3 / 4`, sizes measured as
  header+padded-payload per entry), the file is kept as-is; a file already at
  letter **`z` is never rewritten** (generation letters stop there); and if the
  freshly written next-generation file fails to re-open through its index, the
  original reader is kept. The swept file's `.gph`/`.brf` are rebuilt by
  filtering the old file's trailers — edges/references whose source (or, for
  edges, target) segment was reclaimed are dropped.

## 10. AEM safety invariants (writer checklist)

A Rust implementation may consider the produced tar file AEM-safe iff all of the
following hold:

1. **Name**: `data%05d%s.tar`, letter from `[a-z]`, index unique in the directory;
   a *new* writer file uses letter `a` and index strictly greater than every
   existing data-file index.
2. **Size cap**: the file length after the last segment entry (before trailers)
   must be ≤ `i32::MAX` — that is what Oak enforces (`TarWriter.writeEntry` checks
   `getLength() <= Integer.MAX_VALUE` after each segment write; the trailer writes
   at close are not re-checked, but offsets only ever point into the entry
   region); every `.idx` offset is the segment *data* offset (after its 512-byte
   header) as a non-negative `i32`.
3. **Entry geometry**: every entry = one 512-byte header + payload + zero padding
   to the next 512 boundary; segment entries pad *after* the payload with header
   size = unpadded payload size; `.brf`/`.gph` pad *before* the payload with header
   size = padded size; `.idx` payload is internally front-padded to a 512 multiple
   with header size = padded payload size.
4. **Header bytes**: exactly as §2 — v7 header, mode `0000400`, uid/gid `0000000`,
   `%011o` size and mtime, typeflag `'0'`, checksum `"%06o\0 "` over the header with
   the checksum field as 8 spaces; name ≤ 100 bytes.
5. **Segment entry names**: `<lowercase-uuid>.<crc32 as %08x>` where the CRC32 is
   over exactly the payload bytes written.
6. **Trailer order**: `.brf`, then `.gph`, then `.idx`, then exactly 1024 zero
   bytes, and nothing after them; trailer entry names are the *file's own name* +
   suffix (a renamed file's embedded names go stale — Oak's loaders locate trailers
   by position, not name). The recovery scanner (`SegmentTarManager.recoverEntries`,
   lines 247–284) treats trailer entries as follows: any entry whose name matches
   neither the segment pattern nor `<currentFileName>.idx` — this includes `.brf`,
   `.gph`, and *stale* `.idx` names after a rename — is skipped with an
   "Unexpected entry" warning by seeking past its (padded) payload; an entry named
   exactly `<currentFileName>.idx` is silently **not skipped** — the scanner does
   not seek past its payload and would misparse the following blocks — which is
   harmless only because `.idx` is always the last entry. A port's writer must
   therefore keep `.idx` last for recovery-friendliness, not just for the
   EOF−1024 locator.
7. **`.idx` correctness**: one entry per segment entry in the file (no more, no
   less), 33 bytes each, sorted ascending by **signed** `(msb, lsb)`; footer
   `crc32(entries) | count | totalSize(padded) | 0x0A314B0A`; CRC over the entry
   region only.
8. **`.gph` correctness**: every segment-reference edge of every data segment in
   the file, footer `crc32(content) | edgeCount | unpaddedSize | 0x0A30470A`.
9. **`.brf` correctness**: every blob reference of every data segment, grouped by
   the segment's exact `(generation, fullGeneration, compacted)` triple, footer
   `crc32(content) | generationCount | unpaddedSize | 0x0A31420A`.
10. **Generations passthrough**: the `(generation, fullGeneration, compacted)`
    written to `.idx`/`.brf` are byte-identical to the values in the segments'
    `GCGeneration` — the tar writer must never alter them.
11. **Endianness**: all integers in the three trailer payloads big-endian; CRC32 is
    zlib CRC-32 truncated to 32 bits.
12. **Durability order**: a journal head may only be persisted after every segment
    it references has been written *and* fsynced in its tar file
    (`segmentWriter.flush → tarFiles.flush (fsync) → journal append`). Fsync of the
    finalized trailer before close is optional in Oak but recommended.
13. **Rotation**: after finalizing a file, the next file's index is previous + 1;
    never write two files with the same index+letter; never append to a finalized
    file.
14. **Recovery friendliness**: if interrupted, leave the partial file as-is (no
    truncation, no partial trailer fabrication) — Oak's startup recovery handles a
    trailer-less or torn file, and it verifies per-entry name CRCs, so any entry
    fully written is recoverable.
15. **Read-your-writes**: while a file is open for write, lookups must serve
    segments from it before consulting older files, and a segment must become
    visible only after its bytes are completely on the file (or in the page cache).
16. *(Addition.)* **No empty or trailer-only tar files**: because file creation is
    lazy on the first `writeSegment` and `close()`/rotation on an untouched writer
    write nothing, every tar file that exists contains at least one segment entry
    before its trailers. Never emit a file consisting only of trailers and/or zero
    blocks — `TarReader.open` would still open it (an empty index is valid), but
    Oak can never produce one and downstream tooling does not expect it.
17. *(Addition.)* **Generation-letter discipline for rewrites**: a rewritten
    (swept) file keeps its index and increments only the letter by exactly one
    (`b` follows `a`, …, capped at `z` — a file at `z` is never rewritten); the
    rewritten file must contain a subset of the original's segments with
    unchanged bytes, offsets recomputed, and filtered `.gph`/`.brf`. Both letters
    may legitimately coexist on disk until the reaper deletes the older one, and
    the reader picks the highest letter with a valid index, deleting lower ones
    (read-write mode) — so never reuse a letter, and never delete the old
    generation yourself before the new journal state is durable.
