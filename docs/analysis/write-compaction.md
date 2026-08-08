# Write Path: Compaction (Full, Tail, Offline Tool)

Byte-exact and behavior-exact specification of the *compaction* subsystem of
`oak-segment-tar`, for a Rust port that must produce a store AEM can start against.
All citations are relative to
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/`.

This document builds on the reader-side specifications in this directory and does not
repeat them:

* `tar-layer.md` — TAR archive byte format, index, graph, binary references.
* `segment-layer.md` — segment header (which persists the three GCGeneration
  components per segment), segment info string, writer ids.
* `record-layer.md` / `node-layer.md` — record serialization, node templates,
  **stable IDs** (node-layer §2.2) which compaction must preserve.
* `filestore-layer.md` — directory layout, `journal.log` line format and flush
  protocol (`TarRevisions.doFlush`), `gc.log` line format, `repo.lock`, manifest,
  TAR file naming/generation letters, cleanup's TAR sweep ("a"→"b" rewrite),
  store opening.

New here: GCGeneration arithmetic, the compactor algorithms (Classic, Checkpoint,
Parallel), the retry/force loop, cleanup reclaim predicates, and the offline
`compact` tool's end-to-end file protocol.

---

## 1. GCGeneration arithmetic

Source: `spi/persistence/GCGeneration.java`.

A `GCGeneration` is a triple `(generation: i32, fullGeneration: i32, isCompacted: bool)`.
It is persisted **per segment** in the segment header and mirrored in every TAR index
entry (see `segment-layer.md` / `tar-layer.md` for the byte positions).

```
NULL          = (0, 0, false)                        // GCGeneration.NULL
nextFull()    = (generation + 1, fullGeneration + 1, true)   // lines 116-118
nextTail()    = (generation + 1, fullGeneration,     true)   // lines 125-127
nextPartial() = (generation,     fullGeneration,     true)   // lines 134-136
nonGC()       = (generation,     fullGeneration,     false)  // lines 143-145
```

* Arithmetic is plain Java `int` addition (silent two's-complement wrap at
  2^31−1; unreachable in practice but the Rust port should use `wrapping_add`
  on `i32` for bit-fidelity).
* `compareWith(other) = this.generation - other.generation` (int subtraction,
  line 152-154). `compareFullGenerationWith` analogous on `fullGeneration`.
* `equals` compares **all three** components (lines 166-178). This matters for
  `GCJournal.persist`'s NOOP check and `GCIncrement.isFullyCompacted`.
* `newGCGeneration(g, fg, c)` interns instances via a weak set (lines 62-81) —
  a pure memory optimization, no behavioral requirement.

Semantics (class javadoc, lines 30-54):

* `generation` increments on **every** GC cycle regardless of type.
* `fullGeneration` increments **only** on full GC.
* `isCompacted` is set **only** on segments written by a compaction operation.
  Segments written by normal repository writes carry the head's generation pair
  with the compacted flag **cleared**: the FileStore's normal ("sys") writer is
  built `withGeneration(() -> getGcGeneration().nonGC())`
  (`file/FileStore.java` lines 146-151), where `getGcGeneration()` is the
  generation of the segment containing the current head record
  (`FileStore.getGcGeneration`, lines 277-280). This supplier is evaluated
  dynamically, so after a successful compaction all subsequent normal writes
  automatically move to the new `(g, fg, false)` generation.

### 1.1 Which transitions each operation applies

Source: `file/FullCompactionStrategy.java`, `file/TailCompactionStrategy.java`
(both extend `file/AbstractCompactionStrategy.java`), `file/GCIncrement.java`.

For a base generation `B` = generation of the segment containing the current head
(`AbstractCompactionStrategy.getGcGeneration`, line 80-82):

| Operation | `partialGeneration` | `targetGeneration` |
|---|---|---|
| FULL compaction | `B.nextPartial()` | `B.nextFull()` |
| TAIL compaction | `B.nextPartial()` | `B.nextTail()` |

(`FullCompactionStrategy` lines 36-43; `TailCompactionStrategy` lines 38-45.)

`GCIncrement` holds `(base, partial, target)` and builds **two** segment writers:
one at `partial`, one at `target` (`GCIncrement.createPartialWriter/createTargetWriter`,
lines 41-47). Fully compacted nodes are written with the target writer, partial
(soft-cancelled) intermediate states with the partial writer
(`file/CompactionWriter.java` lines 73-87). In the default server flow and in the
offline tool the soft canceller is a never-firing `Canceller.newCanceller()`
(`file/AbstractGarbageCollectionStrategy.java` lines 238-246), so **only the target
writer produces segments**; the partial generation `(g, fg, true)` with unchanged
numbers exists only for the incremental-compaction feature.

```
GCIncrement.isFullyCompacted(gen) :=
    gen.compareWith(base) > 0  &&  gen.equals(target)      // GCIncrement lines 54-56
