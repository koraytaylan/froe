//! The writable repository: Oak's read-write file store lifecycle.
//!
//! Opening takes the exclusive repository lock first (a documented,
//! strictly-safer deviation from Java, which creates `journal.log`
//! before locking — a contended open here leaves no trace), then opens
//! the journal handle, then the
//! manifest check-and-update (always rewriting `store.version=2`), then
//! archive initialization with *destructive* generation selection — the
//! newest valid generation letter of each archive number wins and stale
//! letters are deleted; archives without any valid index are backed up
//! to `.bak` names and regenerated from a raw scan — and finally journal
//! binding, bootstrapping the initial `{ "root": {} }` node into a fresh
//! store.
//!
//! Durability follows Oak's contract exactly: segment bytes are appended
//! and fsynced *before* the journal line referencing them is appended
//! and fdatasynced, and a journal line is written only when the head
//! actually moved.
//!
//! Segments written during the session are kept in memory (shared
//! buffers) so reads resolve them immediately; on disk they live in the
//! archives this writer produces.

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

mod archive_certificate;
mod archive_numbering;
mod cleanup_apply;
mod file_identity;
mod providers;
mod reclaim;
mod recovery;
mod repair;
mod repository;
mod session;
mod startup;
mod sweep;
mod sweep_plan;

pub(crate) use self::archive_certificate::{
    certify_active_archive, certify_active_archives_with_progress,
};
pub(crate) use self::archive_numbering::next_cleanup_archive_number;
pub(crate) use self::cleanup_apply::apply_standalone_segment_cleanup;
pub(crate) use self::file_identity::{preserve_file_metadata, sync_directory_strict};
pub use self::reclaim::ArchiveRewritePolicy;
pub(crate) use self::reclaim::{ReclaimRule, predict_post_compaction_reclamation};
pub(crate) use self::repair::{
    AuthorizeVersionTwoWrite, repair_indexless_archive_numbers, repair_target_names,
    survey_indexless_archive_numbers, unrepairable_archives_refusal,
};
pub use self::repository::{StoreSink, WritableRepository};
pub(crate) use self::startup::{RepairedArchive, reject_duplicate_archive_generations};
pub(crate) use self::sweep::is_reclaimable;
pub(crate) use self::sweep_plan::{
    GenerationReclaimRequest, PlannedArchiveSweep, RETAINED_GENERATIONS, SegmentSweepOutcome,
    StandaloneSegmentCompactionOutcome, StandaloneSegmentCompactionPlan,
    measure_unvetoed_reclamation, plan_reclaimed_totals, plan_standalone_segment_cleanup,
    planned_unavailable_segments,
};
