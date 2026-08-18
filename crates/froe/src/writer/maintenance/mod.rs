//! Conservative offline maintenance for an existing segment-tar repository.
//!
//! Cleanup is deliberately split into a read-only plan and a prepared apply
//! session. Planning never acquires `repo.lock` and never opens the ordinary
//! writable repository (whose startup lifecycle repairs archives and rewrites
//! the manifest). A prepared session takes the repository lock, rebuilds the
//! plan from disk, fingerprints every directory entry, and holds the lock
//! until application and fresh post-operation verification complete.
//!
//! Every test lives with the stage it exercises, including the end-to-end
//! ones: a claim about what the apply phase refuses belongs beside the
//! apply phase even when it is made through `plan_compaction`.

#[cfg(test)]
mod test_support;

mod apply;
mod apply_identity;
mod checkpoints;
mod file_removal;
/// Everything maintenance does with the journal: classifying its lines,
/// and rewriting the file that holds them.
mod journal;
mod manifest;
mod options;
mod plan;
mod planning;
mod prepared;
mod reclamation;
mod recovery_backups;
mod stale_archives;
mod temporaries;

#[cfg(test)]
pub(crate) use self::options::MaintenanceTask;
pub use self::options::{CompactionOptions, RecoveryBackupPolicy};
pub use self::plan::{
    CompactedGeneration, CompactionAction, CompactionOutcome, CompactionPlan, FileDeletionFailure,
    JournalLineRemoval, JournalRemovalReason, StaleArchiveReason,
};
pub use self::prepared::{
    PreparedCompaction, compact, compact_with_progress, plan_compaction,
    plan_compaction_with_progress,
};
