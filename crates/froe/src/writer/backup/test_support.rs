//! The stores these tests back up and recover, including one whose
//! binary content lives in a bulk segment.

use crate::content::node::PropertyValues;
use crate::content::property::PropertyValue;
use crate::store::Repository;
use crate::writer::commit::create_checkpoint;
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::store_writer::WritableRepository;

pub(crate) struct TestDirectory {
    pub(crate) path: std::path::PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Populates a store with `/content` (a title property, two children)
/// and one checkpoint.
pub(crate) fn populate(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let alpha = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("alpha");
    let beta = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("beta");
    let title = writer.write_string("Backup Source").expect("value");
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Many(vec![("alpha".to_owned(), alpha), ("beta".to_owned(), beta)]),
            &[PropertyToWrite {
                name: "title".to_owned(),
                property_type: crate::content::property::PropertyType::String,
                values: PropertyValuesToWrite::Single(title),
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
    create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
    store.close().expect("close");
}

pub(crate) fn assert_content(directory: &std::path::Path, expected_title: &str) {
    let repository = Repository::open(directory).expect("reader");
    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    assert_eq!(content.child_node_count().expect("count"), 2);
    assert_eq!(
        content
            .property("title")
            .expect("read")
            .expect("present")
            .values,
        PropertyValues::Single(PropertyValue::String(expected_title.to_owned()))
    );
}

/// Writes one `/content` revision whose child count is `child_count`,
/// then a checkpoint so it is a full super-root candidate.
pub(crate) fn write_revision_with_children(directory: &std::path::Path, child_count: usize) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let mut children = Vec::new();
    for index in 0..child_count {
        let child = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("child");
        children.push((format!("child-{index}"), child));
    }
    let child_structure = if children.is_empty() {
        ChildNodesToWrite::Zero
    } else {
        ChildNodesToWrite::Many(children)
    };
    let content = writer
        .write_node(Some("nt:unstructured"), &[], &child_structure, &[])
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
    create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
    store.close().expect("close");
}

/// A binary long enough that its blocks land in a bulk segment, which
/// is the only shape that distinguishes copying from referencing.
///
/// Blocks are 4 KiB and a full 256 KiB run becomes a bulk segment, so
/// this is comfortably over that threshold.
pub(crate) const BULK_BINARY_BYTES: usize = 1024 * 1024;

pub(crate) fn bulk_binary_content() -> Vec<u8> {
    (0..BULK_BINARY_BYTES)
        .map(|index| (index % 251) as u8)
        .collect()
}

/// Writes a store whose head carries one binary big enough to occupy a
/// bulk segment.
pub(crate) fn populate_with_bulk_binary(directory: &std::path::Path) -> Vec<u8> {
    let content = bulk_binary_content();
    let store = WritableRepository::open(directory).expect("open the source");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let binary = writer
        .write_binary_content(&content)
        .expect("write the binary");
    let resource = writer
        .write_node(
            Some("nt:resource"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: crate::content::property::PropertyType::Binary,
                values: PropertyValuesToWrite::Single(binary),
            }],
        )
        .expect("write the resource");
    let root = writer
        .write_node(
            Some("rep:root"),
            &[],
            &ChildNodesToWrite::One {
                name: "file".to_owned(),
                node: resource,
            },
            &[],
        )
        .expect("write the root");
    let checkpoints = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("write the checkpoints container");
    let super_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("root".to_owned(), root),
                ("checkpoints".to_owned(), checkpoints),
            ]),
            &[],
        )
        .expect("write the super-root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(
        store.compare_and_set_head(previous, super_root),
        "advance the head"
    );
    store.close().expect("close");
    content
}
