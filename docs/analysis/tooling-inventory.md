# oak-segment-tar Feature Inventory

Scope: complete feature map of the `oak-segment-tar` module — core library, garbage
collection/compaction, oak-run segment tooling, cold-standby replication, and monitoring.
This is an inventory document, not a byte-format specification. Byte-level details live in the
companion format specifications.

All Java paths below are relative to
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/`.

Legend: **RO** = read-only against the segment store; **RW** = mutates the store (or another
store); **RO+out** = reads the store, writes only external output files.

---

## 1. Core library features

### 1.1 SegmentNodeStore — the NodeStore API implementation

`SegmentNodeStore.java` — `public class SegmentNodeStore implements NodeStore, Observable`.

The public entry point of the module. Provides the full Oak `NodeStore` contract on top of a
`SegmentStore`:

| Feature | Method(s) | Notes (source citations) |
|---|---|---|
| Root access | `getRoot()` | Returns current head `SegmentNodeState` (child `root` of the super-root). |
| Commits | `merge(NodeBuilder, CommitHook, CommitInfo)` | Commits are serialized through a scheduler (`scheduler/` package, `LockBasedScheduler`); a commit semaphore/queue is instrumented by `SegmentNodeStoreStats`. |
| Rebase / reset | `rebase(NodeBuilder)`, `reset(NodeBuilder)` | |
| Blobs | `createBlob(InputStream)`, `getBlob(String reference)` | Delegates to the configured `BlobStore` if any, else segment-stored blobs (`SegmentBlob.java`). |
| Checkpoints | `checkpoint(long lifetime)`, `checkpoint(long, Map)`, `checkpointInfo(String)`, `checkpoints()`, `retrieve(String)`, `release(String)` | Checkpoints are stored as children of the super-root under the child node named by constant `CHECKPOINTS = "checkpoints"` (`SegmentNodeStore.java` line 163). A read-only implementation must know the super-root layout: `{root, checkpoints/<id>/{properties…, root}}`. |
| Observation | `addObserver(Observer)` (from `Observable`) | Change dispatch is optional: `SegmentNodeStoreBuilder.dispatchChanges(boolean)`. |
| Stats | `getStats()` returns `SegmentNodeStoreStats` | See §5. |
| Logging hook | `SegmentNodeStoreBuilder.withLoggingHook(Consumer<String>)` | Wraps the writer with `LoggingHook.java` which serializes every write operation to a log line (write-path debugging aid). |

### 1.2 Stores

| Class | Role | RO/RW |
|---|---|---|
| `file/FileStore.java` | The production TarMK store. Flushing (`flush()`, `tryFlush()`), segment read/write, GC entry points (`fullGC()`, `tailGC()`, `compactFull()`, `compactTail()`, `cleanup()`, `cancelGC()`, `getGCRunner()`), blob reference collection (`collectBlobReferences(Consumer<String>)`). | RW |
| `file/ReadOnlyFileStore.java` | Read-only view used by all diagnostic tooling. Extra tooling API: `setRevision(String)` (rewind head to an arbitrary revision), `getTarReaderIndex()`, `getTarGraph(String)`, `getSegmentIds()`. Never writes; never triggers recovery writes. | RO |
| `memory/` (`MemoryStore`) | In-memory SegmentStore for tests/tools. | n/a |
| `file/AbstractFileStore.java` | Shared logic: manifest checking (`MIN_STORE_VERSION = 1`, `MAX_STORE_VERSION = 2`, lines 86–105), segment reading, recovery hooks. Store version 1 = Oak 1.6, 2 = Oak 1.8+ (segment format versions 12/13 respectively). |
| `file/FileStoreBuilder.java` | Builder: `withMemoryMapping(boolean)`, `withSegmentCacheSize(int)` (MB), `withStringCacheSize`, `withTemplateCacheSize`, `withStringDeduplicationCacheSize`, `withTemplateDeduplicationCacheSize`, `withNodeDeduplicationCacheSize`, `withBlobStore`, `withGCOptions`, `withIOMonitor`, `withStrictVersionCheck(boolean)`, `withCustomPersistence(SegmentNodeStorePersistence)`, `build()` / `buildReadOnly()`. |

### 1.3 Reading and caching stack

| Class | Feature |
|---|---|
| `SegmentReader.java` / `CachingSegmentReader.java` | Decodes records into `SegmentNodeState`, `SegmentPropertyState`, `MapRecord`, `Template`, strings; caches strings and templates per segment store. |
| `SegmentCache.java` | Segment-level cache; `DEFAULT_SEGMENT_CACHE_MB = 256`. |
| `StringCache.java`, `TemplateCache.java`, `ReaderCache.java` | Read-side deduplication caches. |
| `SegmentTracker.java`, `SegmentIdTable.java`, `SegmentIdFactory.java`, `SegmentIdProvider.java` | Canonical `SegmentId` instances (msb/lsb → shared object identity); tracks segment id → store binding. |
| `SegmentStream.java` | Streaming reads of large binary values across bulk segments. |
| `SegmentParser.java` | Low-level record traversal/visitor used by analysis tools. |
| `RecordUsageAnalyser.java` | Accounts record space usage per record type (used by `DebugStore`). |
| `SegmentDump.java` | Human-readable hex dump of a segment (used by `Segment.toString()` / `debug` tooling). |

### 1.4 Writing stack (relevant to on-disk layout only)

`DefaultSegmentWriter.java`, `SegmentBufferWriter.java`, `SegmentBufferWriterPool.java`,
`RecordWriters.java`, `WriterCacheManager.java`, `RecordCache.java`, `file/PriorityCache.java`
(generation-aware deduplication cache for node records). Not needed for a read-only port
except as documentation of how records were laid out.

### 1.5 Revisions and journal

| Class | Feature |
|---|---|
| `file/TarRevisions.java` | Head pointer with optimistic `setHead` and journal-file persistence; flush appends `"<recordId> root <timestamp>"` lines to `journal.log`. |
| `file/ReadOnlyRevisions.java` | Read-only head resolution for `ReadOnlyFileStore`. |
| `file/JournalReader.java` / `file/JournalEntry.java` | Iterates `journal.log` lines backwards (newest first). |
| `file/Manifest.java` / `file/ManifestChecker.java` | The `manifest` file; property `store.version` (`Manifest.java` line 27). Strict check requires version == 2; non-strict accepts 1..2 and upgrades on open (`AbstractFileStore.java` lines 86–105). |
| `file/GCJournal.java` | `gc.log` file: `repoSize, reclaimedSize, timestamp, gcGeneration, gcFullGeneration, nodesCompacted, rootId` per cleanup (class javadoc). |

### 1.6 SPI / persistence abstraction

`spi/persistence/` defines `SegmentNodeStorePersistence` (archive manager, journal file,
manifest file, gc journal, repository lock) letting the same store logic run against local TAR
files (`file/tar/TarPersistence`), Azure/AWS remote stores (separate modules), or split/wrapping
persistences. `spi/monitor/` defines `IOMonitor`, `RemoteStoreMonitor`; `spi/` also hosts
`RepositoryNotReachableException`.

### 1.7 Misc core features

- `file/proc/Proc.java` — exposes the store's internals (tar files, segments, records, journal,
  commits) as a virtual read-only node tree (backs `oak-run explore`). RO.
- OSGi integration: `SegmentNodeStoreService.java`, `SegmentNodeStoreFactory.java`,
  `SegmentNodeStoreRegistrar.java`, `osgi/` package.
- `SegmentDiscoveryLiteDescriptors.java` — cluster/discovery-lite descriptor support.
- `SegmentBlobReferenceRetriever.java` — blob GC integration (mark phase provider).
- `SegmentNotFoundException` / `SegmentNotFoundExceptionListener` — the canonical "referenced
  segment is gone (usually GC'd)" error; tooling generally catches and continues, the library
  treats it as fatal for the affected read.
- `file/FileStoreUtil.java`, `util/` helpers, `cancel/` (`Canceller`) — cooperative cancellation
  used by GC and check tooling.

---

## 2. Garbage collection and compaction

### 2.1 Options — `compaction/SegmentGCOptions.java`

Defaults (constants in `SegmentGCOptions.java`):

| Constant | Value | Meaning |
|---|---|---|
| `PAUSE_DEFAULT` | `false` | GC paused flag. |
| `RETRY_COUNT_DEFAULT` | `5` | Compaction retry cycles before giving up/forcing. |
| `FORCE_TIMEOUT_DEFAULT` | `60` (s) | Time budget for the final forced compaction (acquires exclusive write lock). |
| `RETAINED_GENERATIONS_DEFAULT` | `2` | GC generations kept during cleanup. |
| `SIZE_DELTA_ESTIMATION_DEFAULT` | `1024*1024*1024` (1 GiB) | Repository growth threshold that triggers GC. |
| `MEMORY_THRESHOLD_DEFAULT` | `15` (%) | Minimum free heap; below it GC is skipped/cancelled (`GCMemoryBarrier`). |
| `GC_PROGRESS_LOG_DEFAULT` | `-1` | Compacted-nodes log interval (−1 = disabled). |
| `DEFAULT_CONCURRENCY` | `1` | Compaction threads. |

Enums: `GCType { FULL, TAIL }`; `CompactorType { CLASSIC_COMPACTOR("classic"),
CHECKPOINT_COMPACTOR("diff"), PARALLEL_COMPACTOR("parallel") }`.
`setOffline()` switches to offline-GC behavior (used by the `compact` tool).

### 2.2 Orchestration — `file/GarbageCollector.java` and strategies

`GarbageCollector` (package-private, owned by `FileStore`) runs the three phases
**estimation → compaction → cleanup** per `fullGC()`/`tailGC()` invocation. Notable constant:
`GC_BACKOFF = getInteger("oak.gc.backoff", 10*3600*1000)` ms (line 60). Mutates the store. RW.

Strategy classes (`file/`):

| Class | Role |
|---|---|
| `GarbageCollectionStrategy` / `DefaultGarbageCollectionStrategy` / `CleanupFirstGarbageCollectionStrategy` / `SynchronizedGarbageCollectionStrategy` | Phase sequencing; "cleanup first" variant reclaims space before compaction. |
| `EstimationStrategy`, `FullSizeDeltaEstimationStrategy`, `TailSizeDeltaEstimationStrategy` | Decide whether GC is needed: compares current repo size against the size recorded in `gc.log`; skipped when `sizeDeltaEstimation == 0`, or when no gc-journal data exists (first run ⇒ GC runs). Full estimation also forces full GC if the previous run was tail. |
| `CompactionStrategy`, `FullCompactionStrategy`, `TailCompactionStrategy`, `FallbackCompactionStrategy`, `AbstractCompactionStrategy` | Full = rewrite entire head; Tail = compact only the diff since the last compacted state; Fallback chains tail→full when the tail base is missing. |
| `CleanupStrategy`, `DefaultCleanupStrategy`, `CleanupFirstCompactionStrategy`, `DefaultCleanupContext` | Mark/sweep of tar entries by GC generation. |
| `Reclaimers.java` | Predicates over `GCGeneration` deciding segment reclaimability: `newOldReclaimer` (full/tail variants honoring `retainedGenerations`), `newEmptyReclaimer`. |
| `FileReaper.java` | Deferred deletion of tar files marked reclaimable. |
| `GCIncrement.java`, `CompactionResult.java`, `EstimationResult.java`, `CompactedNodeState.java`, `CompactionWriter.java` | Value/helper types. |
| `GCMemoryBarrier.java` | Cancels GC when heap free % drops below `memoryThreshold`. |
| `GCNodeWriteMonitor.java` | Progress accounting (compacted nodes, estimated completion). |

### 2.3 Compactor implementations (root package)

| Class | Description |
|---|---|
| `ClassicCompactor.java` | Baseline: deep-clones a node state into the new generation without value sharing (except bulk segments backing binaries). Single-threaded. |
| `CheckpointCompactor.java` | Checkpoint-aware ("diff"): rebases checkpoints and root on top of each other in chronological order and caches compacted checkpoints for deduplication — avoids rewriting shared checkpoint content repeatedly. |
| `ParallelCompactor.java` | Extends checkpoint-aware approach; explores tree breadth-first until `EXPLORATION_LOWER_LIMIT = 10_000` nodes (line 64), then compacts subtrees in parallel with `concurrency` threads. |
| `Compactor.java` | Common contract; documents "down compaction" (partial, soft-cancellable, mixed generations possible) vs "up compaction" (all-or-nothing, uniform generation). |
| `CheckpointCompactor` vs `LegacyCheckpointCompactor.java` | Legacy variant kept for comparison/compat. |
| `CancelableDiff.java`, `CompactorUtils.java` | Support classes. |

Consequence for readers: after online GC, segments of multiple GC generations coexist; a
partially "down-compacted" root can reference records across generations. Readers must not
assume generation homogeneity.

---

## 3. Tooling commands (oak-run segment tooling, `tool/` package)

All commands are exposed as oak-run sub-commands; the classes here are the engine, oak-run
provides argument parsing. Every builder requires `withPath(File)` pointing at a segment-store
directory (validated by `tool/Utils.isValidFileStore`: must be a directory containing
`journal.log`). Read-only opens honor system properties `tar.memoryMapped` (default false) and
`cache` (segment cache MB, default 256) (`tool/Utils.java` lines 43–45).

### 3.1 Check — `tool/Check.java` + `tool/check/CheckHelper.java` (oak-run `check`) — RO

Consistency-checks an existing store. Opens a `ReadOnlyFileStore` plus a `JournalReader` and
walks revisions newest→oldest until a fully consistent revision is found, traversing every node
and property (optionally full binary content). Reports the latest good revision per scope
(head and each checkpoint) and an overall good revision.

Options (builder methods): `withMmap(boolean)`, `withJournal(File)` (defaults to
`<path>/journal.log`), `withDebugInterval(long seconds)` (progress print period, default
`Long.MAX_VALUE`), `withCheckBinaries(boolean)`, `withCheckHead(boolean)`,
`withCheckpoints(Set<String>)` (`"all"` or specific ids; default all), `withFilterPaths(Set)`
(default `/`), `withRevisionsCount(Integer)` (default `Integer.MAX_VALUE` in code; oak-run
passes 1 by default), `withIOStatistics(boolean)` (prints segment-read op count/bytes/ns via an
`IOMonitorAdapter`), `withFailFast(boolean)`, `withRepositoryStatistics(...)` (collects head
node/property counts). Exit code 0 iff a good revision exists.

### 3.2 Compact — `tool/Compact.java` (oak-run `compact`) — RW

Offline compaction. Opens a **writable** `FileStore` with `defaultGCOptions().setOffline()`,
runs `compactFull()` or `compactTail()` (`GCType`, default FULL), then `cleanup()`, then
**rewrites `journal.log` to a single line** `"<revision> root <currentTimeMillis>"` (truncate +
write, lines 338–348). Options: `withMmap(Boolean)` (tri-state; on Windows regular file access
is always enforced — `newFileAccessMode`, line 234), `withForce(boolean)` (disables strict
manifest version check ⇒ irreversibly upgrades store version 1→2), `withSegmentCacheSize(int)`
(default 256 MB), `withGCLogInterval(long)` (default 150000 nodes), `withGCType(FULL|TAIL)`,
`withCompactorType(classic|diff|parallel)` (default PARALLEL_COMPACTOR),
`withConcurrency(int)` (default 1; oak-run defaults to available processors). Prints
before/after file listings and sizes. Exit 0 on success, 1 on cancellation/failure.

### 3.3 Backup — `tool/Backup.java` (oak-run `backup`) — RO on source, RW on target

Opens source as `ReadOnlyFileStore` and delegates to `FileStoreBackupImpl.backup(reader,
revisions, targetDir)` (module `oak-backup`): incremental content-level copy of the current head
into the target store, followed by cleanup of the target. Option: `withFakeBlobStore(boolean)`
(default from system property `oak.backup.UseFakeBlobStore`) — simulates a file blob store so
external-blob references survive the copy.

### 3.4 Restore — `tool/Restore.java` (oak-run `restore`) — RW

Counterpart of Backup: `FileStoreRestoreImpl.restore(source, target)` copies the backup's head
state back into an existing store. Options: only `withSource(File)` / `withTarget(File)`.

### 3.5 DebugTars — `tool/DebugTars.java` (oak-run `debug PATH file.tar…`) — RO

For each named `*.tar` file: prints which content paths in the current head reference segments
contained in that tar (walks the whole head tree comparing each record's segment UUID against
the tar's index, including template records and bulk-segment references of binaries), and prints
the tar's segment reference graph (`store.getTarGraph(name)`). Option: system property
`max.char.display` (default 60) truncates printed string values.

### 3.6 DebugSegments — `tool/DebugSegments.java` (oak-run `debug PATH items…`) — RO

Argument grammar (regex `SEGMENT_REGEX`, line 47:
`([0-9a-f-]+)|(([0-9a-f-]+:[0-9a-f]+)(-([0-9a-f-]+:[0-9a-f]+))?)?(/.*)?`):
- bare segment UUID → prints the segment dump (`SegmentId.getSegment()` → `Segment.toString()`);
- `uuid:recordNumber[/path]` → reads that node record and prints each node down the path with
  its record id (note: record numbers here are parsed as **hex**);
- `uuid:rec1-uuid:rec2[/path]` → JSOP diff between the two node records at the path.
Without a record id, the head revision is the starting node.

### 3.7 DebugStore — `tool/DebugStore.java` (oak-run `debug PATH`) — RO

Whole-store statistics: iterates all segment ids; counts/sizes data vs bulk segments; runs
`RecordUsageAnalyser` over every NODE record of every data segment; then computes reachability
from the head's segment through the segment reference graph and reports the size/count of
unreachable ("available for garbage collection") data and bulk segments.

### 3.8 Diff — `tool/Diff.java` + `tool/PrintingDiff.java` (oak-run `tarmkdiff --diff`) — RO+out

Prints a content diff between two revisions given as `left..right` (record ids in the
`uuid:recordNumber` form; case-insensitive placeholder `head` = current head). Diffs the
`root` child of each super-root, optionally restricted with `withFilter(path)`. With
`withIncremental(true)`, diffs every successive revision pair in the journal between the two
endpoints (revisions listed by a `Revisions.RevisionsProcessor`, normally
`Utils.readRevisions`). `withIgnoreMissingSegments(true)` continues past
`SegmentNotFoundException` (which is otherwise recorded as `#SNFE <id>` and stops the run).
Output goes to a mandatory output file.

