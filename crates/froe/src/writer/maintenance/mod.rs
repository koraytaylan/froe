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
mod gate_observation;
mod indexless_refusal;
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
mod surveys;
mod temporaries;

#[cfg(test)]
#[cfg(unix)]
pub(crate) use self::options::MaintenanceTask;
// The open protocol's gates, for the index module's own prepare. See
// `planning/mod.rs` for why they are widened rather than moved.
//
// The expectations name task 0707, whose `PreparedReindex::prepare` is their
// first caller: this is a `refactor:` commit that lands the exposure apart
// from the mutating diff it serves, as `CONTRIBUTING.md` asks, so the
// re-exports are deliberately ahead of their use and say so.
#[expect(
    unused_imports,
    reason = "task 0707's PreparedReindex::prepare is the first caller"
)]
pub(crate) use self::apply_identity::{
    validate_apply_environment, validate_apply_identity, validate_metadata_source_apply_identity,
};
pub use self::options::{CompactionOptions, RecoveryBackupPolicy};
pub use self::plan::{
    CompactedGeneration, CompactionAction, CompactionOutcome, CompactionPlan,
    ExternalBinaryFootprint, FileDeletionFailure, JournalLineRemoval, JournalRemovalReason,
    OrphanedVersionHistoryReport, StaleArchiveReason,
};
#[expect(
    unused_imports,
    reason = "task 0707's PreparedReindex::prepare is the first caller"
)]
pub(crate) use self::planning::{
    DirectoryFingerprint, available_filesystem_bytes, canonical_repository_directory,
    directory_fingerprint, validate_repository_shape,
};
pub use self::prepared::{
    PreparedCompaction, compact, compact_with_progress, plan_compaction,
    plan_compaction_with_progress,
};
pub use self::surveys::{
    ArchiveIndexSurvey, RecoveryBackupSurvey, survey_archive_indexes, survey_recovery_backups,
};
