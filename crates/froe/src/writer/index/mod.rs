//! Rebuilding Oak's indexes offline.
//!
//! Oak rebuilds a flagged property index synchronously inside the first
//! commit after startup, and has no offline path that writes the result back.
//! On a large store that blocks AEM for hours. This module performs the same
//! computation — Oak's editors' key derivation, over the same state, with the
//! same bookkeeping — and publishes it in one head move under the lock.
//!
//! The safety case is
//! `docs/plans/0007-property-index-reindex/ARCHITECTURE.md`, and it is worth
//! reading before this code: what `--from-head` authorizes, which facts are
//! rechecked under the lock, and the one key-proportional memory term the
//! bounded-memory case admits are all decided there.
//!
//! # What the state root is, and why it is not always the head
//!
//! Oak's editors see the state of the commit they run in. For a synchronous
//! definition that is the head; for an asynchronous lane it is the lane's
//! checkpoint at the end of the cycle, which the lane property records. An
//! offline rebuild from the head would be *wrong* for a counter: the lane's
//! next cycle diffs from its recorded checkpoint, and every node added since
//! would be counted twice. So an asynchronous definition is indexed from
//! `/checkpoints/<lane checkpoint>/root`, and a dangling checkpoint is a
//! refusal rather than a fallback.

use std::path::PathBuf;

pub mod apply;
pub mod counter_builder;
pub mod definition_update;
pub mod lucene_directory;
pub mod lucene_import;
pub mod plan;
pub mod prepared;
pub mod property_builder;
pub mod property_collector;
pub mod selection;

// The external sort lives at the crate root so plan 0009's Lucene inversion
// can use it without depending on the segment-store write path. Its public
// surface is re-exported here because this module's public signatures name
// it: a `pub` function may not name a type reachable only through a
// `pub(crate)` module, nor a public generic carry a crate-private bound —
// rustc's `private_interfaces` and `private_bounds`, warn-by-default and
// therefore errors under the `-D warnings` gate.
// The operation's public surface, mirroring `compact`'s.
pub use apply::{DefinitionReport, ReindexOutcome};
pub use lucene_directory::{DEFAULT_BLOB_SIZE, DirectoryListing, OakDirectoryWriter};
pub use plan::{NoWorkReason, ReindexAction, ReindexPlan, ReindexWarning};
pub use prepared::{
    PreparedReindex, plan_reindex, plan_reindex_with_progress, reindex, reindex_with_progress,
};

pub use crate::external_sort::{
    MAXIMUM_FAN_IN, RunLocation, SortBudget, SortedPass, SortedPasses, SpillRecord,
};

/// One entry of a property, unique or reference index: a key and the content
/// path indexed under it.
///
/// Ordered by `(key, path elements)` with the elements compared as byte
/// strings, which is exactly the order a depth-first construction of the
/// mirror trie needs: when the sequence leaves a subtree, that subtree's node
/// can be written with all its children known. Comparing the path as one
/// string would not do — `/a/b` and `/a-b/c` order differently under the two
/// comparisons, and the trie writer would see a subtree it had already left.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexEntry {
    /// The derived key, already URL-encoded as Oak encodes it.
    pub key: String,
    /// The content path the entry names, absolute, without a trailing slash.
    pub path: String,
}

impl IndexEntry {
    /// An entry for `path` under `key`.
    #[must_use]
    pub fn new(key: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            path: path.into(),
        }
    }

    /// The path's elements, empty ones dropped, as the ordering compares
    /// them.
    fn path_elements(&self) -> impl Iterator<Item = &str> {
        self.path.split('/').filter(|element| !element.is_empty())
    }
}

impl Ord for IndexEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.as_bytes().cmp(other.key.as_bytes()).then_with(|| {
            self.path_elements()
                .map(str::as_bytes)
                .cmp(other.path_elements().map(str::as_bytes))
        })
    }
}

impl PartialOrd for IndexEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl SpillRecord for IndexEntry {
    fn encode(&self, buffer: &mut Vec<u8>) {
        let key = self.key.as_bytes();
        buffer.extend_from_slice(&u32::try_from(key.len()).unwrap_or(u32::MAX).to_le_bytes());
        buffer.extend_from_slice(key);
        buffer.extend_from_slice(self.path.as_bytes());
    }

