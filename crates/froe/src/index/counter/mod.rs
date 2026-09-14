//! The counter index: Oak's `SipHash` variant, the `:cnt` map it maintains, and
//! the estimated node count read back from it.
//!
//! `docs/analysis/index-property-storage.md` §9 and §10 specify the hash, the
//! chain the editor drives over it, the narrowing of the stored `seed` to 32
//! bits, and the estimate's rules with the constants they actually use. The
//! reader lands in the task that fills this module.
