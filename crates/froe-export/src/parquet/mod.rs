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
//!
//! Every export stamps both file footers with an [`ExportProvenance`]:
//! the head revision the rows reflect, the exported root path, and the
//! depth limit. The stamp is what makes an existing export *refreshable*
//! — see the `refresh` module — and it is written identically into both
//! files, so a crash between the two renames of a refresh leaves
//! disagreeing footers that the next refresh simply treats as
//! "not reusable".

use std::io::Write;
use std::sync::Arc;

use ::parquet::basic::{Compression, ZstdLevel};
use ::parquet::data_type::{
    BoolType, ByteArray, ByteArrayType, DataType, DoubleType, Int32Type, Int64Type,
};
use ::parquet::file::metadata::KeyValue;
use ::parquet::file::properties::WriterProperties;
use ::parquet::file::writer::{SerializedFileWriter, SerializedRowGroupWriter};
use ::parquet::schema::parser::parse_message_type;
use froe::content::value::BinaryValue;
use froe::content::{PropertyValue, PropertyValues};

use crate::export::{ExportSink, ExportedNode};

mod provenance;
mod row;
mod schema;
#[cfg(test)]
mod test_support;

pub use provenance::*;
pub(crate) use row::*;
pub(crate) use schema::*;

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

/// An [`ExportSink`] writing the nodes and properties Parquet tables.
pub struct ParquetSink<W: Write + Send> {
    pub(crate) nodes_writer: SerializedFileWriter<W>,
    pub(crate) properties_writer: SerializedFileWriter<W>,
    pub(crate) nodes: NodesBuffer,
    pub(crate) properties: PropertiesBuffer,
    pub(crate) row_group_row_limit: usize,
}

impl<W: Write + Send> ParquetSink<W> {
    /// Creates a sink writing the nodes table to `nodes_output` and the
    /// properties table to `properties_output`. The files carry no
    /// provenance; exports a refresh should build on use
    /// [`ParquetSink::new_with_provenance`].
    pub fn new(
        nodes_output: W,
        properties_output: W,
        options: &ParquetExportOptions,
    ) -> froe::Result<Self> {
        Self::build(nodes_output, properties_output, options, None)
    }

    /// Creates a sink like [`ParquetSink::new`], additionally stamping
    /// both file footers with `provenance`.
    pub fn new_with_provenance(
        nodes_output: W,
        properties_output: W,
        options: &ParquetExportOptions,
        provenance: &ExportProvenance,
    ) -> froe::Result<Self> {
        Self::build(nodes_output, properties_output, options, Some(provenance))
    }

    pub(super) fn build(
        nodes_output: W,
        properties_output: W,
        options: &ParquetExportOptions,
        provenance: Option<&ExportProvenance>,
    ) -> froe::Result<Self> {
        let writer_properties = Arc::new(
            WriterProperties::builder()
                .set_compression(Compression::ZSTD(ZstdLevel::default()))
                .set_key_value_metadata(provenance.map(ExportProvenance::to_metadata))
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

    /// Appends one row to the nodes buffer, flushing a row group when
    /// the row limit is reached. The export root passes `parent_path`
    /// `None`.
    pub(crate) fn append_node_row(
        &mut self,
        path: &str,
        parent_path: Option<&str>,
        name: &str,
        depth: i32,
        primary_type: Option<&str>,
    ) -> froe::Result<()> {
        self.nodes
            .paths
            .push(ByteArray::from(path.as_bytes().to_vec()));
        self.nodes
            .parent_paths
            .append(parent_path.map(|parent| ByteArray::from(parent.as_bytes().to_vec())));
        self.nodes
            .names
            .push(ByteArray::from(name.as_bytes().to_vec()));
        self.nodes.depths.push(depth);
        self.nodes
            .primary_types
            .append(primary_type.map(|name| ByteArray::from(name.as_bytes().to_vec())));
        if self.nodes.row_count() >= self.row_group_row_limit {
            self.flush_nodes()?;
        }
        Ok(())
    }

    /// Appends one row to the properties buffer: the value in its
    /// physical shape — at most one of the value columns is `Some`,
    /// matching the table's one-column-per-shape layout.
    #[allow(
        clippy::too_many_arguments,
        reason = "one argument per table column; a parameter struct would restate the schema"
    )]
    pub(crate) fn append_property_columns(
        &mut self,
        path: &str,
        name: &str,
        property_type: &str,
        multiple: bool,
        position: Option<i32>,
        text: Option<&str>,
        long_value: Option<i64>,
        double_value: Option<f64>,
        boolean_value: Option<bool>,
        binary_length: Option<i64>,
        binary_reference: Option<&str>,
    ) -> froe::Result<()> {
        let buffer = &mut self.properties;
        buffer.paths.push(ByteArray::from(path.as_bytes().to_vec()));
        buffer.names.push(ByteArray::from(name.as_bytes().to_vec()));
        buffer
            .property_types
            .push(ByteArray::from(property_type.as_bytes().to_vec()));
        buffer.multiples.push(multiple);
        buffer.positions.append(position);
        buffer
            .values
            .append(text.map(|text| ByteArray::from(text.as_bytes().to_vec())));
        buffer.long_values.append(long_value);
        buffer.double_values.append(double_value);
        buffer.boolean_values.append(boolean_value);
        buffer.binary_lengths.append(binary_length);
        buffer.binary_references.append(
            binary_reference.map(|reference| ByteArray::from(reference.as_bytes().to_vec())),
        );
        if self.properties.row_count() >= self.row_group_row_limit {
            self.flush_properties()?;
        }
        Ok(())
    }

