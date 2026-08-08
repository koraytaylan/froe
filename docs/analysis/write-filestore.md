# Oak Segment-Tar: Read-Write FileStore Lifecycle — Writer Specification

Scope: everything the **read-write** `FileStore` does to the disk over its lifetime —
open (lock, manifest, TAR init/recovery, journal binding, initial-node bootstrap),
`writeSegment`, `flush()`/`tryFlush()` durability ordering, `setHead` contract,
`close()`, gc.log persistence, and the complete file inventory of a write session.

This document builds on the read-side specs in this directory and does **not** repeat
them:

* `tar-layer.md` — byte-exact TAR container, index/graph/brf trailer formats, tar
  header fields, recovery scan byte-level details.
* `filestore-layer.md` — journal line grammar (§5), record-id string grammar (§4),
  manifest encoding (§3), TAR name pattern and generation selection (§6), gc.log line
  format (§7), read-only open sequence (§8.2).
* `segment-layer.md`, `record-layer.md`, `node-layer.md` — segment/record/node
  serialization.

Java sources are cited by bare file name; all live under
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/` (subpackages
`file/`, `file/tar/`, `spi/persistence/`).

---

## 1. Open sequence for a read-write store

Driver: `FileStoreBuilder.build()` (`FileStoreBuilder.java`):

```
build():
    assert not already built
    directory.mkdirs()                                   # creates segmentstore dir if absent
    revisions = new TarRevisions(persistence)            # step A (journal writer opened HERE)
    store = new FileStore(this)                          # steps B–F
        on InvalidFileStoreVersionException | IOException:
            revisions.close(); rethrow                   # journal file handle released on failure
    store.bind(revisions)                                # step G (head resolution / initial node)
    return store
```

Ordering is normative: the journal writer is opened **before** the repository lock is
taken, and the lock is taken **before** the manifest is touched or any TAR file is
opened.

### 1.A Journal writer open — `TarRevisions` constructor (`TarRevisions.java`)

`this.journalFileWriter = journalFile.openJournalWriter()` →
`LocalJournalFileWriter` (`LocalJournalFile.java`):

```
open journal.log as RandomAccessFile("rw")   # CREATES the file if missing (possibly empty)
seek(file.length())                          # position at EOF; file is append-only from here
```

No lock, no fsync at open. `head` and `persistedHead` are both `null` until `bind`.

### 1.B Repository lock — `FileStore` constructor → `TarPersistence.lockRepository()`

`FileStore.java` ctor first line after `super(builder)`:
`repositoryLock = persistence.lockRepository();`

`TarPersistence.lockRepository()` (`TarPersistence.java`):

```
lockFile = RandomAccessFile(directory/"repo.lock", "rw")   # creates the file if absent
lock     = lockFile.getChannel().lock()                    # BLOCKING exclusive lock,
                                                           # whole file (0..Long.MAX_VALUE)
on OverlappingFileLockException:                           # same-JVM double open
    throw IllegalStateException("<dir> is in use by another store.")
```

Semantics the Rust port must match:

* **Blocking** acquire (`FileChannel.lock()`, not `tryLock()`): if another process holds
  the OS advisory lock, open blocks indefinitely. On Linux this is a POSIX/OFD advisory
  write lock over the entire file (Rust: `flock`-style is *not* identical; use
  `fcntl(F_OFD_SETLKW)` or equivalent whole-file exclusive lock — AEM/Oak uses
  `FileChannel.lock()` which maps to `fcntl` OFD locks on OpenJDK/Linux).
* The lock file's **content is never written**; only its existence and the advisory
  lock matter. Never delete `repo.lock`.
* Unlock (at close): `lock.release(); lockFile.close();` — file stays on disk.

### 1.C Segment writer construction (no I/O)

`FileStore` ctor builds the shared `SegmentWriter`
(`defaultSegmentWriterBuilder("sys").withGeneration(() -> getGcGeneration().nonGC()).withWriterPool()...`).
No disk I/O; but note the **generation rule for every normally-written segment**:

* `getGcGeneration()` = `revisions.getHead().getSegmentId().getGcGeneration()`
  (`FileStore.getGcGeneration`) — the generation of the segment containing the current
  head record.
* `.nonGC()` clears the compacted flag: `new GCGeneration(generation, fullGeneration, false)`
  (`GCGeneration.nonGC`).

So after a full compaction moved the head into generation `(g+1, f+1, compacted=true)`,
all subsequent normal commits write segments tagged `(g+1, f+1, compacted=false)`.
The supplier is evaluated lazily per new segment buffer, not at store open.

### 1.D Manifest check-and-update — `AbstractFileStore.newManifestChecker(...).checkAndUpdateManifest()`

(`AbstractFileStore.java` `newManifestChecker`, `ManifestChecker.java`, `Manifest.java`,
`LocalManifestFile.java`; constants `MIN_STORE_VERSION = 1`, `MAX_STORE_VERSION = 2` in
`AbstractFileStore.java`.)

```
shouldExist = persistence.segmentFilesExist()      # any "*.tar" directly in dir (TarPersistence.segmentFilesExist)
min = strictVersionCheck ? 2 : 1 ; max = 2