    fn decode(bytes: &[u8]) -> crate::Result<Self> {
        let invalid = || crate::Error::InvalidFormat {
            details: "a spilled index entry is truncated".to_owned(),
        };
        let length: [u8; 4] = bytes
            .get(..4)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?;
        let length = u32::from_le_bytes(length) as usize;
        let key = bytes.get(4..4 + length).ok_or_else(invalid)?;
        let path = bytes.get(4 + length..).ok_or_else(invalid)?;
        Ok(Self {
            key: String::from_utf8_lossy(key).into_owned(),
            path: String::from_utf8_lossy(path).into_owned(),
        })
    }

    fn resident_size(&self) -> usize {
        self.key.len() + self.path.len()
    }
}

/// Where a run puts the files it spills.
///
/// The distinction is not cosmetic. A froe-named subdirectory left by an
/// earlier run is a **refusal** in a directory the operator named — they
/// chose it, so something they did not expect is there — and a **warning**
/// under the default, where froe's own leftovers are unsurprising and
/// removing them is not froe's call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WorkDirectory {
    /// A directory the operator named.
    OperatorNamed(PathBuf),
    /// The system temporary directory.
    Default,
}

impl WorkDirectory {
    /// The directory itself.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        match self {
            Self::OperatorNamed(path) => path.clone(),
            Self::Default => std::env::temp_dir(),
        }
    }

    /// Whether the operator named it.
    #[must_use]
    pub fn is_operator_named(&self) -> bool {
        matches!(self, Self::OperatorNamed(_))
    }
}

/// How much a sort may hold before it spills, by default.
///
/// 64 MiB: large enough that a modest store sorts in memory, small enough
/// that a run on a constrained host does not fail on the allocation. An
/// operator who knows their machine can raise it; the safety case states
/// what the number bounds.
pub const DEFAULT_SORT_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// How a reindex run is configured.
///
/// Private fields with `new` and `with_*` setters, as `CompactionOptions`
/// has: private fields, not `#[non_exhaustive]`, are what actually block a
/// struct literal from a downstream crate, so plan 0010's
/// `with_binary_text_policy` composes on a stated contract.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReindexOptions {
    requested_paths: Vec<String>,
    from_head: bool,
    work_directory: WorkDirectory,
    sort_budget_bytes: usize,
}

impl Default for ReindexOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ReindexOptions {
    /// Rebuild every definition flagged `reindex = true`, from the state Oak
    /// would have indexed, spilling under the default work directory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            requested_paths: Vec::new(),
            from_head: false,
            work_directory: WorkDirectory::Default,
            sort_budget_bytes: DEFAULT_SORT_BUDGET_BYTES,
        }
    }

    /// Restrict the run to these definition paths.
    #[must_use]
    pub fn with_indexes(mut self, paths: impl IntoIterator<Item = String>) -> Self {
        self.requested_paths = paths.into_iter().collect();
        self
    }

    /// Authorize the two side effects of `--from-head`. See
    /// `writer/index/selection.rs` for exactly what they are.
    #[must_use]
    pub fn with_from_head(mut self, from_head: bool) -> Self {
        self.from_head = from_head;
        self
    }

    /// Spill under this directory rather than the default.
    #[must_use]
    pub fn with_work_directory(mut self, directory: WorkDirectory) -> Self {
        self.work_directory = directory;
        self
    }

    /// How much one definition's sort may hold before it spills.
    #[must_use]
    pub fn with_sort_budget_bytes(mut self, bytes: usize) -> Self {
        self.sort_budget_bytes = bytes;
        self
    }

    /// The definition paths the operator named.
    #[must_use]
    pub fn requested_paths(&self) -> &[String] {
        &self.requested_paths
    }

    /// Whether `--from-head` was given.
    #[must_use]
    pub fn from_head(&self) -> bool {
        self.from_head
    }

    /// Where the run spills.
    #[must_use]
    pub fn work_directory(&self) -> &WorkDirectory {
        &self.work_directory
    }

    /// The sort budget in bytes.
    #[must_use]
    pub fn sort_budget_bytes(&self) -> usize {
        self.sort_budget_bytes
    }
}
