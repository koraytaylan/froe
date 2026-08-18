//! A store to export and the database file the export leaves behind.

use super::{SqliteExportOptions, SqliteSink};
use crate::export::export_subtree;
use froe::content::PropertyType;
use froe::store::Repository;
use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use froe::writer::store_writer::WritableRepository;
use rusqlite::Connection;

pub(crate) struct TestDirectory {
    pub(crate) path: std::path::PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-sqlite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create");
        Self { path }
    }

    pub(crate) fn store(&self) -> std::path::PathBuf {
        self.path.join("segmentstore")
    }

    pub(crate) fn database(&self) -> std::path::PathBuf {
        self.path.join("export.db")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Writes a store whose content tree is `/content/jcr:content`, with
/// one property of every physical value shape on `/content`.
pub(crate) fn populate(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let page_content = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("jcr:content");

    let title = writer.write_string("Hello").expect("title");
    let tag_a = writer.write_string("a").expect("tag a");
    let tag_b = writer.write_string("b").expect("tag b");
    let count = writer.write_string("42").expect("count");
    let ratio = writer.write_string("2.5").expect("ratio");
    let flag = writer.write_string("true").expect("flag");
    let data = writer.write_binary_content(&[1, 2, 3]).expect("data");
    let external = writer
        .write_external_binary_identifier("blob-1")
        .expect("external");
    let single = |value| PropertyValuesToWrite::Single(value);
    let properties = [
        PropertyToWrite {
            name: "title".to_owned(),
            property_type: PropertyType::String,
            values: single(title),
        },
        PropertyToWrite {
            name: "tags".to_owned(),
            property_type: PropertyType::String,
            values: PropertyValuesToWrite::Multiple(vec![tag_a, tag_b]),
        },
        PropertyToWrite {
            name: "empty_tags".to_owned(),
            property_type: PropertyType::String,
            values: PropertyValuesToWrite::Multiple(Vec::new()),
        },
        PropertyToWrite {
            name: "count".to_owned(),
            property_type: PropertyType::Long,
            values: single(count),
        },
        PropertyToWrite {
            name: "ratio".to_owned(),
            property_type: PropertyType::Double,
            values: single(ratio),
        },
        PropertyToWrite {
            name: "flag".to_owned(),
            property_type: PropertyType::Boolean,
            values: single(flag),
        },
        PropertyToWrite {
            name: "data".to_owned(),
            property_type: PropertyType::Binary,
            values: single(data),
        },
        PropertyToWrite {
            name: "external".to_owned(),
            property_type: PropertyType::Binary,
            values: single(external),
        },
    ];
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:content".to_owned(),
                node: page_content,
            },
            &properties,
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

/// Exports `path` from the store into the test database and returns
/// a fresh read connection to it.
pub(crate) fn export(directory: &TestDirectory, path: &str) -> Connection {
    export_with_options(
        directory,
        path,
        SqliteExportOptions {
            create_indexes: false,
        },
    )
}

/// Exports exactly like [`export`], additionally creating the lookup
/// indexes.
pub(crate) fn export_with_indexes(directory: &TestDirectory, path: &str) -> Connection {
    export_with_options(
        directory,
        path,
        SqliteExportOptions {
            create_indexes: true,
        },
    )
}

pub(crate) fn export_with_options(
    directory: &TestDirectory,
    path: &str,
    options: SqliteExportOptions,
) -> Connection {
    let repository = Repository::open(&directory.store()).expect("open");
    let mut sink =
        SqliteSink::create(&directory.store(), &directory.database(), options).expect("sink");
    export_subtree(&repository, path, None, &mut sink)
        .expect("export")
        .expect("root present");
    drop(sink);
    Connection::open(directory.database()).expect("reopen")
}