checkAndUpdateManifest():
    if manifest file exists: props = java.util.Properties load
    else if shouldExist:     throw InvalidFileStoreVersionException
                             ("Using oak-segment-tar, but oak-segment should be used")
    else:                    props = empty

    v = int(props["store.version"]) or max-if-missing/unparseable   # Manifest.getStoreVersion
    if v <= 0:    throw IllegalStateException("Invalid store version")
    if v < min:   throw InvalidFileStoreVersionException("Using a too recent version of oak-segment-tar")
    if v > max:   throw InvalidFileStoreVersionException("Using a too old version of oak-segment tar")

    props["store.version"] = "2"                    # ManifestChecker.updateManifest: ALWAYS
    save                                            # even if it was already "2"
```

Write path (`LocalManifestFile.save`): `new FileWriter(file)` (truncate-in-place, default
charset) + `Properties.store(w, null)`. That produces a `#`-prefixed date comment line
followed by `store.version=2` (encoding details: `filestore-layer.md` §3.1). **No
fsync, no temp-file/rename** — the write is not atomic; Oak accepts that risk. The Rust
port must write the same shape (comment line optional for Oak's reader — any `#` line is
a Properties comment — but keeping `store.version=2` is mandatory) and **must always
rewrite the manifest on read-write open**, because a store created by a writer that
omitted the manifest would break a later `oak-segment-tar` open when `.tar` files exist.

### 1.E TAR files init — `TarFiles.builder()...build()` + `tarFiles.init()`

(`FileStore.java` ctor, `TarFiles.java`.) `withMaxFileSize(builder.getMaxFileSize() * MB)`
— default 256 MB (`FileStoreBuilder`, `MB = 1024*1024`). Read-write mode (`readOnly = false`).

`TarFiles.init()`:

1. `collectFiles(archiveManager)`: list `*.tar` names, match
   `(data)((0|[1-9][0-9]*)[0-9]{4})([a-z])?.tar`, group by integer index →
   `{generation letter → name}` (missing letter = `'a'`); duplicate (index, letter) is a
   fatal `checkState` failure. (Details: `filestore-layer.md` §6.1.)
2. For each index ascending (parallel in Java, order restored before list assembly):
   `TarReader.open(map, recovery, archiveManager)` — the **destructive**
   read-write open (`TarReader.java` `open(Map,...)`):
   * Try generations in **descending letter order**; first file whose index loads and
     validates wins (`openFirstFileWithValidIndex`).
   * On success, **all other generations of that index are deleted immediately**
     (`archiveManager.delete(other)`, log "Removing unused tar file").
   * If none has a valid index → recovery:
     ```
     entries = LinkedHashMap<UUID, byte[]>            # ascending letter order, later
     for file in generations ascending by letter:     # generations overwrite earlier
         collectFileEntries(file, entries, backup=true)
             archiveManager.recoverEntries(file, entries)   # raw scan; see tar-layer.md §8.3
             backupSafely(...):                        # TarReader.backupSafely
                 backup = findAvailGen(file, ".bak")   # "name.bak", then "name.2.bak", "name.3.bak", ...
                 archiveManager.backup(file, backup, entries.keySet())
                     # SegmentTarManager.backup: RENAME file -> backup;
                     # if rename fails: COPY then delete original (delete failure -> IOException)
     regenerate FIRST (lowest-letter) generation file name via generateTarFile(entries, ...)
         # a fresh TarWriter writes every recovered segment via the store's TarRecovery
         # callback (AbstractFileStore.recovery -> writeSegment(uuid, data, entryRecovery)),
         # which re-derives generation from the segment bytes, repopulates the graph
         # (populateTarGraph) and binary references (populateTarBinaryReferences),
         # then TarWriter.close() writes .brf/.gph/.idx trailers
     reopen it; failure to reopen -> IOException("Failed to open recovered tar file ...")
     ```
   * `findAvailGen(name, ext)` (`TarReader.java`): `name + ext`; while exists,
     `name + "." + i + ext` for `i = 2, 3, ...`. So backups are e.g.
     `data00012a.tar.bak`, `data00012a.tar.2.bak`.