```

The `> 0` guard exists because "compaction may be used to copy a repository to the
same generation as before" (comment, lines 49-53).

The compaction writers are created by `SegmentWriterFactory` closure in
`FileStore` (lines 201-206): `defaultSegmentWriterBuilder("c")` — writer id
prefix `"c"` (visible in the segment info string, see `segment-layer.md`) — with a
THREAD_SPECIFIC buffer-writer pool and the "COMPACT" cache tracker.

---

## 2. Strategy plumbing (who calls what)

* `FileStore.compactFull()/compactTail()` (`file/FileStore.java` lines 403-419) →
  `GarbageCollector.compactFull/compactTail` (`file/GarbageCollector.java`
  lines 303-319) → `SynchronizedGarbageCollectionStrategy` (a mutex wrapper,
  `file/SynchronizedGarbageCollectionStrategy.java`) around
  `CleanupFirstGarbageCollectionStrategy` (default) or
  `DefaultGarbageCollectionStrategy` if system property `gc.classic` is set
  (`FileStore.newGarbageCollectionStrategy`, lines 85-90).
* Both strategies inherit `compactFull/compactTail` from
  `AbstractGarbageCollectionStrategy` (lines 77-84), which invoke the compaction
  strategy **directly** — no estimation, no GC backoff, no memory barrier, and no
  pre-compaction cleanup. (The `CleanupFirstCompactionStrategy` wrapper and
  estimation only apply to the scheduled-GC path `run(...)`, lines 97-149 and
  `CleanupFirstGarbageCollectionStrategy.run`, lines 52-54.)
* Full strategy: `FullCompactionStrategy`. Tail strategy:
  `FallbackCompactionStrategy(new TailCompactionStrategy(), new FullCompactionStrategy())`
  (`AbstractGarbageCollectionStrategy` has them per subclass;
  `DefaultGarbageCollectionStrategy` lines 35-42,
  `CleanupFirstGarbageCollectionStrategy` lines 37-44): if tail compaction returns
  `notApplicable`, **full compaction runs instead**
  (`file/FallbackCompactionStrategy.java` lines 36-44).
* `GarbageCollector.compactFull/compactTail` store the `CompactionResult` when
  `result.requiresGCJournalEntry()` (true only for `succeeded`,
  `file/CompactionResult.java` lines 79-81) into `lastCompactionResult`
  (lines 303-319); a subsequent standalone `FileStore.cleanup()` consumes it so the
  `gc.log` entry gets written (lines 321-330 with comment lines 116-123).

### 2.1 Tail compaction prerequisites and fallback triggers

Source: `file/TailCompactionStrategy.java`.

The tail base state is the **root record id of the last `gc.log` entry**:
`context.getGCJournal().read().getRoot()` parsed via
`RecordId.fromString(tracker, root)` (lines 85-95). Tail compaction is
`notApplicable` (→ falls back to FULL under `FallbackCompactionStrategy`) when:

1. the parsed id equals `RecordId.NULL` (empty/absent `gc.log`, or entry written
   with a NULL root) — lines 62-64; or
2. reading the base node fails with `SegmentNotFoundException`. The read is forced
   by calling `node.getPropertyCount()` (lines 68-83, with explanatory comment:
   accessing content forces a segment read; success proves only the root node is
   accessible).

Consequence for the port: **tail compaction requires a `gc.log` whose last line
carries a resolvable root record id** (the compacted root of the previous
successful compaction+cleanup). Full compaction has no prerequisite; its base
state for the diff is `EMPTY_NODE` (`FullCompactionStrategy.compact`, lines 46-48).

---

## 3. The compactors

`AbstractCompactionStrategy.newCompactor` (lines 296-323) chooses by
`SegmentGCOptions.getCompactorType()`:

* `PARALLEL_COMPACTOR` (default, `compaction/SegmentGCOptions.java` line 170) →
  `CheckpointCompactor(gcListener, new ParallelCompactor(...concurrency...))`
* `CHECKPOINT_COMPACTOR` ("diff") → `CheckpointCompactor(gcListener, new ClassicCompactor(...))`
* `CLASSIC_COMPACTOR` ("classic") → bare `ClassicCompactor`
* System property `oak.compaction.legacy=true` substitutes deprecated
  `LegacyCheckpointCompactor` variants (not specified here).

All compactors implement `Compactor.compact(before, after, onto, canceller)`:
"compact the differences between `after` and `before` on top of `onto`"
(`Compactor.java` lines 136-150). Derived modes (lines 85-134):

* `compactUp(before, after, c)` = `compact(before, after, before, c)` — full rewrite,
  all-or-nothing.
* `compactDown(before, after, hard, soft)` = diff applied on top of `after` itself —
  supports partial (soft-cancelled) results; partial states mix generations but
  keep stable ids (class javadoc lines 37-67).

### 3.1 ClassicCompactor (reference algorithm)

Source: `ClassicCompactor.java`.

Constants: `UPDATE_LIMIT = Integer.getInteger("compaction.update.limit", 10000)`
(lines 64-65).

```
internalCompact(before, after, onto, hard, soft):
    cached = writer.getPreviouslyCompactedState(after)     # §3.4 dedup
    if cached != null: return cached
    return CompactDiff(onto, hard, soft).diff(before, after)   # lines 130-141

CompactDiff state: builder = MemoryNodeBuilder(onto); modifiedProperties = [];
                   modCount = 0                                # lines 161-183

