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
| **Planned** | Read-only feature that fits `froe`'s scope; a natural next step. |
| **Write path** | Mutates the store. Out of scope while `froe` is read-only by design. |
| **Not applicable** | JVM-, OSGi-, or cluster-specific machinery with no Rust counterpart. |

`froe`'s initial goal is deliberately narrow and deep: open an existing
TarMK repository *read-only* — including a live one, since no lock is
taken and no file is ever written — resolve the head state, and traverse
and extract node data fast.

## 1. Core library

### 1.1 Node store API (`SegmentNodeStore`)

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Root access | `SegmentNodeStore.getRoot()` | `Repository::content_root` | **Implemented** |
| Node traversal | `SegmentNodeState`, `getChildNode`, `getChildNodeEntries` | `NodeState::child_node`, `NodeState::child_node_entries` | **Implemented** |
| Properties | `SegmentPropertyState`, typed accessors | `NodeState::properties`, `NodeState::property`, typed `PropertyValue` | **Implemented** |
| `jcr:primaryType` / `jcr:mixinTypes` synthesis from templates | `Template` head fields | `Template::primary_type`, `Template::mixin_types`, synthesized in `NodeState::properties` | **Implemented** |
| Checkpoint listing and reading | `checkpoints()`, `retrieve(String)` | `Repository::checkpoints` (each checkpoint's `root` child is a full snapshot) | **Implemented** |
| Checkpoint create/release | `checkpoint(long)`, `release(String)` | — | **Write path** |
| Commits, rebase, reset | `merge`, `rebase`, `reset` | — | **Write path** |
| Observation | `Observable.addObserver` | — | **Write path** (only meaningful on a mutating store) |
| Blob creation | `createBlob(InputStream)` | — | **Write path** |
| Blob reading (inline) | `SegmentBlob`, `SegmentStream` | `BinaryValue::Inline`, `content::value::read_binary_content` | **Implemented** |
| Blob reading (external blob store) | `BlobStore` integration | `BinaryValue::External` exposes the blob identifier; content requires the external store | **Implemented** (identifier surface); external blob store connectors **Planned** |

### 1.2 Stores

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Read-only store over tar archives | `ReadOnlyFileStore` + `ReadOnlyRevisions` | `Repository::open` | **Implemented** |
| Head resolution with journal rewind | `FileStoreUtil.findPersistedRecordId` | journal scan in `Repository::open` | **Implemented** |
| Manifest validation (`store.version` 1 and 2) | `ManifestChecker` (read-only semantics) | `store::check_manifest` | **Implemented** |
| Tar file generation selection (highest letter per archive number) | `TarReader.openRO` | `tar_archive::select_newest_file_generations` | **Implemented** |
| Recovery of archives without a valid index | `TarReader.openRO` third strategy (writes `.ro.bak`) | in-memory recovery scan — same algorithm, but never writes into the repository | **Implemented** (deliberate improvement: strictly no writes) |
| Time travel to an arbitrary revision | `ReadOnlyFileStore.setRevision` | `Repository::node` accepts any record identifier; journal entries are exposed | **Implemented** (as a library primitive; a `--revision` command option is **Planned**) |
| Writable store, flushing | `FileStore` | — | **Write path** |
| Memory mapping | `FileStoreBuilder.withMemoryMapping` | always memory-mapped (`memmap2`) | **Implemented** |
| In-memory store for tests | `memory/MemoryStore` | `SegmentProvider` trait; tests use an in-memory implementation | **Implemented** |
| Pluggable persistence (Azure, AWS) | `spi/persistence` | the `SegmentProvider` trait is the seam a remote backend would implement | **Planned** |

### 1.3 Reading and caching

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Segment parsing (versions 12 and 13) | `Segment`, `SegmentDataV12/V13` | `segment::ParsedSegment` | **Implemented** |
| Record decoding (values, lists, maps, templates, nodes) | `CachingSegmentReader`, `MapRecord`, `ListRecord`, `Template` | `content` module | **Implemented** |
| Segment cache | `SegmentCache` (256 MB default) | bounded parsed-segment cache | **Implemented** |
| String cache | `StringCache` | bounded string cache on `Repository` | **Implemented** |
| Template cache | `TemplateCache` | bounded template cache on `Repository` | **Implemented** |
| Streaming reads of large binaries | `SegmentStream` | `read_binary_content` materializes; streaming reader | **Planned** |
| Record-level visitor / grammar walker | `SegmentParser` | record readers are public functions usable the same way | **Implemented** |
| Record space usage analysis | `RecordUsageAnalyser` | — | **Planned** (`froe segment` prints per-type record counts today) |
| Segment hex dump | `SegmentDump` | — | **Planned** |

### 1.4 Journal and metadata files

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Journal reading (backwards, tolerant) | `JournalReader` | `journal::read_journal` | **Implemented** |
| Record identifier string forms (`uuid:decimal` and `uuid.hex8`) | `RecordId.fromString` | `journal::parse_record_identifier_text` | **Implemented** |
| `gc.log` parsing | `GCJournal` | — | **Planned** (informational display) |
| Repository lock | `TarPersistence.lockRepository` | never taken — `froe` is read-only | **Implemented** (by design) |

## 2. Garbage collection and compaction

The entire garbage collection subsystem mutates the store and is out of
scope for a read-only reader. What a reader *must* understand — and
`froe` does — is the residue garbage collection leaves behind:

| Feature | Java | froe | Status |
| --- | --- | --- | --- |
| Reading generation metadata (generation, full generation, compacted flag) | segment headers, index entries | `ParsedSegment` and `SegmentIndexEntry` expose all three | **Implemented** |
| Tolerating mixed generations after partial compaction | reader-side invariants | no generation homogeneity is assumed anywhere | **Implemented** |
| Stable identifiers across compaction | `SegmentNodeState.getStableId` | `NodeState::stable_identifier` | **Implemented** |
| Estimation, compaction (full/tail; classic/diff/parallel), cleanup, `Reclaimers`, `FileReaper`, GC monitoring | `compaction/`, `file/GarbageCollector` | — | **Write path** |

## 3. Tooling (oak-run segment commands)

| oak-run command | Java | froe command | Status |
| --- | --- | --- | --- |
| — (overview) | — | `froe summary` | **Implemented** (no direct oak-run equivalent; closest is `FileStoreStats`) |
| `tarmkdiff --list` (revisions) | `tool/Revisions` | `froe journal` | **Implemented** |
| `debug PATH` (store statistics) | `tool/DebugStore` | `froe archives`, `froe segments` | **Implemented** (per-archive and per-segment statistics; reachability analysis **Planned**) |
| `debug PATH file.tar` | `tool/DebugTars` | `froe archives` (graph and binary references parsing exist in the library) | Partially **Implemented**; path-to-tar attribution **Planned** |
| `debug PATH uuid:record/path` | `tool/DebugSegments` | `froe segment`, `froe node` | **Implemented** (segment structure and node display; node-record diffing **Planned**) |
| `check` | `tool/Check` | — | **Planned** (`froe check`: walk revisions newest to oldest, verify full traversability, report the newest consistent revision) |
| `history` | `tool/History` | — | **Planned** (`froe history`: a node's states across journal revisions) |
| `tarmkdiff --diff` | `tool/Diff` | — | **Planned** (`froe difference` between two revisions) |
| `search-nodes` | `tool/SearchNodes` | — | **Planned** (scan all node records matching property/child filters) |
| `iotrace` | `tool/iotrace` | — | **Not applicable** (measures the Java store's IO behavior) |
| `recover-journal` | `tool/RecoverJournal` | — | **Write path** (rewrites `journal.log`); a dry-run variant that only *reports* recoverable revisions is **Planned** |
| `compact` | `tool/Compact` | — | **Write path** |
| `backup` / `restore` | `tool/Backup`, `tool/Restore` | `froe extract` covers content-level export; store-level backup is a file copy | **Write path** (store-level); content export **Implemented** |
| `explore` (GUI) | `oak-run` + `file/proc/Proc` | terminal equivalents: `froe tree`, `froe node` | **Implemented** (terminal form) |
| `segment-copy` (remote persistences) | `oak-segment-azure` tooling | — | **Planned** alongside remote persistence support |

### The `froe` command surface today

| Command | Purpose |
| --- | --- |
| `froe summary REPOSITORY` | Archives, segment counts, journal size, head revision, checkpoints. |
| `froe journal REPOSITORY [--limit N]` | Revisions newest first, with validity annotations. |
| `froe archives REPOSITORY` | Per-archive size, segment count, index version or recovery state. |
| `froe segments REPOSITORY` | Every segment with kind, size, and generation data. |
| `froe segment REPOSITORY UUID` | One segment's header, referenced segments, record statistics. |
| `froe node REPOSITORY PATH` | One node: record identifiers, typed properties, children. |
| `froe tree REPOSITORY [PATH] [--depth N]` | Indented content tree with primary types. |
| `froe checkpoints REPOSITORY` | Checkpoint names with creation and expiry times. |
| `froe extract REPOSITORY [--path P] [--depth N] [--output FILE]` | Stream the subtree as JSON lines, one node per line. |

## 4. Replication and cold standby

The `standby/` package (Netty-based primary/standby segment streaming,
TLS, JMX status beans) exists to keep a second *writing* store in sync.

| Feature | Status |
| --- | --- |
| Standby client (writes segments into a local store) | **Write path** |
| Standby server (serves segments from a primary) | **Planned** — serving segments read-only fits `froe`'s model and would let a Rust process act as a seed/mirror source; low priority |

## 5. Monitoring

JMX beans (`SegmentNodeStoreStats`, `FileStoreStats`, `SegmentRevisionGC`,
checkpoint and standby beans) instrument a running Java store.
**Not applicable** as JMX; the equivalent *facts* a reader can compute —
archive counts and sizes, segment counts, journal length — are exposed by
the `froe` library API and `froe summary`.

## 6. Using `froe` as a Rust dependency

```rust
use froe::store::Repository;

fn main() -> froe::Result<()> {
    let repository = Repository::open(std::path::Path::new("/path/to/segmentstore"))?;
    let content_root = repository.content_root()?;
    for (name, child) in content_root.child_node_entries()? {
        println!("{name}: {} children", child.child_node_count()?);
    }
    if let Some(node) = repository.node_at_path("/content")? {
        for property in node.properties()? {
            println!("{} = {:?}", property.name, property.values);
        }
    }
    Ok(())
}
```

The crate exposes each layer independently: `tar_archive` (archives,
indexes, graphs, binary reference catalogs), `segment` (segment parsing
and addressing), `content` (records to node states), `journal`, and
`store` (the assembled repository). Custom stores — in-memory fixtures,
remote backends — implement the `SegmentProvider` trait and reuse the
whole content layer unchanged.