3. Readers are kept newest-index-first. Writer index = `maxIndex + 1` (or `0` for an
   empty store): `writer = new TarWriter(archiveManager, writeNumber, ...)`
   (`TarFiles.init`). The writer's archive is named
   `format("data%05d%s.tar", writeIndex, "a")` (`TarWriter` ctor,
   `TarConstants.FILE_NAME_FORMAT`) but **the file is not created on disk until the
   first segment write** (`SegmentTarWriter.writeSegment` lazily opens the
   `RandomAccessFile`; `isCreated()` = handle exists).

### 1.F Remaining ctor work

`fileReaper = tarFiles.createFileReaper()`; `GarbageCollector` wired with
`new GCJournal(persistence.getGCJournalFile())`; three background tasks scheduled
(`FileStore.java` ctor): flush via `tryFlush()` every **5 s**, `fileReaper::reap` every
**5 s**, disk-space check every 1 min. A single-threaded port must substitute explicit
`tryFlush()`/`reap()` calls (e.g. periodically and before exit); nothing else in the
scheduler affects the disk.

### 1.G Binding and the initial node — `FileStore.bind` → `TarRevisions.bind`

(`FileStore.java` `bind`, `initialNode`; `TarRevisions.java` `bind`;
`FileStoreUtil.java` `findPersistedRecordId`.)

```
bind(store, idProvider, writeInitialNode):
    if head != null: return                       # already bound; no-op
    persistedId = findPersistedRecordId(store, idProvider, journalFile)
        # newest-to-oldest journal scan; first line whose record id parses AND whose
        # segment exists in the store wins; unparseable lines skipped with a warning
        # (grammar and validation: filestore-layer.md §5.3–5.4)
    if persistedId == null:                        # empty/corrupt journal OR no matching segment
        head = writeInitialNode.get()              # WRITES SEGMENTS, see below
        # persistedHead stays null  -> first flush will append a journal line
    else:
        persistedHead = head = persistedId
```

Initial-node bootstrap (`FileStore.initialNode`):

```
writer  = defaultSegmentWriterBuilder("init").build(this)   # generation supplier defaults
                                                            # to GCGeneration.NULL = (0,0,false)
                                                            # (DefaultSegmentWriterBuilder.java:
                                                            #  "generation = () -> GCGeneration.NULL")
builder = EMPTY_NODE.builder()
builder.setChildNode("root", EMPTY_NODE)
recordId = writer.writeNode(builder.getNodeState())         # node-layer.md serialization
writer.flush()                                              # segment(s) written through
                                                            # FileStore.writeSegment into the tar
return recordId
```

Exact structure: the head record is a node with **no properties and exactly one child
node named `"root"` whose value is the empty node** (no properties, no children). Its
segment carries generation `(0, 0, compacted=false)` and segment info string
`{"wid":"init","sno":<segmentIdCount>,"t":<millis>}` — the non-pooled
`SegmentBufferWriter` uses the builder name (`"init"`) verbatim as `wid`
(`SegmentBufferWriter` ctor; pooled writers would suffix it). No journal line is written by `bind` itself — the head exists only in
memory (and its segments in the tar) until the first `flush()`. AEM's first repository
start performs exactly this bootstrap, so a Rust tool creating a fresh store must
reproduce it (plus a subsequent flush to persist the journal line) for Oak to find a
head at next open. On `IOException` the supplier throws `IllegalStateException("Failed
to write initial node")` — the store is unusable.

---

## 2. `writeSegment` — from flushed buffer into TarFiles

`FileStore.writeSegment(SegmentId id, byte[] buffer, int offset, int length)`
(`FileStore.java`):

```
generation       = GCGeneration.NULL          # (0,0,false) — used for BULK segments
references       = null
binaryReferences = null

if id.isDataSegmentId():                      # (lsb >>> 60) == 0xA  (SegmentId.isDataSegmentId)
    data = (offset > 4096) ? copy-of-range : wrapping view     # bytes identical either way
    segment = new Segment(tracker, id, data)                   # parses the segment header
    [segmentCache.putSegment(segment)]                         # cache only; no disk effect
    generation       = segment.getGcGeneration()               # from segment header bytes
    references       = readReferences(segment)                 # AbstractFileStore.readReferences:
                                                               #   the segment's referenced-segment-id
                                                               #   table, as a SET of UUIDs (dedup'd)
    binaryReferences = readBinaryReferences(segment)           # AbstractFileStore.readBinaryReferences:
                                                               #   for each record of type BLOB_ID in the
                                                               #   segment, SegmentBlob.readBlobId(...)
                                                               #   -> SET of blob-id strings (dedup'd)

tarFiles.writeSegment(id.asUUID(), buffer, offset, length,
                      generation, references, binaryReferences)
```

Key writer-side facts:

