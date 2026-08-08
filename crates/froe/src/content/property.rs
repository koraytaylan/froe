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
            Self::Double(value) => Some(double_to_text(*value)),
            Self::Boolean(value) => Some(value.to_string()),
            Self::Binary(_) => None,
        }
    }
}

/// Renders a double the way `java.lang.Double::toString` does, so a
/// value re-stored by the writer (compaction re-renders doubles exactly
/// as Oak's `SegmentWriter` does) reproduces the text Oak originally
/// wrote: `NaN`/`Infinity` spellings, a signed `0.0`, plain decimal form
/// with at least one fractional digit for magnitudes in
/// `[10^-3, 10^7)`, and `d.dddEn` computerized scientific notation
/// outside that range. Digit selection is the shortest representation
/// that round-trips, like modern Java; a handful of extreme subnormals
/// pick a different — equally round-tripping — shortest form (known
/// residue: `Double.MIN_VALUE` renders `5.0E-324` here where Java prints
/// `4.9E-324`; both parse to the identical bits, and a later Java
/// compaction would re-render its own spelling).
#[must_use]
pub fn double_to_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        };
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0".to_owned()
        } else {
            "0.0".to_owned()
        };
    }
    // Shortest round-trip digits and the decimal exponent, from Rust's
    // scientific rendering `d[.ddd]e<exponent>`. The fallbacks are
    // unreachable — the format always contains a parseable exponent —
    // but a plain rendering is safer than a panic.
    let scientific = format!("{:e}", value.abs());
    let Some((mantissa, exponent_text)) = scientific.split_once('e') else {
        return value.to_string();
    };
    let Ok(exponent) = exponent_text.parse::<i32>() else {
        return value.to_string();
    };
    let digits: String = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();

    let sign = if value.is_sign_negative() { "-" } else { "" };
    let digit_count = digits.len() as i32;
    let mut rendered = String::new();
    if (-3..7).contains(&exponent) {
        // Plain decimal form, always with a fractional part.
        if exponent < 0 {
            rendered.push_str("0.");
            for _ in 0..(-exponent - 1) {
                rendered.push('0');
            }
            rendered.push_str(&digits);
        } else if exponent >= digit_count - 1 {
            rendered.push_str(&digits);
            for _ in 0..(exponent - (digit_count - 1)) {
                rendered.push('0');
            }
            rendered.push_str(".0");
        } else {
            let (integer_digits, fraction_digits) = digits.split_at(exponent as usize + 1);
            rendered.push_str(integer_digits);
            rendered.push('.');
            rendered.push_str(fraction_digits);
        }
    } else {
        // Computerized scientific notation with at least one fractional
        // digit: `d.dddEn`.
        let (first_digit, rest) = digits.split_at(1);
        rendered.push_str(first_digit);
        rendered.push('.');
        if rest.is_empty() {
            rendered.push('0');
        } else {
            rendered.push_str(rest);
        }
        rendered.push('E');
        rendered.push_str(&exponent.to_string());
    }
    format!("{sign}{rendered}")
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
        // Java reads booleans with `Boolean.parseBoolean`, which never
        // fails: any string other than a case-insensitive "true" is
        // `false`. Being stricter would call content corrupt that a real
        // AEM instance reads without complaint.
        PropertyType::Boolean => PropertyValue::Boolean(text.eq_ignore_ascii_case("true")),
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
        // Booleans never fail: Java's Boolean.parseBoolean reads any
        // string other than a case-insensitive "true" as false.
        assert_eq!(
            read_property_value(&provider, record, PropertyType::Boolean).expect("boolean"),
            PropertyValue::Boolean(false)
        );
    }

    #[test]
    fn booleans_decode_like_java_parse_boolean() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (0, 4, small_string_record("TRUE")),
                    (1, 8, small_string_record("false")),
                ],
            ),
        );
        assert_eq!(
            read_property_value(
                &provider,
                RecordIdentifier::new(segment, 0),
                PropertyType::Boolean
            )
            .expect("case-insensitive true"),
            PropertyValue::Boolean(true)
        );
        assert_eq!(
            read_property_value(
                &provider,
                RecordIdentifier::new(segment, 1),
                PropertyType::Boolean
            )
            .expect("false"),
            PropertyValue::Boolean(false)
        );
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

    #[test]
    fn doubles_render_like_java_double_to_string() {
        // Expected strings are Java `Double.toString` outputs, written
        // out by hand — a compaction re-render must reproduce the text
        // Oak originally wrote.
        use super::double_to_text;
        assert_eq!(double_to_text(0.0), "0.0");
        assert_eq!(double_to_text(-0.0), "-0.0");
        assert_eq!(double_to_text(1.0), "1.0");
        assert_eq!(double_to_text(-1.5), "-1.5");
        assert_eq!(double_to_text(100.0), "100.0");
        assert_eq!(double_to_text(0.5), "0.5");
        assert_eq!(double_to_text(12345.678), "12345.678");
        assert_eq!(double_to_text(0.001), "0.001");
        assert_eq!(double_to_text(0.0001), "1.0E-4");
        assert_eq!(double_to_text(-0.000_025), "-2.5E-5");
        assert_eq!(double_to_text(9_999_999.0), "9999999.0");
        assert_eq!(double_to_text(10_000_000.0), "1.0E7");
        assert_eq!(double_to_text(123_456_789.0), "1.23456789E8");
        assert_eq!(
            double_to_text(f64::MAX),
            "1.7976931348623157E308",
            "the maximum double keeps its full seventeen digits"
        );
        assert_eq!(double_to_text(f64::NAN), "NaN");
        assert_eq!(double_to_text(f64::INFINITY), "Infinity");
        assert_eq!(double_to_text(f64::NEG_INFINITY), "-Infinity");
        // Every rendering round-trips to the identical bits, which is
        // what Oak's Double.parseDouble relies on.
        for value in [
            0.0,
            -0.0,
            1.0,
            -1.5,
            12345.678,
            0.0001,
            9_999_999.0,
            1.0e300,
            5e-324,
            f64::MAX,
        ] {
            let round_tripped: f64 = double_to_text(value).parse().expect("parses");
            assert_eq!(
                round_tripped.to_bits(),
                value.to_bits(),
                "{value} round-trips"
            );
        }
    }
}
