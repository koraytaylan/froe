# Write Path: Backup, Restore, and Journal Recovery — Byte-Exact Specification

Specification of the three offline write tools of `oak-segment-tar` — `backup`,
`restore`, and `recover-journal` — for a Rust port that must produce stores an
AEM instance can subsequently open and run without problems.

All Java paths are relative to
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/` unless prefixed with
`oak-run/`. Reader-side byte formats (TAR container, segment layout, record
serialization, journal grammar, store opening) are specified in the companion
documents and are **not** repeated here:

* `tar-layer.md` — TAR container, index/graph/binary-refs entries, generation letters.
* `segment-layer.md` — segment header, record tables, record addressing.
* `record-layer.md` — record serialization (node, template, value, map records).
* `node-layer.md` — node-state semantics, stable ids, `compareAgainstBaseState`.
* `filestore-layer.md` — directory layout, `journal.log` grammar (§4, §5),
  `manifest`, `gc.log` (§7), `repo.lock` (§2), store-open sequences (§8).

This document specifies only what the three tools *add*: their algorithms, order
of operations, generation handling, every file they touch, their console output
and exit codes, and the resulting on-disk state.

---

## 1. Shared machinery

### 1.1 The offline copy engine: `ClassicCompactor.compactUp`

All three of backup and restore (not journal recovery) copy content with the
same engine: `Compactor.compactUp(before, after, canceller)` which is defined as
`compact(before, after, /*onto=*/before, canceller)`
(`segment/Compactor.java`, `compactUp`, lines 128–134).

`ClassicCompactor.compact(before, after, onto, hardCanceller, null)`
(`segment/ClassicCompactor.java`) works as follows:

```
compact(before, after, onto):
  # 1. Previously-compacted shortcut
  if after is a SegmentNodeState
     and gcIncrement.isFullyCompacted(after.gcGeneration):   # see §1.2 — ALWAYS FALSE here
       return after as-is (no copy)

  # 2. Diff-based copy
  builder := MemoryNodeBuilder(onto)
  success := after.compareAgainstBaseState(before, CancelableDiff(this)):
      propertyAdded/Changed(p)   -> collect p into modifiedProperties (written at end)
      propertyDeleted(p)         -> builder.removeProperty(p.name)
      childNodeAdded(name, a)    -> recurse compact(EMPTY_NODE, a, onto.child(name) or EMPTY)
      childNodeChanged(n, b, a)  -> recurse compact(b, a, onto.child(n) or EMPTY)
      childNodeDeleted(n, b)     -> builder.child(n).remove()
      every UPDATE_LIMIT (= Integer.getInteger("compaction.update.limit", 10000))
      child updates/deletes: write builder state to a segment ("purge") and
      rebase builder onto the written state (ClassicCompactor.CompactDiff.updated)
  if diff threw IOException: rethrow
  if success:
      apply collected modifiedProperties (rewriting property records; binary
      properties keep the same Blob objects — bulk segments are shared,
      list records are rewritten; ClassicCompactor.compact(PropertyState))
      return writer.writeFullyCompactedNode(builder.getNodeState(),
                                            stableIdBytes(after))
  if hardCanceller cancelled: return null
  else: write partially compacted node (not reachable with the tools below,
        whose softCanceller is null and hardCanceller never fires)
```

Two behaviors are essential for byte/structure correctness:

* **Stable ids are preserved.** `writeFullyCompactedNode(nodeState, stableId)`
  passes the *source* node's stable-id bytes (**20-byte** serialized record id —
  msb 8 + lsb 8 + record number 4, `RecordId.SERIALIZED_RECORD_ID_BYTES = 20`,
  `SegmentNodeState.getStableIdBytes` — of the record where the node was first
  written; `CompactorUtils.getStableIdBytes`)
  to `SegmentWriter.writeNode`, which stores them as a BLOCK record referenced
  by the node record (`DefaultSegmentWriter.writeNodeUncached` → `writeBlock`;
  `RecordWriters.newNodeStateWriter(stableId, ids)`). The copied node record
  therefore stores the
  source's stable id, so `SegmentNodeState.fastEquals` (stable-id comparison,
  see `node-layer.md`) treats source node and copy as equal. This is what makes
  **incremental backup** work (§2.5) and is **required**, not an optimization.
* **Unchanged subtrees are not rewritten.** The copy is a *diff* application:
  only paths where `after` differs from `before` are visited. Any child equal
  by record id or stable id is skipped and the `onto` (target-store) record is
  kept.

Deduplication caches (`WriterCacheManager.Default`, `segment/WriterCacheManager.java`:
string cache 15000 entries, template cache 3000, node cache 1048576, overridable
via `oak.tar.stringsCacheSize` / `templatesCacheSize` / `nodeCacheSize`) change
*which* record ids get reused inside the written segments; they never change
structural correctness. A port may dedup less (bigger output) or equally, but
every emitted record must be well-formed per `record-layer.md`.

### 1.2 `CompactionWriter` in single-generation mode

Backup and restore construct
`new CompactionWriter(reader, blobStore, gen, writer)`
(`segment/file/CompactionWriter.java`, ctor lines 49–59), which sets
`gcIncrement = GCIncrement(gen, gen, gen)` and uses **one** `SegmentWriter` for
both partial and target writes.

`GCIncrement.isFullyCompacted(g)` (`segment/file/GCIncrement.java`, lines 54–56)
is `g.compareWith(base) > 0 && g.equals(target)`. With `base == target == gen`
this is **always false**, so `getPreviouslyCompactedState` always returns
`null` and every node goes through the diff (`CompactionWriter.getPreviouslyCompactedState`,
lines 94–104). Copy-avoidance comes solely from stable-id equality in the diff.

`CompactionWriter.flush()` = `partialWriter.flush(); targetWriter.flush();` —
with one writer this is two calls to `DefaultSegmentWriter.flush()`, which
flushes the current `SegmentBufferWriter` buffer as a segment into the store's
TAR writer (second call is a no-op since the buffer is no longer dirty).

### 1.3 Generation stamping and segment info of copied segments

Both tools create the private writer as:

```java
SegmentBufferWriter bufferWriter = new SegmentBufferWriter(
        targetStore.getSegmentIdProvider(), WID, gen);   // WID = "b" (backup) / "r" (restore)
