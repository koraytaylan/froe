//! Output helpers: JSON encoding and timestamp formatting.
//!
//! The command line emits JSON without a serialization dependency — the
//! values are simple enough that direct encoding is both smaller and
//! faster than a generic serializer.

use std::fmt::Write;

use froe::content::value::BinaryValue;
use froe::content::{PropertyValue, PropertyValues};

/// Appends `text` to `buffer` as a JSON string literal with escaping.
/// C1 controls and DEL are escaped too — JSON permits them literally,
/// but terminals interpret C1 bytes as control sequences.
pub(crate) fn append_json_string(buffer: &mut String, text: &str) {
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
pub(crate) fn append_json_value(buffer: &mut String, value: &PropertyValue) {
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
pub(crate) fn append_json_values(buffer: &mut String, values: &PropertyValues) {
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

/// Renders repository-controlled text for plain terminal output: C0
/// control characters and DEL become visible `\u{..}` escapes, so a
/// hostile node name or value cannot inject ANSI or OSC terminal control
/// sequences. JSON output paths escape through
/// [`append_json_string`] instead.
pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_owned();
    }
    let mut sanitized = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            let _ = write!(sanitized, "\\u{{{:x}}}", character as u32);
        } else {
            sanitized.push(character);
        }
    }
    sanitized
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

/// Formats milliseconds since the Unix epoch as an ISO-8601 UTC timestamp,
/// or `"unknown"` for the -1 sentinel the journal reader produces.
pub(crate) fn format_timestamp(milliseconds: i64) -> String {
    if milliseconds < 0 {
        return "unknown".to_owned();
    }
    let seconds = milliseconds.div_euclid(1000);
    let millisecond_part = milliseconds.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millisecond_part:03}Z",
        second_of_day / 3600,
        (second_of_day / 60) % 60,
        second_of_day % 60,
    )
}

/// Converts days since 1970-01-01 to a civil (year, month, day) date.
/// This is the classic days-to-civil algorithm over the proleptic
/// Gregorian calendar.
fn civil_date_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = (if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{append_json_string, format_timestamp};

    #[test]
    fn escapes_json_strings() {
        let mut buffer = String::new();
        append_json_string(&mut buffer, "say \"hi\"\n\tback\\slash\u{1}");
        assert_eq!(buffer, "\"say \\\"hi\\\"\\n\\tback\\\\slash\\u0001\"");
    }

    #[test]
    fn formats_known_timestamps() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_timestamp(1_700_000_000_000),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(
            format_timestamp(951_827_696_789),
            "2000-02-29T12:34:56.789Z"
        );
        assert_eq!(format_timestamp(-1), "unknown");
    }
}
