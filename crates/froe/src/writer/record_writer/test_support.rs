//! Test-only conveniences over the in-memory segment store.
//!
//! The store itself is production code — the Lucene import materializes
//! its definitions file through it — and lives in
//! [`crate::writer::memory_segments`]. Only the writer and the fixed
//! generation below are test scaffolding.

use super::RecordWriter;
use crate::writer::segment_builder::GarbageCollectionGeneration;

pub(crate) use crate::writer::memory_segments::MemorySegments as MemoryStore;

pub(crate) fn test_generation() -> GarbageCollectionGeneration {
    GarbageCollectionGeneration {
        generation: 1,
        full_generation: 1,
        is_compacted: false,
    }
}

pub(crate) fn new_writer() -> RecordWriter<MemoryStore> {
    RecordWriter::new(MemoryStore::default(), test_generation())
}
