//! Compaction end to end on real stores: what a full or tail copy
//! preserves, what it reclaims, and what survives a second run.

use super::*;
use super::{CompactionKind, compact};
use crate::content::node::PropertyValues;
use crate::content::property::PropertyValue;
use crate::store::Repository;
use crate::writer::commit::{create_checkpoint, list_checkpoints};
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::store_writer::WritableRepository;

#[test]
fn full_compaction_preserves_content_and_checkpoints() {
    let directory = TestDirectory::new("full");
    build_populated_store(&directory);

    let outcome = {
        let mut store = WritableRepository::open(&directory.path).expect("open for compaction");
        let before_generation = store
            .segment_generation(store.head().segment)
            .expect("generation");
        let outcome = compact(&mut store, CompactionKind::Full).expect("compact");
        let after_generation = store
            .segment_generation(store.head().segment)
            .expect("generation");
        assert_eq!(
            after_generation.generation,
            before_generation.generation + 1
        );
        assert_eq!(
            after_generation.full_generation,
            before_generation.full_generation + 1
        );
        assert!(after_generation.is_compacted);
        store.close().expect("close");
        outcome
    };
    assert!(outcome.compacted_nodes > 0);

    assert_content_intact(&directory);

    // The journal is a single line and the reader opens cleanly.
    let journal = std::fs::read_to_string(directory.path.join("journal.log")).expect("journal");
    assert_eq!(journal.lines().count(), 1, "journal rewritten to one line");
    // A gc.log line was appended.
    let gc_log = std::fs::read_to_string(directory.path.join("gc.log")).expect("gc.log");
    assert_eq!(gc_log.lines().count(), 1);
    assert_eq!(gc_log.split(',').count(), 7, "seven gc.log fields");
}

#[test]
fn compaction_preserves_stable_identifiers() {
    let directory = TestDirectory::new("stable-ids");
    build_populated_store(&directory);

    // Record the content node's stable identifier before compaction.
    let before = {
        let repository = Repository::open(&directory.path).expect("reader");
        repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present")
            .stable_identifier()
            .expect("stable id")
    };
    {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        compact(&mut store, CompactionKind::Full).expect("compact");
        store.close().expect("close");
    }
    let after = {
        let repository = Repository::open(&directory.path).expect("reader");
        repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present")
            .stable_identifier()
            .expect("stable id")
    };
    assert_eq!(
        before, after,
        "the stable identifier survives compaction so Oak's fast path keeps matching"
    );
}

#[test]
fn compaction_preserves_infinite_doubles_and_type_named_properties() {
    let directory = TestDirectory::new("edge-values");
    {
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        // A DOUBLE property holding positive infinity, and a STRING
        // property literally named jcr:primaryType (a non-name-typed
        // reserved name, stored as an ordinary property by Oak).
        let infinity_value = writer.write_string("Infinity").expect("value");
        let odd_name_value = writer.write_string("literal").expect("value");
        // No synthesized (Name-typed) primary type, so the String
        // property literally named jcr:primaryType is the only carrier
        // of that name — exactly the shape Oak stores as an ordinary
        // property and that a name filter would drop.
        let content = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Zero,
                &[
                    PropertyToWrite {
                        name: "ratio".to_owned(),
                        property_type: crate::content::property::PropertyType::Double,
                        values: PropertyValuesToWrite::Single(infinity_value),
                    },
                    PropertyToWrite {
                        name: "jcr:primaryType".to_owned(),
                        property_type: crate::content::property::PropertyType::String,
                        values: PropertyValuesToWrite::Single(odd_name_value),
                    },
                ],
            )
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content,
                },
                &[],
            )
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.compare_and_set_head(previous, head));
        store.close().expect("close");
    }
    {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        compact(&mut store, CompactionKind::Full).expect("compact");
        store.close().expect("close");
    }
    let repository = Repository::open(&directory.path).expect("reader");
    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    // The infinite double survives with a value AEM can parse.
    let ratio = content.property("ratio").expect("read").expect("present");
    assert_eq!(
        ratio.values,
        PropertyValues::Single(PropertyValue::Double(f64::INFINITY))
    );
    // The oddly-typed jcr:primaryType survives as a String property,
    // not silently dropped.
    let odd = content
        .property("jcr:primaryType")
        .expect("read")
        .expect("present");
    assert_eq!(
        odd.property_type,
        crate::content::property::PropertyType::String
    );
    assert_eq!(
        odd.values,
        PropertyValues::Single(PropertyValue::String("literal".to_owned()))
    );
}

