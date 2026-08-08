# Write path: Cleanup (mark/sweep phase of garbage collection)

Byte- and behavior-exact specification of the cleanup (sweep) subsystem of
`oak-segment-tar`, for a Rust port that must leave a repository AEM can start
against. This document covers what happens *after* a compaction (or instead of
one): deciding which segments are garbage, rewriting or deleting TAR files,
appending to `gc.log`, and invalidating in-memory state.

Builds on (does not repeat):

- `tar-layer.md` — TAR container layout, `.idx`/`.gph`/`.brf` entry formats,
  padding rule, file-name pattern, generation-letter selection at open.
- `filestore-layer.md` — §7 `gc.log` line format and I/O discipline, §6 TAR
  discovery, §8 store-opening order.
- `segment-layer.md` — segment header, segment-reference list (source of graph
  edges), segment id type nibble.

Java sources cited below are under
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/`.

---

## 1. Entry points and orchestration

There are three ways cleanup runs; all funnel into `TarFiles.cleanup`:

1. **As part of a GC cycle** (`FileStore.fullGC`/`tailGC` →
   `GarbageCollector.run*` → `AbstractGarbageCollectionStrategy.run`,
   `file/AbstractGarbageCollectionStrategy.java`): after
   `compactionStrategy.compact(...)` returns a `CompactionResult`, the
   strategy calls `cleanup(context, compactionResult)` and feeds the returned
   list of removable file names to the `FileReaper`
   (`AbstractGarbageCollectionStrategy.run`, line
   `context.getFileReaper().add(cleanup(context, compactionResult))`).
   Cleanup runs **even when compaction failed** ("cleaning up after failed
   compaction") — but with an *empty* reclaimer (see §5.4).

2. **Standalone** (`FileStore.cleanup()` →
   `GarbageCollector.cleanup(strategy)`, `file/FileStore.java` +
   `file/GarbageCollector.java`): if a `lastCompactionResult` is pending from
   an earlier `compactFull`/`compactTail` call in the same process, it is used
   (and cleared) so a GC-journal entry gets written; otherwise a
   `CompactionResult.skipped(...)` placeholder is built from
   `lastCompactionType` (defaults to `FULL` after restart — "conservative and
   safe", `GarbageCollector` field comment), the current head generation, the
   configured `retainedGenerations`, and the current head record id
   (`AbstractGarbageCollectionStrategy.cleanup(Context)`).

3. **Pre-compaction cleanup** (`CleanupFirstGarbageCollectionStrategy.run`
   wraps the compaction strategy in `CleanupFirstCompactionStrategy`): a
   cleanup with *custom* predicates runs immediately **before** compaction.
   Differences from the default path (`file/CleanupFirstCompactionStrategy.java`
   comments): no GC-journal entry is written, the segment cache is *not*
   cleared, and the reclaimer assumes `retainedGenerations == 2` (hard-coded
   predicate shapes, §5.3).

### 1.1 `DefaultCleanupStrategy.cleanup` — normative sequence

`file/DefaultCleanupStrategy.java`, `cleanup(Context)`:

```
1.  log "cleanup started using reclaimer <reclaimer.toString()>"
2.  segmentCache.clear()                       # evict all cached Segment objects
3.  System.gc()                                # hint: drop stale weak SegmentId refs
4.  cleanupResult = tarFiles.cleanup(new DefaultCleanupContext(
        segmentTracker, reclaimer, compactedRootIdString))   # §§2-4
5.  if cleanupResult.interrupted: log "cleanup interrupted"
6.  segmentTracker.clearSegmentIdTables(cleanupResult.reclaimedSegmentIds,
        segmentEvictionReason)                 # §8
7.  finalSize = tarFiles.size()                # writer length + Σ reader lengths
8.  fileStoreStats.reclaimed(cleanupResult.reclaimedSize)    # in-memory stat
9.  if gcJournal != null:                      # only when requiresGCJournalEntry()
        gcJournal.persist(reclaimedSize, finalSize,
            revisions.getHead().segmentId.gcGeneration,      # head generation NOW
            compactionMonitor.compactedNodes, compactedRootId)   # §7
10. return cleanupResult.removableFiles        # caller hands to FileReaper
```

`compactedRootId` is `compactionResult.getCompactedRootId().toString10()` —
`RecordId.NULL.toString10()` for aborted compactions
(`AbstractGarbageCollectionStrategy.newCleanupStrategyContext`).
`getGCJournal()` returns `null` unless
`compactionResult.requiresGCJournalEntry()` (true only for
`CompactionResult.succeeded`), so **only a fully successful compaction appends
a `gc.log` line** (`CompactionResult.java`).

---

## 2. `TarFiles.cleanup` — the mark/sweep driver

`file/tar/TarFiles.java`, `cleanup(CleanupContext)`. All steps below are
normative order.

### 2.1 Writer rotation (snapshot barrier)

```
acquire writeLock; acquire readLock
    internalNewWriter()            # release writeLock in finally
    head = readers                 # snapshot of reader linked list
    references = copy of context.initialReferences()
