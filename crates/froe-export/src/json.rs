//! JSON encoding of property values, without a serialization dependency.
//!
//! The values are simple enough that direct encoding is both smaller and
//! faster than a generic serializer. These helpers are public because
//! consumers rendering property values outside a full export — the
//! command line's node and tree displays, for example — must emit the
//! exact same JSON forms.

use std::fmt::Write;

use froe::content::value::BinaryValue;
use froe::content::{PropertyValue, PropertyValues};

/// Appends `text` to `buffer` as a JSON string literal with escaping.
/// C1 controls and DEL are escaped too — JSON permits them literally,
/// but terminals interpret C1 bytes as control sequences.
pub fn append_json_string(buffer: &mut String, text: &str) {
    buffer.push('"');
    for character in text.chars() {
        match character {
            '"' => buffer.push_str("\\\""),
            '\\' => buffer.push_str("\\\\"),
            '\n' => buffer.push_str("\\n"),
            '\r' => buffer.push_str("\\r"),
            '\t' => buffer.push_str("\\t"),
            control if control.is_control() => {
                let _ = write!(buffer, "\\u{:04x}", control as u32);
            }
            other => buffer.push(other),
        }
    }
    buffer.push('"');
}

/// Appends one property value as JSON: strings as strings, longs and
/// finite doubles as numbers, booleans as booleans, binaries as objects
/// describing their length or external identifier.
pub fn append_json_value(buffer: &mut String, value: &PropertyValue) {
    match value {
        PropertyValue::String(text)
        | PropertyValue::Date(text)
        | PropertyValue::Name(text)
        | PropertyValue::Path(text)
        | PropertyValue::Reference(text)
        | PropertyValue::WeakReference(text)
        | PropertyValue::Uri(text)
        | PropertyValue::Decimal(text) => append_json_string(buffer, text),
        PropertyValue::Long(number) => buffer.push_str(&number.to_string()),
        PropertyValue::Double(number) => {
            if number.is_finite() {
                buffer.push_str(&number.to_string());
            } else {
                // JSON has no NaN or infinity; fall back to the Java
                // spellings as strings.
                append_json_string(buffer, &non_finite_double_text(*number));
            }
        }
        PropertyValue::Boolean(truth) => buffer.push_str(if *truth { "true" } else { "false" }),
        PropertyValue::Binary(BinaryValue::Inline { length, .. }) => {
            buffer.push_str("{\"binary_length\":");
            buffer.push_str(&length.to_string());
            buffer.push('}');
        }
        PropertyValue::Binary(BinaryValue::External { blob_identifier }) => {
            buffer.push_str("{\"binary_reference\":");
            append_json_string(buffer, blob_identifier);
            buffer.push('}');
        }
    }
}

/// Appends a property's value or value array.
pub fn append_json_values(buffer: &mut String, values: &PropertyValues) {
    match values {
        PropertyValues::Single(value) => append_json_value(buffer, value),
        PropertyValues::Multiple(values) => {
            buffer.push('[');
            for (position, value) in values.iter().enumerate() {
                if position > 0 {
                    buffer.push(',');
                }
                append_json_value(buffer, value);
            }
            buffer.push(']');
        }
    }
}

/// The Java spelling of a non-finite double.
fn non_finite_double_text(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_owned()
    } else if number.is_sign_positive() {
        "Infinity".to_owned()
    } else {
        "-Infinity".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{append_json_string, append_json_value};
    use froe::content::PropertyValue;

    #[test]
    fn escapes_json_strings() {
        let mut buffer = String::new();
        append_json_string(&mut buffer, "say \"hi\"\n\tback\\slash\u{1}");
        assert_eq!(buffer, "\"say \\\"hi\\\"\\n\\tback\\\\slash\\u0001\"");
    }

    #[test]
    fn spells_non_finite_doubles_like_java() {
        let mut buffer = String::new();
        append_json_value(&mut buffer, &PropertyValue::Double(f64::NAN));
        buffer.push(' ');
        append_json_value(&mut buffer, &PropertyValue::Double(f64::INFINITY));
        buffer.push(' ');
        append_json_value(&mut buffer, &PropertyValue::Double(f64::NEG_INFINITY));
        assert_eq!(buffer, "\"NaN\" \"Infinity\" \"-Infinity\"");
    }
}
