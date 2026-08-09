//! The Parquet sink: two flat tables built for analytical SQL.
//!
//! One export produces two files, sharing `path` as the join key:
//!
//! * **nodes** — one row per node: `path`, `parent_path` (null for the
//!   export root), `name`, `depth`, and `primary_type`. `parent_path`
//!   plus `depth` make hierarchy queries (recursive CTEs, subtree
//!   aggregates, `PARTITION BY parent_path`) plain column work.
//! * **properties** — one row per property *value*, multi-valued
//!   properties exploded with a `position`: `path`, `name`,
//!   `property_type` (the JCR type name), `multiple`, `position`, and
//!   one value column per physical shape — `value` for textual types,
//!   `long_value`, `double_value`, `boolean_value`, and for binaries
//!   `binary_length` (inline) or `binary_reference` (external); binary
//!   *content* is never embedded. An *empty* multi-valued property is
//!   one row with a null `position`, so its existence stays visible.
//!
//! Rows arrive in document order, so `path` is depth-first sorted and
//! Parquet's per-row-group statistics prune subtree predicates
//! (`WHERE path LIKE '/content/dam/%'`) without reading the skipped
//! groups. Columns are zstd-compressed and dictionary-encoded.

use std::io::Write;
use std::sync::Arc;

use ::parquet::basic::{Compression, ZstdLevel};
use ::parquet::data_type::{
    BoolType, ByteArray, ByteArrayType, DataType, DoubleType, Int32Type, Int64Type,
};
use ::parquet::file::properties::WriterProperties;
use ::parquet::file::writer::{SerializedFileWriter, SerializedRowGroupWriter};
use ::parquet::schema::parser::parse_message_type;
use froe::content::value::BinaryValue;
use froe::content::{PropertyValue, PropertyValues};

use crate::export::{ExportSink, ExportedNode};

/// The nodes table: one row per node.
const NODES_SCHEMA: &str = "
    message nodes {
        required binary path (UTF8);
        optional binary parent_path (UTF8);
        required binary name (UTF8);
        required int32 depth;
        optional binary primary_type (UTF8);
    }
";

/// The properties table: one row per property value.
const PROPERTIES_SCHEMA: &str = "
    message properties {
        required binary path (UTF8);
        required binary name (UTF8);
        required binary property_type (UTF8);
        required boolean multiple;
        optional int32 position;
        optional binary value (UTF8);
        optional int64 long_value;
        optional double double_value;
        optional boolean boolean_value;
        optional int64 binary_length;
        optional binary binary_reference (UTF8);
    }
";

/// Tuning knobs for a Parquet export.
pub struct ParquetExportOptions {
    /// Rows buffered per table before a row group is written. Smaller
    /// groups prune better and parallelize wider; larger groups compress
    /// better and keep metadata small.
    pub row_group_row_limit: usize,
}

impl Default for ParquetExportOptions {
    fn default() -> Self {
        Self {
            // Large enough for effective dictionary compression, small
            // enough that every core of a query engine gets groups to
            // scan and per-group statistics stay selective.
            row_group_row_limit: 262_144,
        }
    }
}

/// One optional column's buffer: definition levels for every row, values
/// for the non-null rows only — the shape Parquet batch writes expect.
struct OptionalColumn<T> {
    definition_levels: Vec<i16>,
    values: Vec<T>,
}

impl<T> OptionalColumn<T> {
    fn new() -> Self {
        Self {
            definition_levels: Vec::new(),
            values: Vec::new(),
        }
    }

    fn append(&mut self, value: Option<T>) {
        match value {
            Some(value) => {
                self.definition_levels.push(1);
                self.values.push(value);
            }
            None => self.definition_levels.push(0),
        }
    }

    fn clear(&mut self) {
        self.definition_levels.clear();
        self.values.clear();
    }
}

/// Buffered rows of the nodes table, column by column.
struct NodesBuffer {
    paths: Vec<ByteArray>,
    parent_paths: OptionalColumn<ByteArray>,
    names: Vec<ByteArray>,
    depths: Vec<i32>,
    primary_types: OptionalColumn<ByteArray>,
}