#[test]
fn compaction_streams_long_binaries_through_bulk_segments() {
    let directory = TestDirectory::new("long-binary");
    // A binary spanning multiple 4 KiB blocks plus a full 256 KiB bulk
    // run, so the streaming copy path (not the inline materialization)
    // is exercised.
    let content: Vec<u8> = (0..300 * 1024).map(|index| (index % 251) as u8).collect();
    {
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let binary_value = writer.write_binary_content(&content).expect("binary");
        let content_node = writer
            .write_node(
                Some("nt:file"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "data".to_owned(),
                    property_type: crate::content::property::PropertyType::Binary,
                    values: PropertyValuesToWrite::Single(binary_value),
                }],
            )
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content_node,
                },
                &[],
            )
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.compare_and_set_head(previous, head));
        store.close().expect("close");
    }
    {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        compact(&mut store, CompactionKind::Full).expect("compact");
        store.close().expect("close");
    }
    // The binary content survives compaction byte for byte.
    let repository = Repository::open(&directory.path).expect("reader");
    let content_node = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    let data = content_node
        .property("data")
        .expect("read")
        .expect("present");
    let record = match &data.values {
        PropertyValues::Single(PropertyValue::Binary(
            crate::content::value::BinaryValue::Inline {
                record_identifier, ..
            },
        )) => *record_identifier,
        other => panic!("expected an inline binary, got {other:?}"),
    };
    let read_back =
        crate::content::value::read_binary_content(&repository, record).expect("content");
    assert_eq!(
        read_back, content,
        "the long binary round-trips through compaction"
    );
}

#[test]
fn committing_after_compaction_in_one_session_persists_the_journal() {
    let directory = TestDirectory::new("commit-after-compact");
    build_populated_store(&directory);
    {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        compact(&mut store, CompactionKind::Full).expect("compact");
        // A checkpoint create moves the head; its journal line must
        // reach the live journal, not the orphaned pre-rewrite inode.
        create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
        store.close().expect("close");
    }
    // The reader resolves the post-compaction checkpoint head.
    let repository = Repository::open(&directory.path).expect("reader");
    assert_eq!(
        repository.checkpoints().expect("checkpoints").len(),
        2,
        "the checkpoint created after compaction is visible in the journal"
    );
}

#[test]
fn tail_compaction_keeps_the_full_generation() {
    let directory = TestDirectory::new("tail");
    build_populated_store(&directory);
    {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        let before = store
            .segment_generation(store.head().segment)
            .expect("generation");
        compact(&mut store, CompactionKind::Tail).expect("compact");
        let after = store
            .segment_generation(store.head().segment)
            .expect("generation");
        assert_eq!(after.generation, before.generation + 1);
        assert_eq!(
            after.full_generation, before.full_generation,
            "tail compaction keeps the full generation"
        );
        store.close().expect("close");
    }
    assert_content_intact(&directory);
}

#[test]
fn compaction_reclaims_disk_space_from_garbage() {
    let directory = TestDirectory::new("reclaim");
    // Write many revisions that leave garbage behind.
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        for revision in 0..30 {
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let value = writer
                .write_string(&format!("revision-{revision}").repeat(2000))
                .expect("value");
            let content = writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "data".to_owned(),
                        property_type: crate::content::property::PropertyType::String,
                        values: PropertyValuesToWrite::Single(value),
                    }],
                )
                .expect("content");
            let root = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "content".to_owned(),
                        node: content,
                    },
                    &[],
                )
                .expect("root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("super root");
            writer.finish().expect("finish");
            let previous = store.head();
            assert!(store.compare_and_set_head(previous, head));
            store.flush().expect("flush");
        }
        store.close().expect("close");
    }

    let mut store = WritableRepository::open(&directory.path).expect("open");
    let outcome = compact(&mut store, CompactionKind::Full).expect("compact");
    store.close().expect("close");
    assert!(
        outcome.size_after < outcome.size_before,
        "compaction reclaims garbage: {} -> {}",
        outcome.size_before,
        outcome.size_after
    );

    // Only the newest content survives; the reader opens cleanly.
    let repository = Repository::open(&directory.path).expect("reader");
    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    let data = content.property("data").expect("read").expect("present");
    assert_eq!(
        data.values,
        PropertyValues::Single(PropertyValue::String("revision-29".repeat(2000)))
    );
}

