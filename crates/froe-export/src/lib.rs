//! Exporting Apache Jackrabbit Oak `segment-tar` (`TarMK`) content trees.
//!
//! The [`froe`] core reads a repository; this crate turns a subtree of it
//! into flat, analysis-friendly output. One depth-first traversal
//! ([`froe::content::traversal::DepthFirstTraversal`]) drives every
//! format: [`export_subtree`] walks the tree once and hands each node to
//! an [`ExportSink`], so a new output format is a new sink, not a new
//! traversal.
//!
//! Formats:
//!
//! * [`JsonLinesSink`] — one JSON object per node (`froe export`'s
//!   default format). Binary *content* is never embedded: inline
//!   binaries appear as `{"binary_length":N}` and external binaries as
//!   `{"binary_reference":"..."}`.
//! * `ParquetSink` (behind the `parquet` feature) — two flat,
//!   zstd-compressed tables built for analytical SQL: one row per node
//!   and one row per property value. See the `parquet`
//!   module. `refresh_parquet_export` brings an existing Parquet
//!   export up to date by decoding only what changed since it was
//!   taken; see the `refresh` module.
//! * `SqliteSink` (behind the `sqlite` feature) — a single `.db` file
//!   with interned strings, a clustered properties table, and a view
//!   layer presenting flat, directly queryable rows. See the `sqlite`
//!   module.
//!
//! Exporting is read-only and safe against a live repository — it shares
//! the core's reading guarantees. [`create_export_output`] is the one
//! blessed way to open an output file: it refuses to overwrite existing
//! files and refuses to write inside the repository directory, where a
//! stray file could be mistaken for a damaged archive at the next open.
//! The Parquet refresh replaces its two table files instead — atomically
//! via [`replace_export_output`] — after validating them as its own
//! earlier output.
//!
//! # Example
//!
//! ```no_run
//! use froe::store::Repository;
//! use froe_export::{JsonLinesSink, export_subtree};
//!
//! fn main() -> froe::Result<()> {
//!     let repository = Repository::open(std::path::Path::new("/path/to/segmentstore"))?;
//!     let mut sink = JsonLinesSink::new(std::io::stdout().lock());
//!     if let Some(node_count) = export_subtree(&repository, "/content", None, &mut sink)? {
//!         eprintln!("exported {node_count} nodes");
//!     }
//!     Ok(())
//! }
//! ```

pub mod export;
pub mod json;
pub mod json_lines;
pub mod output_file;
#[cfg(feature = "parquet")]
pub mod parquet;
#[cfg(feature = "parquet")]
pub mod refresh;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "parquet")]
pub use crate::parquet::{
    ExportProvenance, ParquetExportOptions, ParquetSink, read_export_provenance,
};
#[cfg(feature = "parquet")]
pub use crate::refresh::{
    ExportReplacement, NODES_FILE_NAME, PROPERTIES_FILE_NAME, ParquetRefresh,
    assess_export_replacement, refresh_parquet_export,
};
#[cfg(feature = "sqlite")]
pub use crate::sqlite::{SqliteExportOptions, SqliteSink};
pub use export::{ExportSink, ExportedNode, export_node, export_subtree};
pub use json_lines::JsonLinesSink;
pub use output_file::{
    ExportDirectoryLock, create_export_directory, create_export_output, lock_export_directory,
    replace_export_output, sweep_temporary_outputs, temporary_output_name,
};
