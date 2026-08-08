# Write Path: Node-Store Commit and Checkpoints

Byte-exact and behavior-exact specification of the `SegmentNodeStore` commit path and
checkpoint operations for a **single-process, single-writer** Rust port that must leave a
repository in a state a subsequent AEM start can safely consume.

Sources analyzed (all paths relative to the Oak checkout at
`oak/` = `apache/jackrabbit-oak`, module paths as in the repo):

| File | Role |
|---|---|
| `oak-segment-tar/.../segment/SegmentNodeStore.java` | NodeStore facade: merge/rebase/reset/checkpoint API |
| `oak-segment-tar/.../segment/scheduler/LockBasedScheduler.java` | Commit serialization, checkpoint create/remove (`CPCreator`) |
| `oak-segment-tar/.../segment/scheduler/Commit.java` | Rebase-and-apply of one commit |
| `oak-segment-tar/.../segment/SegmentNodeBuilder.java` | `UPDATE_LIMIT` flush-to-segment builder |
| `oak-segment-tar/.../segment/SegmentNodeState.java` | `fastEquals`, `getStableId`, `compareAgainstBaseState`, `builder()` |
| `oak-segment-tar/.../segment/DefaultSegmentWriter.java` | `writeNode` incremental serialization and deduplication |
| `oak-segment-tar/.../segment/RecordWriters.java` | `NodeStateWriter` stable-id slot |
| `oak-segment-tar/.../segment/file/TarRevisions.java` | `setHead` CAS + journal flush (`doFlush`) |
| `oak-segment-tar/.../segment/file/FileStore.java` | `doFlush` ordering, background flush, initial node |
| `oak-segment-tar/.../segment/file/tar/LocalJournalFile.java` | Journal append + fsync |
| `oak-store-spi/.../plugins/memory/MemoryNodeBuilder.java` | Builder state machine |
| `oak-store-spi/.../plugins/memory/MutableNodeState.java` | Mutation bookkeeping |
| `oak-store-spi/.../plugins/memory/ModifiedNodeState.java` | `unwrap` (base collapse) |
| `oak-store-spi/.../plugins/memory/EmptyNodeState.java` | `EMPTY_NODE` / `MISSING_NODE` |
| `oak-store-spi/.../spi/state/ConflictAnnotatingRebaseDiff.java` | `:conflict` annotation format |
| `oak-run/.../run/CheckpointsCommand.java` | `checkpoints` CLI: operations, output, exit codes |
| `oak-run/README.md` | `rm-unreferenced` semantics |
| `oak-segment-tar/.../segment/SegmentCheckpointMBean.java` | checkpoint listing fields |
| `oak-segment-tar/.../segment/spi/persistence/GCGeneration.java` | `nonGC()` |

Reader-side prerequisites (NOT repeated here):

- Node/template/map/property record byte layouts, stable-id slot, property ordering:
  `docs/analysis/node-layer.md` (§0–§4) and `docs/analysis/record-layer.md`.
- Segment format, record allocation, referenced-segment table: `docs/analysis/segment-layer.md`.
- TAR entry naming/index/graph: `docs/analysis/tar-layer.md`.
- `journal.log` line grammar, head resolution, `repo.lock`, manifest: `docs/analysis/filestore-layer.md`
  (§2, §4, §5, §8).

---

## 1. Store model the commit path operates on

The head record id in `journal.log` names the **super-root**, a node with (at least) two
children (`SegmentNodeStore.java` class comment and constants, lines 161–163):

```
super-root
 ├─ "root"          ← the JCR content tree AEM sees      (constant ROOT = "root")
 └─ "checkpoints"   ← checkpoint container               (constant CHECKPOINTS = "checkpoints")
     └─ <uuid>      ← one child per checkpoint (see §7)
```

`SegmentNodeStore.getRoot()` returns `head.getChildNode("root")`
(`SegmentNodeStore.getRoot`, line 197–200). All merges replace the `root` child of a new
super-root; checkpoint create/release edit the `checkpoints` child. **The super-root itself is
rewritten on every commit** — its record id is what goes into the journal.

An empty store is initialized by writing a node with a single empty child `"root"`
(`FileStore.initialNode()`, `FileStore.java` lines 256–275). Details that matter for a port
*(verified against sources)*:

- The initial-node supplier is invoked by `TarRevisions.bind` **only when the journal
  resolves to no head** (`TarRevisions.bind`: `findPersistedRecordId == null ⇒
  head.set(writeInitialNode.get())`; otherwise both `head` and `persistedHead` are set to the
  resolved id).
- It is written by a dedicated `"init"` writer built with the builder-default generation
  `GCGeneration.NULL` = `(generation=0, fullGeneration=0, isCompacted=false)`
  (`DefaultSegmentWriterBuilder.java` line 59). So a store created from scratch starts at
  generation (0, 0, false).
- `initialNode()` calls only `writer.flush()` — segments reach the tar writer, but **no
  journal line is written and no tar fsync happens there**; the first `journal.log` line for
  the initial head is appended by the next regular flush (§6.2; `persistedHead` is still
  null, so the `persistedHead == head` skip cannot trigger).

