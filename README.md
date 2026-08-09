# froe

A fast, dependency-light Rust implementation of Apache Jackrabbit Oak's
`segment-tar` ("TarMK") storage format — the repository format used by
Apache Jackrabbit Oak and Adobe Experience Manager.

`froe` opens a segment store directly from disk, without a running Oak
instance and without the JVM's startup and garbage collection overhead:

* **Reading** is read-only and safe against a *live* repository — it never
  takes the repository lock and never writes. Traverse the content tree,
  export node data as JSON lines or Parquet, inspect archives and segments, check
  consistency, diff revisions, trace node history, and search nodes.
  Like Oak itself, the reader memory-maps archives and relies on the
  store's file protocol (existing archive bytes are never modified in
  place); a process that truncates or rewrites archives outside that
  protocol would disturb froe and a running Oak instance alike.
* **Writing** takes the exclusive repository lock and produces stores
  byte-for-byte compatible with what Oak writes, so a subsequent AEM start
  consumes the result cleanly. Compact offline, back up and restore,
  recover a lost journal, and manage checkpoints — against a *stopped*
  repository.

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

## Quick start

```console
$ cargo build --release

# Read-only — safe against a live repository:
$ target/release/froe summary /path/to/segmentstore
$ target/release/froe tree /path/to/segmentstore /content --depth 2
$ target/release/froe export /path/to/segmentstore --path /content --output content.jsonl
$ target/release/froe export /path/to/segmentstore --path /content --format parquet --output ./export
$ target/release/froe check /path/to/segmentstore

# Maintenance — stopped repository only (each asks for confirmation):
$ target/release/froe compact /path/to/segmentstore
$ target/release/froe backup /path/to/segmentstore /path/to/backup
$ target/release/froe recover-journal /path/to/segmentstore
```

Every command takes the segment store directory (the one containing
`journal.log` and the `data*.tar` archives). `froe export` is the fast
path for pulling content out of a repository. Its default format,
`json-lines`, streams one JSON object per node — typed property values
included — to a file or standard output; `--format parquet` writes two
zstd-compressed Parquet tables (`nodes.parquet` and `properties.parquet`)
ready for analytical SQL in DuckDB, DataFusion, or Polars:

```console
$ duckdb -c "
  SELECT primary_type, count(*) AS nodes,
         rank() OVER (ORDER BY count(*) DESC) AS rank
  FROM './export/nodes.parquet'
  GROUP BY primary_type ORDER BY nodes DESC LIMIT 10;"
```

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
| [`crates/froe-export`](crates/froe-export) | Content tree export: one traversal, pluggable format sinks — JSON lines, and Parquet behind the `parquet` feature. |
| [`crates/froe-cli`](crates/froe-cli) | The `froe` command-line interface built on the core library. |

## Documentation

* [`docs/oak-segment-tar-feature-map.md`](docs/oak-segment-tar-feature-map.md)
  — the complete feature inventory of `oak-segment-tar` and how each feature
  maps to `froe` (implemented, planned, or intentionally out of scope).
* [`docs/storage-format.md`](docs/storage-format.md)
  — the on-disk format as implemented by this workspace: tar archive layout,
  segment headers, and every record encoding.

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
