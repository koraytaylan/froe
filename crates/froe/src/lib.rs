//! Reader for Apache Jackrabbit Oak `segment-tar` (`TarMK`) repositories.
//!
//! `TarMK` is the storage engine used by Apache Jackrabbit Oak and Adobe
//! Experience Manager: content is stored as immutable *segments* packed into
//! tar archives, and a *journal* records the sequence of repository head
//! states. This crate opens such a repository directly from disk, resolves
//! the current head state, and exposes the content tree for traversal and
//! extraction — without a running Oak instance.
//!
//! The reading API ([`Repository`], [`store`], [`content`], [`tooling`]) is
//! read-only by design: it never takes the repository lock and never
//! modifies any file, so it is safe to point at a live repository. Like Oak,
//! the reader memory-maps archives and relies on the store's
//! never-modify-in-place file protocol; an external process that truncates or
//! rewrites an archive would disturb both froe and a running Oak instance.
//!
//! The mutating writing API ([`writer`]) covers commits, checkpoints,
//! compaction and the reclamation it performs, backup, restore, and journal
//! recovery. It takes the exclusive repository lock first, so it cannot race a
//! cooperating running instance, and produces stores byte-for-byte compatible
//! with Oak (apart from the documented extreme-subnormal rendering residue;
//! see [`content::property::double_to_text`]). Planning a compaction is the
//! read-only exception and never takes the lock. Run mutations only against a *stopped*
//! repository. The writer requires a Unix operating-system entropy source and
//! therefore refuses to open on Windows.
//!
//! **The writing API is verified against a real Oak instance**: the
//! workspace interoperability suite round-trips it through Apache
//! Jackrabbit Oak `oak-segment-tar` 1.90.0 — Oak writes the store, froe
//! commits, checkpoints, compacts, cleans up, backs up, restores and
//! recovers the journal, and Oak then boots against each result and serves
//! a byte-identical content tree without logging any of its own repair
//! messages. Still unverified against a live instance: `store.version=1`
//! stores, external blob stores, native macOS or Windows execution, and
//! Adobe AEM itself, which ships its own Oak build. Writing still requires
//! a stopped repository, and keeping a copy before a destructive operation
//! on irreplaceable data remains ordinary prudence.
//!
//! # Example
//!
//! ```no_run
//! use froe::store::Repository;
//!
//! fn main() -> froe::Result<()> {
//!     let repository = Repository::open(std::path::Path::new("/path/to/segmentstore"))?;
//!     if let Some(node) = repository.node_at_path("/content")? {
//!         for property in node.properties()? {
//!             println!("{} = {:?}", property.name, property.values);
//!         }
//!         for (name, child) in node.child_node_entries()? {
//!             println!("{name}: {} children", child.child_node_count()?);
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Layers
//!
//! Each layer is usable on its own:
//!
//! * [`tar_archive`] — archives, indexes, segment graphs, binary
//!   reference catalogs;
//! * [`segment`] — segment parsing and record addressing;
//! * [`content`] — decoding records into nodes, properties, and values;
//! * [`journal`] — the head revision log;
//! * [`gc_journal`] — optional garbage-collection history;
//! * [`store`] — the assembled read-only repository.
//!
//! Long-running operations — opening a large store, planning a compaction,
//! compacting, checking consistency — have a `_with_progress` twin that
//! reports what they are doing to a [`progress::ProgressObserver`], so a
//! caller need not guess whether a silent minute means work or a hang.
//!
//! Custom backends (in-memory fixtures, remote stores) implement
//! [`SegmentProvider`] and reuse the whole content layer unchanged.

pub(crate) mod cache;
pub mod checksum;
pub mod content;
pub mod error;
pub mod gc_journal;
pub mod hashing;
mod java;
pub mod journal;
pub(crate) mod packed_records;
pub(crate) mod parallel;
pub mod progress;
pub mod segment;
pub mod store;
pub mod tar_archive;
pub mod tooling;
pub mod units;
pub mod writer;

pub use content::{
    BinaryStream, BinaryValue, NodeState, PropertyState, PropertyType, PropertyValue,
    PropertyValues, SegmentProvider, Template, read_binary_stream,
};
pub use error::{Error, Result};
pub use gc_journal::GarbageCollectionJournalEntry;
pub use journal::JournalEntry;
pub use progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
pub use segment::{
    GarbageCollectionGeneration, RecordIdentifier, RecordType, SegmentIdentifier, SegmentKind,
};
pub use store::Repository;
pub use units::{format_byte_size, format_count};
pub use writer::{
    ArchiveIndexSurvey, ArchiveRewritePolicy, CompactedGeneration, CompactionAction,
    CompactionKind, CompactionOptions, CompactionOutcome, CompactionPlan, ExternalBinaryFootprint,
    FileDeletionFailure, JournalLineRemoval, JournalRemovalReason, OrphanedVersionHistoryReport,
    PreparedCompaction, RecoveryBackupPolicy, RecoveryBackupSurvey, RecoveryOutcome,
    StaleArchiveReason, WritableRepository, backup, backup_with_progress, compact,
    compact_with_progress, plan_compaction, plan_compaction_with_progress, recover_journal,
    recover_journal_with_progress, restore, restore_with_progress, survey_archive_indexes,
    survey_recovery_backups,
};