No `checkpoints` child
exists until the first checkpoint is created; all readers use `getChildNode` which returns
`MISSING_NODE` for absent children (`EmptyNodeState.MISSING_NODE`, `EmptyNodeState.java` line 38).

---

## 2. NodeBuilder semantics (`MemoryNodeBuilder` + `MutableNodeState`)

These are **in-memory** semantics; they affect the bytes only through which node states are
ultimately handed to `writeNode`. A port must reproduce the *observable* behavior below; the
head/revision bookkeeping (`baseRevision`, `RootHead.revision`) is an implementation device
for Java's lazily-connected child builders and need not be copied for a single-threaded port.

### 2.1 Data model

A root builder wraps a base `NodeState` in a `MutableNodeState`
(`MemoryNodeBuilder.RootHead` ctor, line 781–786). `MutableNodeState` keeps
(`MutableNodeState.java` lines 46–74):

```
MutableNodeState {
    base:       NodeState                       # immutable, unwrapped (never a ModifiedNodeState)
    properties: Map<String, PropertyState?>     # added/changed; null value = removed
    nodes:      Map<String, MutableNodeState>   # added/changed; child with base=MISSING_NODE = removed
    replaced:   bool
}
```

`snapshot()` returns `base` unchanged if both maps are empty, else a
`ModifiedNodeState(base, properties, nodes)` (`MutableNodeState.snapshot`, lines 76–83).
`ModifiedNodeState.unwrap` collapses a `ModifiedNodeState` base into the maps so `base` is
always a "real" (persisted or empty) state (`ModifiedNodeState.unwrap`, lines 66–90) —
this matters for `writeNode` (§4), which relies on `ModifiedNodeState.getBaseState()` being a
`SegmentNodeState` whenever possible.

### 2.2 Operations (exact rules)

All from `MutableNodeState.java` unless noted; `MemoryNodeBuilder` methods validate and
delegate.

- **`setProperty(p)`** — `properties[p.name] = p` after name validation (`setProperty`,
  lines 206–210; `MemoryNodeBuilder.setProperty` lines 504–511 additionally requires
  `exists()` and calls `updated()`).
- **`removeProperty(name)`** — if `base.hasProperty(name)`: `properties[name] = null`,
  return true; else remove any transient entry (`removeProperty`, lines 192–201).
  `updated()` fires only if something was removed (`MemoryNodeBuilder.removeProperty`,
  lines 527–535).
- **`child(name)`** — `hasChildNode(name) ? getChildNode(name) : setChildNode(name)`
  (`MemoryNodeBuilder.child`, lines 315–323). Note: `getChildNode` never checks existence —
  a builder for a non-existent child simply has `exists() == false`.
- **`setChildNode(name)`** = `setChildNode(name, EMPTY_NODE)` (line 332–336).
- **`setChildNode(name, state)`** — requires this builder `exists()`. In the mutable state:
  if `nodes[name]` absent → `nodes[name] = new MutableNodeState(state)` and set
  `replaced = true` iff `base.hasChildNode(name)`; if present → `child.replaced = true; child.reset(state)`
  (`MutableNodeState.setChildNode`, lines 102–120). Fires `updated()`.
- **`remove()`** (on child builder) — only if `!isRoot() && exists()`; connects itself, then
  `parent.removeChildNode(name)`: if `nodes[name]` present → `child.reset(MISSING_NODE)`;
  else `nodes[name] = new MutableNodeState(MISSING_NODE)`; returns whether the child existed
  (`MemoryNodeBuilder.remove` lines 348–358, `MutableNodeState.removeChildNode` lines 174–185).
- **`getNodeState()`** — snapshot of current state (immutable); **`getBaseState()`** — the
  base (`MemoryNodeBuilder` lines 259–267).
- **`reset(newBase)`** — root only (`checkState(parent == null)`); **all transient changes
  are dropped**: `base = newBase` and the root's mutable state is replaced by a fresh
  `new MutableNodeState(newBase)` with empty maps (`MemoryNodeBuilder.reset` lines 236–240 →
  `RootHead.setState`, which also bumps the revision twice so child builders detect the
  reset; the `MutableNodeState` ctor runs `ModifiedNodeState.unwrap`, a no-op collapse when
  `newBase` is not a `ModifiedNodeState`).
- **`set(newState)`** (protected) — root: replace current state, base unchanged; non-root:
  `parent.setChildNode(name, newState)` (`MemoryNodeBuilder.set`, lines 248–255). Used by
  `SegmentNodeBuilder.getNodeState()` to swap the in-memory tree for its persisted form.
- **`moveTo`** annotates the source path in property `:source-path`
  (`MoveDetector.SOURCE_PATH`) unless transiently added (`MemoryNodeBuilder.moveTo` +
  `annotateSourcePath`, lines 376–461). Offline tools normally don't need this; if the port
  implements move it must reproduce the annotation, since the JCR layer's move detection
  consumes it (but for offline single-writer use, plain copy+delete without annotation is
  also structurally valid content).
- **`updated()`** — fired on every mutation of *this* node (not descendants); default
  propagates to the root builder (`MemoryNodeBuilder.updated`, lines 210–214). This is the
  hook `SegmentNodeBuilder` uses for `UPDATE_LIMIT` (§3).