### 3.9 History — `tool/History.java` (oak-run `history`) — RO

Prints how a node changed over the revisions in the journal, via
`file/tooling/RevisionHistory.getHistory(journalFile, nodePath)`. Options: `withJournal(File)`
(required), `withNode(String path)` (required by builder; oak-run defaults to `/`),
`withDepth(int)` (default 0 = node only; >0 prints subtree content to that depth).

### 3.10 Revisions — `tool/Revisions.java` (oak-run `tarmkdiff --list`) — RO+out

Writes the list of revisions found in `journal.log` (via the supplied `RevisionsProcessor`,
normally `Utils.readRevisions` which returns each journal line's revision field, newest first)
to an output file.

### 3.11 RecoverJournal — `tool/RecoverJournal.java` (oak-run `recover-journal`) — RW (journal only)

Rebuilds `journal.log` by scanning content:
1. For every **data** segment, parses the segment-info JSON and takes property `"t"` as the
   timestamp (`Utils.parseSegmentInfoTimestamp`); segments without a timestamp are skipped with
   a warning.
2. Every NODE record whose node state has both child `root` **and** child `checkpoints` is a
   candidate head state (`recoverEntries`, line 334).
3. Sorts candidates by (timestamp asc, segmentId, recordNumber asc).
4. From newest backwards, runs `ConsistencyChecker.checkTreeConsistency` on the head **and every
   checkpoint** (the official docs claim checkpoints are not checked — the code
   `RecoverJournal.recoverEntries` lines 277–294 does check them); inconsistent revisions are
   dropped until the newest surviving entry is fully consistent.
5. Backs up the old journal to `journal.log.bak.NNN` (NNN = 000–999; fails if all taken) and
   writes the recovered entries as `"<recordId-toString10> root <timestamp>"` lines (oldest
   first). Rolls the backup back on write failure.
Duplicate `SegmentNotFoundException`s are reported once per segment id.

### 3.12 SearchNodes — `tool/SearchNodes.java` — RO

Scans every NODE record in every data segment and prints those matching all configured
matchers: `withProperty(name)`, `withChild(name)`, `withValue(name, value)` (string or
string-array equality). Output formats (`Output` enum): `TEXT` (`timestamp\trecordId`) or
`JOURNAL` (`recordId root timestamp` — i.e. lines suitable for a journal file). Timestamps come
from segment-info `"t"` as in RecoverJournal.

### 3.13 IOTrace — `tool/iotrace/` (oak-run `iotrace`) — RO+out

Collects CSV IO traces (`timestamp,file,segmentId,length,elapsed`) of segment reads triggered
by a synthetic access pattern (`IOTracer` + `IOTraceMonitor` plugged in as `IOMonitor`).
Patterns: `BreadthFirstTrace` (BFS to a depth), `DepthFirstTrace` (DFS to a depth),
`RandomAccessTrace` (random access over a path list with a seed). oak-run options: `--trace
DEPTH|BREADTH|RANDOM`, `--depth` (default 5), `--path` (default `/root`), `--paths` file,
`--seed`, `--count` (default 1000), `--mmap` (default true), `--segment-cache` (default 256 MB),
`--output` (default `iotrace.csv`).

### 3.14 Related oak-run commands implemented outside this module

`segment-copy` (persistence-to-persistence translation incl. all revisions, in
`oak-segment-azure`/`oak-run` tooling — see oak-doc overview.md §Segment-Copy) and `explore`
(GUI over `Proc`/`ReadOnlyFileStore`). Listed for completeness of the oak-run segment surface.

---

## 4. Replication / cold standby (`standby/` package)

Cold-standby replication streams segments from a primary to a standby instance over a
Netty-based TCP protocol (optionally TLS). The primary side is read-only; the standby side
writes fetched segments into its own `FileStore`.

### 4.1 Server (primary) — `standby/server/`

`StandbyServerSync` (builder: `port`, `fileStore`, `blobChunkSize`, `allowedClientIPRanges`,
`secure` + SSL key/chain/subject-pattern options) starts `StandbyServer`, which answers four
request types with dedicated handlers backed by `Default*Reader` classes:
- get head (`DefaultStandbyHeadReader` — current head record id),
- get segment by id (`DefaultStandbySegmentReader` — raw segment bytes),
- get blob by reference (`DefaultStandbyBlobReader` — chunked via `ChunkedBlobStream`),
- get referenced segment ids (`DefaultStandbyReferencesReader`).
Client filtering by IP range (`ClientIpFilter`). Observation hooks (`RequestObserverHandler`,
`ResponseObserverHandler`) feed `CommunicationObserver` (§5). RO with respect to the primary
store.

### 4.2 Client (standby) — `standby/client/`

`StandbyClientSync` (options: `host`, `port`, `readTimeoutMs`, `autoClean` — run cleanup after
sync when the store grew, `spoolFolder` for large blob spooling, `secure` + SSL options; client
id from system property `standbyID`, constant `CLIENT_ID_PROPERTY_NAME`, line 127) runs
`StandbyClientSyncExecution`: compares the local head with the primary's head, then walks the
segment reference graph fetching missing segments (and blobs via `RemoteBlobProcessor`) before
atomically advancing the local head. Mutates the standby store. Failure modes surface as
`BlobFetchTimeoutException`, `BlobTypeUnknownException`, `BlobWriteException`.

### 4.3 Wire protocol — `standby/codec/`

`Messages.java`: requests are ASCII lines `"Standby-CMD@<clientId>:<body>\r\n"` with bodies
`GET_HEAD = "h"`, `GET_SEGMENT = "s."+uuid`, `GET_BLOB = "b."+ref`, `GET_REFERENCES = "r."+uuid`.
Response frames are typed by a header byte: `HEADER_RECORD = 0x00`, `HEADER_SEGMENT = 0x01`,
`HEADER_BLOB = 0x02`, `HEADER_REFERENCES = 0x03`. Segment/blob payloads carry a hash for
integrity (`HashUtils`). Encoders/decoders per message type; `ResponseDecoder` validates
payloads.

### 4.4 OSGi / JMX

`standby/store/StandbyStoreService.java` registers primary or standby mode from OSGi config.
JMX: `StandbyStatusMBean` (`org.apache.jackrabbit.oak:name=Status,type="Standby"`; mode,
status in {initializing, stopped, starting, running, closing, closed}, start/stop),
`ClientStandbyStatusMBean` (per-client sync status, failed requests, seconds since last
success), `ObservablePartnerMBean`/`CommunicationPartnerMBean` (per-partner transfer stats).

---

## 5. Monitoring (JMX beans and stats)

| Bean / class | Type constant | What it exposes | Mutating operations |
|---|---|---|---|
| `SegmentNodeStoreStats.java` implements `SegmentNodeStoreStatsMBean` + `SegmentNodeStoreMonitor` | `"SegmentStoreStats"` | Commit metrics: commits count, queue size, commit/queueing times (time series `COMMITS_COUNT`, `COMMIT_QUEUE_SIZE`, `COMMIT_TIME`, `QUEUEING_TIME`), per-writer-group tables, currently queued writers, current writer; via `CommitsTracker.java`. | Config only: `setCollectStackTraces`, `setNumberOfOtherWritersToDetail`, `setWriterGroupsForLastMinuteCounts`. |
| `file/FileStoreStats.java` implements `FileStoreStatsMBean` | `"FileStoreStats"` | `getApproximateSize()`, `getTarFileCount()`, `getSegmentCount()`, journal write count, `fileStoreInfoAsString()`. | None. |
| `compaction/SegmentRevisionGC.java` (impl `SegmentRevisionGCMBean.java`) | `"SegmentRevisionGarbageCollection"` | Full GC control surface: pause, retryCount, forceTimeout, retainedGenerations, sizeDeltaEstimation, estimationDisabled, GC type, memoryThreshold, progress log; status: last compaction/cleanup times, last repo/reclaimed size, last error/log message, running flag, compacted nodes, estimated completion %. | `startRevisionGC()`, `cancelRevisionGC()`, all setters — these mutate the store when GC runs. |
| `SegmentCheckpointMBean.java` | (Oak `CheckpointMBean`) | Lists/creates/releases checkpoints of the `SegmentNodeStore`. | Create/release mutate. |
| `file/FileStoreGCMonitor.java` | GCMonitor | Timestamped GC lifecycle events for JMX/logging. | None. |
| `SegmentBufferMonitor.java` | metrics | Segment write-buffer allocation stats. | None. |
| `spi/monitor/IOMonitor` (+ `file/MetricsIOMonitor.java`), `MetricsRemoteStoreMonitor.java` | metrics | Per-segment read/write latencies and counts, remote-store request stats. | None. |
| `RecordCacheStats.java`, `CacheAccessTracker.java` | metrics | Deduplication/reader cache hit statistics. | None. |
| `standby/store/CommunicationObserver.java` + partner MBeans | see §4.4 | Standby transfer observability. | None. |
| `SegmentNodeStoreMonitorService.java` | OSGi | Wires `SegmentNodeStoreMonitor` config (stack-trace collection, writer groups). | Config only. |

---

## Appendix: read-only safety summary for a Rust port

Safe to model after: `ReadOnlyFileStore` + `ReadOnlyRevisions` + `JournalReader` +
`ConsistencyChecker`-style traversal (Check, DebugTars, DebugSegments, DebugStore, Diff,
History, Revisions, SearchNodes, iotrace are all built exclusively on these; Backup reads via
the same path).

Store-mutating features (out of scope for a read-only port): `FileStore` writes/flushes,
Compact, Restore, RecoverJournal (journal file only), all GC (§2), standby client sync,
checkpoint create/release, `SegmentRevisionGC.startRevisionGC()`.