* **Generation and tar metadata are derived from the segment's own bytes**, not from
  writer state. The Rust segment writer must therefore emit a header whose generation
  fields and referenced-segment-id table are already correct; the tar layer just copies
  them into the index entry, graph, and binary-reference structures.
* `readBinaryReferences` records **every** `BLOB_ID` record (external blob ids); these
  become `.brf` entries keyed by `(generation, fullGeneration, isCompacted)` of the
  containing segment.
* Bulk segments get `GCGeneration.NULL` in the tar index and contribute **no** graph
  edges and no binary references (`TarFiles.writeSegment` skips null sets).

`TarFiles.writeSegment` (under the writer lock, `TarFiles.java`):

```
size = writer.writeEntry(msb, lsb, buffer, offset, length, generation)
for reference in references:        writer.addGraphEdge(id, reference)       # in-memory graph
for reference in binaryReferences:  writer.addBinaryReference(generation, id, reference)
if size >= maxFileSize or writer.entryCount >= writer.maxEntryCount:         # maxEntryCount =
    internalNewWriter()                                                      # Integer.MAX_VALUE for tar
```

`TarWriter.writeEntry` → `SegmentTarWriter.writeSegment` (`SegmentTarWriter.java`) —
appends at the current file pointer:

```
entryName = format("%s.%08x", uuid, crc32(data[offset..offset+size]))   # lowercase uuid,
                                                                        # 8 hex digits of CRC32
write 512-byte tar header (fields per tar-layer.md §2.1; mtime = now/1000)
dataOffset = current position
write data[offset .. offset+size]
write zero padding to next 512 boundary (getPaddingSize)
index[uuid] = (msb, lsb, dataOffset, size, generation, fullGeneration, compacted)   # in memory
```

No fsync per segment. Rollover (`internalNewWriter`, `TarWriter.createNextGeneration`):
if the current archive was never created (`isCreated() == false`) it is reused as-is;
otherwise the current writer is **closed** (writes `.brf`, `.gph`, `.idx` trailer
entries and two 512-byte zero blocks — byte formats in `tar-layer.md` §§4–6; then
`access.close()`, **no fsync**), reopened as a `TarReader` (index-first open must
succeed or `TarFiles.internalNewWriter` throws), prepended to the reader list, and a new
`TarWriter` with index+1 (letter `a`) becomes current.

The optional `persistentCache.writeSegment(...)` at the end of `FileStore.writeSegment`
is an off-store cache — irrelevant to on-disk state of the repository.

### 2.1 Dedup/caching: what is required vs optimization

The `SegmentWriter`'s node/string/template deduplication caches
(`WriterCacheManager`) only decide whether a record is **re-referenced or re-written**.
They change which record ids appear in the output but never the format. For byte-level
requirements the Rust port must only guarantee:

* every record id referenced from a segment resolves (via the segment's
  referenced-segment-id table) to a live segment containing that record;
* a segment only references segments whose generation will survive at least as long
  (Oak enforces this by giving each `SegmentBufferWriter` the generation from the
  supplier of §1.C and flushing caches across GC generations).

Skipping deduplication entirely is safe (larger store, valid bytes). What is **not**
optional: the graph (`.gph`) must list exactly the referenced-segment-id table of each
data segment written to that tar file, and `.brf` must list every `BLOB_ID` record —
cleanup and blob GC rely on them.

---

## 3. `flush()` / `tryFlush()` — exact durability ordering

`FileStore.doFlush` → `TarRevisions.flush/tryFlush` → `TarRevisions.doFlush`
(`FileStore.java:333-377`, `TarRevisions.java:186-239`):

```
flush():     if head == null: return                   # not bound yet ("No head available,
                                                       # skipping flush") — TarRevisions.flush
             lock journalFileLock (BLOCKING);   doFlush(flusher); unlock
tryFlush():  if head == null: return                   # same guard
             if !tryLock(journalFileLock): log "skipping flush"; return   # non-blocking
             doFlush(flusher); unlock

doFlush(flusher):
    if journalFileWriter == null: return                  # revisions closed
    before = persistedHead                                # last id written to journal.log
    after  = head                                         # current in-memory head
    if after == before: return                            # *** NOTHING flushed at all:
                                                          # no segment flush, no fsync,
                                                          # no journal line ***
    flusher.flush():                                      # FileStore.doFlush lambda:
        1. segmentWriter.flush()                          #    drain in-memory segment buffers
                                                          #    -> FileStore.writeSegment -> tar
        2. tarFiles.flush() -> TarWriter.flush()          #    only if archive.isCreated() && !closed
             -> SegmentTarWriter.flush()
                  access.getFD().sync()                   #    fsync(2) — data AND metadata
        3. stats.flushed()                                #    monitoring only
    journalFileWriter.writeLine(after.toString10() + " root " + System.currentTimeMillis())
        # LocalJournalFileWriter.writeLine:
        #   RandomAccessFile.writeBytes(line + "\n")      # low byte of each char; the line is
        #                                                 # pure ASCII (record-id grammar:
        #                                                 # filestore-layer.md §4)
        #   channel.force(false)                          # fdatasync — file data only
    persistedHead = after
```

