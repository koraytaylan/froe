//! Garbage-collection generation metadata shared across segment-tar layers.
//!
//! Oak records this triple in segment headers and archive metadata. The
//! `gc.log` text format stores only the two integer fields and reconstructs
//! `is_compacted` as `false` when reading an entry.

/// A garbage-collection generation in segment-tar metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GarbageCollectionGeneration {
    /// The generation, incremented by every compaction.
    pub generation: i32,
    /// The full generation, incremented only by full compactions.
    pub full_generation: i32,
    /// Whether the segment was produced by a compactor.
    pub is_compacted: bool,
}
