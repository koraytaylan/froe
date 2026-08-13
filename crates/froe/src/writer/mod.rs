//! The write path: building segments, archives, and repository state.
//!
//! Everything below this module ultimately serves one contract: a store
//! written by `froe` must be indistinguishable, to Oak and AEM, from a
//! store written by Oak itself (one documented rendering residue:
//! extreme-subnormal doubles; see
//! [`crate::content::property::double_to_text`]). Byte layouts,
//! durability ordering, file naming, and locking all follow the
//! specifications in `docs/analysis/` extracted from the Java
//! implementation. Writing requires a Unix operating system entropy
//! source; the writable store refuses to open on Windows.

pub mod backup;
pub mod cleanup;
#[cfg(test)]
mod cleanup_fault_injection;
pub mod commit;
pub mod compaction;
pub mod identifier_generator;
pub(crate) mod journal_maintenance;
pub mod record_writer;
pub mod repository_lock;
pub mod segment_builder;
pub mod store_writer;
pub mod tar_writer;

pub use backup::{
    RecoveryOutcome, backup, backup_with_progress, recover_journal, recover_journal_with_progress,
    restore, restore_with_progress,
};
pub use cleanup::{
    CleanupAction, CleanupDeletionFailure, CleanupOptions, CleanupOutcome, CleanupPlan,
    CleanupTask, JournalLineRemoval, JournalRemovalReason, PreparedCleanup, RecoveryBackupPolicy,
    StaleArchiveReason, cleanup, cleanup_with_progress, plan_cleanup, plan_cleanup_with_progress,
};
pub use commit::{
    CheckpointDescription, create_checkpoint, list_checkpoints, release_checkpoint,
    remove_all_checkpoints, remove_checkpoints, remove_unreferenced_checkpoints,
    replace_content_root,
};
pub use compaction::{
    CompactionKind, CompactionOutcome, compact, compact_with_progress, deep_copy_tree,
    deep_copy_tree_with_progress,
};
pub use identifier_generator::{new_bulk_segment_identifier, new_data_segment_identifier};
pub use record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
    sort_properties_for_template,
};
pub use repository_lock::RepositoryLock;
pub use segment_builder::{
    BuiltSegment, GarbageCollectionGeneration, SegmentBufferBuilder, SegmentBufferFull,
};
pub use store_writer::{StoreSink, WritableRepository};
pub use tar_writer::TarArchiveWriter;