    /// Appends one property *value* as a row, decomposing the value into
    /// its physical column. `position` is the value's index within a
    /// multi-valued property, `None` for the marker row of an empty one;
    /// `value` is `None` for that same marker row.
    pub(crate) fn append_property_row(
        &mut self,
        path: &str,
        name: &str,
        property_type: &str,
        multiple: bool,
        position: Option<i32>,
        value: Option<&PropertyValue>,
    ) -> froe::Result<()> {
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
            ) => text = Some(content.as_str()),
            Some(PropertyValue::Long(number)) => long_value = Some(*number),
            // Parquet doubles carry NaN and the infinities natively; no
            // textual fallback is needed.
            Some(PropertyValue::Double(number)) => double_value = Some(*number),
            Some(PropertyValue::Boolean(truth)) => boolean_value = Some(*truth),
            Some(PropertyValue::Binary(BinaryValue::Inline { length, .. })) => {
                binary_length = Some(*length as i64);
            }
            Some(PropertyValue::Binary(BinaryValue::External { blob_identifier })) => {
                binary_reference = Some(blob_identifier.as_str());
            }
        }
        self.append_property_columns(
            path,
            name,
            property_type,
            multiple,
            position,
            text,
            long_value,
            double_value,
            boolean_value,
            binary_length,
            binary_reference,
        )
    }

    /// Writes the buffered nodes rows as one row group.
    pub(super) fn flush_nodes(&mut self) -> froe::Result<()> {
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
    pub(super) fn flush_properties(&mut self) -> froe::Result<()> {
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
        self.append_node_row(
            node.path,
            parent_path,
            name,
            node.depth as i32,
            primary_type,
        )?;

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
                    )?;
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
                    )?;
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
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> froe::Result<()> {
        self.flush_nodes()?;
        self.flush_properties()?;
        self.nodes_writer.finish().map_err(parquet_error)?;
        self.properties_writer.finish().map_err(parquet_error)?;
        // The Parquet writers flushed their own buffers on finish, but
        // an outer buffering writer may still hold bytes; flush it here,
        // where an error propagates — a buffer flushed on drop would
        // swallow a failed final write, and a refresh renames these
        // files over a good export.
        self.nodes_writer.inner_mut().flush()?;
        self.properties_writer.inner_mut().flush()?;
        Ok(())
    }
}

/// Wraps a Parquet library error as an output error.
pub(crate) fn parquet_error(error: ::parquet::errors::ParquetError) -> froe::Error {
    froe::Error::InputOutput(std::io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use super::ParquetExportOptions;
    use crate::parquet::test_support::{
        TestDirectory, export, field, nodes_name, populate, properties_name, read_rows,
    };
    use ::parquet::file::reader::{FileReader, SerializedFileReader};
    use ::parquet::record::{Field, Row};

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

    #[expect(
        clippy::cognitive_complexity,
        reason = "30 assertions and no branches; the lint counts each \
                  `assert!` expansion as a decision point"
    )]
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