`EMPTY_NODE` is an existing node with no properties/children; `MISSING_NODE` is identical but
`exists() == false` (`EmptyNodeState.java` lines 36–48). Child removal is represented as
"child whose state is MISSING_NODE".

---

## 3. `SegmentNodeBuilder` and `UPDATE_LIMIT`

`SegmentNodeBuilder extends MemoryNodeBuilder` (`SegmentNodeBuilder.java`).

- `UPDATE_LIMIT = Integer.getInteger("update.limit", 10000)` (lines 49–50).
- `updateCount` field: `>= 0` on the **root** builder, `-1` on every child builder (child
  builders never count; lines 63–75, 92–106).
- `updated()` override (lines 117–127):

```
updated():
    if updateCount < 0:            # child builder
        super.updated()            # propagate to root
    else:
        updateCount += 1
        if updateCount > UPDATE_LIMIT:      # strictly greater: flush on the 10001st update
            getNodeState()
```

- `getNodeState()` override (lines 135–152):

```
getNodeState():
    state   = super.getNodeState()                          # snapshot (may be ModifiedNodeState)
    sState  = SegmentNodeState(reader, writer, blobStore, writer.writeNode(state))
    if state is not already sState:
        set(sState)                                         # replace in-memory tree by persisted state
        if root builder: updateCount = 0
    return sState
```

**Why it exists**: it bounds heap usage — without it, a large import would accumulate the
whole change set as `MutableNodeState` maps. Flushing serializes the intermediate state into
segment records ("and that might persist the changes (if the segment is flushed)", class
javadoc lines 36–41) and swaps the in-memory tree for the compact persisted form.

**Byte/garbage consequence**: each intermediate flush produces node/map/template records that
become garbage the moment a later flush supersedes them. Oak tolerates this garbage — it is
unreachable from any journal head and reclaimed only by compaction+cleanup. A port MAY use a
different (even infinite) limit without affecting correctness; only peak memory and garbage
volume change. An `IOException` during flush is wrapped in `IllegalStateException`
(lines 148–151) — the builder is then in an undefined state and must be discarded.

`createBlob(stream)` writes a segment stream via `writer.writeStream` and returns a
`SegmentBlob` (lines 160–163) — i.e. blob bytes are written **immediately**, not at merge time.

---

## 4. `writeNode`: what a commit serializes

`SegmentNodeBuilder.getNodeState()` and `Commit.apply` funnel into
`DefaultSegmentWriter.SegmentWriteOperation.writeNode(state, stableIdBytes=null)`
(`DefaultSegmentWriter.java` lines 825–946). Byte layouts are in `node-layer.md`; this
section specifies the **writer's choice of records**, which determines the bytes actually
emitted for a commit.

### 4.1 Algorithm

```
writeNode(state, stableIdBytes):
    # 1. Deduplication (lines 827–831, 971–995)
    if state is SegmentNodeState from this store:
        if NOT isOldGeneration(state.recordId):
            return state.recordId                    # reuse, write nothing
        else:
            cached = nodeCache.get(state.stableId)   # dedup cache keyed by stable-id string
            if cached != null: return cached
    # 2. Stable id (lines 833–835)
    if state is SegmentNodeState and stableIdBytes == null:
        stableIdBytes = state.getStableIdBytes()
        # reachable for (a) same-store states of an OLD generation that missed the nodeCache
        # and (b) SegmentNodeStates from a DIFFERENT store (dedup step returns null for
        # those) — both are rewritten with their original stable id preserved
    id = writeNodeUncached(state, stableIdBytes)
    if stableIdBytes != null:
        nodeCache.put(stableIdString, id, cost)      # lines 838–843
    return id

writeNodeUncached(state, stableIdBytes):             # lines 852–946
    beforeId = null
    if state is ModifiedNodeState:
        beforeId = deduplicateNode(state.getBaseState())   # base reuse, may be null
    before = beforeId != null ? readNode(beforeId) : null

    template = Template(state)                       # computed from state's properties/children
    ids = []
    ids.add(template == before.template ? before.templateId : writeTemplate(template))

    if template.childName == MANY_CHILD_NODES:
        # incremental map update against before's child map when both sides have > 1 child
        # (writeChildNodes, lines 948–960): diff before→after, write only changed children,
        # produce map BRANCH/LEAF updates against before.childNodeMap
        ids.add(writeChildNodes(before, state))
    elif template.childName != ZERO_CHILD_NODES:
        ids.add(writeNode(state.getChildNode(childName), null))

    pIds = []
    for pt in template.propertyTemplates:            # template order — see node-layer.md §1.4
        property = state.getProperty(pt.name)
        if before != null and property.equals(before.getProperty(pt.name)):
            property = before.getProperty(pt.name)   # prefer base instance (lines 891–900)
        if property is a SegmentPropertyState from this store:      # lines 902–908
            if isOldGeneration(property.recordId):
                pIds.add(writeProperty(property))    # rewrite — does NOT fall through to
                                                     # the before-based reuse branches below
            else:
                pIds.add(property.recordId)          # reuse property record
        elif before == null or before not from this store:
            pIds.add(writeProperty(property))
        else:
            bt = beforeTemplate.getPropertyTemplate(pt.name)
            if bt == null:                pIds.add(writeProperty(property))         # new
            elif property.equals(bp):     pIds.add(bp.recordId)                     # unchanged
            elif bp.isArray && type != BINARIES:
                                          pIds.add(writeProperty(property, bp.valueRecords))
                                          # reuse unchanged list entries (lines 916–925)
            else:                         pIds.add(writeProperty(property))
    if pIds not empty: ids.add(writeList(pIds))

    stableId = stableIdBytes == null ? null
             : writeBlock(bytes(stableIdBytes))      # 20-byte BLOCK record (lines 934–942)
    return execute(NodeStateWriter(stableId, ids))
```