release readLock
```

`internalNewWriter()` (`TarFiles.java`): calls
`writer.createNextGeneration()`. If nothing was ever written to the current
writer (`!archive.isCreated()` — `SegmentTarWriter` creates the file lazily on
first segment write) it returns the same writer and **nothing changes**.
Otherwise the writer is closed (`TarWriter.close`: appends `.brf` entry, then
`.gph` entry, then — inside `SegmentTarWriter.close` — the `.idx` entry and
two 512-byte zero blocks; see `tar-layer.md` §§4–6 for the entry bytes), a new
`TarWriter` on `data%05d%s.tar` with index+1 and letter `"a"` is created
(`TarWriter` ctor, `FILE_NAME_FORMAT`), and the closed file is reopened as a
`TarReader` prepended to `readers`. Consequence: the just-closed writer file
*participates* in this cleanup; segments written to the *new* writer after the
snapshot can never be reclaimed in this pass.

Note `SegmentTarWriter.close()` does **not** fsync (`writeIndex(); write 2
zero blocks; access.close()`); only `flush()` calls `access.getFD().sync()`.
Oak relies on the old generation staying on disk until the reaper deletes it
(§6) for crash safety, not on syncing the new file.

### 2.2 Snapshot accounting

```
cleaned = ordered map { reader -> reader } for every reader in head   # newest tar first
result.reclaimedSize = Σ reader.size()      # archive.length(): full physical file size
```

Reader list order: `readers` is built newest-index-first (`TarFiles.init`
prepends ascending indices), so iteration is **descending tar index =
newest → oldest**.

### 2.3 Mark

```
for reader in cleaned.keySet():             # newest tar → oldest tar
    if shutdown: result.interrupted = true; return result
    reader.mark(references, reclaim, context)      # §3
```

### 2.4 Sweep

```
for reader in cleaned.keySet():             # newest → oldest
    if shutdown: result.interrupted = true; return result
    cleaned[reader] = reader.sweep(reclaim, result.reclaimedSegmentIds)   # §4
```

`sweep` returns `null` (whole file removable), the same reader (kept), or a
new reader on the next generation letter (rewritten).

### 2.5 Reader-list swap (CAS loop) and reclaimed-size arithmetic

The new list `swept` is rebuilt from the current `readers`: for each reader,
if it was part of this mark/sweep (`cleaned.containsKey`), substitute
`cleaned[reader]` (skip if `null`); readers added concurrently are kept as-is.
Under `writeLock`: if `readers` still `== head`, set `readers = swept` and
exit; otherwise re-read `readers` and rebuild (`closeables`/`reclaimed` reset
each iteration). Then:

```
result.reclaimedSize -= reclaimed           # reclaimed = Σ size of surviving
                                            #   (kept or rewritten) participants
for reader in closeables:                   # originals replaced or removed
    reader.close()
    result.removableFiles.add(reader.getFileName())
```

So `reclaimedSize = Σ physical size of participating tars before − Σ physical
size of the tars that represent them after` (deleted files count in full,
rewritten files count `old − new`). It is computed **before** anything is
physically deleted; `gc.log` therefore records bytes *marked* for deletion.
`removableFiles` contains only the *old* file names (replaced originals and
fully-swept files); it never contains the new generation files.

---

## 3. Mark phase

### 3.1 Seed references — what starts the mark

**The mark is NOT seeded from the head state, checkpoints, or any record
id.** Data segments are retained *purely by generation arithmetic* (§5);
reachability is used **only for bulk segments**.

`DefaultCleanupContext.initialReferences()`
(`file/DefaultCleanupContext.java`):

```java
return segmentTracker.getReferencedSegmentIds().stream()
        .filter(SegmentId::isBulkSegmentId)
        .map(SegmentId::asUUID)
        .collect(Collectors.toSet());
```

i.e. the UUIDs of every **bulk** segment id currently referenced *in memory*
(weakly-held `SegmentId` instances in the `SegmentIdTable`s,
`SegmentTracker.getReferencedSegmentIds`). This protects (a) bulk segments
whose referencing data segment was already reclaimed in a previous cycle but
which a live in-memory reader still holds, and (b) bulk segments whose *only*
referencing data segments were written **after the §2.1 snapshot** (they live
in the new writer file, which no participating reader's graph covers) or are
still in flight — in both cases the writing thread holds their `SegmentId`s,
so the seed set is the *sole* protection. For an **offline tool on a
quiescent, stopped repository this set may be empty** — correctness then
rests entirely on the graph indexes (§3.3) — but an empty seed is safe *only*
when no writes can happen concurrently with the mark/sweep. Checkpoints need
no special seeding: they are part of the head super-root, whose segments
survive by generation.

### 3.2 Per-tar mark — `TarReader.mark`

`file/tar/TarReader.java`, `mark(Set<UUID> references, Set<UUID> reclaimable,
CleanupContext context)`:

```
if archiveManager.isReadOnly(fileName): return      # default false (SegmentArchiveManager)
graph   = getGraph()          # pre-compiled .gph entry, else computed from
                              # each data segment's segment-reference list
                              # (SegmentGraph.load / SegmentGraph.compute)
