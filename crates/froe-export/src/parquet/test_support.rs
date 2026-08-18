//! A store to export and the rows read back out of the result.

use super::{ParquetExportOptions, ParquetSink};
use crate::export::export_subtree;
use ::parquet::file::reader::{FileReader, SerializedFileReader};
use ::parquet::record::{Field, Row};
use froe::content::PropertyType;
use froe::store::Repository;
use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use froe::writer::store_writer::WritableRepository;

pub(crate) struct TestDirectory {
    pub(crate) path: std::path::PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-parquet-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create");
        Self { path }
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

/// Exports the whole store into `nodes.parquet` and
/// `properties.parquet` inside `directory`, returning the node count.
pub(crate) fn export(directory: &std::path::Path, options: &ParquetExportOptions) -> u64 {
    let repository = Repository::open(directory).expect("open");
    let nodes_file = std::fs::File::create(
        directory
            .parent()
            .expect("parent")
            .join(nodes_name(directory)),
    )
    .expect("nodes file");
    let properties_file = std::fs::File::create(
        directory
            .parent()
            .expect("parent")
            .join(properties_name(directory)),
    )
    .expect("properties file");
    let mut sink = ParquetSink::new(nodes_file, properties_file, options).expect("sink");
    export_subtree(&repository, "/", None, &mut sink)
        .expect("export")
        .expect("root present")
}

pub(crate) fn nodes_name(directory: &std::path::Path) -> String {
    format!(
        "{}-nodes.parquet",
        directory.file_name().expect("name").to_string_lossy()
    )
}

pub(crate) fn properties_name(directory: &std::path::Path) -> String {
    format!(
        "{}-properties.parquet",
        directory.file_name().expect("name").to_string_lossy()
    )
}

pub(crate) fn read_rows(path: &std::path::Path) -> Vec<Row> {
    let reader =
        SerializedFileReader::new(std::fs::File::open(path).expect("open")).expect("reader");
    reader
        .get_row_iter(None)
        .expect("row iterator")
        .map(|row| row.expect("row"))
        .collect()
}

pub(crate) fn field<'row>(row: &'row Row, name: &str) -> &'row Field {
    row.get_column_iter()
        .find(|(column, _)| column.as_str() == name)
        .map_or_else(|| panic!("column {name} missing"), |(_, value)| value)
}
