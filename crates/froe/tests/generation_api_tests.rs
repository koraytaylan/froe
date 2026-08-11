//! Public-path compatibility checks for garbage-collection generation metadata.

use froe::GarbageCollectionGeneration as RootGeneration;
use froe::segment::GarbageCollectionGeneration as NeutralGeneration;
use froe::writer::GarbageCollectionGeneration as WriterGeneration;
use froe::writer::segment_builder::GarbageCollectionGeneration as SegmentBuilderGeneration;

#[test]
fn legacy_writer_exports_are_the_neutral_type() {
    let neutral = NeutralGeneration {
        generation: 3,
        full_generation: 2,
        is_compacted: true,
    };
    let segment_builder_export: SegmentBuilderGeneration = neutral;
    let writer_export: WriterGeneration = segment_builder_export;
    let root_export: RootGeneration = writer_export;

    assert_eq!(root_export, neutral);
}