entries = getEntries()        # index entries sorted by FILE POSITION
                              # (SegmentTarReader.listSegments, IndexEntry.POSITION_ORDER)
for i = entries.length - 1 down to 0:               # reverse write order
    entry = entries[i]
    id  = UUID(entry.msb, entry.lsb)
    gen = GCGeneration(entry.generation, entry.fullGeneration, entry.isCompacted)
    if context.shouldReclaim(id, gen, references.remove(id)):
        reclaimable.add(id)
    else:
        for refId in graph.getEdges(id):
            if context.shouldFollow(id, refId):     # only edges to BULK targets
                references.add(refId)
```

Key properties:

- `references.remove(id)` both queries and removes: once a bulk segment entry
  itself is visited, its membership has been consumed (comment in `mark`: a
  bulk segment is always written *before* any data segment referencing it, so
  backward iteration sees all referencing data segments first).
- The `references` set is **shared across all tar files** and the tars are
  visited newest→oldest, so a kept data segment in a newer tar protects bulk
  segments in older tars. Combined with the per-tar backward loop, the global
  visit order is exact **reverse chronological write order**.
- `shouldFollow(from, to)` (`DefaultCleanupContext`) is
  `!isDataSegmentId(to.lsb)` — graph edges are followed only *into bulk
  segments* (`isDataSegmentId(lsb)` ⇔ `(lsb >>> 60) == 0xA`,
  `SegmentId.java`; bulk nibble is `0xB`).

### 3.3 `shouldReclaim` — the three-part predicate

`DefaultCleanupContext.shouldReclaim(id, generation, referenced)`:

```
reclaim ⇔ isDanglingFutureSegment(id, generation)
        ∨ isUnreferencedBulkSegment(id, referenced)
        ∨ isOldDataSegment(id, generation)

isUnreferencedBulkSegment(id, referenced) ⇔ ¬isDataSegmentId(id.lsb) ∧ ¬referenced
isOldDataSegment(id, generation)          ⇔  isDataSegmentId(id.lsb) ∧ old.test(generation)   # §5
```

**Dangling future segments** (stateful, order-dependent):

```java
private boolean isDanglingFutureSegment(UUID id, GCGeneration generation) {
    return (aheadOfRoot &= !id.equals(rootSegmentUUID)) && generation.isCompacted();
}
```

`rootSegmentUUID` is the segment UUID of the compacted-root record id string
passed to the context (parsed with `RecordId.fromString`); if that string is
`RecordId.NULL`, `aheadOfRoot` starts `false` and the rule is disabled.
Otherwise every **compacted-flagged** segment visited *before* (i.e. written
*after*) the compacted root's own segment is reclaimed, regardless of
generation. Rationale (class javadoc): an aborted incremental compaction
persists compacted segments *newer than* the last committed compacted root;
"compacted segments are unused iff they are persisted after the last compacted
root". The `&=` mutation means the rule switches off permanently once the
root's segment is encountered. This is why the global reverse-order visit is
**normative**, not an optimization.

---

## 4. Sweep phase — `TarReader.sweep`

`file/tar/TarReader.java`, `sweep(Set<UUID> reclaim, Set<UUID> reclaimed)`.

### 4.1 Decision arithmetic (Java `int`, 32-bit wrapping)

```
if archiveManager.isReadOnly(name): return this

cleaned = ∅ ; afterSize = 0 ; beforeSize = 0 ; afterCount = 0    # all int
entries = getEntries()                        # file-position order
for i in 0 .. entries.length-1:
    e = entries[i]
    beforeSize += getEntrySize(e.length)      # 512 + length + padTo512(length)
    if UUID(e.msb, e.lsb) ∈ reclaim:
        cleaned.add(id); entries[i] = null
    else:
        afterSize += getEntrySize(e.length); afterCount += 1