diff(before, after):
    success = after.compareAgainstBaseState(before, CancelableDiff(this,
                  () -> soft.cancelled || hard.cancelled))     # lines 185-191
    if exception: throw IOException(exception)
    if success:
        # property compaction is DELAYED to the end (cancellation safety)
        for p in modifiedProperties: builder.setProperty(compact(p))   # line 196
        return writeNodeState(builder.getNodeState(),
                              stableIdBytes(after), complete=true)     # line 199
    else if hard.cancelled: return null                       # lines 200-201
    else: return writeNodeState(builder.getNodeState(),
                                stableIdBytes(after), complete=false)  # line 203
```

Diff callbacks (lines 207-262):

* `propertyAdded/propertyChanged(after)` → collect `after` into
  `modifiedProperties`; `propertyDeleted` → `builder.removeProperty(name)` immediately.
* `childNodeAdded(name, after)` = `childNodeUpdated(name, EMPTY_NODE, after)`;
  `childNodeChanged` = `childNodeUpdated(name, before, after)`.
* `childNodeUpdated` (lines 225-240): recursion with
  `onto = base.getChildNode(name)` if it exists else `EMPTY_NODE`; on non-null
  result: `updated()`, `builder.setChildNode(name, compacted)`; return
  `compacted.isComplete()` (returning false stops the parent diff → cascades a
  partial state upwards).
* `childNodeDeleted` → `updated(); builder.getChildNode(name).remove()`.
* `updated()` (lines 170-176): every `UPDATE_LIMIT` modifications, write the
  builder's current state as a **fully compacted node with `stableId = null`**
  and rebase the builder onto it — this bounds memory and incidentally flushes
  intermediate records to the target generation. These intermediates have
  implicit (self) stable ids; only the final node per subtree gets the original
  stable id.

Property/value rewriting `compact(property)` (lines 265-282):

* `BINARY` → `binaryProperty(name, property.getValue(BINARY))`;
  `BINARIES` → list of blobs re-wrapped. **Blob record content is value-shared**:
  the class javadoc (lines 51-56) states binaries stored in bulk segments keep
  sharing the bulk segments, but the *list records* are rewritten. Concretely,
  when the SegmentWriter writes a `SegmentBlob` of the same store it re-writes
  the blob record structure but reuses bulk segment ids (see the segment-writer
  spec / `DefaultSegmentWriter` blob path).
* Everything else → `createProperty(name, property.getValue(type), type)` —
  i.e. values are materialized and re-serialized from scratch (no record-id reuse
  across generations for value records).

`writeNodeState` (lines 143-155): complete → `writer.writeFullyCompactedNode(node,
stableIdBytes)` + `compactionMonitor.onNode()`; else
`writer.writePartiallyCompactedNode(...)`. The stable-id buffer passed is
`CompactorUtils.getStableIdBytes(after)` — the *original* node's stable id, so the
compacted node record persists the same 20-byte stable id (byte format in
`node-layer.md` §2.2). **This is required, not an optimization**: `SegmentNodeState
equality and the `RecordCache`-based deduplication in later runs key on stable ids.

`CancelableDiff` (`CancelableDiff.java`): every callback first checks the supplier;
if cancelled it returns `false`, aborting `compareAgainstBaseState` → `diff` sees
`success == false` and consults the hard canceller to decide null vs partial.

### 3.2 CheckpointCompactor

Source: `CheckpointCompactor.java`. Wraps a `ClassicCompactor` (or
`ParallelCompactor`) and understands the **super-root** layout: the head node state
whose children are `root` (`SegmentNodeStore.ROOT`) and `checkpoints`
(`SegmentNodeStore.CHECKPOINTS`), each checkpoint being
`checkpoints/<name>/{root, properties, @created,...}`.

`doCompact(before, after, onto, hard, soft)` (lines 119-167), where `soft` must be
null unless `after.equals(onto)` (line 126):

1. `superRoots = collectSuperRootPaths(before, after)` (lines 235-266): diff
   `after.checkpoints` against `before.checkpoints`, collect **added** checkpoint
   names; sort ascending by the checkpoint node's long property `"created"`
   (constant `CREATED = "created"`, line 68); result is the ordered set
   `{"checkpoints/<name1>", ..., ""}` — checkpoints chronologically, then the
   root (empty path) **last**.
2. `stableIdBytes = requireNonNull(getStableIdBytes(after))` — the super-root's
   stable id (after must be a SegmentNodeState).
3. `rootBuilder = onto.builder()`.
4. Deleted checkpoints: diff of `checkpoints` collecting `childNodeDeleted`
   names (lines 214-228); each is removed from
   `rootBuilder.checkpoints.<name>` (lines 134-137).