Normative ordering — **segment bytes are made durable strictly before the journal line
that references them exists**:

1. all pending segments appended to the current tar file;
2. `fsync` of the tar file (`FileDescriptor.sync()`);
3. append one journal line (`<recordid> root <millis>\n`) at EOF;
4. `force(false)` (fdatasync) of `journal.log`;
5. update in-memory `persistedHead`.

Consequences the port must preserve:

* A crash between (2) and (4) leaves the journal pointing at the **previous** head;
  extra fsynced segments in the tar are unreferenced garbage — harmless, cleaned by GC.
* A crash mid-append of the journal line leaves a truncated last line; Oak's reader
  scans the journal **backwards** and skips lines that don't parse or whose segment is
  missing (`filestore-layer.md` §5.4), so this is tolerated.
* A **journal line is skipped entirely** when the head record id did not change since
  the last flush — flushing twice in a row never produces duplicate lines.
* The journal is append-only in this path; nothing ever rewrites or truncates it
  (`truncate()` exists on `JournalFileWriter` but the FileStore never calls it).
* `tryFlush` failure handling (`FileStore.tryFlush`): an
  `UnrecoverableArchiveException` (thrown by `TarWriter.close` wrapping trailer-write
  failures) closes the whole store; a plain `IOException` is only logged and retried at
  the next flush.
* **[added] Rollover durability caveat**: `tarFiles.flush()` fsyncs only the **current**
  writer archive, and `TarWriter.close` (rollover and shutdown) writes trailers and
  closes the file handle **without any fsync**. Segments that landed in a tar which
  rolled over between two flushes are therefore *not* explicitly fsynced before the
  journal line referencing them is written — Oak itself has this window and relies on
  OS writeback plus the startup recovery scan. A port may additionally fsync an archive
  before closing it (strictly safer, still Oak-compatible); it must never do *less*
  than Oak (fsync the current writer archive before the journal append).

## 4. `TarRevisions.setHead` / `getHead` — concurrency contract

(`TarRevisions.java:264-321`.) In-memory only; **no disk I/O**. Persistence happens
solely in `doFlush`.

* `getHead()` / `getPersistedHead()` throw `IllegalStateException` until `bind` set a
  non-null head (`checkBound`).
* `setHead(expected, head, options)`: takes the **read** side of a fair RW-lock (or the
  write side when `EXPEDITE_OPTION` is passed), then performs
  `head.compareAndSet(current, new)` after `current.equals(expected)`. Multiple
  committers may CAS concurrently; only equality with `expected` gates the swap.
* `setHead(Function, timeout)`: takes the **write** lock (blocking with timeout;
  default `INFINITY`), applies the function to the current head, sets it if non-null.
  Used by compaction to swap in the compacted head atomically w.r.t. all committers.

Obligations for a single-threaded port: a plain `if current == expected { current =
new; true } else { false }` CAS plus a "replace head via closure" primitive is fully
equivalent. What must survive the port: **the head is only advanced through these two
operations**, flush reads the head at line-write time (so a flush persists whatever head
is current when it runs), and nothing else mutates `persistedHead`.

## 5. `close()` — shutdown ordering

`FileStore.close()` (`FileStore.java:478-497`):

```
1. shutDown.shutDown()               # future keepAlive() calls fail; tryKeepAlive() no-ops
2. fileStoreScheduler.close()        # stop + join background tasks (flush/reaper/diskcheck)
3. doFlush()                         # full flush as §3 (journal line if head moved);
                                     #   IOException only logged ("Unable to flush the store")
4. doClose() -> Closer with registrations (FileStore.registerCloseables):
       register(repositoryLock::unlock)
       register(tarFiles)
       register(revisions)
       super.registerCloseables()    # persistentCache, if any
   # org.apache.jackrabbit.oak.commons.pio.Closer is Oak's copy of Guava's Closer and
   # closes in REVERSE registration order (LIFO). Effective order:
   #   a. persistentCache.close()
   #   b. revisions.close()          # TarRevisions.close: journal RandomAccessFile.close();
   #                                 #   writer set to null (no extra line, no truncate)
   #   c. tarFiles.close()           # TarWriter.close(): if archive created, write .brf,
   #                                 #   .gph, .idx trailers + two zero blocks, then
   #                                 #   access.close()  — NO explicit fsync of trailers;
   #                                 #   then close all TarReaders
   #   d. repositoryLock unlock      # FileLock.release() + lock file close — LAST
5. fileReaper.reap()                 # delete any tar files still queued for removal
```

