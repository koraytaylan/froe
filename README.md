# froe

Fast, read-only access to Apache Jackrabbit Oak `segment-tar` ("TarMK")
repositories — the storage format used by Apache Jackrabbit Oak and Adobe
Experience Manager — implemented in Rust.

`froe` opens a segment store directly from disk, resolves the current head
state from the journal, and lets you traverse and extract node data without a
running Oak instance and without the startup and garbage collection overhead
of the JVM. It never takes the repository lock and never writes to the store,
so it is safe to point at a live repository or a backup.

## Quick start

```console
$ cargo build --release

$ target/release/froe summary /path/to/segmentstore
$ target/release/froe tree /path/to/segmentstore /content --depth 2
$ target/release/froe extract /path/to/segmentstore --path /content --output content.jsonl
```

Every command takes the segment store directory (the one containing
`journal.log` and the `data*.tar` archives). `froe extract` streams one
JSON object per node — typed property values included — which is the
fast path for pulling content out of a repository.

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