```

where `gen = sourceHead.getRecordId().getSegmentId().getGcGeneration()` —
**the source head's full `GCGeneration` triple `(generation, fullGeneration,
isCompacted)` is stamped verbatim into every segment written into the target**
(`backup/impl/FileStoreBackupImpl.java` lines 74–79;
`backup/impl/FileStoreRestoreImpl.java` lines 66–72). There is no generation
advance in backup/restore; the triple is written into the segment header
exactly as specified in `segment-layer.md` §5 (`SegmentBufferWriter.newSegment`,
`segment/SegmentBufferWriter.java` lines 186–202: generation at
`GC_GENERATION_OFFSET`, full generation at `GC_FULL_GENERATION_OFFSET` with bit
31 set iff `isCompacted`).

Every new segment's first record is the segment-info string record
(`SegmentBufferWriter.newSegment`, lines 208–219):

```
{"wid":"<wid>","sno":<segmentIdCount>,"t":<System.currentTimeMillis()>}
```

UTF-8, written via `RecordWriters.newValueWriter`. For backup `wid` is the
literal `"b"`, for restore `"r"` (a `null` wid would become
`"w-" + identityHashCode`, `SegmentBufferWriter` ctor lines 140–149). The `"t"`
key of this JSON is exactly what `recover-journal` later parses (§4.2), so the
port **must** emit it as a plain decimal number.

### 1.4 Durability order (both tools)

Neither tool calls `fsync` directly. The order that makes the result crash-safe:

1. `compactionWriter.flush()` — the private writer's current segment buffer is
   written into the target's TAR writer (`data%05da.tar`). This is explicit and
   **required**: `FileStore.close()` only flushes the store's *own*
   `segmentWriter`, not this private writer
   (`segment/file/FileStore.java`, `doFlush` lines 333–343).
2. `targetStore.getRevisions().setHead(expectedHead, newHead)` — pure in-memory
   compare-and-set on `TarRevisions.head`
   (`segment/file/TarRevisions.java`, `setHead(RecordId,RecordId,Option...)`
   lines 265–284). Returns `false` (no change) if `expected` doesn't match.
   Nothing is written to disk here.
3. `targetStore.close()` (`FileStore.close`, lines 478–497):
   `doFlush()` → `revisions.flush(flusher)` → inside the journal lock
   (`TarRevisions.doFlush`, lines 224–239):
   a. `segmentWriter.flush()` (store's writer),
   b. `tarFiles.flush()` — TAR data + index forced to disk (see `tar-layer.md`),
   c. append one line to `journal.log`:
      `head.toString10() + " root " + System.currentTimeMillis()` — i.e.
      `<uuid>:<decimal-offset> root <millis>` + LF (grammar: `filestore-layer.md` §4/§5).
   The **whole a–c sequence** is skipped (`doFlush` returns before invoking the
   flusher) when the in-memory head equals the last persisted head — not just
   the journal append.
   Then `doClose()` releases `tarFiles`, `revisions`, and `repo.lock`.

So: **segments are durable before the journal line that references them**, and
the journal line is the commit point. A crash before (c) leaves a store whose
journal still points at the previous head — Oak tolerates that (head resolution
walks the journal bottom-up skipping unresolvable ids, `filestore-layer.md`
§5.4). Note the read-write `FileStore` also runs a background flush every 5 s
while open, so intermediate journal lines may appear.

Also note: a **fresh** target `FileStore` writes an initial head on open —
`revisions.bind` writes a node `{ "root": {} }` with writer id `"init"` at
generation `(0,0,false)` when `journal.log` is empty or unresolvable
(`FileStore.java`, ctor line 246 + `initialNode()` lines 257–275;
`TarRevisions.bind` lines 156–170). A fresh backup/restore target therefore
always contains this extra segment, and if the store is closed after a failed
copy, `journal.log` gets a line pointing to that empty initial node.

---

## 2. Backup

### 2.1 Command-line entry (`oak-run backup`)

`oak-run/.../run/BackupCommand.java`: two positional arguments, **source first,
target second**. Fewer than 2 args → prints
`This command requires a source and a target folder` to stderr, `System.exit(1)`.
Otherwise runs `Backup.builder().withSource(source).withTarget(target).build().run()`
and exits with its return value. `fakeBlobStore` defaults to the system
property `oak.backup.UseFakeBlobStore` (`FileStoreBackupImpl.USE_FAKE_BLOBSTORE`,
line 54); the oak-run command exposes no flag for it.

`Backup.run()` (`segment/tool/Backup.java`, lines 129–137): returns **0** on
success; on any exception prints the stack trace to stderr and returns **1**.
The only success output is a log line `Backup finished in {}.` via slf4j
(`FileStoreBackupImpl.backup`, line 115).

### 2.2 Opening the source (read-only)

`Backup.newFileStore()` → `Utils.openReadOnlyFileStore(source[, blobStore])`
(`segment/tool/Utils.java`, lines 49–62):

* Validates the source dir: must exist, be a directory, and directly contain a
  file named `journal.log` (`Utils.isValidFileStore`, lines 93–114); otherwise
  `IllegalArgumentException("Invalid FileStore directory <path>")` → exit 1.
* `fileStoreBuilder(source).withSegmentCacheSize(Integer.getInteger("cache",256))
  .withMemoryMapping(Boolean.getBoolean("tar.memoryMapped")).buildReadOnly()`.
  With `fakeBlobStore`, additionally `.withBlobStore(new BasicReadOnlyBlobStore())`.
* Read-only open takes no `repo.lock` and never modifies the source
  (`filestore-layer.md` §8.2).

The state to copy is `current = reader.readHeadState(revisions)` — the head
resolved from the source's `journal.log` (`FileStoreBackupImpl.backup`, line 71).

### 2.3 Opening / creating the target (read-write)

`FileStoreBackupImpl.backup` (lines 57–70):

```java
SegmentGCOptions gcOptions = SegmentGCOptions.defaultGCOptions().setOffline();
FileStoreBuilder builder = fileStoreBuilder(destination)
    .withStrictVersionCheck(true)
    .withDefaultMemoryMapping();          // resets to MEMORY_MAPPING_DEFAULT
