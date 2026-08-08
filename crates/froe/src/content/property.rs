//! Property types and values.
//!
//! Every JCR value except binaries is stored as the UTF-8 string of its
//! Oak representation: longs as decimal text, doubles via Java's
//! `Double.toString`, booleans as `true`/`false`, decimals and dates as
//! their string forms. The property's type tag — kept in the node's
//! template — says how to interpret the string. Binaries are value records
//! of their own (inline or external, see [`BinaryValue`]).
//!
//! [`BinaryValue`]: crate::content::value::BinaryValue

use crate::content::provider::SegmentProvider;
use crate::content::value::{BinaryValue, read_binary_value, read_string};
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;

/// The JCR property types, with their standard numeric tags.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum PropertyType {
    /// An arbitrary string.
    String = 1,
    /// Binary content, inline or in an external blob store.
    Binary = 2,
    /// A 64-bit signed integer.
    Long = 3,
    /// A double-precision floating point number.
    Double = 4,
    /// A date, stored as an ISO-8601 string.
    Date = 5,
    /// A boolean.
    Boolean = 6,
    /// A JCR name, such as `nt:file`.
    Name = 7,
    /// A JCR path.
    Path = 8,
    /// A hard reference to another node's UUID.
    Reference = 9,
    /// A weak reference to another node's UUID.
    WeakReference = 10,
    /// A URI.
    Uri = 11,
    /// An arbitrary-precision decimal, stored as its string form.
    Decimal = 12,
}

impl PropertyType {
    /// Decodes a JCR property type tag (1 through 12).
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::String),
            2 => Some(Self::Binary),
            3 => Some(Self::Long),
            4 => Some(Self::Double),
            5 => Some(Self::Date),
            6 => Some(Self::Boolean),
            7 => Some(Self::Name),
            8 => Some(Self::Path),
            9 => Some(Self::Reference),
            10 => Some(Self::WeakReference),
            11 => Some(Self::Uri),
            12 => Some(Self::Decimal),
            _ => None,
        }
    }

    /// The JCR name of the type, as used in content serializations.
    #[must_use]
    pub const fn jcr_name(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Binary => "Binary",
            Self::Long => "Long",
            Self::Double => "Double",
            Self::Date => "Date",
            Self::Boolean => "Boolean",
            Self::Name => "Name",
            Self::Path => "Path",
            Self::Reference => "Reference",
            Self::WeakReference => "WeakReference",
            Self::Uri => "URI",
            Self::Decimal => "Decimal",
        }
    }
}

/// One decoded property value.
#[derive(Clone, PartialEq, Debug)]
pub enum PropertyValue {
    /// A string.
    String(String),
    /// A binary, inline or external.
    Binary(BinaryValue),
    /// A 64-bit integer.
    Long(i64),
    /// A double-precision floating point number.
    Double(f64),
    /// A date in its stored ISO-8601 string form.
    Date(String),
    /// A boolean.
    Boolean(bool),
    /// A JCR name.
    Name(String),
    /// A JCR path.
    Path(String),
    /// A hard reference (a UUID string).
    Reference(String),
    /// A weak reference (a UUID string).
    WeakReference(String),
    /// A URI.
    Uri(String),
    /// A decimal in its stored string form.
    Decimal(String),
}

impl PropertyValue {
    /// The stored string form of the value, when the value is a string
    /// kind. Binaries return `None`; numeric and boolean values return
    /// their canonical rendering.
    #[must_use]
    pub fn as_text(&self) -> Option<String> {
        match self {
            Self::String(text)
            | Self::Date(text)
            | Self::Name(text)
            | Self::Path(text)
            | Self::Reference(text)
            | Self::WeakReference(text)
            | Self::Uri(text)
            | Self::Decimal(text) => Some(text.clone()),
            Self::Long(value) => Some(value.to_string()),
            Self::Double(value) => Some(java_double_to_string(*value)),
            Self::Boolean(value) => Some(value.to_string()),
            Self::Binary(_) => None,
        }
    }
}

/// Renders a double the way `java.lang.Double::toString` does, so a value
/// re-stored by the writer round-trips through Oak's `Double.parseDouble`.
/// The only meaningful differences from Rust's `f64::Display` are the
/// non-finite spellings.
#[must_use]
pub fn java_double_to_string(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        }
    } else {
        value.to_string()
    }
}

