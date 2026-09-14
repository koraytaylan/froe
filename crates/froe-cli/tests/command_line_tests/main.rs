//! Command-line compatibility tests: the `extract` spelling shipped in
//! v0.1.0 and must keep working as a hidden alias of `export`.

use std::io::{Read as _, Write as _};

use froe::writer::record_writer::ChildNodesToWrite;
use froe::writer::store_writer::WritableRepository;

mod compaction;
mod compaction_decisions;
mod diagnostics;
mod export;
mod index;
mod reporting;
mod support;

// The same snapshot utility proves read-only behaviour in both crates, and
// two copies of it could drift apart silently. Included from `froe`'s test
// tree rather than copied; it depends only on `std`.
#[path = "../../../froe/tests/support/filesystem_snapshot.rs"]
pub(crate) mod filesystem_snapshot;

pub(crate) use support::*;
