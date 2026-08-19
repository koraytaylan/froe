//! Orphaned version histories, end to end: an independent oracle for the
//! detection, the purge's exact removals, its exclusions, and the
//! checkpoint scoping.
//!
//! The oracle walks the store through the read-only content API and applies
//! the field query's logic directly — version histories whose
//! `jcr:versionableUuid` matches no live `jcr:uuid` outside version storage
//! — so the planner's collector machinery is checked by a second,
//! independent implementation, never by itself.

mod detection;
mod purge;
mod support;
