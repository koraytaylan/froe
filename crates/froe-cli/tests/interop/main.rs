//! End-to-end interop tests against a real Apache Sling / Jackrabbit Oak
//! `TarMK` store.
//!
//! These tests verify that froe can read stores written by Oak, write stores
//! that Oak can read, and perform maintenance operations (compact,
//! backup, restore, recover-journal) that leave the store in a state Oak
//! boots against cleanly.
//!
//! # Prerequisites
//!
//! - `podman` installed and runnable by the current user.
//! - Network access to pull `docker.io/apache/sling:14` once.
//! - The `interop` feature enabled: `cargo test -p froe-cli --features interop`.
//!
//! # Running
//!
//! ```console
//! $ cargo test -p froe-cli --features interop -- --ignored
//! ```
//!
//! Or an individual phase:
//!
//! ```console
//! $ cargo test -p froe-cli --features interop -- --ignored interop::read
//! ```
//!
//! # Dependency chain
//!
//! The tests run in a strict dependency chain, in the order `interop_full`
//! runs them. Each phase depends on the previous one and aborts the chain on
//! failure. `docs/interop.md` carries the same list with a section per phase.
//!
//!  1. **`generate`** — Boot Sling, populate content, add the index shapes
//!     the later plans rebuild, churn subtrees, stop. Produces the shared
//!     Oak store fixture every later phase reads.
//!
//!  2. **`read`** — froe reads the Oak store: summary, tree, check, search,
//!     export. Without a working reader no write-path verification means
//!     anything, because there is no way to confirm froe's output is correct.
//!
//!  3. **`judge_smoke`** — the Oak-side judge is compiled inside the pinned
//!     image and each of its verdicts shown reachable. A judge that silently
//!     ran against the wrong class path would make every later comparison
//!     meaningless *while passing*.
//!
//!  4. **`index_inventory`** — froe's index readers against Oak's own
//!     printers. Before `commit`, and load-bearing: froe's direct commits run
//!     none of Oak's index editors, so afterwards the fixture is legitimately
//!     short an index entry.
//!
//!  5. **`property_reindex`** — froe's offline rebuild against Oak's own
//!     rebuild of the same bytes. Before `commit` for the same reason
//!     `index_inventory` is: froe's direct commits run none of Oak's index
//!     editors, so afterwards the fixture is legitimately short an entry
//!     and the oracle would report a difference that means nothing.
//!
//!  6. **`lucene_dump`** — froe's dump of a Lucene index against Oak's own
//!     dumper, byte for byte, with Lucene's own index checker and Oak's own
//!     consistency checker at its full level over the result. Read-only, so
//!     its position is free; it runs here because the fixture's Lucene index
//!     reflects the state Sling left.
//!
//!  7. **`lucene_import`** — froe's import against Oak, both directions: a
//!     round trip through froe's own dump, and an index Oak's own editors
//!     built out of band, with a booted Oak answering a fulltext query
//!     through each result.
//!
//!  8. **`commit`** — froe adds nodes with typed properties through the
//!     library's commit API; Sling reads them back. The core interop claim.
//!
//!  9. **`checkpoint`** — froe writes a checkpoint: a metadata-only
//!     write-path test the compaction phases' checkpoint handling rests on.
//!
//! 10. **`compact`** — froe compacts a copy and Sling boots the result.
//!
//! 11. **`compact_tail`** — the same with `--tail`, which retains the shared
//!     full generation and so reclaims strictly less.
//!
//! 12. **`checkpoint_removal`** — remove by name, remove-unreferenced and
//!     remove-all; the checkpoint Oak's indexer resumes from survives the
//!     middle one.
//!
//! 13. **`cleanup`** — a multi-generational store with an expired
//!     checkpoint, a stale archive, a truncated journal and corrupt journal
//!     lines, all resolved in one run.
//!
//! 14. **`journal_retention`** — a plain compact retires every revision but
//!     the head it wrote and sweeps the segments behind them.
//!
//! 15. **`compact_convergence`** — the run after a full compaction proves
//!     the store fully compacted, mutates nothing, and says so.
//!
//! 16. **`version_history_purge`** — Oak versions two nodes and deletes one;
//!     froe purges the orphaned history under a digest with the purge as its
//!     only exclusion.
//!
//! 17. **`repair`** — Oak's JVM is killed with SIGKILL holding an archive
//!     open; an authorized compact rebuilds the index.
//!
//! 18. **`backup`** — froe backup and restore; Sling boots the result.
//!
//! 19. **`recover`** — froe recover-journal after deleting `journal.log`.
//!     Last because it is the most destructive.
//!
//! All code in the loop is Apache-2.0 (Apache Sling + Apache Jackrabbit
//! Oak); no Adobe license is involved at any point.

#![cfg(feature = "interop")]
#![allow(clippy::case_sensitive_file_extension_comparisons)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use froe::content::PropertyType;
use froe::segment::record::RecordIdentifier;
use froe::writer::commit::rewrite_node_with_child_edits;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use froe::writer::store_writer::WritableRepository;

mod content;
mod definition_edits;
mod digest;
mod environment;
mod fixtures;
mod judge;
mod oak;
mod phase_baseline;
mod phase_index_inventory;
mod phase_judge;
mod phase_lucene_import;
mod phase_lucene_transport;
mod phase_lucene_writer;
mod phase_maintenance;
mod phase_property_reindex;
mod phase_recovery;
mod phase_writing;
mod podman;
mod sling;
mod store;

use content::*;
use definition_edits::*;
use digest::*;
use environment::*;
use fixtures::*;
use judge::*;
use oak::*;
use phase_baseline::*;
use phase_index_inventory::*;
use phase_judge::*;
use phase_lucene_import::*;
use phase_lucene_transport::*;
use phase_maintenance::*;
use phase_property_reindex::*;
use phase_writing::*;
use podman::*;
use sling::*;
use store::*;
