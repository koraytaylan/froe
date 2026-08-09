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
//! modifies any file, so it is safe to point at a live repository. Like
//! Oak itself the reader memory-maps archives, relying on the store's
//! file protocol (existing archive bytes are never modified in place); a
//! process mutating archives outside that protocol would disturb froe
//! and a running Oak instance alike. The
//! writing API ([`writer`]) — commits, checkpoints, compaction, backup,
//! restore, journal recovery — takes the exclusive repository lock first,
//! so it can never race a running instance, and produces stores byte-for-byte
//! compatible with what Oak itself writes (one documented rendering
//! residue: extreme-subnormal doubles; see
//! [`content::property::double_to_text`]). Only run the writing API against
//! a *stopped* repository; it requires a Unix operating system entropy
//! source and refuses to open on Windows.
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
    CompactionKind, CompactionOutcome, RecoveryOutcome, WritableRepository, backup, compact,
    recover_journal, restore,
};