if afterCount == 0:            return null    # whole file removable
if afterSize >= beforeSize*3/4: return this   # savings < 25% → keep as-is
generationChar = name[len(name) - 5]          # position of 'a' in "...a.tar"
if generationChar == 'z':       return this   # cannot advance past 'z'
newFile = name[0 .. len-5) + (generationChar+1) + ".tar"
```

`getEntrySize(size) = BLOCK_SIZE + size + getPaddingSize(size)` with
`BLOCK_SIZE = 512` (`SegmentTarReader.getEntrySize`; padding rule in
`tar-layer.md` §2.2). `beforeSize`/`afterSize` count **only segment data
entries** (header + payload + padding) — not the `.idx`/`.gph`/`.brf` entries
or the trailer — while the reclaimed-size accounting of §2 uses full physical
file lengths. `beforeSize * 3 / 4` is Java `int` arithmetic: multiply first
(wrapping at 2^31), then truncating division. With Oak's 256 MB default max
file size overflow cannot occur; a port must still not "fix" the expression to
floating point (use 64-bit or the exact int sequence).

### 4.2 Rewriting the next generation

```
writer = TarWriter(archiveManager, newFile)          # writeIndex = -1 variant
for e in entries where e != null:                    # original file order preserved
    data = archive.readSegment(e.msb, e.lsb)         # segment PAYLOAD only, e.length bytes
    writer.writeEntry(e.msb, e.lsb, data, 0, e.length,
                      GCGeneration(e.generation, e.fullGeneration, e.isCompacted))

# graph: keep only edges whose BOTH endpoints survived
for (from, tos) in getGraph().getEdges():
    if from ∉ cleaned:
        for to in tos:
            if to ∉ cleaned:
                writer.addGraphEdge(from, to)

# binary references: keep entries of surviving segments, generation preserved
if getBinaryReferences() != null:
    forEach (gen, full, compacted, id, reference):
        if id ∉ cleaned:
            writer.addBinaryReference(GCGeneration(gen, full, compacted), id, reference)

writer.close()      # appends .brf, .gph, .idx entries + 2 zero blocks (tar-layer.md §§4-6)

reader = openFirstFileWithValidIndex([newFile])      # re-open, index-first
if reader != null: reclaimed.addAll(cleaned); return reader
else:              log warn; return this             # fall back to original
```

Facts a port must reproduce:

- Surviving entries keep a **byte-identical payload** (same UUID, same
  `e.length` payload bytes, same `GCGeneration` triple) in **original
  file-position order** (`listSegments` sorts by `IndexEntry.POSITION_ORDER`).
  The 512-byte tar entry *header* is regenerated by `writeSegment`, not copied:
  the entry name `"%s.%08x" % (uuid, crc32(payload))` comes out identical
  (same payload ⇒ same CRC32), but the mtime field is stamped with the current
  time and the header checksum changes accordingly. Only the payload (and its
  zero padding) must be reproduced bit-exact.
- The graph and binary-reference indexes are **regenerated by filtering**, not
  recomputed from segment contents. Graph edges pointing *out of* the file to
  segments in other (lower-index) tars survive as long as the target UUID is
  not in this file's `cleaned` set — cross-tar bulk references keep working.
- Generations recorded in `.brf` entries keep their original
  `(generation, fullGeneration, isCompacted)` values.
- The index for the new file is rebuilt by the writer as entries are appended
  (formats per `tar-layer.md` §4; the `.idx` entry is written by
  `SegmentTarWriter.close` via `writeIndex()`).
- If the freshly written file cannot be re-opened with a valid index, the
  original reader stays in service and the defective `…(g+1).tar` file is
  simply **left on disk** (tolerated at next startup, §9).
- **(Added — durability gap in Java)** Nothing in the sweep path fsyncs the
  new `g+1` file: `TarWriter.close` → `SegmentTarWriter.close` is
  `writeIndex(); write 2 zero blocks; access.close()` with no
  `getFD().sync()`, and no directory fsync is ever issued. Java Oak therefore
  has a crash window where the reaper's deletion of `g` (a metadata
  operation) becomes durable while `g+1`'s data is still only in the page
  cache — losing *live* segments on power failure. A safe Rust port should
  fsync the `g+1` file (and the store directory) after the validating re-open
  and **before** the old `g` file is deleted; this is a strictly stronger
  behavior than Java and is the recommended deviation (§11.15).
- Only the rewrite path adds ids to `reclaimedSegmentIds`
  (`reclaimed.addAll(cleaned)`). **Segments of a tar removed in its entirety
  (`afterCount == 0`) are *not* added**, so `clearSegmentIdTables` never
  annotates them (Oak quirk; reproduce as-is or accept a superset — the set
  only feeds in-memory diagnostics, §8).

---

## 5. Reclaim predicates (`file/Reclaimers.java`) and generation arithmetic

### 5.1 `GCGeneration` recap (`spi/persistence/GCGeneration.java`)

Triple `(generation:int, fullGeneration:int, isCompacted:boolean)`; `NULL =
(0,0,false)`. Advancement (used by the compaction spec, listed here because
cleanup's arithmetic depends on it):

| op | result | used by |
|---|---|---|
| `nextFull()` | `(g+1, f+1, true)` | full compaction |
| `nextTail()` | `(g+1, f, true)` | tail compaction |
| `nextPartial()` | `(g, f, true)` | intermediate states of incremental compaction |
| `nonGC()` | `(g, f, false)` | normal writes after a compaction |

Comparisons are plain Java `int` subtraction (wrapping):
`compareWith(o) = generation - o.generation`,
`compareFullGenerationWith(o) = fullGeneration - o.fullGeneration`.

### 5.2 `newOldReclaimer(lastGCType, referenceGeneration, retainedGenerations)`

`referenceGeneration` = the generation created by the compaction that just
succeeded (`CompactionResult.succeeded`), or the current head generation for
skipped compaction (`CompactionResult.skipped`). `retainedGenerations` comes
from `SegmentGCOptions` (default 2). Exact boolean expressions, with `h` =
referenceGeneration, `s` = segment generation, `n` = retainedGenerations:

**FULL** (`newOldFullReclaimer`):

```
reclaim(s) ⇔ (h.fullGeneration - s.fullGeneration >= n)
           ∨ ((h.generation - s.generation >= n) ∧ ¬s.isCompacted)
