# froe

A fast, dependency-light Rust implementation of Apache Jackrabbit Oak's
`segment-tar` ("TarMK") storage format — the repository format used by
Apache Jackrabbit Oak and Adobe Experience Manager.

`froe` opens a segment store directly from disk, without a running Oak
instance and without the JVM's startup and garbage collection overhead:

* **Reading** is read-only and safe against a *live* repository — it never
  takes the repository lock and never writes. Traverse the content tree,
  export node data as JSON lines, Parquet, or SQLite, inspect archives and
  segments, check consistency, diff revisions, trace node history, and
  search nodes.
  Like Oak itself, the reader memory-maps archives and relies on the
  store's file protocol (existing archive bytes are never modified in
  place); a process that truncates or rewrites archives outside that
  protocol would disturb froe and a running Oak instance alike.
* **Writing** takes the exclusive repository lock and produces stores
  byte-for-byte compatible with what Oak writes, so a subsequent AEM start
  consumes the result cleanly. Compact offline, back up and restore,
  safely clean orphaned storage and stale metadata, recover a lost journal,
  and manage checkpoints — against a *stopped* repository.

The writing path reproduces every invariant Oak depends on — locking,
durability ordering, generation arithmetic, archive and trailer layout —
verified against byte-exact specifications extracted from the Oak sources.
(One documented rendering residue: a handful of extreme-subnormal doubles
re-render during compaction to a different — equally round-tripping —
shortest form than Java's; see `double_to_text`.)

> **Maintenance commands are beta.** The write path is verified against
> byte-exact specifications extracted from the Oak sources and an
> extensive test suite, but has not yet been validated end-to-end against
> stores produced by — or consumed by — a real Oak/AEM instance. Until
> that interoperability round-trip lands, take a copy of your repository
> before running any maintenance command against data you care about.
> The read-only commands never write anything and carry no such caveat.

## Platform support

Rust 1.89 or newer. Linux and macOS are fully supported and CI-tested;
other Unixes build with a classic POSIX `fcntl` lock in the same lock
space Oak uses, but are not CI-verified. On Windows the reading API works;
the writing API refuses to open because segment identifiers require an
operating system entropy source, which froe currently reads only from
`/dev/urandom`.

### Repository format versions

Reading supports both segment-tar manifest versions: `store.version=1`
(Oak 1.6, AEM 6.3 and earlier) and `store.version=2` (Oak 1.8 and later,
so AEM 6.4 onwards). Maintenance is scoped to version two, and that is a
deliberate choice rather than an omission.

Every AEM line still in support has written version two for roughly eight
years, so a version-one store encountered today is almost always an
archive: a decommissioned instance, an old backup, a repository someone
needs to extract content from. That work is reading, which is supported and
carries no caveat. Spending the interoperability and maintenance-hardening
effort on version two puts it where real repositories are.

Cleanup is the one maintenance path that touches a version-one store, and
only conditionally: if a run would write version-two state, it first
upgrades the manifest. That upgrade is one-way, appears as an explicit
action in the read-only preview before anything is confirmed, and is
described in [`docs/cleanup.md`](docs/cleanup.md). The journal,
stale-archive, stale-temporary, and recovery-backup passes never upgrade.
If you need to keep a version-one store readable by the Oak that created
it, copy it before running cleanup.

## Quick start

```console
$ cargo build --release

# Read-only — safe against a live repository:
$ target/release/froe summary /path/to/segmentstore
$ target/release/froe tree /path/to/segmentstore /content --depth 2
$ target/release/froe export /path/to/segmentstore --path /content --output content.jsonl
$ target/release/froe export /path/to/segmentstore --path /content --format parquet --output ./export
$ target/release/froe check /path/to/segmentstore
$ target/release/froe segment /path/to/segmentstore SEGMENT-UUID --hex
$ target/release/froe debug /path/to/segmentstore data00000a.tar

# Maintenance — stopped repository only (mutating forms ask for confirmation):
$ target/release/froe cleanup /path/to/segmentstore --dry-run
$ target/release/froe cleanup /path/to/segmentstore
$ target/release/froe compact /path/to/segmentstore
$ target/release/froe backup /path/to/segmentstore /path/to/backup
$ target/release/froe recover-journal /path/to/segmentstore
```

Archive debug output follows Oak's UTF-16 STRING preview and full rendering for
other values; unavailable scalar external binaries appear as `{-1 bytes}`.
Each named archive is a separate read-only traversal, bounded by default to
250,000 retained attribution rows, 64 MiB of retained path/name/value text,
100,000,000 logical work units, 250,000 materialized children per node, and
16 MiB of stored child/template-name bytes per node, with at most 250,000
pending child visits, 250,000 graph rows, and 1,000,000 graph edges.
Crossing a bound is a typed refusal rather than partial successful output;
multiple archive arguments are not yet batched under one global work budget.

`froe cleanup` first prints a strictly read-only plan. When that plan contains
mutations, it then acquires the repository lock and rebuilds the plan before
applying it; an empty plan returns without taking the lock. If the locked plan
changed, it is printed and confirmed again. The conservative defaults preserve
every readable journal revision while pruning dangling or unreadable journal
entries, reclaiming eligible segments, removing expired checkpoints, and
cleaning only proven stale files. See the [cleanup guide](docs/cleanup.md) for
task selection, retention rules, resource expectations, and failure behavior.
When a planned checkpoint removal or segment-archive rewrite needs to write
version-two state, the plan also shows the one-way `store.version=1` to `2`
manifest upgrade before apply.

Every mutating command takes `repo.lock`. If that file is absent, froe first
creates and fsyncs a mode-`0600` staging inode, then publishes it with an
absent-only, same-directory hard link. Consequently, all mutating commands
require same-directory hard-link and durable directory-fsync support when
`repo.lock` has not already been created; unsupported filesystems fail without
falling back to an unsafe lock-creation sequence.

Archive rewrites performed by either `froe cleanup` or the cleanup phase of
`froe compact` publish validated successors with absent-only, same-directory
hard links. A filesystem that does not support those links cannot perform such
a rewrite; the operation fails with the original archive still active.

Every command takes the segment store directory (the one containing
`journal.log` and the `data*.tar` archives). `froe export` is the fast
path for pulling content out of a repository. Its default format,
`json-lines`, streams one JSON object per node — typed property values
included — to a file or standard output; `--format parquet` writes two
zstd-compressed Parquet tables (`nodes.parquet` and `properties.parquet`)
ready for analytical SQL in DuckDB, DataFusion, or Polars; `--format sqlite`
writes a single SQLite database (interned `nodes`/`properties` tables with
`node_paths` and `properties_expanded` views on top) for querying with any
SQLite client:

```console
$ duckdb -c "
  SELECT primary_type, count(*) AS nodes,
         rank() OVER (ORDER BY count(*) DESC) AS rank
  FROM './export/nodes.parquet'
  GROUP BY primary_type ORDER BY nodes DESC LIMIT 10;"
```

Re-running a Parquet export into the same directory does not start
over: the tables carry the head revision they were built from, so froe
diffs that revision against the current head and decodes only the
changed subtrees — a kept export stays queryable at Parquet speed for
the price of the delta. Each file is written out completely, forced to
disk, and swapped in atomically, keeping the previous file's Unix
permission bits, and a lock file serializes concurrent exports into
the same directory. The two files are swapped one after the other — a
completed run always leaves their stamps agreeing, but a crash or a
query mid-swap can observe a mixed pair. The next froe run detects the
disagreement and rebuilds; a query-side sanity check (diagnostic, not
a snapshot guarantee) can demand every stamp key, present and agreeing
in both files:

```console
$ duckdb -c "
  WITH required(key) AS (
    VALUES ('froe.format'), ('froe.revision'), ('froe.root_path'), ('froe.depth_limit')
  ), stamps AS (
    SELECT 'nodes' AS file, CAST(key AS VARCHAR) AS key, CAST(value AS VARCHAR) AS value
    FROM parquet_kv_metadata('./export/nodes.parquet')
    UNION ALL
    SELECT 'properties', CAST(key AS VARCHAR), CAST(value AS VARCHAR)
    FROM parquet_kv_metadata('./export/properties.parquet')
  ), per_key AS (
    SELECT required.key,
           count(stamps.value) AS entries,
           count(DISTINCT stamps.file) AS files,
           count(DISTINCT stamps.value) AS values
    FROM required LEFT JOIN stamps ON stamps.key = required.key
    GROUP BY required.key
  )
  SELECT bool_and(entries = 2 AND files = 2 AND values = 1) AS consistent_pair FROM per_key;"
```

A base whose stamped revision no longer resolves (the store was
compacted since, or the export belongs to a different repository —
the two are indistinguishable) is rebuilt only with an explicit
`--full`; files froe did not write, or an export of a different root
path or depth, are likewise never replaced uninvited. The `json-lines`
and `sqlite` formats keep the strict never-overwrite contract instead.

`froe --help` lists every command.

As a library:

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

## Workspace layout

| Crate | Purpose |
| --- | --- |
| [`crates/froe`](crates/froe) | The core library: tar archive parsing, segment and record decoding, journal handling, and the node traversal API. Published to crates.io as `froe`. |
| [`crates/froe-export`](crates/froe-export) | Content tree export: one traversal, pluggable format sinks — JSON lines, Parquet behind the `parquet` feature, SQLite behind the `sqlite` feature. |
| [`crates/froe-cli`](crates/froe-cli) | The `froe` command-line interface built on the core library. |

## Documentation

* [`docs/oak-segment-tar-feature-map.md`](docs/oak-segment-tar-feature-map.md)
  — the complete feature inventory of `oak-segment-tar` and how each feature
  maps to `froe` (implemented, planned, or intentionally out of scope).
* [`docs/storage-format.md`](docs/storage-format.md)
  — the on-disk format as implemented by this workspace: tar archive layout,
  segment headers, and every record encoding.
* [`docs/cleanup.md`](docs/cleanup.md)
  — the safety model, default and opt-in tasks, retention rules, and examples
  for offline repository cleanup.

## Relationship to Apache Jackrabbit Oak

`froe` is an independent implementation of the storage format defined and
maintained by the [Apache Jackrabbit Oak](https://jackrabbit.apache.org/oak/)
project in its
[`oak-segment-tar`](https://github.com/apache/jackrabbit-oak/tree/trunk/oak-segment-tar)
module. It is not a byte-for-byte translation of the Java code: the format
semantics are preserved exactly, while the implementation follows Rust idioms.
Apache Jackrabbit Oak is a trademark of the Apache Software Foundation.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
