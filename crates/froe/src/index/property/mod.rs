//! The property family's storage: the `:index` subtrees a `property`,
//! unique, node-type or `reference` index writes, the computation that
//! produces them, and a structural consistency check over the pair.
//!
//! `docs/analysis/index-property-storage.md` specifies all of it. The
//! module divides the way the specification does:
//!
//! * [`key_encoding`] — the exact derivation from a property's values to the
//!   child names a `:index` subtree is keyed by;
//! * [`type_predicate`] — `declaringNodeTypes` resolved against
//!   `/jcr:system/jcr:nodeTypes`;
//! * [`mirror`] — `:index/<key>/<path elements…>` with `match = true`, which
//!   is also how the reference index stores under `:references` and
//!   `:weakreferences`;
//! * [`unique`] — `:index/<key>` with an `entry` array of absolute paths;
//! * [`consistency`] — the two-halved check, with a budget for each half.
//!
//! **Which strategy applies to a definition follows from the definition, not
//! from what is on disk.** Oak decides it with one strict `BOOLEAN` read of
//! `unique`, in the editor, the lookup and the query planner alike, so a
//! `unique` stored as the `STRING` `"true"` is a *mirror* index to Oak. Every
//! reader here goes through the same field for the same reason: a reader that
//! guessed from the shape it found would disagree with Oak exactly on the
//! stores where the disagreement matters.

pub mod consistency;
pub mod key_encoding;
pub mod mirror;
pub mod type_predicate;
pub mod unique;

pub use consistency::{
    DuplicateEntry, EntryCheckBudget, IndexEntryFault, MissingEntry, NodeCheckBudget,
    PropertyIndexReport, check, check_entries,
};
pub use key_encoding::keys_for_property;
pub use mirror::{MirrorEntry, MirrorIndex};
pub use type_predicate::TypePredicate;
pub use unique::{UniqueEntry, UniqueIndex};