impl NodesBuffer {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            parent_paths: OptionalColumn::new(),
            names: Vec::new(),
            depths: Vec::new(),
            primary_types: OptionalColumn::new(),
        }
    }

    fn row_count(&self) -> usize {
        self.paths.len()
    }

    fn clear(&mut self) {
        self.paths.clear();
        self.parent_paths.clear();
        self.names.clear();
        self.depths.clear();
        self.primary_types.clear();
    }
}

/// Buffered rows of the properties table, column by column.
struct PropertiesBuffer {
    paths: Vec<ByteArray>,
    names: Vec<ByteArray>,
    property_types: Vec<ByteArray>,
    multiples: Vec<bool>,
    positions: OptionalColumn<i32>,
    values: OptionalColumn<ByteArray>,
    long_values: OptionalColumn<i64>,
    double_values: OptionalColumn<f64>,
    boolean_values: OptionalColumn<bool>,
    binary_lengths: OptionalColumn<i64>,
    binary_references: OptionalColumn<ByteArray>,
}

impl PropertiesBuffer {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            names: Vec::new(),
            property_types: Vec::new(),
            multiples: Vec::new(),
            positions: OptionalColumn::new(),
            values: OptionalColumn::new(),
            long_values: OptionalColumn::new(),
            double_values: OptionalColumn::new(),
            boolean_values: OptionalColumn::new(),
            binary_lengths: OptionalColumn::new(),
            binary_references: OptionalColumn::new(),
        }
    }

    fn row_count(&self) -> usize {
        self.paths.len()
    }

    fn clear(&mut self) {
        self.paths.clear();
        self.names.clear();
        self.property_types.clear();
        self.multiples.clear();
        self.positions.clear();
        self.values.clear();
        self.long_values.clear();
        self.double_values.clear();
        self.boolean_values.clear();
        self.binary_lengths.clear();
        self.binary_references.clear();
    }
}

/// An [`ExportSink`] writing the nodes and properties Parquet tables.
pub struct ParquetSink<W: Write + Send> {
    nodes_writer: SerializedFileWriter<W>,
    properties_writer: SerializedFileWriter<W>,
    nodes: NodesBuffer,
    properties: PropertiesBuffer,
    row_group_row_limit: usize,
}

impl<W: Write + Send> ParquetSink<W> {
    /// Creates a sink writing the nodes table to `nodes_output` and the
    /// properties table to `properties_output`.
    pub fn new(
        nodes_output: W,
        properties_output: W,
        options: &ParquetExportOptions,
    ) -> froe::Result<Self> {
        let writer_properties = Arc::new(
            WriterProperties::builder()
                .set_compression(Compression::ZSTD(ZstdLevel::default()))
                .build(),
        );
        let nodes_schema = parse_message_type(NODES_SCHEMA).map_err(parquet_error)?;
        let properties_schema = parse_message_type(PROPERTIES_SCHEMA).map_err(parquet_error)?;
        Ok(Self {
            nodes_writer: SerializedFileWriter::new(
                nodes_output,
                Arc::new(nodes_schema),
                Arc::clone(&writer_properties),
            )
            .map_err(parquet_error)?,
            properties_writer: SerializedFileWriter::new(
                properties_output,
                Arc::new(properties_schema),
                writer_properties,
            )
            .map_err(parquet_error)?,
            nodes: NodesBuffer::new(),
            properties: PropertiesBuffer::new(),
            row_group_row_limit: options.row_group_row_limit.max(1),
        })
    }