#[test]
fn compacted_stores_survive_a_second_compaction() {
    let directory = TestDirectory::new("twice");
    build_populated_store(&directory);
    for _ in 0..2 {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        compact(&mut store, CompactionKind::Full).expect("compact");
        store.close().expect("close");
        assert_content_intact(&directory);
    }
    let store = WritableRepository::open(&directory.path).expect("open");
    assert_eq!(list_checkpoints(&store).expect("list").len(), 1);
    store.close().expect("close");
}

#[test]
fn compaction_certifies_base_archives_before_writing_a_retry_copy() {
    let directory = TestDirectory::new("preflight-base-certificate");
    build_populated_store(&directory);
    let repository = Repository::open(&directory.path).expect("open healthy repository");
    let archive_name = repository.archives()[0].file_name().to_owned();
    drop(repository);
    corrupt_graph_checksum(&directory.path.join(&archive_name));

    let journal_before =
        std::fs::read(directory.path.join("journal.log")).expect("read journal before");
    let archives_before =
        crate::store::list_archive_file_names(&directory.path).expect("list archives before");
    let bytes_before: Vec<_> = archives_before
        .iter()
        .map(|name| {
            (
                name.clone(),
                std::fs::read(directory.path.join(name)).expect("read archive before"),
            )
        })
        .collect();

    for attempt in 1..=2 {
        let mut store = WritableRepository::open(&directory.path)
            .expect("ordinary read path tolerates an invalid optional graph");
        let error = compact(&mut store, CompactionKind::Full)
            .expect_err("strict reclaim source preflight must refuse the graph");
        assert!(error.to_string().contains("segment graph"), "{error}");
        drop(store);
        assert_eq!(
            crate::store::list_archive_file_names(&directory.path)
                .expect("list archives after refused attempt"),
            archives_before,
            "refused retry {attempt} must not allocate another compacted TAR"
        );
    }

    assert_eq!(
        crate::store::list_archive_file_names(&directory.path).expect("list archives after"),
        archives_before,
        "preflight refusal must not allocate a compacted TAR"
    );
    for (name, expected) in bytes_before {
        assert_eq!(
            std::fs::read(directory.path.join(name)).expect("read archive after"),
            expected
        );
    }
    assert_eq!(
        std::fs::read(directory.path.join("journal.log")).expect("read journal after"),
        journal_before,
        "preflight refusal must not publish another head"
    );
}

#[test]
fn tail_compaction_keeps_bulk_segments_referenced_by_retained_data_segments() {
    let directory = TestDirectory::new("tail-bulk-mark");
    build_populated_store(&directory);

    // A value long enough to force a full 256 KiB block run, stored
    // as a bulk segment referenced by the data segment holding the
    // value's block list.
    {
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let large = writer
            .write_string(&"bulk-backed-value ".repeat(20_000))
            .expect("large value");
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "data".to_owned(),
                    property_type: crate::content::property::PropertyType::String,
                    values: PropertyValuesToWrite::Single(large),
                }],
            )
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content,
                },
                &[],
            )
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.compare_and_set_head(previous, head));
        store.close().expect("close");
    }

    // Full compaction rewrites everything into compacted segments —
    // including fresh bulk segments at (0, 0, false), the triple the
    // format mandates for bulk.
    {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        compact(&mut store, CompactionKind::Full).expect("full compact");
        store.close().expect("close");
    }
    assert_no_dangling_segment_references(&directory);

    // Tail compaction *retains* the full-compacted data segments
    // (same full generation, compacted) — the mark phase must then
    // keep the generation-(0,0,false) bulk segments they reference,
    // which the generation predicate alone would reclaim.
    {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        compact(&mut store, CompactionKind::Tail).expect("tail compact");
        store.close().expect("close");
    }
    assert_no_dangling_segment_references(&directory);

    // The large value itself is still fully readable.
    let repository = Repository::open(&directory.path).expect("reader opens");
    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    let data = content.property("data").expect("read").expect("present");
    assert_eq!(
        data.values,
        PropertyValues::Single(PropertyValue::String("bulk-backed-value ".repeat(20_000)))
    );
}