if (USE_FAKE_BLOBSTORE) builder.withBlobStore(new BasicReadOnlyBlobStore());
builder.withGCOptions(gcOptions);
FileStore backup = builder.build();
```

* `setOffline()` sets `offline = true` **and `retainedGenerations = 1`**
  (`segment/compaction/SegmentGCOptions.java`, lines 335–339). This only
  matters for the cleanup pass (§2.6).
* `withStrictVersionCheck(true)`: an existing target with an older store
  version fails the open instead of being upgraded (manifest semantics:
  `filestore-layer.md` §3).
* The target directory is created if missing; a read-write open acquires
  `repo.lock`, writes/validates `manifest`, and (fresh store) writes the
  initial node (§1.4).
* If the target already contains a store, it is opened as-is — this is the
  incremental case (§2.5). The target's existing `journal.log`, `gc.log`,
  manifest and TAR files are all kept.

### 2.4 Copy algorithm

`FileStoreBackupImpl.backup` (lines 71–100), exact order:

```
current := source.reader.readHeadState(source.revisions)      # source head
gen     := current.recordId.segmentId.gcGeneration            # source triple
bufferWriter := SegmentBufferWriter(backup.segmentIdProvider, "b", gen)
writer  := DefaultSegmentWriter(backup, backup.reader, backup.segmentIdProvider,
                                backup.blobStore, WriterCacheManager.Default(),
                                bufferWriter, backup.binariesInlineThreshold)
                                # binariesInlineThreshold default = Segment.MEDIUM_LIMIT
                                # (FileStoreBuilder.java line 100)
cw      := CompactionWriter(backup.reader, backup.blobStore, gen, writer)
compactor := ClassicCompactor(cw, GCNodeWriteMonitor.EMPTY)
head    := backup.getHead()                                   # target head
after   := compactor.compactUp(head, current, Canceller.newCanceller())
          # = compact(before=head, after=current, onto=head); canceller never fires
cw.flush()                                                    # ALWAYS, even if after == null
if after != null:
    backup.revisions.setHead(head.recordId, after.recordId)   # CAS, in-memory