    /// Appends one row to the properties buffer. `position` is the value's
    /// index within a multi-valued property, `None` for the marker row of
    /// an empty one; `value` is `None` for non-textual types.
    #[allow(
        clippy::too_many_arguments,
        reason = "one argument per table column; a parameter struct would restate the schema"
    )]
    fn append_property_row(
        &mut self,
        path: &str,
        name: &str,
        property_type: &'static str,
        multiple: bool,
        position: Option<i32>,
        value: Option<&PropertyValue>,
    ) {
        let buffer = &mut self.properties;
        buffer.paths.push(ByteArray::from(path.as_bytes().to_vec()));
        buffer.names.push(ByteArray::from(name.as_bytes().to_vec()));
        buffer
            .property_types
            .push(ByteArray::from(property_type.as_bytes().to_vec()));
        buffer.multiples.push(multiple);
        buffer.positions.append(position);

        let mut text = None;
        let mut long_value = None;
        let mut double_value = None;
        let mut boolean_value = None;
        let mut binary_length = None;
        let mut binary_reference = None;
        match value {
            None => {}
            Some(
                PropertyValue::String(content)
                | PropertyValue::Date(content)
                | PropertyValue::Name(content)
                | PropertyValue::Path(content)
                | PropertyValue::Reference(content)
                | PropertyValue::WeakReference(content)
                | PropertyValue::Uri(content)
                | PropertyValue::Decimal(content),
            ) => text = Some(ByteArray::from(content.as_bytes().to_vec())),
            Some(PropertyValue::Long(number)) => long_value = Some(*number),
            // Parquet doubles carry NaN and the infinities natively; no
            // textual fallback is needed.
            Some(PropertyValue::Double(number)) => double_value = Some(*number),
            Some(PropertyValue::Boolean(truth)) => boolean_value = Some(*truth),
            Some(PropertyValue::Binary(BinaryValue::Inline { length, .. })) => {
                binary_length = Some(*length as i64);
            }
            Some(PropertyValue::Binary(BinaryValue::External { blob_identifier })) => {
                binary_reference = Some(ByteArray::from(blob_identifier.as_bytes().to_vec()));
            }
        }
        buffer.values.append(text);
        buffer.long_values.append(long_value);
        buffer.double_values.append(double_value);
        buffer.boolean_values.append(boolean_value);
        buffer.binary_lengths.append(binary_length);
        buffer.binary_references.append(binary_reference);
    }

    /// Writes the buffered nodes rows as one row group.
    fn flush_nodes(&mut self) -> froe::Result<()> {
        if self.nodes.row_count() == 0 {
            return Ok(());
        }
        let mut row_group = self.nodes_writer.next_row_group().map_err(parquet_error)?;
        append_column::<W, ByteArrayType>(&mut row_group, &self.nodes.paths, None)?;
        append_column::<W, ByteArrayType>(
            &mut row_group,
            &self.nodes.parent_paths.values,
            Some(&self.nodes.parent_paths.definition_levels),
        )?;
        append_column::<W, ByteArrayType>(&mut row_group, &self.nodes.names, None)?;
        append_column::<W, Int32Type>(&mut row_group, &self.nodes.depths, None)?;
        append_column::<W, ByteArrayType>(
            &mut row_group,
            &self.nodes.primary_types.values,
            Some(&self.nodes.primary_types.definition_levels),
        )?;
        row_group.close().map_err(parquet_error)?;
        self.nodes.clear();
        Ok(())
    }

    /// Writes the buffered properties rows as one row group.
    fn flush_properties(&mut self) -> froe::Result<()> {
        if self.properties.row_count() == 0 {
            return Ok(());
        }
        let buffer = &self.properties;
        let mut row_group = self
            .properties_writer
            .next_row_group()
            .map_err(parquet_error)?;
        append_column::<W, ByteArrayType>(&mut row_group, &buffer.paths, None)?;
        append_column::<W, ByteArrayType>(&mut row_group, &buffer.names, None)?;
        append_column::<W, ByteArrayType>(&mut row_group, &buffer.property_types, None)?;
        append_column::<W, BoolType>(&mut row_group, &buffer.multiples, None)?;
        append_column::<W, Int32Type>(
            &mut row_group,
            &buffer.positions.values,
            Some(&buffer.positions.definition_levels),
        )?;
        append_column::<W, ByteArrayType>(
            &mut row_group,
            &buffer.values.values,
            Some(&buffer.values.definition_levels),
        )?;
        append_column::<W, Int64Type>(
            &mut row_group,
            &buffer.long_values.values,
            Some(&buffer.long_values.definition_levels),
        )?;
        append_column::<W, DoubleType>(
            &mut row_group,
            &buffer.double_values.values,
            Some(&buffer.double_values.definition_levels),
        )?;
        append_column::<W, BoolType>(
            &mut row_group,
            &buffer.boolean_values.values,
            Some(&buffer.boolean_values.definition_levels),
        )?;
        append_column::<W, Int64Type>(
            &mut row_group,
            &buffer.binary_lengths.values,
            Some(&buffer.binary_lengths.definition_levels),
        )?;
        append_column::<W, ByteArrayType>(
            &mut row_group,
            &buffer.binary_references.values,
            Some(&buffer.binary_references.definition_levels),
        )?;
        row_group.close().map_err(parquet_error)?;
        self.properties.clear();
        Ok(())
    }
}

