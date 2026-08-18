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
//! The tests run in a strict dependency chain. Each phase depends on the
//! previous one and aborts the chain on failure:
//!
//! 1. **`generate`** — Boot Sling, populate content, churn subtrees, stop.
//!    Produces the shared Oak store fixture. If this fails, nothing else
//!    can run because every later phase reads this store.
//!
//! 2. **`read`** — froe reads the Oak store: summary, tree, check, search,
//!    export. If this fails, froe cannot read Oak's format and no write-path
//!    verification is meaningful — there is no way to confirm that froe's
//!    output is correct without a working reader.
//!
//! 3. **`commit`** — froe adds nodes with typed properties to the content
//!    tree via the library's commit API, then Sling reads them back. If
//!    this fails, froe cannot write content that Oak reads — the core
//!    interop claim. There is no point testing checkpoint, compact,
//!    backup, or recover if the writer cannot produce content
//!    Oak reads.
//!
//! 4. **`checkpoint`** — froe writes a checkpoint against the Oak store.
//!    A metadata-only write-path test (logical head update). If this
//!    fails, the writer's checkpoint machinery is broken, which affects
//!    compact's expired-checkpoint handling and its checkpoint
//!    preservation.
//!
//! 5. **`compact`** — froe compacts a copy of the store and Sling boots
//!    against the result. Depends on `read` (to verify the result) and
//!    `commit` (to trust the writer). If this fails, the reclamation
//!    multi-generational fixture cannot be built (it uses two compactions).
//!
//! 6. **`cleanup`** — froe compact against a multi-generational store built
//!    by two compactions, with an expired checkpoint, a stale archive, a
//!    truncated journal, and corrupt journal lines. Depends on `compact`
//!    (to build the gen 0→1→2 fixture) and `checkpoint` (for the expired
//!    checkpoint). If this fails, the write path's plan-and-apply
//!    machinery is broken.
//!
//! 7. **`backup`** — froe backup + restore, Sling boots against the
//!    restored store. Depends on `read` and `commit`. Independent of
//!    compact but later in the chain because it is lower-risk.
//!
//! 8. **`recover`** — froe recover-journal after deleting journal.log,
//!    Sling boots against the recovered store. Depends on `read`. Last
//!    because it is the most destructive (deletes the journal).
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
mod digest;
mod environment;
mod fixtures;
mod oak;
mod phase_baseline;
mod phase_maintenance;
mod phase_recovery;
mod phase_writing;
mod podman;
mod sling;
mod store;

use content::*;
use digest::*;
use environment::*;
use fixtures::*;
use oak::*;
use phase_baseline::*;
use phase_maintenance::*;
use phase_writing::*;
use podman::*;
use sling::*;
use store::*;
