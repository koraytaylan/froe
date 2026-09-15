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
//!  8. **`lucene_writer_conformance`** — froe's own Lucene writer against
//!     Lucene's: the same committed corpus written by both, Lucene's index
//!     checker over froe's directory, and the two indexes enumerated and
//!     compared line for line. Read-only over the fixture — it writes only
//!     into its own work directory — so its position is free; it runs here
//!     because it is what plan 0010's rebuild rests on.
//!
//!  9. **`lucene_reindex`** — froe's offline Lucene rebuild against Oak's
//!     own rebuild of the same bytes: both indexes enumerated by the judge
//!     and compared outside one declared binary difference, the definition
//!     nodes compared, every query pair and plan equal through Oak's own
//!     engine, and a reset on a lost lane ending in Oak's own from-scratch
//!     rebuild. Before `commit`, for the reason `property_reindex` is.
//!
//! 10. **`commit`** — froe adds nodes with typed properties through the
//!     library's commit API; Sling reads them back. The core interop claim.
//!
//! 11. **`checkpoint`** — froe writes a checkpoint: a metadata-only
//!     write-path test the compaction phases' checkpoint handling rests on.
//!
//! 12. **`compact`** — froe compacts a copy and Sling boots the result.
//!
//! 13. **`compact_tail`** — the same with `--tail`, which retains the shared
//!     full generation and so reclaims strictly less.
//!
//! 14. **`checkpoint_removal`** — remove by name, remove-unreferenced and
//!     remove-all; the checkpoint Oak's indexer resumes from survives the
//!     middle one.
//!
//! 15. **`cleanup`** — a multi-generational store with an expired
//!     checkpoint, a stale archive, a truncated journal and corrupt journal
//!     lines, all resolved in one run.
//!
//! 16. **`journal_retention`** — a plain compact retires every revision but
//!     the head it wrote and sweeps the segments behind them.
//!
//! 17. **`compact_convergence`** — the run after a full compaction proves
//!     the store fully compacted, mutates nothing, and says so.
//!
//! 18. **`version_history_purge`** — Oak versions two nodes and deletes one;
//!     froe purges the orphaned history under a digest with the purge as its
//!     only exclusion.
//!
//! 19. **`repair`** — Oak's JVM is killed with SIGKILL holding an archive
//!     open; an authorized compact rebuilds the index.
//!
//! 20. **`backup`** — froe backup and restore; Sling boots the result.
//!
//! 21. **`recover`** — froe recover-journal after deleting `journal.log`.
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
mod lucene_enumeration;
mod lucene_reindex_queries;
mod lucene_reindex_reset;
mod oak;
mod phase_baseline;
mod phase_index_inventory;
mod phase_judge;
mod phase_lucene_import;
mod phase_lucene_reindex;
mod phase_lucene_transport;
mod phase_lucene_writer;
mod phase_maintenance;
mod phase_property_reindex;
mod phase_recovery;
mod phase_writing;
mod podman;
mod sling;
mod sling_lucene;
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
use phase_lucene_reindex::*;
use phase_lucene_transport::*;
use phase_lucene_writer::*;
use phase_maintenance::*;
use phase_property_reindex::*;
use phase_writing::*;
use podman::*;
use sling::*;
use sling_lucene::*;
use store::*;
