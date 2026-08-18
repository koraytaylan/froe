//! The store and export fixtures every refresh test starts from: a
//! repository it can revise, a full export to refresh against, and the
//! rows read back out of one.

use super::{ParquetRefresh, refresh_parquet_export};
use crate::export::export_subtree;
use crate::parquet::{ExportProvenance, NodeRow, ParquetExportOptions, ParquetSink, PropertyRow};
use froe::content::PropertyType;
use froe::store::Repository;
use froe::writer::StoreSink;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter,
};
use froe::writer::store_writer::WritableRepository;
use std::path::{Path, PathBuf};

pub(crate) struct TestDirectory {
    pub(crate) path: PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-refresh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create");
        Self { path }
    }

    pub(crate) fn store(&self) -> PathBuf {
        self.path.join("segmentstore")
    }

    pub(crate) fn export(&self) -> PathBuf {
        self.path.join("export")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A record writer with terse helpers for the fixture trees.
pub(crate) struct RevisionWriter<'store> {
    pub(crate) writer: RecordWriter<StoreSink<'store>>,
}

impl RevisionWriter<'_> {
    pub(crate) fn property(
        &mut self,
        name: &str,
        property_type: PropertyType,
        text: &str,
    ) -> PropertyToWrite {
        let value = self.writer.write_string(text).expect("write string");
        PropertyToWrite {
            name: name.to_owned(),
            property_type,
            values: PropertyValuesToWrite::Single(value),
        }
    }

    pub(crate) fn binary(&mut self, name: &str, content: &[u8]) -> PropertyToWrite {
        let value = self
            .writer
            .write_binary_content(content)
            .expect("write binary");
        PropertyToWrite {
            name: name.to_owned(),
            property_type: PropertyType::Binary,
            values: PropertyValuesToWrite::Single(value),
        }
    }

    pub(crate) fn node(
        &mut self,
        properties: &[PropertyToWrite],
        children: &ChildNodesToWrite,
    ) -> froe::RecordIdentifier {
        self.writer
            .write_node(Some("nt:unstructured"), &[], children, properties)
            .expect("write node")
    }

    pub(crate) fn leaf(&mut self, properties: &[PropertyToWrite]) -> froe::RecordIdentifier {
        self.node(properties, &ChildNodesToWrite::Zero)
    }

    pub(crate) fn child(
        &mut self,
        name: &str,
        node: froe::RecordIdentifier,
        properties: &[PropertyToWrite],
    ) -> froe::RecordIdentifier {
        self.node(
            properties,
            &ChildNodesToWrite::One {
                name: name.to_owned(),
                node,
            },
        )
    }
}

/// Commits one revision: `build` produces the content root record,
/// and the helper wraps it in root and super-root nodes and advances
/// the head.
pub(crate) fn revise(
    directory: &Path,
    build: impl FnOnce(&mut RevisionWriter) -> froe::RecordIdentifier,
) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = RevisionWriter {
        writer: store.record_writer(generation),
    };
    let root = build(&mut writer);
    let head = writer.child("root", root, &[]);
    writer.writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
}

/// The first fixture revision: `/content` with typed properties, a
/// `jcr:content` child, a `kept` subtree, and a `subtree` subtree.
/// `build` returns the content root, so the last node wraps
/// `/content` into it.
pub(crate) fn populate_first(directory: &Path) {
    revise(directory, |writer| {
        let ratio = writer.property("ratio", PropertyType::Double, "2.5");
        let jcr_content = writer.leaf(&[ratio]);
        let name = writer.property("name", PropertyType::String, "leaf");
        let leaf = writer.leaf(&[name]);
        let kept = writer.child("leaf", leaf, &[]);
        let x_node = writer.leaf(&[]);
        let flag = writer.property("flag", PropertyType::Boolean, "true");
        let subtree = writer.child("x", x_node, &[flag]);
        let title = writer.property("title", PropertyType::String, "Hello");
        let count = writer.property("count", PropertyType::Long, "42");
        let data = writer.binary("data", &[1, 2, 3]);
        let content = writer.node(
            &[title, count, data],
            &ChildNodesToWrite::Many(vec![
                ("jcr:content".to_owned(), jcr_content),
                ("kept".to_owned(), kept),
                ("subtree".to_owned(), subtree),
            ]),
        );
        writer.child("content", content, &[])
    });
}

