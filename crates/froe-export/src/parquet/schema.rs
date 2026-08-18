//! The two tables' columns, and the buffers a row is accumulated into
//! before a row group is written.

use super::{ByteArray, DataType, SerializedRowGroupWriter, Write, parquet_error};

/// The nodes table: one row per node.
pub(crate) const NODES_SCHEMA: &str = "
    message nodes {
        required binary path (UTF8);
        optional binary parent_path (UTF8);
        required binary name (UTF8);
        required int32 depth;
        optional binary primary_type (UTF8);
    }
";

/// The properties table: one row per property value.
pub(crate) const PROPERTIES_SCHEMA: &str = "
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

/// One optional column's buffer: definition levels for every row, values
/// for the non-null rows only — the shape Parquet batch writes expect.
pub(crate) struct OptionalColumn<T> {
    pub(crate) definition_levels: Vec<i16>,
    pub(crate) values: Vec<T>,
}

impl<T> OptionalColumn<T> {
    pub(crate) fn new() -> Self {
        Self {
            definition_levels: Vec::new(),
            values: Vec::new(),
        }
    }

    pub(crate) fn append(&mut self, value: Option<T>) {
        match value {
            Some(value) => {
                self.definition_levels.push(1);
                self.values.push(value);
            }
            None => self.definition_levels.push(0),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.definition_levels.clear();
        self.values.clear();
    }
}

/// Buffered rows of the nodes table, column by column.
pub(crate) struct NodesBuffer {
    pub(crate) paths: Vec<ByteArray>,
    pub(crate) parent_paths: OptionalColumn<ByteArray>,
    pub(crate) names: Vec<ByteArray>,
    pub(crate) depths: Vec<i32>,
    pub(crate) primary_types: OptionalColumn<ByteArray>,
}

impl NodesBuffer {
    pub(crate) fn new() -> Self {
        Self {
            paths: Vec::new(),
            parent_paths: OptionalColumn::new(),
            names: Vec::new(),
            depths: Vec::new(),
            primary_types: OptionalColumn::new(),
        }
    }

    pub(crate) fn row_count(&self) -> usize {
        self.paths.len()
    }

    pub(crate) fn clear(&mut self) {
        self.paths.clear();
        self.parent_paths.clear();
        self.names.clear();
        self.depths.clear();
        self.primary_types.clear();
    }
}

/// Buffered rows of the properties table, column by column.
pub(crate) struct PropertiesBuffer {
    pub(crate) paths: Vec<ByteArray>,
    pub(crate) names: Vec<ByteArray>,
    pub(crate) property_types: Vec<ByteArray>,
    pub(crate) multiples: Vec<bool>,
    pub(crate) positions: OptionalColumn<i32>,
    pub(crate) values: OptionalColumn<ByteArray>,
    pub(crate) long_values: OptionalColumn<i64>,
    pub(crate) double_values: OptionalColumn<f64>,
    pub(crate) boolean_values: OptionalColumn<bool>,
    pub(crate) binary_lengths: OptionalColumn<i64>,
    pub(crate) binary_references: OptionalColumn<ByteArray>,
}

impl PropertiesBuffer {
    pub(crate) fn new() -> Self {
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

    pub(crate) fn row_count(&self) -> usize {
        self.paths.len()
    }

    pub(crate) fn clear(&mut self) {
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

/// Writes one column's batch into the row group and closes it. Columns
/// must be appended in schema order; `definition_levels` is `None` for
/// required columns.
pub(crate) fn append_column<W: Write + Send, T: DataType>(
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