5. For each `path` in `superRoots` (lines 140-164):
   * `afterSuperRoot = descendant(after, path)`; its `root` child is the state to
     compact (`getRoot`, lines 268-270; missing → `EMPTY_NODE`).
   * `baseRoot = compacted != null ? compacted : getRoot(before)`;
     `ontoRoot  = compacted != null ? compacted : getRoot(onto)` — i.e. each
     checkpoint/root is rebased **on top of the previously compacted one**,
     minimizing deltas (class javadoc lines 58-64).
   * `compacted = compactRootState(baseRoot, getRoot(afterSuperRoot), ontoRoot, hard, soft)`
     (lines 169-177): soft canceller is forwarded only on the first iteration
     while `ontoRoot.equals(afterRoot)`; later iterations pass `soft = null`.
     Internally `compactWithCache` (lines 195-212) consults `cpCache`
     (a `HashMap<NodeState, CompactedNodeState>`) keyed by the *uncompacted*
     state; on miss delegates to `compactor.compact(before, after, onto, hard, soft)`
     and caches complete results. (The cache makes identical checkpoint states
     compact to the *same* record — an output-shaping optimization that is
     structurally optional but keeps size down; equality is
     `SegmentNodeState.equals`, which compares stable ids fast-path.)
   * `null` → hard cancelled → return null (lines 147-150).
   * `builder = descendant(rootBuilder, path)` (creating children as needed);
     `builder.setChildNode("root", compacted)`.
   * If `path` starts with `"checkpoints/"`: `compactCheckpointMetadata(builder,
     afterSuperRoot)` (lines 179-189) — sets child node `"properties"` fresh
     (`setChildNode("properties")` = replace with empty, then copy each property
     through `compactor.compact(property)`), and copies every property of the
     checkpoint node itself (e.g. `created`, `timestamp`) through
     `compactor.compact(property)`. **This preserves checkpoint metadata in the
     compacted super-root.**
   * Soft-cancelled → break out of the loop (lines 161-163).
6. Final write (line 166):
   `compactor.writeNodeState(rootBuilder.getNodeState(), stableIdBytes,
   complete = !softCancelled)` — the assembled super-root is written as one node
   with the original super-root's stable id.

### 3.3 ParallelCompactor

Source: `ParallelCompactor.java`. Extends `ClassicCompactor`; only changes
*scheduling*, not bytes: it explores the diff breadth-first
(`EXPLORATION_LOWER_LIMIT = 10_000`, `EXPLORATION_UPPER_LIMIT = 100_000`,
lines 64-69) building a `CompactionTree` of modified children, then compacts
subtrees on `numWorkers = max(0, nThreads-1)` executor threads via
`ClassicCompactor.compact` (line 210-211), merging results bottom-up in
`CompactionTree.compact()` (lines 232-285): children set into a
`MemoryNodeBuilder(onto)`, then removed children, then modified properties
(compacted via `compact(property)`), then removed property names, final
`writeNodeState(builder.getNodeState(), stableIdBytes(after), true)`.
With `concurrency = 1` (offline tool default) `initializeExecutor()` returns
false and it degrades to plain `ClassicCompactor.compact`
(lines 368-386). A Rust port may implement only the sequential path; the
parallel path changes segment/record placement (hence record ids) but not
logical content, which is the only thing Oak requires (§6).

### 3.4 Deduplication semantics — required vs optional

* **Required for correctness**: `CompactionWriter.getPreviouslyCompactedState`
  (`file/CompactionWriter.java` lines 94-104) — returns the node itself as
  "already compacted" iff it is a `SegmentNodeState` whose generation satisfies
  `gcIncrement.isFullyCompacted` (strictly newer than base **and** equal to
  target, including the compacted flag). This is what makes retry cycles
  (§4) reuse work already committed to the target generation, and it is *safe*
  precisely because such records are already in target-generation segments.
  A port that never reuses (always rewrites) is also structurally correct but
  re-copies everything each retry cycle.
* **Optional (affects record ids, not correctness)**: the SegmentWriter's
  deduplication caches (node/string/template caches, keyed by stable id for
  nodes — see the segment-writer spec), `cpCache` in CheckpointCompactor, and
  the `UPDATE_LIMIT` intermediate flushes. Changing these alters which record
  ids appear and total size, but the resulting node *content*, stable ids, and
  generation stamps must be identical in meaning.
* **Required**: passing the original stable-id bytes for every node that
  replaces an existing node (`CompactorUtils.getStableIdBytes`,
  `CompactorUtils.java` lines 27-33); for nodes built from memory states
  (intermediate `updated()` flushes) `null` is passed and the stable id becomes
  implicit/self (node-layer §2.2).

---

## 4. The retry / force loop

Source: `AbstractCompactionStrategy.compact(Context, NodeState base)`
(lines 134-294). Pseudocode of the exact control flow:

```
B      = generation(head segment)              # getGcGeneration
P      = partialGeneration(B); T = targetGeneration(B)
incr   = GCIncrement(B, P, T)
gcEntry = gcJournal.read()                     # previous repoSize/nodes for ETA
writer = CompactionWriter(reader, blobStore, incr, writerFactory)
monitor.init(gcEntry.repoSize, gcEntry.nodes, tarFiles.size())
compactor = newCompactor(...)
retryCount = max(0, gcOptions.retryCount)      # default 5 (SegmentGCOptions.RETRY_COUNT_DEFAULT)
flusher = { writer.flush(); context.flusher.flush() }   # lines 173-176

compacted = null
do:                                            # outer loop, lines 178-232
    head  = readHeadState()
    after = (compacted == null) ? head : compacted
    if stateSaveTrigger cancelable:  compacted = compactor.compactDown(base, after, hard, saveState)
    elif softCanceller cancelable:   compacted = compactor.compactDown(base, after, hard, soft)
    else:                            compacted = compactor.compactUp(base, after, hard)
    if compacted == null: return aborted(T)    # hard cancel, lines 195-199

    cycles = 0
    while !(success = setHead(head, compacted)) && cycles < retryCount:   # line 206
        cycles++                                # concurrent commits happened
        newHead   = readHeadState()
        compacted = compactor.compact(head, newHead, compacted, hard)     # line 217
        if compacted == null: return aborted(T)
        head = newHead
    if success: flusher.flush()                 # lines 229-231
while success && !compacted.isComplete() && !soft.cancelled

if !success:                                    # lines 234-267
    if forceTimeout > 0:                        # default 60 s (FORCE_TIMEOUT_DEFAULT)
        cycles++
        fc = hard.withTimeout(forceTimeout s)
        compacted = forceCompact(context, head, compacted, compactor, fc)
        if compacted != null: success = true; flusher.flush()

if success:
    onSuccessfulCompaction(type); monitor.finished()
    if compacted.isComplete(): return succeeded(type, T, opts, compacted.recordId)
    else:                      return partiallySucceeded(P, compacted.recordId)
else: return aborted(T)
```

