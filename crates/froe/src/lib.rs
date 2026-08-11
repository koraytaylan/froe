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
//! The mutating writing API ([`writer`]) covers commits, checkpoints, applying
//! `cleanup`, compaction, backup, restore, and journal recovery. It takes the
//! exclusive repository lock first, so it cannot race a cooperating running
//! instance, and produces stores byte-for-byte compatible with Oak (apart from
//! the documented extreme-subnormal rendering residue; see
//! [`content::property::double_to_text`]). Planning `cleanup` is the read-only
//! exception and never takes the lock. Run mutations only against a *stopped*
//! repository. The writer requires a Unix operating-system entropy source and
//! therefore refuses to open on Windows.
//!
//! **The writing API is beta**: it is verified against byte-exact
//! specifications extracted from the Oak sources and an extensive test
//! suite, but has not yet been validated end-to-end against stores
//! produced by — or consumed by — a real Oak/AEM instance. Until that
//! interoperability round-trip lands, take a copy of your repository
//! before writing to data you care about. The reading API carries no
//! such caveat.
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
//! * [`store`] — the assembled read-only repository.
//!
//! Custom backends (in-memory fixtures, remote stores) implement
//! [`SegmentProvider`] and reuse the whole content layer unchanged.

pub mod checksum;
pub mod content;
pub mod error;
pub mod hashing;
pub mod journal;
pub mod segment;
pub mod store;
pub mod tar_archive;
pub mod tooling;
pub mod writer;

pub use content::{
    BinaryValue, NodeState, PropertyState, PropertyType, PropertyValue, PropertyValues,
    SegmentProvider, Template,
};
pub use error::{Error, Result};
pub use journal::JournalEntry;
pub use segment::{RecordIdentifier, RecordType, SegmentIdentifier, SegmentKind};
pub use store::Repository;
pub use writer::{
    CleanupAction, CleanupDeletionFailure, CleanupOptions, CleanupOutcome, CleanupPlan,
    CleanupTask, CompactionKind, CompactionOutcome, JournalLineRemoval, JournalRemovalReason,
    PreparedCleanup, RecoveryBackupPolicy, RecoveryOutcome, StaleArchiveReason, WritableRepository,
    backup, cleanup, compact, plan_cleanup, recover_journal, restore,
};
