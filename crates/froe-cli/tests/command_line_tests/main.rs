//! Command-line compatibility tests: the `extract` spelling shipped in
//! v0.1.0 and must keep working as a hidden alias of `export`.

use std::io::{Read as _, Write as _};

use froe::writer::record_writer::ChildNodesToWrite;
use froe::writer::store_writer::WritableRepository;

mod compaction;
mod compaction_decisions;
mod diagnostics;
mod export;
mod reporting;
mod support;

pub(crate) use support::*;