`NodeStateWriter.writeRecordContent` (`RecordWriters.java` lines 487–513):

> "Write the stable record ID. If no stable ID exists (in case of a new node state), it is
> generated from the current record ID. In this case, the generated stable ID is only a
> marker and is not a reference to another record."

i.e. slot 0 of the node record = **its own record id** for fresh nodes, or a reference to a
20-byte block holding `(msb, lsb, offset-int)` for rewritten nodes (matches the read side,
`node-layer.md` §2.2; `SegmentNodeState.getStableIdBytes`, lines 163–177).

`isOldGeneration(id)` (`DefaultSegmentWriter.java` lines 1016–1029):

```
thatGen = generation of id's segment; thisGen = writer generation
if thatGen.isCompacted: old ⇔ thatGen.fullGeneration < thisGen.fullGeneration
else:                   old ⇔ thatGen.generation    < thisGen.generation
```

### 4.2 Required vs. optimization

- **Required for correctness**: the record layouts, template property ordering, the stable-id
  slot rule above, and that every record id referenced from a record of generation G points
  to a segment whose generation Oak will retain (same generation, or compacted segments of
  the same full generation — the `isOldGeneration` rule). Referencing an old-generation
  record from a new record is what causes `SegmentNotFoundException` after cleanup.
- **Optimizations (change bytes, not semantics)**: all deduplication (returning an existing
  record id instead of rewriting), base-state record reuse (`beforeId`), property/list-entry
  reuse, the `nodeCache`/`stringCache`/`templateCache`. A port may rewrite everything from
  scratch on every commit; the result is a structurally different but fully valid store —
  merely larger. Record ids of "equal" content then differ, which is fine because Oak
  compares by stable id / content, never by expecting particular record ids.
- **Practically required**: the fast-path reuse `state is SegmentNodeState && !old ⇒ return its id`.
  Without it, `builder.setChildNode(ROOT, existingSegmentNodeState)` (checkpoint create,
  §7.1) would deep-copy the entire content tree. Any port MUST implement at least this
  case to keep checkpoint creation O(1) and to keep checkpoint roots *hard links* — the
  checkpoint's `root` child must have the **same record id** as the head's `root` child at
  creation time so segments stay shared.

---

## 5. Merge (`SegmentNodeStore.merge` → `LockBasedScheduler.schedule`)

### 5.1 Entry checks

`SegmentNodeStore.merge(builder, commitHook, info)` requires `builder instanceof SegmentNodeBuilder`
and `builder.isRootBuilder()` (lines 204–213); wraps the hook in a `CompositeHook` with a
`LoggingHook` **only if configured** via `withLoggingHook` (off by default). Then
`scheduler.schedule(new Commit(builder, commitHook, info))`.

### 5.2 Scheduling (single lock, retry loop)

`LockBasedScheduler.schedule` (lines 253–288) acquires the single-permit fair semaphore
(`commitSemaphore`, fairness from `oak.segmentNodeStore.commitFairLock`, default true),
then `execute(commit)`, then `commit.applied(merged)` (resets the caller's builder onto the
merged state, `Commit.applied` lines 116–118). Failures map to
`CommitFailedException("Segment", 2, "Merge interrupted")` on interrupt and
`("Segment", 3, "Merge failed")` on `SegmentOverflowException`.

`execute` (lines 290–323):

```
if not commit.hasChanges(): return head.root          # hasChanges = !fastEquals(before, after)
for backoff in 1,2,4,... while backoff < 10_000 ms:   # MAXIMUM_BACKOFF = 10 s (line 129)
    refreshHead(true)                                 # re-read journal head from revisions
    before = head
    after  = commit.apply(before)                     # §5.3
    if revisions.setHead(before.recordId, after.recordId):   # CAS, §6.1
        head = after; dispatch; return after.child("root")
    sleep(backoff ms + rand ns)
throw CommitFailedException("Segment", 3, "The commit could not be executed after {n} attempts…")
```

For a **single-writer port** the CAS can never fail; the loop degenerates to one iteration.

`hasChanges` uses `SegmentNodeState.fastEquals` (`Commit.hasChanges` lines 128–130), which
"cannot guarantee against false negatives" — a no-op commit may still be executed; harmless.

### 5.3 Applying a commit (`Commit.apply`, lines 92–109)

```
builder = base.builder()                # base = current super-root head
if fastEquals(changes.getBaseState(), base.getChildNode("root")):
    # fast path: no external changes since the builder was created
    builder.setChildNode("root", hook.processCommit(before, after, info))
else:
    # full rebase: replay after-vs-before diff onto the new head
    diff = ConflictAnnotatingRebaseDiff(builder.child("root"))
    getAfterState().compareAgainstBaseState(getBeforeState(), diff)
    builder.setChildNode("root", hook.processCommit(newBase.root, rebased.root, info))
return builder.getNodeState()           # SegmentNodeBuilder.getNodeState ⇒ writeNode
```