```

**TAIL** (`newOldTailReclaimer`):

```
reclaim(s) ⇔ (h.generation - s.generation >= n)
           ∧ ¬(s.isCompacted ∧ s.fullGeneration == h.fullGeneration)
```

The `isCompacted` flag's role: under FULL GC an old-by-`generation` segment
survives if it is a compacted segment of a still-retained *full* generation;
under TAIL GC compacted segments sharing the reference's full generation form
the "same compacted tail" and are immune regardless of age.

`toString()` of these predicates appears verbatim in the "cleanup started
using reclaimer …" log line and in the eviction reason string, e.g.
`(full generation older than %d.%d, with %d retained generations)` /
`(generation older than %d.%d, with %d retained generations and not in the
same compacted tail)`.

### 5.3 Pre-compaction variants (`CleanupFirstCompactionStrategy.newCleanupContext`)

`currentGeneration` = current head generation, `compactedRoot` = root from the
*last* `gc.log` entry (`context.getGCJournal().read().getRoot()`). Hard-coded
for `retainedGenerations == 2`:

```
FULL: reclaim(s) ⇔ s.fullGeneration < h.fullGeneration
                 ∨ (s.generation < h.generation ∧ ¬s.isCompacted)

TAIL: reclaim(s) ⇔ s.fullGeneration < h.fullGeneration - 1
                 ∨ (s.fullGeneration == h.fullGeneration - 1 ∧ ¬s.isCompacted)
                 ∨ (s.generation < h.generation ∧ ¬s.isCompacted)