finally: backup.close()                                       # flush + journal line + unlock
```

Only the **head state** is copied. Checkpoints survive because in a segment
store checkpoints are children of the super-root (`checkpoints/<id>/root/...`)
and the journal head *is* the super-root — `compactUp` copies the whole
super-root including `root` and `checkpoints` subtrees. Older journal
revisions and the source's `gc.log` history are **not** transferred.

### 2.5 Incremental behavior

On a pre-existing target, `before = head` is the previous backup's super-root.
The diff `current.compareAgainstBaseState(head, …)` compares source nodes with
target nodes: because previous backups preserved stable ids (§1.1), unchanged
subtrees compare equal via `SegmentNodeState.fastEquals` and are skipped —
their existing target records are reused unmodified. Only changed paths are
rewritten, at generation `gen` (the *current* source head generation, which may
differ from the generation stamped by earlier backups). `setHead` CAS succeeds
because nothing else mutates the target head. If `after == null` cannot happen
here (`Canceller.newCanceller()` never cancels), but the code still guards it
and would simply skip `setHead`, leaving the previous backup head intact.

### 2.6 Cleanup pass on the target

After closing, the target is **reopened** with the same options (but note: *no*
blob store is attached this time even under `USE_FAKE_BLOBSTORE` —
`FileStoreBackupImpl.backup` lines 102–112) and `cleanup(backup)` runs
`FileStore.cleanup()` (`FileStore.java` lines 428–432), then closes again.

`FileStore.cleanup()` → `garbageCollector.cleanup(strategy)`
(`segment/file/GarbageCollector.java`, lines 321–330). Since no compaction ran
in this JVM, `lastCompactionResult == null`, so the no-arg strategy path is
used (`AbstractGarbageCollectionStrategy.cleanup(Context)`, lines 86–95):
`CompactionResult.skipped(lastGCType /*FULL if unknown*/, headGeneration,
gcOptions, revisions.getHead(), gcCount)`, whose reclaimer is
`Reclaimers.newOldReclaimer(FULL, headGeneration, retainedGenerations=1)`
(`segment/file/CompactionResult.java`, `skipped`, lines 139–165).

The FULL reclaimer (`segment/file/Reclaimers.java`, `newOldFullReclaimer`) with
reference `R` (= target head generation = source head's triple) and
`retainedGenerations = 1` reclaims a segment of generation `g` iff:

```
R.fullGeneration - g.fullGeneration >= 1
OR (R.generation - g.generation >= 1 AND NOT g.isCompacted)
```

The generation reclaimer is only one of **three** clauses of the actual reclaim
predicate (`DefaultCleanupContext.shouldReclaim(id, gen, referenced)`,
`segment/file/DefaultCleanupContext.java` lines 96–100; the mark phase walks TAR
entries newest→oldest):

```
shouldReclaim = isDanglingFutureSegment    # compacted segments persisted AFTER
                                           # the compacted-root segment: reclaim
                                           # every isCompacted segment until the
                                           # root's segment UUID is encountered
                                           # (aheadOfRoot flag, lines 80–82)
             OR isUnreferencedBulkSegment  # bulk segment && not referenced
             OR isOldDataSegment           # data segment && generation-reclaimer true
