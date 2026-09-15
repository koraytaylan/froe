//! What a reindex would do, computed without writing anything.
//!
//! The plan is **read-only and lockless**, and it is what an operator
//! confirms — so it has to say what will be written, which means counting
//! the entries rather than estimating them. It counts through task 0704's
//! counting sink and task 0705's hit count, so nothing spills and no record
//! is appended.
//!
//! # Why the plan reports an upper bound rather than the distinct keys
//!
//! The residency the safety case admits is proportional to *distinct keys*,
//! not entries. Counting them at plan time would hold one key per node for a
//! unique index — the very residency the count is there to warn about. So
//! the plan gives the entry count as their upper bound, and the outcome
//! reports the exact figure the builder observed.

use std::path::PathBuf;

use crate::writer::index::selection::{IndexingState, SelectionRefusal};

/// What a run will do to one definition.
///
/// `#[non_exhaustive]`, as the compaction plan's types are, so plan 0010's
/// Lucene variant never breaks the command's rendering.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ReindexAction {
    /// Collect, sort, build and publish.
    Rebuild {
        /// The definition.
        path: String,
        /// Which state it is rebuilt from.
        state: IndexingState,
        /// Entries the walk found. An upper bound on the distinct keys.
        entries: u64,
        /// Their total resident bytes, which is what the sort spills.
        entry_bytes: u64,
    },
    /// Rebuild a Lucene definition: make documents, write a segment,
    /// copy it into `:data`.
    RebuildLucene {
        /// The definition.
        path: String,
        /// Which state it is rebuilt from.
        state: IndexingState,
        /// Indexing rules the definition declares.
        rules: usize,
        /// Documents the counting walk would make: one per included node
        /// with a rule.
        documents: u64,
        /// The bytes of values the rules mark stored.
        stored_bytes: u64,
        /// The bytes of values the rules mark indexed.
        indexed_bytes: u64,
        /// Where a binary property's text comes from, rendered.
        binary_text_policy: String,
    },
    /// Remove the hidden children and leave the rest to Oak's own replay.
    Reset {
        /// The definition.
        path: String,
        /// The lane whose checkpoint could not be resolved.
        lane: String,
        /// The hidden children that will be removed.
        hidden_children: Vec<String>,
    },
    /// Nothing to do, and the store is never opened.
    NothingToDo {
        /// The definition.
        path: String,
        /// Why there is nothing to do.
        reason: NoWorkReason,
    },
}

impl ReindexAction {
    /// The definition this action is about.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Rebuild { path, .. }
            | Self::RebuildLucene { path, .. }
            | Self::Reset { path, .. }
            | Self::NothingToDo { path, .. } => path,
        }
    }

    /// Whether this action would write anything.
    #[must_use]
    pub fn is_no_op(&self) -> bool {
        matches!(self, Self::NothingToDo { .. })
    }
}

/// Why a definition has no work.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum NoWorkReason {
    /// A reset with nothing to remove: a hit-less counter, a never-indexed
    /// definition, or a rerun of a reset that already happened.
    NoRemovableHiddenChild,
}

impl std::fmt::Display for NoWorkReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRemovableHiddenChild => {
                write!(formatter, "it has no hidden child to remove")
            }
        }
    }
}

/// Something an operator should know before confirming.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ReindexWarning {
    /// A definition was not selected, and why.
    Skipped {
        /// The refusal, which names the definition.
        refusal: SelectionRefusal,
    },
    /// The work directory may not hold the spill.
    WorkDirectoryMayBeTooSmall {
        /// The directory.
        directory: PathBuf,
        /// The largest a single definition's spill may reach.
        estimated_bytes: u64,
        /// What the filesystem reports free.
        available_bytes: u64,
    },
    /// A froe-named subdirectory from an earlier run is in the default work
    /// directory. Under an operator-named one this is a refusal instead.
    ResidueUnderDefaultWorkDirectory {
        /// The leftover subdirectory.
        directory: PathBuf,
    },
}

impl std::fmt::Display for ReindexWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skipped { refusal } => write!(formatter, "{refusal}"),
            Self::WorkDirectoryMayBeTooSmall {
                directory,
                estimated_bytes,
                available_bytes,
            } => write!(
                formatter,
                "the work directory {} reports {} free, and one definition's spill may \
                 reach {} — the run refuses on ENOSPC before any index record is \
                 written, but it refuses late",
                directory.display(),
                crate::format_byte_size(*available_bytes),
                crate::format_byte_size(*estimated_bytes)
            ),
            Self::ResidueUnderDefaultWorkDirectory { directory } => write!(
                formatter,
                "{} is left over from an earlier run; it is not in the way, but nothing \
                 will remove it either",
                directory.display()
            ),
        }
    }
}

/// What a run would do, in full.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReindexPlan {
    /// The canonicalized store directory, carried so an alias cannot
    /// redirect the lock acquisition after the preview.
    pub directory: PathBuf,
    /// One action per selected definition.
    pub actions: Vec<ReindexAction>,
    /// Facts an operator should have before confirming.
    pub warnings: Vec<ReindexWarning>,
    /// Where the run will spill.
    pub work_directory: PathBuf,
    /// The largest a single definition's spill may reach.
    ///
    /// The per-definition **maximum**, not the selection's total: runs are
    /// unlinked per definition, so the peak is what has to fit. It includes
    /// the fan-in times the sort budget, because a reduction pass holds that
    /// much more until it unlinks the inputs it merged.
    pub work_directory_estimate_bytes: u64,
}

impl ReindexPlan {
    /// Whether every action is a no-op, in which case the run neither opens
    /// the store nor moves the head.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.iter().all(ReindexAction::is_no_op)
    }

    /// How many definitions will be rebuilt.
    #[must_use]
    pub fn rebuild_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|action| matches!(action, ReindexAction::Rebuild { .. }))
            .count()
    }

    /// How many Lucene definitions will be rebuilt.
    #[must_use]
    pub fn lucene_rebuild_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|action| matches!(action, ReindexAction::RebuildLucene { .. }))
            .count()
    }

    /// How many will be reset.
    #[must_use]
    pub fn reset_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|action| matches!(action, ReindexAction::Reset { .. }))
            .count()
    }
}

/// The work-directory proxy for one Lucene definition.
///
/// The two byte totals are the only ones a counting walk can produce
/// without analyzing anything; the multiplier is a **structural count** and
/// not a measurement — the spill runs, the assembled segment and the
/// compound copy — and `docs/index.md` §5.3 records it as such. Nothing
/// here measures bytes per token or bytes per posting.
#[must_use]
pub fn lucene_work_directory_estimate(
    stored_bytes: u64,
    indexed_bytes: u64,
    sort_budget_bytes: u64,
) -> u64 {
    stored_bytes
        .saturating_add(indexed_bytes)
        .saturating_mul(3)
        .saturating_add(
            (crate::writer::index::MAXIMUM_FAN_IN as u64).saturating_mul(sort_budget_bytes),
        )
}

/// The work-directory estimate for one definition.
///
/// Total entry bytes, plus the fan-in times the sort budget: a reduction
/// pass writes merged runs before unlinking the inputs it merged, so that
/// much more is on disk transiently.
#[must_use]
pub fn work_directory_estimate(entry_bytes: u64, sort_budget_bytes: u64) -> u64 {
    entry_bytes.saturating_add(
        (crate::writer::index::MAXIMUM_FAN_IN as u64).saturating_mul(sort_budget_bytes),
    )
}
