# froe-export

Exports Apache Jackrabbit Oak `segment-tar` ("TarMK") content trees to
flat, analysis-friendly formats, built on the [`froe`](../froe) core
library.

One depth-first traversal drives every format: `export_subtree` walks the
tree once and hands each node to an `ExportSink`, so a new output format
is a new sink, not a new traversal. Two sinks ship today:

* `JsonLinesSink` — one JSON object per node, the `froe export` default
  format.
* `ParquetSink` (behind the `parquet` feature) — two flat,
  zstd-compressed tables built for analytical SQL: `nodes` (one row per
  node: `path`, `parent_path`, `name`, `depth`, `primary_type`) and
  `properties` (one row per property value, multi-valued properties
  exploded with a `position`, typed value columns). Rows arrive in
  depth-first path order, so row-group statistics prune subtree
  predicates like `WHERE path LIKE '/content/dam/%'`. Query them with
  DuckDB, DataFusion, Polars, or anything else that reads Parquet.

Exporting is read-only and safe against a live repository. Binary
*content* is never embedded — binaries appear as an inline length or an
external blob reference.

```rust
use froe::store::Repository;
use froe_export::{JsonLinesSink, export_subtree};

fn main() -> froe::Result<()> {
    let repository = Repository::open(std::path::Path::new("/path/to/segmentstore"))?;
    let mut sink = JsonLinesSink::new(std::io::stdout().lock());
    if let Some(node_count) = export_subtree(&repository, "/content", None, &mut sink)? {
        eprintln!("exported {node_count} nodes");
    }
    Ok(())
}
```