```

Reference marking starts from the tracked referenced **bulk** segment ids only
(`initialReferences`), and the graph traversal only follows references *into*
bulk segments (`shouldFollow`): data segments are reclaimed purely by
generation, bulk segments purely by reachability.

Effect on a fresh backup of a source that has ever been compacted
(`fullGeneration > 0`): the initial-node segment written at `(0,0,false)` is
reclaimed and the single TAR is rewritten to the next generation letter
(`data00000a.tar` → `data00000b.tar`) if it shrinks ≥ 25% (TAR sweep rules:
`tar-layer.md`). On an incremental backup, segments written by older backups
with `fullGeneration` at least 1 behind the current source head are reclaimed.

`DefaultCleanupStrategy.cleanup` (`segment/file/DefaultCleanupStrategy.java`,
lines 35–75) also **appends a line to the target's `gc.log`**
(`GCJournal.persist`) with `gcGeneration =` target head generation, `root` =
current head record id, `nodes` = 0 — *unless* the last `gc.log` entry already
has that generation (`GCJournal.persist` skips equal generations;
`segment/file/GCJournal.java`). **Caveat**: the equality check compares the full
`GCGeneration` triple *including `isCompacted`*, but `GCJournalEntry.fromString`
always re-parses persisted entries with `isCompacted = false` (gc.log stores
only the two integers). So the skip only ever fires when the head generation
has `isCompacted == false`; if the source head is a compacted segment
(`isCompacted == true`, the usual case after any GC), **every backup run
appends one more gc.log line** with identical generation numbers. On a
brand-new backup of a never-compacted
source (head at `(0,0,false)` = the empty `gc.log` default) nothing is written.
Line format: `filestore-layer.md` §7. Reclaimed TAR files are deleted by the
`fileReaper` (renamed/removed after close; deletion list logged).

### 2.7 Fake blob store semantics

`BasicReadOnlyBlobStore` (`segment/file/tooling/BasicReadOnlyBlobStore.java`):
`getBlobId(ref) = ref`, `getReference(id) = id`,
`getBlobLength(id)` = numeric suffix after the last `'#'` (else −1),
`getInputStream` returns an empty stream, writes throw.

Purpose: when the source uses an external data store, opening it *without* a
blob store makes external binary references uncopyable. The copy path for a
binary property on a *changed* path is
`DefaultSegmentWriter.writeBlob(Blob)`:

1. `sameStore(blob)` is false (the blob belongs to the source store, the writer
   to the target), so the same-store record-id shortcut never applies;
2. `blob.getReference()` is called — and `SegmentBlob.getReference()` **throws
   `IllegalStateException("Attempt to read external blob with blobId […] without
   specifying BlobStore")`** when the blob is external and the source store has
   no blob store attached (`segment/SegmentBlob.java`, `getReference`);
3. with the fake store attached to source *and* target, `getReference(blobId)`
   returns the blob-id string, the target's `getBlobId(reference)` returns it
   back, and `writeBlobId` writes an external blob-id record — the binary bytes
   are never read or written; the target segments contain the same external
   blob-id records and the target's `.brf` binary-references index gets the
   same references;
4. inlined (non-external) blobs fall through to `writeStream(blob.getNewStream())`
   and are re-serialized into new (bulk) records in the target.

So **without `USE_FAKE_BLOBSTORE` (default), a backup of a store containing
external binaries fails with exit 1 as soon as the diff visits a changed
property whose value is an external blob**. External binaries under *unchanged*
subtrees are safe only because the diff skips them entirely. Either way,
external binaries are never materialized into the backup.

### 2.8 Files touched by backup

| File (target dir) | When | Content |
|---|---|---|
| `repo.lock` | both opens | 0-byte, exclusively locked while open (`filestore-layer.md` §2) |
| `manifest` | first open (if absent) | store version properties (`filestore-layer.md` §3) |
| `data%05da.tar` (+ embedded `.idx`, `.gph`, `.brf` entries) | copy phase | segments at generation `gen`, wid `"b"`; fresh store also `init` segment at `(0,0,false)` |
| `journal.log` | on each flush/close | appended line `<uuid>:<offset> root <millis>`; never truncated |
| `gc.log` | cleanup phase | one appended line, generation = head generation (skipped if equal to last) |
| `data%05d<next-letter>.tar` | cleanup phase | swept rewrite of a TAR that shrank ≥ 25%; old file deleted by reaper |

The **source** directory is never written (no lock file either — RO open).

### 2.9 Error behavior

Any exception → stack trace to stderr, exit 1. The `finally` blocks close both
stores, so on failure the target may keep: partially written TARs referenced by
no journal line (Oak ignores unreferenced segments; cleanup on the next backup
reclaims them), and a journal whose last line is the *old* head (or the initial
empty node for a fresh target). All such states are openable by Oak — head
resolution skips forward-referencing garbage (`filestore-layer.md` §5.4, §9).
Nothing is rolled back explicitly.

---

## 3. Restore

### 3.1 Command-line entry (`oak-run restore`)

`oak-run/.../run/RestoreCommand.java`: two positional args, **target first,
source second** (opposite of backup!). Fewer than 2 → stderr
`This command requires a target and a source folder`, exit 1.
`Restore.run()` (`segment/tool/Restore.java`, lines 103–111): 0 on success;
stack trace + 1 on exception. Success log line: `Restore finished in {}.`
(slf4j, `FileStoreRestoreImpl.restore`, line 93).

### 3.2 Algorithm (`backup/impl/FileStoreRestoreImpl.java`, `restore`, lines 52–94)

```
if source has no direct child named "journal.log":
    throw IOException("Folder <source> is not a valid FileStore directory")  # exit 1

restore := fileStoreBuilder(source).buildReadOnly()
           # NOTE: plain builder — default segment cache (256 MB) and default
           # memory mapping; the "cache"/"tar.memoryMapped" properties honored
           # by Utils.openReadOnlyFileStore are NOT consulted here.
store   := fileStoreBuilder(destination).withStrictVersionCheck(true).build()
           # read-write; DEFAULT GC options (retainedGenerations = 2, not offline)
current := store.getHead()                                    # target head
head    := restore.getHead()                                  # backup head
gen     := head.recordId.segmentId.gcGeneration               # BACKUP head triple
bufferWriter := SegmentBufferWriter(store.segmentIdProvider, "r", gen)
writer  := DefaultSegmentWriter(store, ..., WriterCacheManager.Default(),
                                bufferWriter, store.binariesInlineThreshold)
cw      := CompactionWriter(store.reader, store.blobStore, gen, writer)
compactor := ClassicCompactor(cw, GCNodeWriteMonitor.EMPTY)
after   := compactor.compactUp(current, head, Canceller.newCanceller())
           # diff backup-head vs target-head, applied ONTO target head
cw.flush()
store.revisions.setHead(current.recordId, after.recordId)
           # NO null check — would NPE if cancelled; canceller never fires.
           # Return value ignored.
finally: restore.close(); store.close()
```

Same engine and semantics as backup with roles swapped: the *backup* store is
the read-only source of truth, and its head is diffed against the live target
head and written into the **existing** target store. Stable ids again make
subtrees that never changed since the backup compare equal, so only divergent
paths are rewritten (at the backup head's generation `gen`, wid `"r"`).

What happens to the target's existing state:

* **Journal**: untouched except one appended line for the restored head at
  close (§1.4). All previous revisions remain in the file. There is **no
  truncation**; the pre-restore head remains reachable as a previous journal
  line until segments are eventually reclaimed by later GC.
* **Existing TARs / generations**: untouched. Restore runs **no cleanup** and
  writes **no `gc.log` entry**. Newly written segments carry the backup head's
  generation triple, which may be *lower* than the target's current head
  generation — the next journal line still wins because head resolution is
  journal-order, not generation-order.
* **Checkpoints**: replaced along with the whole super-root (the restored
  head's `checkpoints` subtree is the backup's).
* If `destination` does not exist, a fresh store is created (lock, manifest,
  initial node) and the restore effectively copies the backup into it.

`FileStoreRestore.restore(File source)` (single-arg, "online restore") only
logs `Restore not available as an online operation.` and does nothing
(lines 96–99).

**[Addition — verified against sources]** Restore attaches **no blob store to
either side** — there is no `USE_FAKE_BLOBSTORE` equivalent in
`FileStoreRestoreImpl`. Consequently, restoring a backup that contains external
binary references fails with `IllegalStateException` (exit 1) as soon as the
diff copies a changed property holding an external blob, exactly as described
in §2.7 step 2. Only external binaries in subtrees unchanged since the backup
(skipped by the diff) survive.

### 3.3 Files touched by restore

| File | When | Content |
|---|---|---|
| target `repo.lock` | open | locked while running |
| target `manifest` | open | validated (strict) / created if fresh |
| target `data%05da.tar` (next free index) | copy | segments wid `"r"` at backup-head generation |
| target `journal.log` | flush/close | appended restored-head line |
| source dir | — | never modified (RO open, no lock) |

Failure behavior: identical to backup — exception → stderr + exit 1, both
stores closed, no rollback; leftover unreferenced segments are harmless.

---

## 4. RecoverJournal

Rebuilds `journal.log` from scratch by scanning every data segment for
plausible head records. Source: `segment/tool/RecoverJournal.java`; command:
`oak-run/.../run/RecoverJournalCommand.java`.

### 4.1 Command-line entry

Options: `-h`/`--help` prints joptsimple help, exit 0. Exactly one positional
path required: none → stderr `Segment Store path not specified`, exit 1; more
than one → stderr `Too many Segment Store paths specified`, exit 1. Then
`RecoverJournal.builder().withPath(dir).withOut(System.out).withErr(System.err)
.build().run()`, exit code = `run()`.

### 4.2 Candidate discovery (`recoverEntries`, lines 204–213 and 307–337)

The store is opened read-only via `Utils.openReadOnlyFileStore(path)` (same
validation and options as §2.2 — note this *reads the existing journal* to bind
an initial head). **The journal must contain at least one resolvable line**:
`ReadOnlyRevisions.bind` walks the journal newest→oldest
(`FileStoreUtil.findPersistedRecordId`, skipping lines whose record id is
malformed or whose segment is absent from the TAR indexes) and **throws
`IllegalStateException("Cannot start readonly store from empty journal")`**
when nothing resolves (`segment/file/ReadOnlyRevisions.java`, `bind`). A
recover-journal run against an empty or fully unresolvable `journal.log`
therefore aborts with `Unable to recover the journal entries, aborting` and
exit 1 *before* scanning any segment — the tool cannot bootstrap a journal from
nothing; at least the last-resort trick of appending one known-good line is
required first. (The same open-failure applies to the backup/restore read-only
source opens, §2.2/§3.2.)

For every segment id in the TAR indexes (`ReadOnlyFileStore.getSegmentIds()`,
`segment/file/ReadOnlyFileStore.java` lines 153–162 — iteration over
`tarFiles.getSegmentIds()`, order irrelevant):

1. **Skip bulk segments** (`segmentId.isBulkSegmentId()`, MSB nibble rules in
   `segment-layer.md` §2).
2. **Parse the timestamp** — `Utils.parseSegmentInfoTimestamp(segmentId)`
   (`segment/tool/Utils.java`, lines 116–142), byte-exactly:
   * `info := segment.getSegmentInfo()` — the segment's **first record** read
     as a string record (`segment/Segment.java`, `getSegmentInfo` lines
     293–298: `readString(recordNumbers.iterator().next().getRecordNumber())`).
     `null` only for non-data segments (never here).
   * Tokenize as JSON: `JsopTokenizer(info, 0); t.read('{'); JsonObject.create(t)`.
     Malformed JSON **throws** (unchecked) — and since `recoverEntries` only
     catches `SegmentNotFoundException`, a malformed segment-info aborts the
     whole run with `Unable to recover the journal entries, aborting` + stack
     trace, exit 1. (A first record that isn't a string record can likewise
     throw or produce garbage that fails JSON parsing.)
   * `timestampString := object.getProperties().get("t")` — the exact JSON key
     is **`"t"`**, and the value must be a bare JSON number whose raw token
     parses with `Long.parseLong` (no sign tolerance beyond `parseLong`, no
     quotes stripping — a quoted `"t":"123"` yields the token `"123"` with
     quotes and fails `parseLong`).
   * Missing key, missing info, or `NumberFormatException` → `null` → the tool
     prints `No timestamp found in segment <uuid>` to **err** and skips the
     segment (not fatal).
3. **Scan records**: `segment.forEachRecord((number, type, offset) -> …)`
   (`Segment.java` lines 449–454, iterating the record table). For every
   record with `type == RecordType.NODE`: read it as a node state
   (`fileStore.getReader().readNode(new RecordId(segmentId, number))`) and
   **keep it as a candidate iff
   `nodeState.hasChildNode("checkpoints") && nodeState.hasChildNode("root")`**
   (lines 331–337) — i.e. it looks like a super-root. Candidate =
   `(timestamp, recordId)` where `timestamp` is the *segment's* info timestamp.
   `SegmentNotFoundException` during the scan is caught; its stack trace is
   printed to err **once per missing segment id** (`handle`, lines 339–343,
   dedup via `notFoundSegments` set).

### 4.3 Sorting (lines 215–243)

Ascending — **oldest first** — by:

1. `timestamp` (long compare);
2. tie → `segmentId.compareTo` (MSB then LSB, arbitrary but deterministic);
3. tie → record **number** ascending (heuristic: higher record number = written
   later).

### 4.4 Consistency filtering (lines 245–304)

A `SegmentNodeStore` is built over the RO store. Iterating the sorted list
**backwards from the newest**, with a shared `corruptedPaths` set (initially
empty) and `ConsistencyChecker` (`segment/file/tooling/ConsistencyChecker.java`;
the base class — all `on*` callbacks are no-ops, so filtering prints nothing
except the `Skipping revision` lines below):

```
for entry from newest to oldest:
    fileStore.setRevision(entry.recordId.toString())
        # RecordId.toString() = "<uuid>.<offset-as-8-hex-digits>"; setRevision
        # re-parses it via RecordId.fromString (pattern accepts ":<decimal>" or
        # ".<8 hex>") and CASes the RO revisions head
        # (ReadOnlyFileStore.setRevision, lines 105–110)
    badPath := checker.checkTreeConsistency(nodeStore.getRoot(), corruptedPaths,
                                            binaries = true)
        # 1) re-probe every known corrupted path first (fast reject);
        # 2) else full DFS of the tree: for each node read every property value;
        #    BINARY/BINARIES values are fully streamed (8 KiB reads) unless the
        #    blob is external (SegmentBlob.isExternal — external refs are NOT
        #    verified); any RuntimeException/IOException marks that path corrupt
        #    (checkNode lines 468–497, traverse lines 533–547)
    if badPath != null:
        print out: "Skipping revision <uuid>.<offset-hex>, corrupted path in head: <badPath>"
        corruptedPaths.add(badPath); remove entry; continue
    for checkpoint in nodeStore.checkpoints():        # child names of "checkpoints"
        root := nodeStore.retrieve(checkpoint)         # checkpoints/<cp>/root
        if root == null:
            print out: "Skipping revision <id>, found unreachable checkpoint <cp>"
            remove entry; continue outer
        badCp := checker.checkTreeConsistency(root, corruptedPaths, true)
        if badCp != null:
            print out: "Skipping revision <id>, corrupted path in checkpoint <cp>: <badCp>"
            corruptedPaths.add(badCp); remove entry; continue outer
    break   # newest surviving entry is consistent — OLDER ENTRIES ARE NOT CHECKED
```

Consequences: only the **suffix** of inconsistent newest entries is removed;
older entries are written to the journal unverified. Only the resulting *last
line* is guaranteed consistent — which is all Oak needs, since head resolution
takes the last resolvable line.

### 4.5 Journal rewrite and backup naming (lines 105–189)

1. If discovery threw: out `Unable to recover the journal entries, aborting`,
   stack trace to err, **exit 1** (journal untouched).
2. If zero candidates: out `No valid journal entries found, aborting`,
   **exit 1** (journal untouched).
3. Choose backup name: first non-existing of
   `journal.log.bak.000`, `journal.log.bak.001`, …, `journal.log.bak.999`
   (`String.format("journal.log.bak.%03d", attempt)`, attempts 0–999). All
   taken → err `Too many journal backups, please cleanup`, **exit 1**.
4. `Files.move(journal.log → journal.log.bak.NNN)` (plain rename, no copy).
   Failure → err `Unable to backup old journal, aborting` + stack trace,
   **exit 1**.
   Success → out `Old journal backed up at journal.log.bak.NNN`.
5. Write a brand-new `journal.log`
   (`PrintWriter(BufferedWriter(FileWriter(journal)))` — created fresh; **no
   fsync is performed**) containing every surviving entry **oldest first**, one
   line each, exactly:

   ```
   printf("%s root %d\n", entry.recordId.toString10(), entry.timestamp)
   ```

   i.e. `<uuid>:<decimal-offset> root <segment-info-timestamp>` + `\n`
   (`RecordId.toString10`, `segment/RecordId.java` lines 136–138 —
   colon + decimal offset, the same grammar variant normal journal writes use;
   `filestore-layer.md` §4). Note the timestamp column is the **segment
   creation time**, not the recovery time.
6. On `IOException` during writing: err
   `Unable to write the recovered journal, rolling back` + stack trace, then
   — **but note**: `PrintWriter.printf` and `PrintWriter.close` never throw;
   they swallow write errors into an internal flag the tool never checks. In
   practice this branch only fires when `new FileWriter(journal)` itself fails
   (file not creatable); a mid-write failure (e.g. disk full) is silently
   ignored and the tool still prints `Journal recovered` / exit 0 over a
   truncated journal. **[Porting note]** A Rust port should surface every write
   error (and ideally fsync) and take this rollback path — strictly safer than
   the Java behavior. Rollback: delete the partial `journal.log`
   (failure → err `Unable to delete the recovered journal, aborting`, exit 1),
   then move `journal.log.bak.NNN` back to `journal.log`
   (failure → err `Unable to roll back the old journal, aborting`, exit 1),
   then out `Old journal rolled back`, **exit 1**.
7. Success: out `Journal recovered`, **exit 0**.

### 4.6 Files touched

| File | Operation |
|---|---|
| `journal.log` | renamed away, then recreated with recovered lines (or restored on rollback) |
| `journal.log.bak.NNN` | rename target of the old journal; left in place on success (never read by Oak — the `.bak.NNN` suffix doesn't match any store pattern) |
| everything else | read-only (no `repo.lock` taken — the store is opened RO; **the repository must be stopped**, nothing prevents a concurrent AEM from racing the rename) |

---

## 5. Console output / exit code summary

| Tool | Exit 0 | Exit 1 |
|---|---|---|
| `backup` | silent (slf4j `Backup finished in {}.`) | missing args (stderr usage line); any exception (stack trace to stderr) |
| `restore` | silent (slf4j `Restore finished in {}.`) | missing args; any exception (stack trace) |
| `recover-journal` | `Old journal backed up at …` + optional `Skipping revision …` lines + `Journal recovered` (stdout) | arg errors; discovery failure; zero candidates; >1000 backups; rename/write/rollback failures (messages in §4.5; per-segment diagnostics on stderr) |

---

## 6. AEM safety invariants (checklist for the Rust implementation)

Backup / restore:

1. **Never write to the source store.** Open it read-only, take no lock, and
   validate `journal.log` presence first.
2. **Hold `repo.lock`** (exclusive OS file lock) on any store you write, for
   the whole duration, and only run against a stopped AEM.
3. **Preserve stable ids** on every copied node record (pass the source node's
   20-byte stable id into the node writer). Without this, incremental backup,
   later restore diffs, and Oak's own `fastEquals`/dedup behavior break.
4. **Stamp copied segments with the source head's exact `GCGeneration` triple**
   `(generation, fullGeneration, isCompacted)` — including the compacted bit in
   bit 31 of the full generation field. Do not invent or advance generations.
5. **Emit a valid segment-info first record** in every new segment:
   `{"wid":"…","sno":N,"t":M}` with `t` a bare decimal `currentTimeMillis` —
   `recover-journal` and other tooling parse this JSON and hard-fail on
   malformed info.
6. **Durability order**: flush the segment writer buffer → force TAR data +
   index → only then append the journal line for the new head. The journal
   append is the commit point; never write a journal line whose record is not
   yet durable in a TAR.
7. **Append, never truncate**, `journal.log` and `gc.log` on backup/restore
   targets; journal line = `<uuid>:<decimal-offset> root <millis>\n`.
8. **Copy the full super-root** (both `root` and `checkpoints` children), or
   AEM loses its checkpoints and async indexing lanes reindex from scratch.
9. **Respect strict version check** semantics on the target `manifest`; create
   a spec-conformant manifest for fresh targets.
10. **External binaries copy by reference only** — never materialize or drop
    external blob-id records; ensure they land in the target's binary
    references index so datastore GC stays correct.
11. Cleanup on the backup target must use the FULL reclaimer with
    `retainedGenerations = 1` against the head generation, must not reclaim
    the head's own generation, must follow the TAR sweep/generation-letter
    rules, and should append the `gc.log` entry only when the generation
    differs from the last entry.
12. On failure, it is safe (and Oak-compatible) to leave unreferenced segments
    and a journal pointing at the previous head; never leave a journal line
    referencing missing segments as the *only* line.

Journal recovery:

13. **Back up before overwrite**: rename the old journal to the first free
    `journal.log.bak.NNN` (000–999) before creating the new one; on any write
    failure restore the original by renaming back and deleting the partial
    file. Never delete the only copy of the old journal.
14. Candidates are **data segments only**, records of type NODE whose node has
    both a `root` and a `checkpoints` child; timestamp from segment-info key
    `"t"`; skip (with a diagnostic) segments lacking a parsable timestamp.
15. Sort ascending by `(timestamp, segmentId, recordNumber)` and write
    **oldest first** — Oak reads the journal bottom-up, so the newest entry
    must be the last line.
16. **The last line must be verified consistent**: full-tree DFS of the head
    (reading every property, streaming every non-external binary) and of every
    checkpoint root, dropping newer revisions until one passes. Unverified
    older lines are acceptable; an unverified last line is not.
17. Journal lines use `toString10` (colon + decimal offset) — both offset
    grammars are accepted by Oak's parser, but stay byte-identical to Oak
    output to keep external tooling happy.