```

(these keep one extra transient generation because the upcoming
`newGeneration` has not been committed yet).

### 5.4 Other reclaimers

- `newEmptyReclaimer()` — constant `false`; used after **aborted** compaction
  (`CompactionResult.aborted`), so a failed compaction's cleanup can still
  reclaim *unreferenced bulk segments* (part 2 of §3.3) but no data segment by
  age. The **dangling-future rule is disabled** on this path:
  `CompactionResult.aborted` does not override `getCompactedRootId()`, so the
  cleanup context receives `RecordId.NULL` and `aheadOfRoot` starts `false`.
- `partiallySucceeded` (incremental compaction, cycle incomplete) also uses a
  constant-`false` reclaimer but **does** override `getCompactedRootId()` with
  the real intermediate compacted root — this is the path where the
  dangling-future rule of §3.3 does its work (reclaiming compacted segments
  persisted after the last committed compacted root).
- `newExactReclaimer(g)` — `generation.equals(g)` (all three fields). Present
  in `Reclaimers.java` but **unused by any main-source code path** in current
  Oak (referenced only from tests); a port does not need it for cleanup.

### 5.5 What cleanup reclaims per GC type — summary

- Full GC advances head to `nextFull()`; subsequent cleanup with the FULL
  reclaimer and `n = 2` removes data segments whose `fullGeneration ≤ h.f−2`,
  plus non-compacted segments with `generation ≤ h.g−2`, plus unreachable bulk
  segments, plus dangling future compacted segments.
- Tail GC advances head to `nextTail()`; cleanup removes data segments with
  `generation ≤ h.g−2` unless they are compacted members of the current full
  generation, plus the same bulk/dangling rules.

---

## 6. `FileReaper` — deferred deletion (`file/FileReaper.java`)

- Thread-safe set of file names; `add(files)` deduplicates.
- `reap()` snapshots-and-clears the set, then calls
  `archiveManager.delete(name)` per file; `SegmentTarManager.delete` is
  `Files.deleteIfExists(dir/name)` returning `false` on `IOException`.
  Failures are logged (`"Unable to remove file"`) and **re-added** for the
  next reap. Note `deleteIfExists` returns `false` for an already-missing
  file, so a name that never existed re-queues forever (harmless, log noise).
- Scheduling (`file/FileStore.java`): a background task runs `fileReaper::reap`
  **every 5 seconds** (`fileStoreScheduler.scheduleWithFixedDelay(..., 5,
  SECONDS, fileReaper::reap)`), and `FileStore.close()` runs `System.gc()`
  then a final `fileReaper.reap()` after flushing and closing everything.
- **Why deferred**: swept/removed tar files may still be memory-mapped by
  concurrently running readers; deletion is decoupled so the swap in §2.5 can
  complete first, and (on OSes that refuse to delete mapped files) retry
  after mappings are GC'd. An offline Rust tool may delete synchronously
  *after* closing all handles, but must preserve the ordering: **new
  generation fully written and re-validated → readers swapped → old file
  deleted**.

### 6.1 `.bak` files

`.bak` files are **not** part of cleanup and never touched by the reaper.
They are produced only by TAR *recovery* at open time
(`TarReader.collectFileEntries` → `backupSafely` → `SegmentTarManager.backup`:
rename `name` → `name + ".bak"`, or `name + "." + i + ".bak"` for `i = 2, 3,
…` until unused (`findAvailGen`); if rename fails, copy then delete, throwing
`IOException` if the delete fails). Read-only opens use extension `".ro.bak"`
the same way (`TarReader.openRO`). Because `TarFiles.collectFiles` only
matches `(data)((0|[1-9][0-9]*)[0-9]{4})([a-z])?.tar`, `.bak` files are
invisible to store initialization and to cleanup — they accumulate until an
operator removes them. A port must never count them in repository size
(`TarFiles.size()` iterates only live readers + writer) nor delete them.

---

## 7. `gc.log` append after cleanup (`file/GCJournal.java`)

Line format and I/O: `filestore-layer.md` §7. Cleanup-specific behavior of
`GCJournal.persist(reclaimedSize, repoSize, gcGeneration, nodes, root)`:

- **Skip condition**: if `read().getGcGeneration().equals(gcGeneration)`
  (all three `GCGeneration` fields), the call is a NOOP — "failed compaction,
  only update the journal if the generation increases". Caveat: entries parsed
  back from disk always carry `isCompacted = false`
  (`GCJournalEntry.fromString` → `newGCGeneration(generation, fullGeneration,
  false)`), while the head generation passed in normally has
  `isCompacted = true`; the equality guard is therefore only effective against
  the in-memory `latest` entry from the *same process*.
- The line written (`GCJournalEntry.toString`, comma-joined):
  `repoSize,reclaimedSize,timestampMillis,generation,fullGeneration,nodes,root`
  where `root` is the compacted root `RecordId.toString10()` and `repoSize` is
  `TarFiles.size()` measured *after* the reader swap but *before* physical
  deletion, and `reclaimedSize` is the §2.5 value ("marked for deletion"
  bytes).
- I/O (`file/LocalGCJournalFile.writeLine`): open `gc.log` with
  `WRITE|APPEND|CREATE|DSYNC`, write line + `\n` (UTF-8), close. `DSYNC`
  makes the append synchronously durable. Write errors are logged and
  swallowed (`GCJournal.persist` catch block) — a failed `gc.log` append does
  not fail cleanup.
- The generation recorded is the **head's generation at cleanup time**
  (`DefaultCleanupStrategy.getGcGeneration` =
  `revisions.getHead().getSegmentId().getGcGeneration()`), not the reclaimer's
  reference generation.

---

## 8. In-memory invalidation

Required so subsequent reads do not resurrect reclaimed segments:

1. **Segment cache** — `context.getSegmentCache().clear()` *before* the
   mark/sweep (`DefaultCleanupStrategy`). Clearing evicts cached `Segment`
   objects; eviction calls `SegmentId.unloaded()` so ids stop memoizing their
   segment bytes. (Skipped intentionally in the pre-compaction variant.)
2. **`System.gc()`** — invoked immediately after, as a *hint* to drop
   `WeakReference<SegmentId>` entries in the `SegmentIdTable`s so
   `initialReferences()` (§3.1) doesn't over-retain bulk segments.
3. **`SegmentTracker.clearSegmentIdTables(reclaimedIds, gcInfo)`** *after*
   `TarFiles.cleanup` returns: for every live `SegmentId` whose UUID is in the
   reclaimed set, call `id.reclaimed(gcInfo)` (`SegmentIdTable.
   clearSegmentIdTables`). This only stamps a diagnostic string
   (`SegmentId.reclaimed` sets `gcInfo`); it is appended to the
   `SegmentNotFoundException` message if anything later dereferences the id.
   `gcInfo` is `CompactionResult.gcInfo()`:
   `gc-count=%d,gc-status=%s,store-generation=%s,reclaim-predicate=%s`
   (status `success`/`failed`, generation via `GCGeneration.toString`).
4. The reader swap itself (§2.5) is what actually removes the segments from
   the lookup path: `TarFiles.readSegment`/`containsSegment` consult only the
   current `readers` list and writer. Old `TarReader`s are closed after the
   swap; their mapped buffers are released when GC collects them.

For a single-threaded offline Rust tool, (1)–(3) reduce to: drop any cached
segment/id state derived from the pre-cleanup file set before serving further
reads, and never cache lookups across the swap.

---

## 9. Error, cancellation, and crash behavior

- **Cancellation**: `TarFiles.cleanup` checks the volatile `shutdown` flag
  (set by `TarFiles.close`) before marking each tar and before sweeping each
  tar; on shutdown it returns immediately with `interrupted = true`, an empty
  `removableFiles`, and whatever `reclaimedSegmentIds` were accumulated. No
  reader swap happens; already-rewritten `…(g+1).tar` files from completed
  sweep steps of this pass are **left on disk unreferenced** (the swap is
  all-or-nothing at the end) and their freshly opened `TarReader`s are leaked
  unclosed. At the *next* startup those valid `g+1` files win the
  generation-letter selection and the `g` originals are deleted, so completed
  sweep steps commit retroactively — safe, because a sweep preserves every
  non-reclaimable segment. `DefaultCleanupStrategy` still proceeds to clear
  tables / write `gc.log` for whatever was reported. Quirk: an interrupted
  result's `reclaimedSize` still holds the §2.2 value — Σ *full physical
  sizes of all participating tars* — because the §2.5 subtraction never runs;
  Oak reports (and, after a successful compaction, records in `gc.log`) this
  inflated number. Reproduce or document the deviation.
- **Sweep write failure**: `TarWriter.close` failure surfaces as
  `UnrecoverableArchiveException` (an `IOException`) and aborts the whole
  cleanup; a partially written next-generation file may remain. If close
  succeeded but re-open finds no valid index, `sweep` falls back to the
  original reader and leaves the bad file (§4.2).
- **Crash before reap**: both `dataNNNNN(g).tar` and `dataNNNNN(g+1).tar`
  exist. Next startup (`TarReader.open(Map,…)`, see `filestore-layer.md`
  §6.3): generations are tried **highest letter first**; the first with a
  valid index wins and *all other generations of that index are deleted*
  (`openFirstFileWithValidIndex`: `archiveManager.delete(other)` after a
  successful open, logged as "Removing unused tar file"). So: valid `g+1` →
  `g` removed (cleanup completes retroactively); invalid/truncated `g+1` → it
  is skipped and then removed, `g` restored. Either way AEM starts.
- **Crash after reap of a rewritten file**: only `g+1` remains — normal.
- **Crash before reap of a fully-removable file** (`afterCount == 0`): the
  file is re-opened at next startup and its stale segments become visible
  again; they are unreferenced and will be reclaimed by a future cleanup.
  Harmless because nothing durable (journal head, checkpoints) references
  them.
- **`gc.log` never written on abort**: aborted compaction ⇒ no
  `requiresGCJournalEntry` ⇒ `gcJournal == null` in the cleanup context.

---

## 10. Files touched by a cleanup pass

| File | Name pattern | Operation | Final content |
|---|---|---|---|
| Rotated writer | `data%05d a.tar` (`TarWriter` `FILE_NAME_FORMAT`, e.g. `data00012a.tar`) | closed (brf+gph+idx+2×512 zero blocks appended), reopened read-only | complete TAR per `tar-layer.md` |
| New writer | `data%05d a.tar`, index+1 | created lazily (empty until first post-cleanup write) | absent or normal writer file |
| Swept tar | `dataNNNNN(g+1).tar`, `g+1 ≤ 'z'` | written whole, closed, validated by re-open | surviving segment entries in original order + filtered `.gph`/`.brf` + rebuilt `.idx` + trailer |
| Replaced/removed tars | `dataNNNNN(g).tar` | queued in `FileReaper`, deleted on a later `reap()` (≤5 s cadence or at close) | deleted |
| `gc.log` | literal | one line appended, `DSYNC` | previous lines + `repoSize,reclaimedSize,ts,gen,fullGen,nodes,rootId` |
| `journal.log`, `manifest`, `repo.lock` | — | untouched by cleanup itself | unchanged |

---

## 11. AEM safety invariants (checklist for the Rust implementation)

1. **Never delete by reachability of data segments.** Retention of data
   segments is decided *only* by the generation predicates of §5 applied to
   the `(generation, fullGeneration, isCompacted)` triple stored in each tar
   index entry. Do not "improve" this with a record-level reachability walk —
   Oak's non-compacted writers share generations with retained states.
2. **Head and checkpoint safety follows from the predicate**: with
   `retainedGenerations ≥ 2` and a reference generation equal to (or newer
   than) the persisted journal head's generation, no segment reachable from
   the head super-root (which includes all checkpoints) is reclaimable.
   Verify before deleting: the journal head's segment generation must satisfy
   `¬reclaim(headGen)`.
3. **Bulk segments** may be removed only if unreachable through the union of
   (a) graph edges from every *kept* data segment across **all** tar files and
   (b) any externally supplied seed set. Visit tars newest-index→oldest and
   entries within a tar in reverse file order; propagate references through
   the shared set exactly as §3.2, following only edges whose target UUID has
   type nibble ≠ `0xA`. An **empty seed set is only safe on a quiescent
   store**: with concurrent writers, bulk segments referenced solely from
   post-snapshot segments (§3.1) have no graph edge in any participating tar
   and would be wrongly reclaimed.
4. **Dangling-future rule ordering**: if implementing §3.3's compacted-root
   rule, the global reverse-chronological visit order is mandatory; getting it
   wrong deletes live compacted segments. Passing `RecordId.NULL` as
   compacted root (rule disabled) is always safe.
5. **Rewrite before delete, validate before swap**: write
   `dataNNNNN(g+1).tar` completely (segments in original order, filtered
   `.gph`/`.brf`, rebuilt `.idx`, two zero-block trailer), close it, re-open
   it index-first; only then stop using — and only afterwards delete — the
   `g` file. Never modify a tar in place. Never create letter beyond `'z'`.
6. **Copied entries must have byte-identical payloads**: same UUID, same
   payload bytes, same generation triple, same 512-byte zero padding. Do not
   renumber, reorder, or re-generation entries during sweep. The tar entry
   *header* is regenerated (name `uuid.crc32` identical, mtime fresh — §4.2);
   only the payload must be bit-exact.
7. **Graph/brf filtering only**: drop rows whose segment UUID was cleaned;
   keep cross-file edges; preserve `.brf` generation triples. Formats must be
   bit-exact per `tar-layer.md` §§5–6 (CRC32 over payload, footer magics
   `\n0G\n` / binary-refs magic, count/size fields).
8. **Rotate the writer first**: any open write file must be finalized (brf,
   gph, idx, trailer) before it participates in cleanup; never sweep a tar
   without a valid index.
9. **`gc.log`**: append exactly one comma-joined line
   (`repoSize,reclaimedSize,ts,generation,fullGeneration,nodes,rootIdString10`)
   with a durable (O_DSYNC-equivalent) append, only after a *successful*
   compaction, and skip if the previous entry has the identical generation
   triple. Never rewrite or truncate existing lines.
10. **Leave what Oak tolerates**: leftover higher-generation files with
    invalid indexes, un-reaped old-generation files, and `*.bak` files are all
    acceptable residue — Oak startup resolves them (§9). Do **not** delete
    `.bak` files, and do not let the file matcher — full-string
    `(data)((0|[1-9][0-9]*)[0-9]{4})([a-z])?.tar`, i.e. ≥5 digits, possibly
    more — pick them up (a `.bak` suffix fails the full match, and
    `listArchives` filters on the `.tar` suffix anyway).
11. **Deletion may fail benignly**: treat delete failures as retryable, never
    fatal; the store must remain consistent with the old file still present.
12. **All comparisons in 32-bit wrapping int arithmetic**: generation deltas
    (`h.g − s.g >= n`) and the 25% rule (`afterSize >= beforeSize * 3 / 4`)
    follow Java `int` semantics; sizes in the 25% rule count only
    `512 + len + pad(len)` per segment entry.
13. **Invalidate caches across the swap**: any in-process segment or id cache
    keyed on pre-cleanup files must be dropped before serving reads, so a
    reclaimed segment can never be returned from cache after its file is gone.
14. **Cancellation must be swap-atomic**: if interrupted, either the complete
    new reader set is installed or none of the originals are deleted;
    partially produced `(g+1)` files without the swap are the only permitted
    residue (they retroactively win generation selection at next startup —
    §9 — which is safe only because every sweep output preserves all
    non-reclaimable segments; never emit a `(g+1)` file that omits a segment
    the mark phase kept).
15. **(Added) Make the new generation durable before deleting the old**: Java
    Oak never fsyncs a swept `(g+1)` file or the directory before the reaper
    unlinks `(g)` (§4.2), leaving a power-failure window that can lose live
    segments. The port must fsync the `(g+1)` file and its directory after
    the validating re-open and before deleting the `(g)` file (and likewise
    make the rotated writer file durable before it can be reaped). This is
    intentionally stricter than Java.
16. **(Added) The dangling-future (compacted-root) rule may only be armed
    with a root that is committed**: pass the compacted root of a
    *successful or partially successful* compaction of this run, or the root
    of the last `gc.log` entry (pre-compaction variant); for aborted or
    unknown states pass `RecordId.NULL` so the rule stays off (§5.4). Arming
    it with a stale or wrong root deletes live compacted segments.