`fastEquals(a, b)` (`SegmentNodeState.java` lines 681–692) = same `(segmentId, recordNumber)`
(`Record.fastEquals`, `Record.java` lines 29–39) **or** equal stable-id strings
(`"<uuid>:<offset-decimal>"`, `getStableId` lines 136–142).

In the rebase path, conflicts are not resolved — they are **written into the content** as
`:conflict/<conflictType>/{:base, :ours}` subtrees (`ConflictAnnotatingRebaseDiff.java`,
constants lines 38–40, markers lines 46–112; conflict type names from `ConflictType`:
`addExistingProperty`, `changeDeletedProperty`, `changeChangedProperty`,
`deleteDeletedProperty`, `deleteChangedProperty`, `addExistingNode`, `changeDeletedNode`,
`deleteDeletedNode`).

### 5.4 Hooks — what runs, and what a port must do

- **The scheduler itself applies no hooks.** The only hook invoked is the one passed to
  `NodeStore.merge` (`Commit.apply` calls `hook.processCommit` and nothing else). Conflict
  *annotation* happens structurally in the rebase diff; conflict *resolution and rejection*
  (`ConflictHook` + `ConflictValidator`) and index maintenance (`EditorHook` with index
  editors), name validation, type checks etc. are hooks composed by the **caller** (Oak's
  `ContentRepository`/AEM runtime), not by `SegmentNodeStore`.
- **Offline oak-run tooling already merges with `EmptyHook`** (e.g. checkpoint
  release goes through `scheduler.removeCheckpoint`, which calls `revisions.setHead`
  directly with **no hook at all** — `LockBasedScheduler.removeCheckpoint` lines 354–382; the
  same is true of checkpoint creation, `CPCreator.call` lines 415–455). So there is Oak
  precedent that super-root maintenance bypasses all commit hooks.
- **`jcr:lastModified`/`jcr:created` etc. are not maintained by node-store hooks**; they are
  set by the JCR/Sling layers before `merge` is called. A port doing checkpoint or
  super-root operations has nothing to emulate.
- **Danger zone for a port**: merging *arbitrary content changes under `/root`* with no hook
  skips synchronous index editors. AEM's query indexes (`/oak:index/uuid`,
  property indexes, reference index) would silently go stale, and duplicate `jcr:uuid`s
  would not be rejected. Safe operations without hooks are exactly the ones Oak itself does
  hook-free: checkpoint create/release/expire (super-root, outside `/root`) and rewrites
  that don't change logical content (compaction). The port must not offer hook-free content
  editing of `/root` as an "AEM-safe" operation.

### 5.5 `rebase` / `reset` (for completeness)

`SegmentNodeStore.rebase(builder)` (lines 215–231): if the builder's base differs from the
current root (by `fastEquals`), capture `after = builder.getNodeState()`, `builder.reset(root)`,
then replay `after` vs old base through `ConflictAnnotatingRebaseDiff(builder)`.
`reset(builder)` (lines 233–243) just `builder.reset(root)`.

---

## 6. Head advance and durability

### 6.1 `setHead` is memory-only

`TarRevisions.setHead(expected, head)` (lines 264–284) is a CAS on an in-memory
`AtomicReference` under a read lock. **Nothing is written to disk by a successful merge.**

### 6.2 Flush: the only durability point

`FileStore.doFlush` (lines 333–343):

```java
revisions.flush(() -> {
    segmentWriter.flush();   // write all open segment buffers into the current tar writer
    tarFiles.flush();        // TarWriter.flush → archive.flush → SegmentTarWriter.flush =
                             // RandomAccessFile.getFD().sync() on the open data%05d%s.tar
                             // (fsync of data AND file metadata; skipped if no tar entry
                             // was written yet: archive.isCreated() guard)
    stats.flushed();
});
```

`TarRevisions.doFlush(flusher)` (lines 224–239), under `journalFileLock`:

```
if persistedHead == currentHead: return          # skip if nothing new
flusher.flush()                                   # segments durable FIRST
journalFileWriter.writeLine(head.toString10() + " root " + System.currentTimeMillis())
persistedHead = head
```

`RecordId.toString10()` = `String.format("%s:%d", segmentId, offset)` — hyphenated lowercase
UUID, colon, **decimal** offset (`RecordId.java` lines 136–138), matching the journal grammar
in `filestore-layer.md` §4. `LocalJournalFileWriter.writeLine` appends the line + `"\n"`
(ASCII, `RandomAccessFile.writeBytes` = low bytes of each char) at EOF and calls
`getChannel().force(false)` — fsync data only, per line (`LocalJournalFile.java` lines 98–102).

**Normative order: segment bytes → tar fsync → journal append → journal fsync.** A journal
line must never become durable before every segment reachable from it.

Flush is triggered by: a background task every **5 seconds** (`tryFlush`, `FileStore.java`
lines 212–219 — skipped if the journal lock is contended), explicit `FileStore.flush()`, and
close. A port MUST call the equivalent of `flush()` before exiting after any mutation,
otherwise the mutation is lost (tolerated by Oak, but pointless).