Notes:

* The journal line (step 3) is written **before** the tar trailer entries (step 4c).
  If the process dies between them, the last tar has segments (fsynced in step 3) but
  no index; the next read-write open runs the destructive recovery of §1.E — renames
  the file to `.bak` and regenerates it — after which the journal head still resolves.
  This is exactly the crash window Oak itself has, and AEM tolerates it.
* `TarWriter.close` on an archive that was never created (empty store, nothing written)
  does nothing — no empty `dataNNNNNa.tar` is left behind.
* Closing errors are logged, not rethrown (`closeAndLogOnFail`).

## 6. gc.log persistence — `GCJournal.persist`

(`GCJournal.java:67-83`, call site `DefaultCleanupStrategy.java:58-67`;
line/parse format in `filestore-layer.md` §7.)

Called once per **cleanup that follows a compaction attempt**, with:

```
gcJournal.persist(reclaimedSize, finalRepoSize,
                  gcGeneration = revisions.getHead().getSegmentId().getGcGeneration(),
                  nodes        = compactionMonitor.getCompactedNodes(),
                  root         = compactedRootId)     # RecordId.NULL.toString10() =
                                                      # "00000000-0000-0000-0000-000000000000:0"
                                                      # when there is no compacted root
                                                      # (CompactionResult.getCompactedRootId
                                                      #  default; stringified via toString10
                                                      #  in AbstractGarbageCollectionStrategy)
```

`persist` semantics:

1. `current = read()` — the **last line** of `gc.log` parsed via
   `GCJournalEntry.fromString`; empty/missing file → `EMPTY` entry with
   `GCGeneration.NULL` and root `RecordId.NULL.toString10()`.
2. **Skip (write nothing) if `current.gcGeneration.equals(gcGeneration)`** — i.e. a
   failed compaction (head generation unchanged) never appends. Equality compares
   `(generation, fullGeneration, isCompacted)` (`GCGeneration.equals`). Caveat the port
   must reproduce: `fromString` always reconstructs entries with
   `isCompacted = false` (`newGCGeneration(generation, fullGeneration, false)`), while
   the head generation after a successful compaction has `isCompacted = true`, so in
   practice a successful compaction always appends and a repeated call with the same
   parsed generation is idempotent.
3. Append one CSV line
   `repoSize,reclaimedSize,timestampMillis,generation,fullGeneration,nodes,rootId`
   (`GCJournalEntry.toString`). I/O (`LocalGCJournalFile.writeLine`): open with
   `WRITE|APPEND|CREATE|DSYNC`, write line + `\n` (UTF-8), close — synchronous data
   writes, one open/close per line. Write errors are **logged and swallowed** (store
   keeps running).

`gc.log` is read at the next GC to decide tail-vs-full compaction viability and the
previous compacted root; a writer that compacts must append it, but a missing/empty
`gc.log` is always tolerated (defaults to `EMPTY`).

## 7. Generation arithmetic and cleanup reclamation (writer summary)

(`GCGeneration.java:112-145`; `FullCompactionStrategy.targetGeneration`,
`TailCompactionStrategy.targetGeneration`; `Reclaimers.java`;
`DefaultCleanupStrategy.java`; `TarFiles.cleanup`.)

* Normal write: `head.generation.nonGC()` → `(g, f, false)` (§1.C).
* Full compaction target: `current.nextFull()` → `(g+1, f+1, true)`.
* Tail compaction target: `current.nextTail()` → `(g+1, f, true)`.
* (`nextPartial()` → `(g, f, true)` exists for partial compaction.)
* Cleanup reclaim predicate, `retainedGenerations` default 2
  (`Reclaimers.newOldReclaimer`), evaluated against every tar index entry's generation
  triple:
  * after FULL gc: reclaim if `reference.fullGeneration - fullGeneration >= retained`
    **or** (`reference.generation - generation >= retained` **and** `!isCompacted`);
  * after TAIL gc: reclaim if `reference.generation - generation >= retained` **and**
    not in the same tail (same tail = `isCompacted && fullGeneration ==
    reference.fullGeneration`).
* `TarFiles.cleanup` first forces `internalNewWriter()` (closing the active tar so it
  has trailers and becomes a reader), then mark/sweeps readers; swept archives are
  rewritten to the **next generation letter** (`data00012a.tar` → `data00012b.tar`,
  see `tar-layer.md` §1) only when ≥ 25 % shrinks, and replaced files are handed to
  `FileReaper` for deferred deletion (`FileStore.cleanup` →
  `fileReaper.add(...)`; `FileReaper.reap` retries failed deletes forever).
