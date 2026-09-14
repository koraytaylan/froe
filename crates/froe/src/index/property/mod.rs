//! The property family's storage: the `:index` subtrees a `property`, unique,
//! node-type or `reference` index writes, and the computation that produces
//! them.
//!
//! `docs/analysis/index-property-storage.md` specifies both. The readers,
//! the key derivation, the node-type predicate and the structural consistency
//! check land in the task that fills this module; the module exists now so
//! that the one which creates it never contends with the one that registers
//! it.