### 6.3 Crash windows (all tolerated by Oak startup)

| Crash point | Disk state | Next startup |
|---|---|---|
| After writing segment records, before flush | partial/complete segments in tar, journal unchanged | head = old journal line; new records are unreachable garbage; tar recovery may rebuild index (`filestore-layer.md` §6.5) |
| After `segmentWriter.flush`/`tarFiles.flush`, before journal append | segments durable, journal unchanged | same as above — old head, extra garbage |
| Torn journal line | last line invalid | `JournalReader` skips invalid lines; falls back to previous line (`filestore-layer.md` §5.3–5.4) |
| After journal fsync | fully committed | new head |

Nothing is ever rolled back on disk; "rollback" of a failed merge is purely dropping
in-memory references. Garbage records/segments from failed or unflushed commits are the
normal state of a TarMK store.

### 6.4 Generation of commit segments

The normal (`"sys"`) writer is built `withGeneration(() -> getGcGeneration().nonGC())`
(`FileStore.java` lines 146–151), where `getGcGeneration()` is the generation of the current
head segment (lines 277–280) and `nonGC()` clears the compacted flag
(`GCGeneration.nonGC`, lines 143–145). Therefore:

- Every commit is written at the head's `(generation, fullGeneration)` with
  **`isCompacted = false`** — even right after a compaction, when the head segment itself
  has `isCompacted = true`.
- Commits **never advance** any generation counter. Only compaction does
  (full: `generation+1, fullGeneration+1, compacted=true`; tail: `generation+1,
  fullGeneration unchanged, compacted=true` — see the compaction write spec; class comment
  of `GCGeneration.java` lines ~40–60 states cleanup reclaims segments whose generation is
  old unless `isCompacted && fullGeneration == head.fullGeneration`).
- Consequence for the port: when opening an existing store for writing, the writer generation
  MUST be derived from the resolved head record's segment (`generation`, `fullGeneration`,
  `compacted=false`), not hardcoded to (0,0,false), or the first cleanup after AEM restart
  could reclaim the port's segments.

---

## 7. Checkpoints

### 7.1 Create (`SegmentNodeStore.checkpoint` → `LockBasedScheduler.checkpoint` + `CPCreator`)

API: `checkpoint(lifetime)` (synchronized, empty properties, lines 274–277) and
`checkpoint(lifetime, properties)` (lines 268–272). `LockBasedScheduler.checkpoint`
(lines 325–352):

```
require lifetime > 0                       # IllegalArgumentException otherwise
name = UUID.randomUUID().toString()        # random v4 UUID, lowercase, hyphenated
if commitSemaphore.tryAcquire(10 s):       # "oak.checkpoints.lockWaitTime", default 10 (line 135)
    try:
        if CPCreator(name, lifetime, properties).call(): return name
    finally:
        refreshHead(true)                  # drop stale head reference (OAK-3347)
        release semaphore
log.warn(...)                              # on timeout / CAS failure / exception
return name                                # !! name returned even on failure
```

**Oak returns the checkpoint name even when creation failed** (only a warn/error is logged).
A port should NOT copy this wart — it must report failure — but must not rely on a returned
name implying existence.

`CPCreator.call()` (lines 415–455), exact algorithm:

```
now = System.currentTimeMillis()
refreshHead(true)
state   = head                                    # super-root
builder = state.builder()
checkpoints = builder.child("checkpoints")

# 1. Purge expired/corrupt checkpoints
for n in checkpoints.getChildNodeNames():
    ts = checkpoints[n].getProperty("timestamp")
    if ts == null or ts.type != LONG or now > ts.value:
        checkpoints[n].remove()

# 2. Create the new checkpoint
cp = checkpoints.child(name)
if Long.MAX_VALUE - now > lifetime:               # overflow guard (Java long, 2^63-1)
    cp.setProperty("timestamp", now + lifetime)   # LONG
else:
    cp.setProperty("timestamp", Long.MAX_VALUE)   # 9223372036854775807
cp.setProperty("created", now)                    # LONG

props = cp.setChildNode("properties")             # replaces any existing node
for (k, v) in properties: props.setProperty(k, v) # STRING properties

cp.setChildNode("root", state.getChildNode("root"))   # hard link to current head root

newState = builder.getNodeState()                 # writeNode of new super-root
return revisions.setHead(state.recordId, newState.recordId)   # + refreshHead(false) on success
```

Written structure (all new records: super-root, checkpoints map, checkpoint node,
`properties` node; the `root` child is **shared by record id** — see §4.2):

```
checkpoints/<uuid>/
    timestamp  : Long   (expiry epoch millis, clamped to Long.MAX_VALUE)
    created    : Long   (creation epoch millis)
    properties/          (always present, possibly empty)
        <key> : String  (one per entry of the properties map)
    root/                (content snapshot; record id == head root's record id)
```

Field semantics on the read side: `retrieve(name)` returns
`head/checkpoints/<name>/root` if it exists (lines 302–313); `checkpointInfo(name)` returns
the string values of `head/checkpoints/<name>/properties` (lines 279–294);
`checkpoints()` lists `head/checkpoints` child names (lines 296–300). The MBean labels
`created` as creation date and `timestamp` as expiry (`SegmentCheckpointMBean.collectCheckpoints`,
lines 46–55).