impl<W: Write + Send> ExportSink for ParquetSink<W> {
    fn write_node(&mut self, node: &ExportedNode<'_>) -> froe::Result<()> {
        // The export root has no parent within the export; every other
        // path splits into its parent and its own name.
        let (parent_path, name) = if node.depth == 0 {
            (None, node.path.rsplit('/').next().unwrap_or(""))
        } else {
            match node.path.rsplit_once('/') {
                Some(("", name)) => (Some("/"), name),
                Some((parent, name)) => (Some(parent), name),
                None => (None, node.path),
            }
        };
        let primary_type = node.properties.iter().find_map(|property| {
            if property.name != "jcr:primaryType" {
                return None;
            }
            match &property.values {
                PropertyValues::Single(PropertyValue::Name(name)) => Some(name.as_str()),
                _ => None,
            }
        });
        self.nodes
            .paths
            .push(ByteArray::from(node.path.as_bytes().to_vec()));
        self.nodes
            .parent_paths
            .append(parent_path.map(|parent| ByteArray::from(parent.as_bytes().to_vec())));
        self.nodes
            .names
            .push(ByteArray::from(name.as_bytes().to_vec()));
        self.nodes.depths.push(node.depth as i32);
        self.nodes
            .primary_types
            .append(primary_type.map(|name| ByteArray::from(name.as_bytes().to_vec())));

        for property in node.properties {
            let property_type = property.property_type.jcr_name();
            match &property.values {
                PropertyValues::Single(value) => {
                    self.append_property_row(
                        node.path,
                        &property.name,
                        property_type,
                        false,
                        Some(0),
                        Some(value),
                    );
                }
                PropertyValues::Multiple(values) if values.is_empty() => {
                    // The marker row: without it, an empty multi-valued
                    // property would vanish from the export entirely.
                    self.append_property_row(
                        node.path,
                        &property.name,
                        property_type,
                        true,
                        None,
                        None,
                    );
                }
                PropertyValues::Multiple(values) => {
                    for (position, value) in values.iter().enumerate() {
                        self.append_property_row(
                            node.path,
                            &property.name,
                            property_type,
                            true,
                            Some(position as i32),
                            Some(value),
                        );
                    }
                }
            }
        }

        if self.nodes.row_count() >= self.row_group_row_limit {
            self.flush_nodes()?;
        }
        if self.properties.row_count() >= self.row_group_row_limit {
            self.flush_properties()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> froe::Result<()> {
        self.flush_nodes()?;
        self.flush_properties()?;
        self.nodes_writer.finish().map_err(parquet_error)?;
        self.properties_writer.finish().map_err(parquet_error)?;
        Ok(())
    }
}

/// Writes one column's batch into the row group and closes it. Columns
/// must be appended in schema order; `definition_levels` is `None` for
/// required columns.
fn append_column<W: Write + Send, T: DataType>(
    row_group: &mut SerializedRowGroupWriter<'_, W>,
    values: &[T::T],
    definition_levels: Option<&[i16]>,
) -> froe::Result<()> {
    let mut column = row_group
        .next_column()
        .map_err(parquet_error)?
        .ok_or_else(|| froe::Error::InvalidFormat {
            details: "the Parquet schema has fewer columns than the export writes".to_owned(),
        })?;
    column
        .typed::<T>()
        .write_batch(values, definition_levels, None)
        .map_err(parquet_error)?;
    column.close().map_err(parquet_error)?;
    Ok(())
}

/// Wraps a Parquet library error as an output error.
fn parquet_error(error: ::parquet::errors::ParquetError) -> froe::Error {
    froe::Error::InputOutput(std::io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use ::parquet::file::reader::{FileReader, SerializedFileReader};
    use ::parquet::record::{Field, Row};
    use froe::content::PropertyType;
    use froe::store::Repository;
    use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use froe::writer::store_writer::WritableRepository;

    use super::{ParquetExportOptions, ParquetSink};
    use crate::export::export_subtree;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-parquet-{name}-{}", std::process::id()));
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
    fn populate(directory: &std::path::Path) {
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
        assert!(store.set_head(previous, head));
        store.close().expect("close");
    }

    /// Exports the whole store into `nodes.parquet` and
    /// `properties.parquet` inside `directory`, returning the node count.
    fn export(directory: &std::path::Path, options: &ParquetExportOptions) -> u64 {
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

    fn nodes_name(directory: &std::path::Path) -> String {
        format!(
            "{}-nodes.parquet",
            directory.file_name().expect("name").to_string_lossy()
        )
    }

    fn properties_name(directory: &std::path::Path) -> String {
        format!(
            "{}-properties.parquet",
            directory.file_name().expect("name").to_string_lossy()
        )
    }

    fn read_rows(path: &std::path::Path) -> Vec<Row> {
        let reader =
            SerializedFileReader::new(std::fs::File::open(path).expect("open")).expect("reader");
        reader
            .get_row_iter(None)
            .expect("row iterator")
            .map(|row| row.expect("row"))
            .collect()
    }

    fn field<'row>(row: &'row Row, name: &str) -> &'row Field {
        row.get_column_iter()
            .find(|(column, _)| column.as_str() == name)
            .map_or_else(|| panic!("column {name} missing"), |(_, value)| value)
    }

    #[test]
    fn exports_the_nodes_table() {
        let directory = TestDirectory::new("nodes-table");
        populate(&directory.path);
        assert_eq!(export(&directory.path, &ParquetExportOptions::default()), 3);

        let output = directory
            .path
            .parent()
            .expect("parent")
            .join(nodes_name(&directory.path));
        let rows = read_rows(&output);
        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(
            directory
                .path
                .parent()
                .expect("parent")
                .join(properties_name(&directory.path)),
        );

        assert_eq!(rows.len(), 3);
        assert_eq!(field(&rows[0], "path"), &Field::Str("/".to_owned()));
        assert_eq!(field(&rows[0], "parent_path"), &Field::Null);
        assert_eq!(field(&rows[0], "name"), &Field::Str(String::new()));
        assert_eq!(field(&rows[0], "depth"), &Field::Int(0));
        assert_eq!(field(&rows[0], "primary_type"), &Field::Null);

        assert_eq!(field(&rows[1], "path"), &Field::Str("/content".to_owned()));
        assert_eq!(field(&rows[1], "parent_path"), &Field::Str("/".to_owned()));
        assert_eq!(field(&rows[1], "name"), &Field::Str("content".to_owned()));
        assert_eq!(field(&rows[1], "depth"), &Field::Int(1));
        assert_eq!(
            field(&rows[1], "primary_type"),
            &Field::Str("nt:unstructured".to_owned())
        );

        assert_eq!(
            field(&rows[2], "path"),
            &Field::Str("/content/jcr:content".to_owned())
        );
        assert_eq!(
            field(&rows[2], "parent_path"),
            &Field::Str("/content".to_owned())
        );
        assert_eq!(field(&rows[2], "depth"), &Field::Int(2));
    }

    #[test]
    fn exports_the_properties_table() {
        let directory = TestDirectory::new("properties-table");
        populate(&directory.path);
        export(&directory.path, &ParquetExportOptions::default());

        let output = directory
            .path
            .parent()
            .expect("parent")
            .join(properties_name(&directory.path));
        let rows = read_rows(&output);
        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(
            directory
                .path
                .parent()
                .expect("parent")
                .join(nodes_name(&directory.path)),
        );

        let content_rows: Vec<&Row> = rows
            .iter()
            .filter(|row| field(row, "path") == &Field::Str("/content".to_owned()))
            .collect();
        let named = |name: &str| -> Vec<&&Row> {
            content_rows
                .iter()
                .filter(|row| field(row, "name") == &Field::Str(name.to_owned()))
                .collect()
        };

        let primary = named("jcr:primaryType");
        assert_eq!(primary.len(), 1);
        assert_eq!(
            field(primary[0], "property_type"),
            &Field::Str("Name".to_owned())
        );
        assert_eq!(field(primary[0], "multiple"), &Field::Bool(false));
        assert_eq!(field(primary[0], "position"), &Field::Int(0));
        assert_eq!(
            field(primary[0], "value"),
            &Field::Str("nt:unstructured".to_owned())
        );

        let title = named("title");
        assert_eq!(title.len(), 1);
        assert_eq!(field(title[0], "value"), &Field::Str("Hello".to_owned()));
        assert_eq!(field(title[0], "long_value"), &Field::Null);

        let tags = named("tags");
        assert_eq!(tags.len(), 2);
        assert_eq!(field(tags[0], "multiple"), &Field::Bool(true));
        assert_eq!(field(tags[0], "position"), &Field::Int(0));
        assert_eq!(field(tags[0], "value"), &Field::Str("a".to_owned()));
        assert_eq!(field(tags[1], "position"), &Field::Int(1));
        assert_eq!(field(tags[1], "value"), &Field::Str("b".to_owned()));

        let empty_tags = named("empty_tags");
        assert_eq!(empty_tags.len(), 1, "the marker row keeps it visible");
        assert_eq!(field(empty_tags[0], "multiple"), &Field::Bool(true));
        assert_eq!(field(empty_tags[0], "position"), &Field::Null);
        assert_eq!(field(empty_tags[0], "value"), &Field::Null);

        let count = named("count");
        assert_eq!(
            field(count[0], "property_type"),
            &Field::Str("Long".to_owned())
        );
        assert_eq!(field(count[0], "long_value"), &Field::Long(42));
        assert_eq!(field(count[0], "value"), &Field::Null);

        let ratio = named("ratio");
        assert_eq!(field(ratio[0], "double_value"), &Field::Double(2.5));

        let flag = named("flag");
        assert_eq!(field(flag[0], "boolean_value"), &Field::Bool(true));

        let data = named("data");
        assert_eq!(
            field(data[0], "property_type"),
            &Field::Str("Binary".to_owned())
        );
        assert_eq!(field(data[0], "binary_length"), &Field::Long(3));
        assert_eq!(field(data[0], "binary_reference"), &Field::Null);

        let external = named("external");
        assert_eq!(
            field(external[0], "binary_reference"),
            &Field::Str("blob-1".to_owned())
        );
        assert_eq!(field(external[0], "binary_length"), &Field::Null);
    }

    #[test]
    fn the_row_group_limit_splits_row_groups() {
        let directory = TestDirectory::new("row-groups");
        populate(&directory.path);
        export(
            &directory.path,
            &ParquetExportOptions {
                row_group_row_limit: 1,
            },
        );

        let output = directory
            .path
            .parent()
            .expect("parent")
            .join(nodes_name(&directory.path));
        let reader =
            SerializedFileReader::new(std::fs::File::open(&output).expect("open")).expect("reader");
        assert_eq!(reader.metadata().num_row_groups(), 3);
        assert_eq!(
            reader.get_row_iter(None).expect("row iterator").count(),
            3,
            "every row survives the split"
        );
        drop(reader);
        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(
            directory
                .path
                .parent()
                .expect("parent")
                .join(properties_name(&directory.path)),
        );
    }
}
