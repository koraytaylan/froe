//! Reading a row back out of a table, column by column, with each
//! column's type named where it is read.

/// One row of the nodes table, decoded back from a Parquet file. The
/// merge half of a refresh replays these into a [`ParquetSink`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeRow {
    pub(crate) path: String,
    pub(crate) parent_path: Option<String>,
    pub(crate) name: String,
    pub(crate) depth: i32,
    pub(crate) primary_type: Option<String>,
}

impl NodeRow {
    /// Decodes one record-API row. `None` means the file does not carry
    /// the export's columns — it is not a froe Parquet export.
    pub(crate) fn decode(row: &::parquet::record::Row) -> Option<Self> {
        Some(Self {
            path: required_string(row, "path")?,
            parent_path: optional_string(row, "parent_path")?.into_option(),
            name: required_string(row, "name")?,
            depth: required_int(row, "depth")?,
            primary_type: optional_string(row, "primary_type")?.into_option(),
        })
    }
}

/// One row of the properties table, decoded back from a Parquet file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PropertyRow {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) property_type: String,
    pub(crate) multiple: bool,
    pub(crate) position: Option<i32>,
    pub(crate) value: Option<String>,
    pub(crate) long_value: Option<i64>,
    pub(crate) double_value: Option<f64>,
    pub(crate) boolean_value: Option<bool>,
    pub(crate) binary_length: Option<i64>,
    pub(crate) binary_reference: Option<String>,
}

impl PropertyRow {
    /// Decodes one record-API row. `None` means the file does not carry
    /// the export's columns — it is not a froe Parquet export.
    pub(crate) fn decode(row: &::parquet::record::Row) -> Option<Self> {
        Some(Self {
            path: required_string(row, "path")?,
            name: required_string(row, "name")?,
            property_type: required_string(row, "property_type")?,
            multiple: required_bool(row, "multiple")?,
            position: optional_int(row, "position")?.into_option(),
            value: optional_string(row, "value")?.into_option(),
            long_value: optional_long(row, "long_value")?.into_option(),
            double_value: optional_double(row, "double_value")?.into_option(),
            boolean_value: optional_bool(row, "boolean_value")?.into_option(),
            binary_length: optional_long(row, "binary_length")?.into_option(),
            binary_reference: optional_string(row, "binary_reference")?.into_option(),
        })
    }
}

/// An optional column's content: distinguishably *null* or a value —
/// the two states an `Option<Option<T>>` would smear together.
pub(crate) enum Column<T> {
    Null,
    Value(T),
}

impl<T> Column<T> {
    /// The column as an optional value.
    pub(crate) fn into_option(self) -> Option<T> {
        match self {
            Self::Null => None,
            Self::Value(value) => Some(value),
        }
    }
}

/// One named column of a record-API row.
pub(crate) fn column<'row>(
    row: &'row ::parquet::record::Row,
    name: &str,
) -> Option<&'row ::parquet::record::Field> {
    row.get_column_iter()
        .find(|(column, _)| column.as_str() == name)
        .map(|(_, field)| field)
}

/// Extractors per physical column shape. Each returns `None` on a
/// missing column or an unexpected field variant — both mean the file
/// was not written by this export.
pub(crate) fn required_string(row: &::parquet::record::Row, name: &str) -> Option<String> {
    match column(row, name)? {
        ::parquet::record::Field::Str(text) => Some(text.clone()),
        _ => None,
    }
}

pub(crate) fn optional_string(row: &::parquet::record::Row, name: &str) -> Option<Column<String>> {
    Some(match column(row, name)? {
        ::parquet::record::Field::Str(text) => Column::Value(text.clone()),
        ::parquet::record::Field::Null => Column::Null,
        _ => return None,
    })
}

pub(crate) fn required_int(row: &::parquet::record::Row, name: &str) -> Option<i32> {
    match column(row, name)? {
        ::parquet::record::Field::Int(number) => Some(*number),
        _ => None,
    }
}

pub(crate) fn optional_int(row: &::parquet::record::Row, name: &str) -> Option<Column<i32>> {
    Some(match column(row, name)? {
        ::parquet::record::Field::Int(number) => Column::Value(*number),
        ::parquet::record::Field::Null => Column::Null,
        _ => return None,
    })
}

pub(crate) fn optional_long(row: &::parquet::record::Row, name: &str) -> Option<Column<i64>> {
    Some(match column(row, name)? {
        ::parquet::record::Field::Long(number) => Column::Value(*number),
        ::parquet::record::Field::Null => Column::Null,
        _ => return None,
    })
}

pub(crate) fn optional_double(row: &::parquet::record::Row, name: &str) -> Option<Column<f64>> {
    Some(match column(row, name)? {
        ::parquet::record::Field::Double(number) => Column::Value(*number),
        ::parquet::record::Field::Null => Column::Null,
        _ => return None,
    })
}

pub(crate) fn required_bool(row: &::parquet::record::Row, name: &str) -> Option<bool> {
    match column(row, name)? {
        ::parquet::record::Field::Bool(truth) => Some(*truth),
        _ => None,
    }
}

pub(crate) fn optional_bool(row: &::parquet::record::Row, name: &str) -> Option<Column<bool>> {
    Some(match column(row, name)? {
        ::parquet::record::Field::Bool(truth) => Column::Value(*truth),
        ::parquet::record::Field::Null => Column::Null,
        _ => return None,
    })
}