No hook runs; the change is outside `/root`. **AEM relies on `checkpoints/<id>/root` being an
exact, immutable snapshot; never rewrite it in place.**

### 7.2 Release (`SegmentNodeStore.release` → `removeCheckpoint`, lines 354–382)

```
for attempt in 1..5:
    if commitSemaphore.tryAcquire():           # non-blocking
        refreshHead(true)
        builder = head.builder()
        cp = builder.child("checkpoints").child(name)
        if cp.exists():
            cp.remove()
            if revisions.setHead(head.recordId, builder.getNodeState().recordId):
                refreshHead(false); return true
        release semaphore
return false
```

Releasing is plain **child-node removal** from the super-root plus `setHead`. Returns false
if the checkpoint does not exist (note: after 5 no-op attempts). The snapshot's segments
remain on disk until compaction/cleanup.

### 7.3 `oak-run checkpoints` command

`CheckpointsCommand.java`. Usage string (line 35):
`checkpoints {<path>|<mongo-uri>|<jdbc-uri>} [list|rm-all|rm-unreferenced|rm <checkpoint>|info <checkpoint>|set <checkpoint> <name> [<value>]] [--segment]`.
Default op is `list` (line 52). For a segment store it delegates to
`org.apache.jackrabbit.oak.checkpoint.Checkpoints.onSegmentTar(new File(storeArg), closer)`
(line 73). **That helper class is not in this checkout** (module `oak-run`'s
`org.apache.jackrabbit.oak.checkpoint` package); the following is fixed by the command's call
sites, `oak-run/README.md` (lines 237–248), and `SegmentNodeStore` behavior:

- Helper contract: `list() → [CP{id, created, expires}]`, `removeAll() → long` (count or −1),
  `removeUnreferenced() → long` (count or −1), `remove(id) → int` (0 = not found, 1 = removed,
  else failure), `getInfo(id) → Map|null`, `setInfoProperty(id, name, value|null) → int`
  (same codes as `remove`).
- Semantics (README): `rm-all` "will wipe clean the 'checkpoints' node"; `rm-unreferenced`
  "will remove all checkpoints except the one referenced from the async indexer
  (/:async@async)"; `rm` removes one; `set` sets/removes (value omitted ⇒ remove) a property
  under `checkpoints/<id>/properties`.
- For the Rust port, the safe reference set for `rm-unreferenced` is: read super-root
  `root/:async` and keep every checkpoint whose id equals a String (or member of a
  multi-String) value of any property of that node (this is a conservative superset of
  "@async"; it also protects `async-async`/`fulltext-async` lanes present on AEM).
  Verify against upstream `Checkpoints.java` before finalizing byte-for-byte parity claims —
  do not ship a `rm-unreferenced` that removes a checkpoint any `/:async` property points at.
- All removals must go through the same mechanics as §7.2 (edit super-root, set head,
  flush) — never by editing tar files.

Exact console output (`System.out`, `%n` = platform newline; timestamps via
`java.sql.Timestamp.toString()`, i.e. `yyyy-mm-dd hh:mm:ss.fffffffff`):

```
Checkpoints <storeArg>
# list:
- <id> created <Timestamp(created)> expires <Timestamp(expires)>      (per checkpoint)
Found <n> checkpoints
# rm-all / rm-unreferenced:
Removed <n> checkpoints in <t>ms.
# rm:
Removed checkpoint <id> in <t>ms.
# info:
<key>\t<value>                                                        (per entry)
# set:
Updated checkpoint <id> in <t>ms.
```

Errors: any failure raises `RuntimeException`, whose message is printed to **stderr**, and
the process exits with code **1** (`success` flag, lines 48, 163–171); success exits 0
(implicit); `--help` prints usage and exits 0 (lines 36–39). Failure messages:
`Unknown operation: <op>`, `Missing checkpoint id`, `Checkpoint '<id>' not found.`,
`Failed to remove all checkpoints.`, `Failed to remove unreferenced checkpoints.`,
`Failed to remove checkpoint <id>` (also used for a failed `set`), `Missing nodestore path/URI`.

### 7.4 How checkpoint operations reach the journal

`CPCreator`/`removeCheckpoint` end at `revisions.setHead` — memory only (§6.1). Durability
comes solely from the next `flush()` (§6.2), which in Oak happens within ≤ 5 s via the
background flusher or at store close. An offline port must:

1. open store (verify `repo.lock` protocol per `filestore-layer.md` §2),
2. apply the checkpoint edit + `setHead`,
3. flush (segments → tar fsync → `journal.log` append `"<recordid> root <millis>"` → fsync),
4. close (which flushes again and releases the lock).

---

## 8. Error and cancellation behavior (summary)

- **Merge failure / interruption**: `CommitFailedException` of type `Segment` code 2
  (interrupt) or 3 (overflow / retry exhaustion). No disk rollback; any records already
  serialized (including `UPDATE_LIMIT` intermediate flushes) remain as unreachable garbage.
- **Checkpoint create failure**: no head change; partial records are garbage; Oak still
  returns the name (§7.1) — port should return an error instead.
