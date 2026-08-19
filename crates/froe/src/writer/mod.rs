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
pub mod commit;
pub mod compaction;
/// Cutpoints the write path calls at its durability boundaries, and the
/// probes that arm them. The cutpoints sit wherever a run touches disk —
/// the tar writer, the sweep, the journal rewrite, the maintenance apply —
/// so this belongs to the write path rather than to any one of them.
#[cfg(test)]
mod fault_injection;
pub mod identifier_generator;
/// The one maintenance pipeline: plan, confirm, and apply a compaction and
/// everything it reclaims. Its surface is re-exported below, so callers name
/// the operation rather than the module it happens to live in.
mod maintenance;
pub mod record_writer;
pub mod repository_lock;
pub mod segment_builder;
pub mod store_writer;
pub mod tar_writer;

pub use backup::{
    RecoveryOutcome, backup, backup_with_progress, recover_journal, recover_journal_with_progress,
    restore, restore_with_progress,
};
pub use commit::{
    CheckpointDescription, create_checkpoint, list_checkpoints, release_checkpoint,
    remove_all_checkpoints, remove_checkpoints, remove_unreferenced_checkpoints,
    replace_content_root,
};
pub use compaction::{
    CompactionKind, SubtreeOmissions, deep_copy_super_root_omitting_subtrees,
    deep_copy_super_root_sharing, deep_copy_super_root_with_progress, deep_copy_tree,
    deep_copy_tree_across_stores_with_progress, deep_copy_tree_with_progress,
};
pub use identifier_generator::{new_bulk_segment_identifier, new_data_segment_identifier};
pub use maintenance::{
    CompactedGeneration, CompactionAction, CompactionOptions, CompactionOutcome, CompactionPlan,
    ExternalBinaryFootprint, FileDeletionFailure, JournalLineRemoval, JournalRemovalReason,
    OrphanedVersionHistoryReport, PreparedCompaction, RecoveryBackupPolicy, StaleArchiveReason,
    compact, compact_with_progress, plan_compaction, plan_compaction_with_progress,
};
pub use record_writer::{
    BulkBlockSharing, ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter,
    SegmentSink, sort_properties_for_template,
};
pub use repository_lock::RepositoryLock;
pub use segment_builder::{
    BuiltSegment, GarbageCollectionGeneration, SegmentBufferBuilder, SegmentBufferFull,
};
pub use store_writer::{ArchiveRewritePolicy, StoreSink, WritableRepository};
pub use tar_writer::TarArchiveWriter;
