# oak-segment-tar feature map

This document inventories every feature of Apache Jackrabbit Oak's
[`oak-segment-tar`](https://github.com/apache/jackrabbit-oak/tree/trunk/oak-segment-tar)
module — the TarMK storage engine used by Apache Jackrabbit Oak and Adobe
Experience Manager — and maps each feature to `froe`. It was produced by
a systematic analysis of the Java sources (the `org.apache.jackrabbit.oak.segment`
package tree) and the official storage documentation.

Every entry carries one of four statuses:

| Status | Meaning |
| --- | --- |
| **Implemented** | Available in `froe` today (crate `froe`, binary `froe`). |
| **Planned** | Fits `froe`'s scope; a natural next step. |
| **Not applicable** | JVM-, OSGi-, or cluster-specific machinery with no Rust counterpart. |

`froe` reads and writes TarMK repositories. The reading API is read-only
and safe against a live repository (no lock, no writes). The writing API
takes the exclusive repository lock and produces stores byte-for-byte
compatible with what Oak writes, so it must run only against a *stopped*
repository — after which a normal AEM start consumes the result cleanly.

## 1. Core library

### 1.1 Node store API (`SegmentNodeStore`)

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Root access | `SegmentNodeStore.getRoot()` | `Repository::content_root` | **Implemented** |
| Node traversal | `SegmentNodeState`, `getChildNode`, `getChildNodeEntries` | `NodeState::child_node`, `NodeState::child_node_entries` | **Implemented** |
| Properties | `SegmentPropertyState`, typed accessors | `NodeState::properties`, `NodeState::property`, typed `PropertyValue` | **Implemented** |
| `jcr:primaryType` / `jcr:mixinTypes` synthesis from templates | `Template` head fields | synthesized in `NodeState::properties` | **Implemented** |
| Checkpoint listing and reading | `checkpoints()`, `retrieve(String)` | `Repository::checkpoints`, `writer::list_checkpoints` | **Implemented** |
| Checkpoint create / release / remove | `checkpoint(long)`, `release(String)` | `writer::create_checkpoint`, `release_checkpoint`, `remove_all_checkpoints`, `remove_unreferenced_checkpoints` | **Implemented** |
| Commit (content mutation) | `merge` | `writer::commit::rewrite_node_with_child_edits` and the record writer (single-writer, hookless) | **Implemented** |
| Rebase / reset, commit hooks, observation | `rebase`, `reset`, `EditorHook`, `Observable` | — | **Not applicable** (offline single-writer tooling applies no hooks; there is no concurrent committer to rebase against or observer to notify) |
| Blob creation | `createBlob(InputStream)` | `RecordWriter::write_binary_content` / `write_external_binary_identifier` | **Implemented** |
| Blob reading (inline) | `SegmentBlob`, `SegmentStream` | `BinaryValue::Inline`, `content::value::read_binary_content` | **Implemented** |
| Blob reading (external blob store) | `BlobStore` integration | `BinaryValue::External` exposes the blob identifier; content requires the external store | **Implemented** (identifier surface); external blob store connectors **Planned** |

### 1.2 Stores

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Read-only store over tar archives | `ReadOnlyFileStore` + `ReadOnlyRevisions` | `Repository::open` | **Implemented** |
| Read-write store lifecycle | `FileStore` | `writer::WritableRepository::open` | **Implemented** |
| Head resolution with journal rewind | `FileStoreUtil.findPersistedRecordId` | journal scan in both stores | **Implemented** |
| Manifest validation and update | `ManifestChecker` | `store::check_manifest` (read); rewrite on write open | **Implemented** |
| Tar generation selection (destructive on write) | `TarReader.open` / `openRO` | `tar_archive::select_newest_file_generations`; write-mode deletes stale letters | **Implemented** |
| Recovery of archives without a valid index | `TarReader` recovery (writes `.ro.bak`) | in-memory on read; write-mode backs up to `.bak` and regenerates | **Implemented** |
| Repository lock | `TarPersistence.lockRepository` (blocking `FileChannel.lock`) | `writer::RepositoryLock` (`fcntl` OFD lock, fails fast) | **Implemented** |
| Durability ordering (segment fsync before journal append) | `FileStore.doFlush` | `WritableRepository::flush` | **Implemented** |
| Initial-node bootstrap for a fresh store | `FileStore.initialNode` | `WritableRepository::write_initial_node` | **Implemented** |
| Segment identifier generation | `SegmentTracker.newSegmentId` | `writer::identifier_generator` (version-4 UUID, kind nibble) | **Implemented** |
| Memory mapping | `FileStoreBuilder.withMemoryMapping` | always memory-mapped (`memmap2`) | **Implemented** |
| In-memory store for tests | `memory/MemoryStore` | the `SegmentProvider` / `SegmentSink` traits | **Implemented** |
| Pluggable persistence (Azure, AWS) | `spi/persistence` | the `SegmentProvider` / `SegmentSink` traits are the seam | **Planned** |

### 1.3 Reading, writing, and caching

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Segment parsing (versions 12 and 13) | `Segment`, `SegmentDataV12/V13` | `segment::ParsedSegment` | **Implemented** |
| Segment building and flushing | `SegmentBufferWriter` | `writer::SegmentBufferBuilder` | **Implemented** |
| Record decoding (values, lists, maps, templates, nodes) | `CachingSegmentReader`, `MapRecord`, `ListRecord`, `Template` | `content` module | **Implemented** |
| Record serialization (all record kinds, HAMT maps, bulk block lists) | `RecordWriters`, `DefaultSegmentWriter` | `writer::RecordWriter` | **Implemented** |
| Segment-info record (`{"wid","sno","t"}`) | `SegmentBufferWriter.newSegment` | written as record 0 by `RecordWriter` | **Implemented** |
| Deduplication caches | `WriterCacheManager`, `PriorityCache` | source-record cache during compaction; content preservation by identity during commit | **Implemented** (correctness; a global write-side dedup cache is **Planned** as a size optimization) |
| Segment, string, and template caches | `SegmentCache`, `StringCache`, `TemplateCache` | bounded caches on `Repository` | **Implemented** |
| Streaming reads of large binaries | `SegmentStream` | `read_binary_content` materializes | **Planned** (streaming reader) |
| Record space usage analysis | `RecordUsageAnalyser` | `froe segment` prints per-type record counts | **Implemented** (summary form) |
| Segment hex dump | `SegmentDump` | — | **Planned** |

### 1.4 Journal and metadata files

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Journal reading (backwards, tolerant) | `JournalReader` | `journal::read_journal` | **Implemented** |
| Journal append and rewrite | `TarRevisions.doFlush`, `Compact` rewrite | `WritableRepository::flush`, compaction journal rewrite | **Implemented** |
| Record identifier string forms | `RecordId.fromString`/`toString10` | `journal::parse_record_identifier_text` | **Implemented** |
| `gc.log` parsing / writing | `GCJournal` | — | **Planned** (informational; not required for AEM safety) |

## 2. Garbage collection and compaction

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Reading generation metadata | segment headers, index entries | `ParsedSegment`, `SegmentIndexEntry` | **Implemented** |
| Tolerating mixed generations | reader-side invariants | no generation homogeneity assumed | **Implemented** |
| Stable identifiers across compaction | `SegmentNodeState.getStableId` | `NodeState::stable_identifier`, preserved through the tree copier | **Implemented** |
| Generation arithmetic (full/tail transitions) | `GCGeneration.nextFull/nextTail` | `writer::compaction` | **Implemented** |
| Offline compaction (deep copy into a fresh generation) | `Compact` + `ClassicCompactor` | `writer::compact` (`CompactionKind::Full` / `Tail`) | **Implemented** |
| Cleanup / reclaim predicate | `Reclaimers`, `DefaultCleanupStrategy`, `FileReaper` | `WritableRepository::reclaim_old_generations` (Oak's `newOldReclaimer` with one retained generation) | **Implemented** |
| Online GC, estimation, parallel/checkpoint compactors, memory barrier | `GarbageCollector`, `ParallelCompactor` | — | **Planned** (throughput optimizations; the offline deep copy produces an equivalent result) |

## 3. Tooling (oak-run segment commands)

| oak-run command | Java | froe command | Status |
| --- | --- | --- | --- |
| `check` | `tool/Check` | `froe check` | **Implemented** |
| `tarmkdiff --diff` | `tool/Diff` | `froe difference` | **Implemented** |
| `tarmkdiff --list` | `tool/Revisions` | `froe journal` | **Implemented** |
| `history` | `tool/History` | `froe history` | **Implemented** |
| `search-nodes` | `tool/SearchNodes` | `froe search-nodes` | **Implemented** |
| `compact` | `tool/Compact` | `froe compact` | **Implemented** |
| `backup` / `restore` | `tool/Backup`, `tool/Restore` | `froe backup`, `froe restore` (plus content export via `froe extract`) | **Implemented** |
| `recover-journal` | `tool/RecoverJournal` | `froe recover-journal` | **Implemented** |
| `checkpoints` | `oak-run checkpoints` | `froe checkpoint list/create/remove/remove-all/remove-unreferenced` | **Implemented** |
| `debug PATH` (store statistics) | `tool/DebugStore` | `froe summary`, `froe archives`, `froe segments` | **Implemented** (reachability analysis **Planned**) |
| `debug PATH uuid:record/path` | `tool/DebugSegments` | `froe segment`, `froe node` | **Implemented** |
| `explore` (GUI) | `oak-run` + `file/proc/Proc` | `froe tree`, `froe node` | **Implemented** (terminal form) |
| `debug PATH file.tar` | `tool/DebugTars` | graph and binary references parsing exist in the library | **Planned** (path-to-tar attribution) |
| `iotrace` | `tool/iotrace` | — | **Not applicable** (measures the Java store's IO behavior) |
| `segment-copy` (remote persistences) | `oak-segment-azure` tooling | — | **Planned** alongside remote persistence support |

### The `froe` command surface

Read-only (safe against a live repository):

| Command | Purpose |
| --- | --- |
| `froe summary REPOSITORY` | Archives, segment counts, journal size, head, checkpoints. |
| `froe journal REPOSITORY [--limit N]` | Revisions newest first, with validity annotations. |
| `froe archives REPOSITORY` | Per-archive size, segment count, index version or recovery state. |
| `froe segments REPOSITORY` | Every segment with kind, size, and generation data. |
| `froe segment REPOSITORY UUID` | One segment's header, references, and record statistics. |
| `froe node REPOSITORY PATH` | One node: record identifiers, typed properties, children. |
| `froe tree REPOSITORY [PATH] [--depth N]` | Indented content tree with primary types. |
| `froe checkpoints REPOSITORY` | Checkpoint names with creation and expiry times. |
| `froe extract REPOSITORY [--path P] [--depth N] [--output FILE]` | Stream the subtree as JSON lines. |
| `froe check REPOSITORY [--path P]… [--binaries]` | The newest fully consistent revision. |
| `froe difference REPOSITORY BEFORE AFTER [--path P]` | Changes between two revisions. |
| `froe history REPOSITORY PATH` | A node's record across journal revisions. |
| `froe search-nodes REPOSITORY [--has-property N]… [--value N=V]…` | Nodes matching predicates, over every segment. |

Mutating (stopped repository only; each confirms before proceeding):

| Command | Purpose |
| --- | --- |
| `froe compact REPOSITORY [--tail]` | Offline full or tail compaction with cleanup and journal rewrite. |
| `froe backup SOURCE TARGET` | Copy a repository's head into a target store. |
| `froe restore BACKUP TARGET` | Copy a backup's head into an existing store. |
| `froe recover-journal REPOSITORY` | Rebuild `journal.log` from the segments. |
| `froe checkpoint create/remove/remove-all/remove-unreferenced REPOSITORY` | Manage checkpoints. |

## 4. Replication and cold standby

The `standby/` package (Netty-based primary/standby segment streaming,
TLS, JMX status beans) keeps a second *writing* store in sync.

| Feature | Status |
| --- | --- |
| Standby client (writes segments into a local store) | **Planned** (the writer and segment provider are the building blocks) |
| Standby server (serves segments from a primary) | **Planned** — serving segments read-only fits `froe`'s model |

## 5. Monitoring

JMX beans (`SegmentNodeStoreStats`, `FileStoreStats`, `SegmentRevisionGC`)
instrument a running Java store. **Not applicable** as JMX; the equivalent
*facts* — archive counts and sizes, segment counts, journal length,
compaction outcomes — are exposed by the `froe` library API and by
`froe summary` and `froe compact`.

## 6. Using `froe` as a Rust dependency

Reading:

```rust
use froe::store::Repository;

fn main() -> froe::Result<()> {
    let repository = Repository::open(std::path::Path::new("/path/to/segmentstore"))?;
    if let Some(node) = repository.node_at_path("/content")? {
        for property in node.properties()? {
            println!("{} = {:?}", property.name, property.values);
        }
    }
    Ok(())
}
```

Writing (against a stopped repository):

```rust
use froe::{compact, CompactionKind, WritableRepository};

fn main() -> froe::Result<()> {
    let mut store = WritableRepository::open(std::path::Path::new("/path/to/segmentstore"))?;
    let outcome = compact(&mut store, CompactionKind::Full)?;
    println!("reclaimed {} bytes", outcome.size_before - outcome.size_after);
    store.close()
}
```

The crate exposes each layer independently: `tar_archive` (archives,
indexes, graphs, binary reference catalogs), `segment` (segment parsing,
addressing, and building), `content` (records to node states), `journal`,
`store` (the read-only repository), `writer` (the write path), and
`tooling` (check, diff, history, search). Custom backends implement the
`SegmentProvider` and `SegmentSink` traits and reuse the whole content and
writer layers unchanged.