* **[added] Sweep edge cases** (`TarReader.sweep`):
  * shrink test is `afterSize >= beforeSize * 3 / 4 → keep original unchanged` (sizes
    are the sum of block-aligned entry sizes, `archive.getEntrySize(length)`);
  * if **every** entry of an archive is reclaimable (`afterCount == 0`), `sweep`
    returns `null`: no successor file is written and the whole archive is queued for
    deletion;
  * an archive already at generation letter **`'z'` is never rewritten** — the letter
    sequence hard-stops at `z` ("No garbage collection after reaching generation z");
  * the rewritten next-letter file is produced by a fresh `TarWriter` (segments in
    index order, graph and binary-reference entries filtered of cleaned ids) and then
    `close()`d; if it fails to reopen with a valid index, the **original** reader stays
    active (the defective next-letter file may remain on disk — the highest-letter-wins
    rule at the next open resolves it);
  * only when the successor reopens successfully are the swept segment ids added to
    `reclaimedSegmentIds`.
* After cleanup, `SegmentTracker.clearSegmentIdTables(reclaimedIds, gcInfo)`
  (`DefaultCleanupStrategy.java:51`, `SegmentTracker.java:144`,
  `SegmentIdTable.clearSegmentIdTables`) walks every in-memory `SegmentId` and calls
  `id.reclaimed(gcInfo)` on reclaimed ones — this only stamps diagnostic info used in
  later `SegmentNotFoundException` messages. **Port obligation**: after cleanup,
  invalidate any cached segment data for reclaimed ids (Oak's segment cache keys on
  `SegmentId` identity; `SegmentIdTable` canonicalizes `(msb,lsb)` → one instance via
  open addressing on `((int) lsb) & (size-1)`, `SegmentIdTable.newSegmentId`). A
  single-run CLI tool that exits after cleanup has no such state to invalidate.

## 8. Error and cancellation behavior — what may remain on disk

| Failure point | Disk state left behind | Oak's tolerance at next startup |
|---|---|---|
| Crash before first flush after commits | Segments in tar (maybe not fsynced), journal unchanged | Head resolves to older journal line; newer segments are garbage; possibly recovery scan if last tar lacks index |
| Crash between tar fsync and journal append | fsynced but unreferenced segments | Same as above |
| Crash mid-journal-line | Truncated last line | Reverse reader skips it (`filestore-layer.md` §5.3–5.4) |
| Crash before `TarWriter.close` trailers | Last tar without `.idx` | Destructive recovery: rename to `*.bak` (`.2.bak`, ...), regenerate lowest-letter file from raw scan (§1.E) |
| Crash during manifest save | Possibly truncated `manifest` | Missing/garbled `store.version` parses to default `max` → passes check; `store.version=0` or negative would be fatal (`IllegalStateException`) — never write such a value |
| Compaction cancelled/failed | New-generation segments in tars; no gc.log line (generation equality rule §6); head unchanged | Segments have `isCompacted=true` but head never moved; future full gc reclaims them via the old-reclaimer rules |
| Cleanup crash before reap | Swept next-letter file plus original (`data00012a.tar` + `data00012b.tar`) | Read-write open picks the **highest letter** with valid index and deletes the others (§1.E step 2) |
| gc.log write failure | No line appended | Logged only; gc falls back to defaults next run |
| `UnrecoverableArchiveException` in flush | Store closes itself (`FileStore.tryFlush`) | Normal recovery path on next open |

Nothing in the write path ever modifies an existing tar file in place (append-only
until close; rewrites always go to a new name), and nothing rewrites `journal.log`.
The only rename in the lifecycle is the recovery backup (`SegmentTarManager.backup`),
and the only deletes are: losing tar generations at open, reaped files after cleanup,
and `LocalGCJournalFile.truncate` (tooling only).

## 9. Complete file inventory of a read-write session

All paths relative to the segmentstore directory (`TarPersistence.java` constants,
`TarFiles`/`TarReader`/`TarWriter` naming):