/// The second fixture revision: `title` changed on `/content`,
/// `extra` added on `/content/jcr:content`, `/content/subtree`
/// removed, `/content/added/deep/x` added, `/content/kept`
/// byte-identical under a fresh record.
pub(crate) fn populate_second(directory: &Path) {
    revise(directory, |writer| {
        let x_node = writer.leaf(&[]);
        let deep = writer.child("x", x_node, &[]);
        let added = writer.child("deep", deep, &[]);
        let ratio = writer.property("ratio", PropertyType::Double, "2.5");
        let extra = writer.property("extra", PropertyType::String, "new");
        let jcr_content = writer.leaf(&[ratio, extra]);
        let name = writer.property("name", PropertyType::String, "leaf");
        let leaf = writer.leaf(&[name]);
        let kept = writer.child("leaf", leaf, &[]);
        let title = writer.property("title", PropertyType::String, "Goodbye");
        let count = writer.property("count", PropertyType::Long, "42");
        let data = writer.binary("data", &[1, 2, 3]);
        let content = writer.node(
            &[title, count, data],
            &ChildNodesToWrite::Many(vec![
                ("added".to_owned(), added),
                ("jcr:content".to_owned(), jcr_content),
                ("kept".to_owned(), kept),
            ]),
        );
        writer.child("content", content, &[])
    });
}

/// The head revision of the store in text form.
pub(crate) fn head_revision(directory: &Path) -> String {
    Repository::open(directory)
        .expect("open")
        .head_record_identifier()
        .to_string()
}

/// Runs a full export of the store into `output`, returning the
/// stamped revision. `stamped_revision` overrides the stamp, for
/// provenance-fixture tests.
pub(crate) fn full_export(
    store: &Path,
    root_path: &str,
    depth: Option<usize>,
    output: &Path,
    stamped_revision: Option<String>,
) -> String {
    std::fs::create_dir_all(output).expect("create export directory");
    let repository = Repository::open(store).expect("open");
    let revision = repository.head_record_identifier().to_string();
    let provenance = ExportProvenance::new(
        stamped_revision.unwrap_or_else(|| revision.clone()),
        root_path,
        depth,
    );
    let nodes = std::fs::File::create(output.join("nodes.parquet")).expect("nodes file");
    let properties =
        std::fs::File::create(output.join("properties.parquet")).expect("properties file");
    let mut sink = ParquetSink::new_with_provenance(
        nodes,
        properties,
        &ParquetExportOptions::default(),
        &provenance,
    )
    .expect("sink");
    export_subtree(&repository, root_path, depth, &mut sink).expect("export");
    revision
}

/// A full export without the provenance stamp — the shape a plain
/// `ParquetSink` produces.
pub(crate) fn full_export_without_stamp(store: &Path, root_path: &str, output: &Path) {
    std::fs::create_dir_all(output).expect("create export directory");
    let repository = Repository::open(store).expect("open");
    let nodes = std::fs::File::create(output.join("nodes.parquet")).expect("nodes file");
    let properties =
        std::fs::File::create(output.join("properties.parquet")).expect("properties file");
    let mut sink =
        ParquetSink::new(nodes, properties, &ParquetExportOptions::default()).expect("sink");
    export_subtree(&repository, root_path, None, &mut sink).expect("export");
}

pub(crate) fn refresh(
    store: &Path,
    root_path: &str,
    depth: Option<usize>,
    output: &Path,
) -> ParquetRefresh {
    let repository = Repository::open(store).expect("open");
    refresh_parquet_export(
        &repository,
        root_path,
        depth,
        output,
        &ParquetExportOptions::default(),
        &mut |_| {},
    )
    .expect("refresh")
}

/// Reads back a table's rows, sorted so order plays no part.
pub(crate) fn node_rows(output: &Path) -> Vec<NodeRow> {
    use ::parquet::file::reader::{FileReader, SerializedFileReader};
    let reader =
        SerializedFileReader::new(std::fs::File::open(output.join("nodes.parquet")).expect("open"))
            .expect("reader");
    let mut rows: Vec<NodeRow> = reader
        .get_row_iter(None)
        .expect("rows")
        .map(|row| NodeRow::decode(&row.expect("row")).expect("decode"))
        .collect();
    rows.sort_by(|first, second| first.path.cmp(&second.path));
    rows
}

/// Reads back the properties table's rows, sorted so order plays no
/// part.
pub(crate) fn property_rows(output: &Path) -> Vec<PropertyRow> {
    use ::parquet::file::reader::{FileReader, SerializedFileReader};
    let reader = SerializedFileReader::new(
        std::fs::File::open(output.join("properties.parquet")).expect("open"),
    )
    .expect("reader");
    let mut rows: Vec<PropertyRow> = reader
        .get_row_iter(None)
        .expect("rows")
        .map(|row| PropertyRow::decode(&row.expect("row")).expect("decode"))
        .collect();
    rows.sort_by(|first, second| {
        (&first.path, &first.name, first.position).cmp(&(
            &second.path,
            &second.name,
            second.position,
        ))
    });
    rows
}