Key mechanics:

* `setHead(previous, head)` is a compare-and-swap on the in-memory head:
  `revisions.setHead(previous.getRecordId(), compacted.getRecordId(), EXPEDITE_OPTION)`
  (line 130-132). `TarRevisions.setHead(expected, head, options)`
  (`file/TarRevisions.java` lines 264-284) takes the **write** lock of a fair
  `ReentrantReadWriteLock` when EXPEDITE is given (normal commits take the read
  lock), then `head.get().equals(expected) && head.compareAndSet(...)`. Nothing
  is written to disk by `setHead` itself; the journal line appears only at the
  next flush.
* `forceCompact` (lines 92-117) uses
  `revisions.setHead(Function, timeout(forceTimeout, SECONDS))`
  (`TarRevisions` lines 299-321): it tries to acquire the write lock for up to
  the timeout — **blocking all commits** — reads the current head inside the
  lock, runs `compactor.compact(head /*base*/, currentHead, compacted /*onto*/,
  fc)`, and installs the result. Function returning null (cancelled/IO error)
  leaves the head unchanged.
* **Failure/rollback semantics**: there is no on-disk rollback. On abort the head
  still points at the uncompacted state; segments already written at generation
  `T` (compacted flag set) remain in the TAR files. They are reclaimed by the
  next cleanup via `DefaultCleanupContext.isDanglingFutureSegment`
  (`file/DefaultCleanupContext.java` lines 72-82): walking TAR entries in
  reverse order, every `isCompacted` segment encountered *before* the segment
  containing the last compacted root is reclaimable. `CompactionResult.aborted`
  itself uses an empty reclaimer (`CompactionResult.java` lines 120-136) — an
  aborted GC run's own cleanup deletes nothing generational.