- **Checkpoint release on missing id**: returns false, no writes.
- **Any crash**: disk is always in one of the states of §6.3, all of which Oak's startup
  (journal head resolution + optional tar recovery) tolerates.
- Startup also tolerates: expired checkpoints (purged lazily on next checkpoint creation,
  §7.1 step 1), a missing `checkpoints` node, unreferenced segments, and orphan record ids
  in tars.

---

## 9. AEM safety invariants (checklist for the Rust implementation)

Every writing operation of the port MUST satisfy all of the following; then a subsequent
AEM start is safe.

1. **Single-writer exclusion**: acquire the OS file lock on `repo.lock` for the whole write
   session and only run against a *stopped* AEM (`FileStore` ctor line 143;
   `filestore-layer.md` §2).
2. **Journal-last ordering**: never append a `journal.log` line before every segment
   reachable from that record id is durable in a `data%05d%s.tar` file
   (`TarConstants.FILE_NAME_FORMAT`; fsync tar via `FileDescriptor.sync`, then append, then
   fsync journal via `FileChannel.force(false)` — `FileStore.doFlush` +
   `TarRevisions.doFlush` + `LocalJournalFileWriter.writeLine`).
3. **Journal line format**: exactly `<lowercase-hyphenated-uuid>:<decimal-offset> root <epoch-millis>\n`,
   appended (never rewritten/truncated except by explicit journal-recovery tooling).
4. **Super-root shape**: every head written has child `root`; the `checkpoints` child, when
   present, contains only `<uuid>/{timestamp:Long, created:Long, properties/*, root}`.
5. **Checkpoint roots are hard links**: on create, `checkpoints/<id>/root` reuses the head
   root's record id (no deep copy, no rewrite); snapshots are never mutated afterwards.
6. **Expiry arithmetic**: `timestamp = (Long.MAX_VALUE - now > lifetime) ? now + lifetime : Long.MAX_VALUE`,
   Java signed-64-bit; `lifetime <= 0` is an error.
7. **Never remove referenced checkpoints** in `rm-unreferenced`: keep every id referenced
   from any property of `/:async` (superset of README's `/:async@async`); AEM's async
   indexers hold their last-indexed state there.
8. **No hook-free content edits under `/root`** are exposed as safe operations; hook-free
   merges are limited to the super-root (`checkpoints`) exactly as Oak itself does.
9. **Generation inheritance**: new segments carry the head segment's
   `(generation, fullGeneration)` with `isCompacted=false` (`nonGC`); generation counters are
   advanced only by compaction. Records of generation G reference only records that the
   `isOldGeneration` rule considers current (same generation, or compacted with the same
   full generation).
10. **Record-level structure**: node records follow `node-layer.md` — slot 0 stable id
    (self record id for new nodes; 20-byte block reference when rewriting with a preserved
    stable id), template property ordering, correct referenced-segment-id tables in each
    segment (`segment-layer.md`).
11. **Stable ids preserved where Oak preserves them**: content rewrites that must remain
    `fastEquals`-equal to their originals (online-style compaction, checkpoint dedup) carry
    the original 20-byte stable id; brand-new content gets the self-referential marker.
12. **Blobs first**: binary values are written (segment stream or external blob id record)
    before the node records referencing them; external blob references only when a blob
    store is configured.
13. **Flush before exit**: every successful CLI mutation ends with the full flush sequence
    and a clean close; on any failure, leave the journal untouched (garbage segments are
    acceptable; a wrong journal line is not).
14. **Tolerated leftovers only**: after a crash of the port, the store may contain extra
    segments/tars and a stale journal — states Oak recovers from; it must never contain a
    journal head whose closure is incompletely written.
15. **Checkpoint property typing is load-bearing** *(added by verification)*: `timestamp`
    and `created` must be single-value Java `Long` properties. `CPCreator` step 1 purges any
    checkpoint whose `timestamp` is absent, not of type `LONG`, or in the past
    (`ts == null || ts.getType() != LONG || now > ts.getValue(LONG)`,
    `LockBasedScheduler.java` lines 425–431) — a mis-typed value written by the port would
    silently destroy the checkpoint at AEM's next checkpoint creation and force a full
    async reindex. Checkpoint `properties/*` values must be single-value `String`s
    (`checkpointInfo` reads them with `getValue(STRING)`).
16. **Empty-store creation** *(added by verification)*: a store created from nothing is
    valid in exactly two shapes — (a) an empty/absent `journal.log` (AEM's first start then
    writes its own initial super-root at generation `(0, 0, compacted=false)` =
    `GCGeneration.NULL`, `TarRevisions.bind` + `FileStore.initialNode`), or (b) an initial
    super-root with a single empty child `root`, written at `(0, 0, false)`, made durable,
    and named by a journal line per invariants 2–3. Never write a journal line whose record
    id does not resolve to a super-root node record shaped as §1.
17. **`repo.lock` and journal writer discipline** *(added by verification)*: the journal is
    append-only via a writer that seeks to EOF on open (`LocalJournalFileWriter` ctor);
    `truncate()` exists only for journal-recovery tooling. The port must never rewrite
    earlier lines, and must fsync each appended line individually (`force(false)` per line)
    before reporting success.
