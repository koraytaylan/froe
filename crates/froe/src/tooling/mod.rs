//! Read-only diagnostic tooling: consistency checking, revision
//! differencing, node history, node search, segment dumps, and archive
//! path attribution.
//!
//! These build on the reader and never modify the store. Each returns a
//! structured result so a caller — the `froe` command line or another
//! program — can render or process it. Traversal is bounded against
//! cyclic corrupt records, matching the rest of the crate.

pub mod archive_debug;
pub mod check;
pub mod diff;
pub mod history;
pub mod search;
pub mod segment_dump;

pub use archive_debug::{
    ArchiveDebugError, ArchiveDebugGraph, ArchiveDebugOptions, ArchiveDebugReport,
    ArchiveDebugResult, ArchiveDebugState, ArchiveDebugWork, ArchiveGraphOrigin,
    ArchiveGraphReferences, ArchiveGraphRow, ArchivePathReference, ArchivePropertyDisplay,
    DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES, DEFAULT_MAXIMUM_ARCHIVE_REFERENCE_TEXT_BYTES,
    debug_archive, debug_archive_with_options,
};
pub use check::{
    ConsistencyReport, NodeTreeVerifier, PathVerdict, check_consistency, verify_node_tree,
};
pub use diff::{NodeDifference, PropertyChange, diff_revisions};
pub use history::{NodeHistoryEntry, node_history};
pub use search::{NodeMatch, SearchQuery, search_nodes};
pub use segment_dump::{dump_segment, dump_segment_bytes};
