//! Read-only diagnostic tooling: consistency checking, revision
//! differencing, node history, and node search.
//!
//! These build on the reader and never modify the store. Each returns a
//! structured result so a caller — the `froe` command line or another
//! program — can render or process it. Traversal is bounded against
//! cyclic corrupt records, matching the rest of the crate.

pub mod check;
pub mod diff;
pub mod history;
pub mod search;

pub use check::{ConsistencyReport, PathVerdict, check_consistency};
pub use diff::{NodeDifference, PropertyChange, diff_revisions};
pub use history::{NodeHistoryEntry, node_history};
pub use search::{NodeMatch, SearchQuery, search_nodes};