/// Reads and decodes the value record at `value_identifier` as a value of
/// `property_type`.
pub fn read_property_value(
    provider: &dyn SegmentProvider,
    value_identifier: RecordIdentifier,
    property_type: PropertyType,
) -> Result<PropertyValue> {
    if property_type == PropertyType::Binary {
        return Ok(PropertyValue::Binary(read_binary_value(
            provider,
            value_identifier,
        )?));
    }
    let text = read_string(provider, value_identifier)?;
    let corrupt = |kind: &str| Error::InvalidFormat {
        details: format!("stored {kind} value {text:?} at {value_identifier} cannot be decoded"),
    };
    Ok(match property_type {
        PropertyType::String => PropertyValue::String(text),
        PropertyType::Long => PropertyValue::Long(text.parse().map_err(|_| corrupt("long"))?),
        PropertyType::Double => {
            // Java writes `Double.toString`: decimal forms plus the
            // spellings "NaN", "Infinity" and "-Infinity", all of which
            // Rust's parser accepts.
            PropertyValue::Double(text.parse().map_err(|_| corrupt("double"))?)
        }
        PropertyType::Date => PropertyValue::Date(text),
        PropertyType::Boolean => match text.as_str() {
            "true" => PropertyValue::Boolean(true),
            "false" => PropertyValue::Boolean(false),
            _ => return Err(corrupt("boolean")),
        },
        PropertyType::Name => PropertyValue::Name(text),
        PropertyType::Path => PropertyValue::Path(text),
        PropertyType::Reference => PropertyValue::Reference(text),
        PropertyType::WeakReference => PropertyValue::WeakReference(text),
        PropertyType::Uri => PropertyValue::Uri(text),
        PropertyType::Decimal => PropertyValue::Decimal(text),
        PropertyType::Binary => unreachable!("handled above"),
    })
}

#[cfg(test)]
mod tests {
    use super::{PropertyType, PropertyValue, read_property_value};
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;

    fn small_string_record(text: &str) -> Vec<u8> {
        let mut bytes = vec![text.len() as u8];
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    #[test]
    fn property_type_tags_round_trip() {
        for tag in 1..=12u8 {
            let property_type = PropertyType::from_tag(tag).expect("valid tag");
            assert_eq!(property_type as u8, tag);
        }
        assert_eq!(PropertyType::from_tag(0), None);
        assert_eq!(PropertyType::from_tag(13), None);
    }

    #[test]
    fn decodes_typed_values_from_stored_strings() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (0, 4, small_string_record("hello")),
                    (1, 4, small_string_record("-42")),
                    (2, 4, small_string_record("2.5")),
                    (3, 4, small_string_record("true")),
                    (4, 4, small_string_record("Infinity")),
                    (5, 4, small_string_record("2024-01-15T10:30:00.000Z")),
                ],
            ),
        );
        let record = |record_number| RecordIdentifier::new(segment, record_number);
        assert_eq!(
            read_property_value(&provider, record(0), PropertyType::String).expect("string"),
            PropertyValue::String("hello".to_owned())
        );
        assert_eq!(
            read_property_value(&provider, record(1), PropertyType::Long).expect("long"),
            PropertyValue::Long(-42)
        );
        assert_eq!(
            read_property_value(&provider, record(2), PropertyType::Double).expect("double"),
            PropertyValue::Double(2.5)
        );
        assert_eq!(
            read_property_value(&provider, record(3), PropertyType::Boolean).expect("boolean"),
            PropertyValue::Boolean(true)
        );
        assert_eq!(
            read_property_value(&provider, record(4), PropertyType::Double).expect("infinity"),
            PropertyValue::Double(f64::INFINITY)
        );
        assert_eq!(
            read_property_value(&provider, record(5), PropertyType::Date).expect("date"),
            PropertyValue::Date("2024-01-15T10:30:00.000Z".to_owned())
        );
    }

    #[test]
    fn rejects_undecodable_stored_values() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(0, 4, small_string_record("not-a-number"))]),
        );
        let record = RecordIdentifier::new(segment, 0);
        assert!(read_property_value(&provider, record, PropertyType::Long).is_err());
        assert!(read_property_value(&provider, record, PropertyType::Boolean).is_err());
    }

    #[test]
    fn as_text_renders_scalar_values() {
        assert_eq!(PropertyValue::Long(7).as_text(), Some("7".to_owned()));
        assert_eq!(
            PropertyValue::Boolean(false).as_text(),
            Some("false".to_owned())
        );
        assert_eq!(
            PropertyValue::Name("nt:file".to_owned()).as_text(),
            Some("nt:file".to_owned())
        );
    }
}