* After success the `SuccessfulCompactionListener` records the compaction type
  (FULL/TAIL) which selects the reclaim predicate of *later* standalone cleanups
  (`GarbageCollector` lines 108-114, 246-248: defaults to FULL, "conservative
  and safe" after restart).
* Durability order on success: `writer.flush()` (pushes all pending compaction
  segments into the open TAR writer; no fsync yet) **then**
  `context.getFlusher().flush()` = `FileStore.flush` → `revisions.flush(...)`
  which runs `segmentWriter.flush(); tarFiles.flush(); stats.flushed()` —
  `tarFiles.flush()` is what fsyncs the TAR data — **before**
  appending the journal line `"<recordid.toString10()> root <currentTimeMillis>"`
  + `\n` + `FileChannel.force(false)` (`FileStore.doFlush` lines 333-343,
  `TarRevisions.doFlush` lines 224-239, `LocalJournalFile.writeLine`).
  `TarRevisions.doFlush` skips everything (including the journal append) when
  the in-memory head equals the last persisted head. So all segment bytes are
  durable strictly before the journal references them.
  Crash before the journal write → journal still points at the pre-compaction
  head; compacted segments are dangling-future and reclaimed later. This
  ordering is the crash-safety contract.

`CompactionResult` variants (`file/CompactionResult.java`):

| Variant | success | reclaimer | gc.log entry |
|---|---|---|---|
| `succeeded(type, T, opts, rootId, n)` | yes | `newOldReclaimer(type, T, retainedGenerations)` | yes (`requiresGCJournalEntry`) |
| `partiallySucceeded(P, rootId, n)` | yes | none (`false`) | no |
| `aborted(currentGen, n)` | no | empty | no |
| `skipped(lastType, curGen, opts, rootId, n)` | yes | `newOldReclaimer(lastType, curGen, retained)` | no |
| `notApplicable(n)` | no | none; `isNotApplicable()=true` (triggers fallback) | no |

---

## 5. Cleanup (what reclaim actually deletes)

Source: `file/DefaultCleanupStrategy.java`, `file/Reclaimers.java`,
`file/DefaultCleanupContext.java`. (TAR sweep byte mechanics — "a"→"b" rewrite,
`.bak` naming, index rewrite — are in `tar-layer.md`/`filestore-layer.md`.)

`DefaultCleanupStrategy.cleanup` sequence (lines 36-76):

1. clear segment cache; `System.gc()` (weak-ref hygiene; port: no-op).
2. `tarFiles.cleanup(DefaultCleanupContext(tracker, reclaimer, compactedRootId))`.
3. `tracker.clearSegmentIdTables(reclaimedIds, reason)`.
4. `stats.reclaimed(reclaimedSize)`.
5. If the compaction result requires it, `gcJournal.persist(reclaimedSize,
   finalSize, generation(head segment), monitor.getCompactedNodes(),
   compactedRootId.toString10())` — appends the `gc.log` line (format in
   `filestore-layer.md`; fields joined with `","`:
   repoSize, reclaimedSize, currentTimeMillis, generation, fullGeneration,
   nodes, root — `GCJournal.GCJournalEntry.toString`, lines 152-161). NOOP if
   the last persisted entry has an `equals` gcGeneration
   (`GCJournal.persist`, lines 67-83; note parsed entries always have
   `isCompacted=false`, line 185).
6. Return removable file names (deleted later by `FileReaper`, i.e. actual
   `File.delete` happens asynchronously / on the next reap; the offline tool's
   store-close triggers it).

Reclaim predicates (`Reclaimers.newOldReclaimer`, lines 73-148), with reference
generation `R` = generation created by the compaction (or current head for
`skipped`) and `n = retainedGenerations` (default 2; **1 in offline mode**,
`SegmentGCOptions.setOffline` lines 335-339):

```
FULL:  reclaim(g)  :=  R.fullGeneration - g.fullGeneration >= n
                    || (R.generation - g.generation >= n  &&  !g.isCompacted)

TAIL:  reclaim(g)  :=  R.generation - g.generation >= n
                    && !(g.isCompacted && g.fullGeneration == R.fullGeneration)
```

Plus, independent of generation (`DefaultCleanupContext.shouldReclaim`,
lines 96-100): dangling future segments (§4), and **bulk segments** are reclaimed
purely by reachability (unreferenced ⇒ reclaimed; graph edges followed only into
non-data segments, `shouldFollow` lines 102-105; initial references = currently
in-memory referenced bulk segment ids, lines 88-94).

The pre-compaction cleanup of the default (`CleanupFirst`) scheduled-GC path uses
hard-coded variants assuming `n == 2` that additionally protect the yet-uncommitted
transient state (`file/CleanupFirstCompactionStrategy.java` lines 103-134). It does
not apply to `compactFull()/compactTail()` tool calls.

---

## 6. Offline compact tool — exact end-to-end sequence

Source: `tool/Compact.java` (oak-run `compact`).

Builder inputs: `path` (required), `mmap` (Boolean or null), `os` string,
`force` (bool), `gcLogInterval` default `150000`, `segmentCacheSize` default
`DEFAULT_SEGMENT_CACHE_MB` (256), `gcType` default FULL, `compactorType` default
`PARALLEL_COMPACTOR`, `concurrency` default 1 (lines 68-211).

Access mode (`newFileAccessMode`, lines 234-245): if `os` contains "windows"
(case-insensitive) → REGULAR_ENFORCED (memoryMapped=false) regardless of `mmap`;
else `mmap == null` → arch-dependent (builder default: mmap on 64-bit),
`true` → memory-mapped, `false` → regular.

Store opening (`newFileStore`, lines 367-380):

```
fileStoreBuilder(path.getAbsoluteFile())
    .withStrictVersionCheck(!force)          # force=true tolerates/upgrades older store format
    .withSegmentCacheSize(segmentCacheSize)
    .withGCOptions(defaultGCOptions()
        .setOffline()                        # offline=true, retainedGenerations := 1
        .setGCLogInterval(gcLogInterval)
        .setCompactorType(compactorType)
        .setConcurrency(concurrency))
    [.withMemoryMapping(m) if decided]
    .build()                                 # acquires repo.lock, manifest check, journal bind
                                             # (see filestore-layer.md §opening)
```

`strictVersionCheck = !force` (line 304): with `force=false` the manifest's store
version must match exactly or open fails (`ManifestChecker`, see
filestore-layer); `force=true` allows opening (and updating) an older-version
store.

`run()` (lines 311-365), exact behavior and printed output (all to `System.out`,
`printf`, `\n` endings):

```
Compacting <path> with <access mode description> and <compactor description> compactor type
    before
        <Date(lastModified)>, <fileName>          # one line per file in dir
    size <humanReadable> (<bytes> bytes)
    -> compacting
```

1. `store.compactFull()` or `store.compactTail()` per `gcType` (lines 323-330).
   Both are the §4 loop; in this offline store there are no concurrent commits,
   so cycle 0's `setHead` CAS succeeds immediately, followed by
   `writer.flush()` + `FileStore.flush()` (segments fsynced, then a
   `journal.log` line for the compacted head appended and forced).
2. On failure: print `Compaction cancelled after <elapsed> (<n>s).`, return 1.
   Any exception: stack trace to stderr, print
   `Compaction failed after <elapsed> (<n>s).`, return 1. (In both cases
   whatever was flushed remains on disk — harmless per §4 crash semantics.)
   Note `FileStore.compactFull/compactTail` swallow `IOException` and the
   strategy converts internal exceptions to `aborted` → `false`, so
   "cancelled" is printed for *any* compaction failure; "failed" only for
   exceptions escaping the try block (store open, cleanup, journal rewrite).
3. Print `    -> cleaning up`; `store.cleanup()` (line 337) — because
   `GarbageCollector.lastCompactionResult` was recorded by the successful
   compaction, this runs the §5 cleanup with the `succeeded` reclaimer
   (retainedGenerations = 1, so **every** pre-compaction generation is
   reclaimable) **and appends the `gc.log` entry** whose root is the compacted
   root — this is what enables a subsequent *tail* compaction.
   TAR files shrunk ≥25% are rewritten to the next generation letter; files whose
   entries are all reclaimed are deleted (via FileReaper) — see tar-layer.
4. **Journal rewrite** (lines 338-348):
   ```java
   JournalFile journal = new LocalJournalFile(path, "journal.log");
   try (JournalReader r = new JournalReader(journal)) {
       head = String.format("%s root %s\n", r.next().getRevision(), System.currentTimeMillis());
   }
   try (JournalFileWriter w = journal.openJournalWriter()) {
       System.out.printf("    -> writing new %s: %s\n", journal.getName(), head);
       w.truncate();          // RandomAccessFile.setLength(0)
       w.writeLine(head);     // writes head + "\n", then FileChannel.force(false)
   }
   ```
   * `JournalReader.next()` yields the **newest parseable** journal entry: the
     file is read backwards (`ReversedLinesFileReader`) and any line *not*
     containing a space character is skipped with a warning (this covers blank
     lines); the revision is the text before the first space, the timestamp the
     third space-separated token if present (`JournalReader.computeNext`,
     lines 53-81). **No validation against the store happens here** — the
     containsSegment check exists only in `FileStoreUtil.findPersistedRecordId`
     used at store open. In this tool the newest line was just written by the
     compaction flush, so its revision is the compacted head record id in
     `toString10()` form (`<segment-uuid>:<offset-decimal>`).
   * The written line is `"<revision> root <millisNow>\n"` **plus** the `"\n"`
     appended by `LocalJournalFileWriter.writeLine` (line 99-103 of
     `file/tar/LocalJournalFile.java`) — i.e. the final `journal.log` content is
     exactly `<revision> root <millis>\n\n` (trailing blank line). Oak's
     `JournalReader` skips blank lines, so this is tolerated and the port should
     reproduce it byte-for-byte or at minimum keep one valid line. The timestamp
     is *regenerated* (current time), not copied from the read entry.
   * After truncation the journal contains only this one head line: all older
     revisions become unreferenced.
5. Store close (try-with-resources): flushes, closes TARs, reaps removable
   files, releases `repo.lock`. The close-time flush does **not** append
   another journal line: `TarRevisions.doFlush` compares the in-memory head to
   `persistedHead` (already equal since the compaction flush) and returns
   early — it is unaware of the tool's out-of-band truncate/rewrite. A port
   that flushes unconditionally on close would emit a duplicate head line
   after the truncate (readers tolerate it, but it breaks byte-exactness).
6. Final printout:
   ```
       after
           <Date>, <fileName>...
       size <humanReadable> (<bytes> bytes)
       removed files [<names of beforeFiles \ afterFiles>]
       added files [<names of afterFiles \ beforeFiles>]
   Compaction succeeded in <elapsed> (<n>s).
   ```
   Return 0.

Note: `Compact` never calls the estimation phase, never uses
`CleanupFirstCompactionStrategy` (§2), and does not create `.bak` files itself
(those come from TAR recovery on open, if any).

### 6.1 oak-run CLI wiring (ADDED by verifier — needed for tool parity)

Source: `oak-run/.../run/CompactCommand.java`.

* Usage: `compact <path> [--force] [--tail] [--mmap [true|false]]
  [--compactor classic|diff|parallel] [--threads N]`. Missing path → usage text
  to stderr + `System.exit(-1)`; otherwise `System.exit(Compact.run())`
  (0 success, 1 failure).
* `--tail` selects `GCType.TAIL`; `--compactor` maps descriptions
  `"classic"→CLASSIC_COMPACTOR`, `"diff"→CHECKPOINT_COMPACTOR`,
  `"parallel"→PARALLEL_COMPACTOR` (`CompactorType.fromDescription`); any other
  string throws `IllegalArgumentException("Unrecognized compactor type ...")`.
* `--threads` defaults to 1 (becomes `concurrency`).
* Segment cache MB comes from system property `cache` (default 256);
  GC log interval from system property `compaction-progress-log`
  (default 150000). `os` is `System.getProperty("os.name")`; `mmap` is the
  optional-arg Boolean (absent → null → arch-dependent).
* Paths starting with `az:`/`aws:` route to the remote variants (out of scope).

---

## 7. Error and cancellation behavior summary

| Event | On-disk effect | Oak tolerance at next startup |
|---|---|---|
| Hard cancel / exception during compaction | Target-generation segments (isCompacted=true) already flushed remain; journal unchanged (or points at last pre-compaction flush) | Head resolves to old generation; dangling future segments ignored by readers, reclaimed by next cleanup (`DefaultCleanupContext.isDanglingFutureSegment`) |
| Crash after `setHead` CAS but before flush | Nothing: head was memory-only | Same as above |
| Crash after segment flush, before journal line | Compacted segments durable, journal points at old head | Same as above |
| Crash after journal line | New head durable; old generations still present | Fully consistent; old generations reclaimed by future cleanup |
| Crash during tool's journal rewrite (after truncate, before writeLine) | Empty `journal.log` | **Dangerous**: an empty journal makes `TarRevisions.bind` write a fresh initial (empty) node — repository content appears lost. The rewrite window is a single buffered write+force; the Rust port should instead write the new journal atomically (write temp + rename) or at least keep a backup, but must end with byte-identical content |
| Cleanup crash mid TAR-sweep | Both `dataNNNNa.tar` and rewritten `dataNNNNb.tar` may exist | Open picks the highest-letter valid archive per index (filestore-layer §6); stale lower-letter files are ignored/removed later |
| `partiallySucceeded` (soft cancel, incremental) | Head is a partial state at generation `(g, fg, true)` (numbers unchanged); no gc.log entry | Valid store; mixed-generation tree with stable ids intact; a later compaction completes the work |

---

## 8. AEM safety invariants (checklist for the Rust implementation)

1. **Lock**: hold the `repo.lock` OS file lock for the whole operation; AEM must
   be stopped (the lock guarantees it).
2. **Generation stamping**: every segment written by compaction carries exactly
   `target = base.nextFull()` (full) or `base.nextTail()` (tail) — generation
   `+1`, fullGeneration `+1`/unchanged, `isCompacted = true` — in both the
   segment header and every TAR index entry, where `base` is the GCGeneration of
   the segment containing the head record id resolved from `journal.log`.
   Never write compaction output with `isCompacted = false`, and never bump
   `fullGeneration` for tail.
3. **Stable IDs**: every compacted node that corresponds to an existing node
   must persist that node's original stable-id bytes; the compacted super-root
   must carry the super-root's stable id. Content equality in Oak relies on it.
4. **Super-root shape**: the new head must contain `root` plus a `checkpoints`
   subtree in which every surviving checkpoint keeps its `root` child, its
   `properties` child node, and all its properties (including `created`), with
   values rewritten into the new generation. Checkpoints deleted between base
   and head must not reappear.
5. **Logical equality**: the compacted head must be node-for-node,
   property-for-property equal to the source head (binary values may share bulk
   segments; all other records are rewritten). Record ids may differ freely;
   dedup caches are optimizations only.
6. **Durability order**: (a) finish and fsync all segment/TAR data (including
   index/graph/binary-ref rewrite per tar-layer), (b) only then append the
   journal head line (`<segment-uuid>:<offset-decimal> root <millis>` + `\n` —
   the `RecordId.toString10()` form, UUID via `new UUID(msb, lsb).toString()`,
   offset in decimal — then `FileChannel.force(false)`), (c) only then run
   cleanup's TAR sweep/deletions (see invariant 9), (d) only then
   truncate/rewrite the journal (offline tool) — and make the truncate-write
   window atomic or backed up, because Oak treats an empty journal as an empty
   repository.
7. **Journal final state** (offline compact): exactly one valid line whose
   revision is the compacted head in `toString10()` form (see
   filestore-layer §4 for the record-id text format), Oak-compatible trailing
   `\n\n` tolerated/expected.
8. **gc.log**: after successful compaction + cleanup, append (never rewrite) a
   line `repoSize,reclaimedSize,timestampMillis,generation,fullGeneration,
   compactedNodeCount,rootRecordId10` matching the new generation (the
   generation/fullGeneration written are those of the *head segment* after
   compaction, i.e. the target pair); without it a later `compact --tail` (by
   Oak or by us) silently degrades to full. The `GCJournal.persist` NOOP check
   compares full `GCGeneration.equals` (all three components) against the
   cached latest entry; entries *parsed from disk* always carry
   `isCompacted=false` while the post-compaction head generation has
   `isCompacted=true`, so against a disk-read entry the check never suppresses
   the append — it only fires within the same process run (e.g. a second
   cleanup after one compaction). Port rule: append exactly once per
   successful compaction+cleanup.
9. **Cleanup safety**: reclaim only segments matching the §5 predicates; treat
   bulk segments by reachability only; walk TAR entries newest→oldest when
   applying the dangling-future rule and stop it at the compacted root's
   segment (the root's own segment must never be treated as dangling; with a
   NULL compacted root the dangling-future rule is disabled entirely). Never
   delete a TAR file before a fully-written replacement (next-letter) with a
   valid index exists, or unless all its entries are reclaimable. **Ordering**:
   run cleanup only after the journal head line referencing the compacted root
   is durable (Java guarantees this because the §4 flusher appends the journal
   line before `compact()` returns and cleanup runs after) — deleting
   old-generation segments while the durable journal still points at the old
   head would corrupt the store on crash.
10. **Version check**: verify the `manifest` store version (strict unless the
    user forces); never downgrade the manifest.
11. **Failure posture**: on any error, leave existing files untouched except
    additions (extra segments of the new generation are safe); never truncate
    the journal unless compaction and cleanup fully succeeded.
12. **Post-conditions Oak relies on**: head record id resolvable from
    `journal.log`; every segment reachable from the head (and from every
    checkpoint) present in some `.tar` with a valid index; the head segment's
    GCGeneration is the maximum generation present except dangling-future
    segments (none, after a clean run); normal AEM writes will then proceed at
    `head.generation.nonGC()` automatically.
