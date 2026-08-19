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
pub mod digest;
pub mod history;
pub mod search;
pub mod segment_dump;

pub use archive_debug::{
    ArchiveDebugError, ArchiveDebugGraph, ArchiveDebugOptions, ArchiveDebugReport,
    ArchiveDebugResult, ArchiveDebugState, ArchiveDebugWork, ArchiveGraphOrigin,
    ArchiveGraphReferences, ArchiveGraphRow, ArchivePathReference, ArchivePropertyDisplay,
    DEFAULT_MAXIMUM_ARCHIVE_GRAPH_EDGES, DEFAULT_MAXIMUM_ARCHIVE_GRAPH_ROWS,
    DEFAULT_MAXIMUM_ARCHIVE_NAME_BYTES_PER_NODE, DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES,
    DEFAULT_MAXIMUM_ARCHIVE_PENDING_NODES, DEFAULT_MAXIMUM_ARCHIVE_REFERENCE_TEXT_BYTES,
    DEFAULT_MAXIMUM_ARCHIVE_SCHEDULED_CHILDREN_PER_NODE, DEFAULT_MAXIMUM_ARCHIVE_WORK_UNITS,
    debug_archive, debug_archive_with_options,
};
pub use check::{
    BinaryCheck, ConsistencyReport, NodeTreeVerifier, PathVerdict, check_consistency,
    check_consistency_with_progress, verify_node_tree,
};
pub(crate) use check::{DiscardedVerifiedContent, VerifiedContentObserver};
pub use diff::{
    NodeDifference, PropertyChange, diff_revisions, diff_revisions_visiting,
    diff_revisions_with_progress,
};
pub use digest::{
    DigestDifference, DigestSummary, compare_digests, digest_repository, parse_digest,
};
pub use history::{NodeHistoryEntry, node_history, node_history_with_progress};
pub use search::{
    NodeMatch, SearchQuery, search_nodes, search_nodes_visiting, search_nodes_with_progress,
};
pub use segment_dump::{dump_segment, dump_segment_bytes};