| File | Created / modified by | Content after a clean session |
|---|---|---|
| `repo.lock` | `lockRepository()` — created if absent, never written | Empty (or pre-existing bytes); advisory lock released on close; file remains |
| `manifest` | `checkAndUpdateManifest()` — rewritten in place at every rw open | Java-Properties text: date comment + `store.version=2` |
| `journal.log` | Created at `TarRevisions` ctor if absent; appended by `doFlush` | One line per persisted head change: `<recordid-string10> root <millis>\n`; append-only |
| `gc.log` | Created/appended by `GCJournal.persist` after cleanup of a successful compaction | CSV lines per §6; append-only (DSYNC) |
| `data%05d[a-z].tar` | `SegmentTarWriter` (letter always `a` for new writers); next-letter files by cleanup sweep | Segment entries + `.brf` + `.gph` + `.idx` + 2 zero blocks per `tar-layer.md` |
| `dataNNNNN?.tar.bak`, `dataNNNNN?.tar.<i>.bak` | Recovery at rw open (`backupSafely`) | Byte-for-byte the damaged original (renamed, or copied then original deleted) |
| `dataNNNNN?.tar.ro.bak`, `...<i>.ro.bak` | Read-only recovery only (`TarReader.openRO`) — not the rw path | Regenerated archive from raw scan |
| (deleted) stale tar generations | `openFirstFileWithValidIndex` at open; `FileReaper.reap` after cleanup | Removed |

A checkpoint-creating or head-advancing tool touches only `journal.log` and tar files;
compaction additionally touches `gc.log` and (via cleanup) tar generations.

---

## 10. AEM safety invariants

Checklist the Rust implementation must satisfy so that AEM starts and runs cleanly
against the store afterwards:

1. **Exclusive access**: acquire the whole-file advisory lock on `repo.lock` before
   reading or writing anything else; hold it for the entire session; release without
   deleting the file. Never operate while AEM could be running.
2. **Manifest**: after any write session the manifest must exist and contain
   `store.version=2` in Java-Properties encoding. Never write `store.version <= 0`.
   If the directory contains `.tar` files but no manifest, refuse (Oak would).
3. **Journal head validity**: the last parseable line of `journal.log` must be
   `<recordid> root <timestamp>` where the record id (a) matches the grammar of
   `filestore-layer.md` §4, (b) points into a **data** segment present in some tar
   with a valid index, and (c) resolves to a node record whose closure of referenced
   records/segments is fully present. Lines are pure ASCII terminated by a single
   `\n` (Java writes them via `RandomAccessFile.writeBytes`, low byte per char; the
   reverse reader requires at least one space in a line to consider it at all).
   Append-only; never reorder or rewrite existing lines.
4. **Durability order**: never let a journal line become durable before every segment
   reachable from its record id is durable in a tar file (segment append → tar fsync →
   journal append → journal fdatasync).
5. **Tar wholeness**: every tar file left behind must either (a) end with valid
   `.brf`/`.gph`/`.idx` trailers and two zero blocks, byte-exact per `tar-layer.md`,
   or (b) be the single "current writer" file, which Oak will destructively recover.
   Prefer always closing archives properly; never leave more than one index-less tar.
6. **Index/graph/brf consistency**: for each data segment entry, the index generation
   triple must equal the generation encoded in the segment header; the graph must
   equal its referenced-segment-id table; `.brf` must contain every `BLOB_ID` record.
   Oak's cleanup and blob GC trust these without re-validating against segment bytes.
7. **Naming discipline**: new archives are `data%05d` + letter `a`; rewrites of an
   existing index bump only the letter and **never go past `z`** (Oak stops sweeping
   an archive at generation `z`); never reuse an (index, letter) pair; indices
   strictly increase for new writers (`maxIndex + 1`). Backups use the `.bak` /
   `.<i>.bak` suffixes so they fall outside `FILE_NAME_PATTERN` and are invisible to
   Oak.
8. **Generation rules**: normal segments carry the head's generation with
   `compacted=false`; full compaction writes `(g+1, f+1, true)`; tail compaction
   `(g+1, f, true)`; bulk segments are indexed with `(0, 0, false)`. Cleanup must
   retain everything the old-reclaimer predicates (§7) would retain for
   `retainedGenerations = 2`, and must never delete a segment reachable from the
   current journal head or any checkpoint.
9. **gc.log**: append a well-formed 7-field CSV line only after a compaction that
   actually advanced the head generation; never rewrite history; identical-generation
   repeats must not duplicate lines.
10. **Empty-store bootstrap**: when creating a store from scratch, write the exact
    initial node (`{ "root": {} }`) with generation `(0,0,false)`, flush segments,
    then append the first journal line — matching `FileStore.initialNode` /
    `TarRevisions.bind`.
11. **No in-place mutation**: existing tar files are immutable once they have an
    index; all rewrites go to new names; the only rename is damaged-file backup.
    `journal.log` and `gc.log` are append-only.
12. **Crash residue must be Oak-shaped**: anything the tool can leave behind on
    failure must be in the table of §8 (unreferenced fsynced segments, truncated last
    journal line, one index-less tar, stale letter generations, `.bak` files) —
    states Oak provably repairs or ignores at startup. Any other residue (e.g. a
    corrupted middle of a tar with a valid index, a rewritten journal) is unsafe.
