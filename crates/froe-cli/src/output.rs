//! Output helpers: terminal sanitization and timestamp formatting.
//!
//! JSON encoding lives in [`froe_export::json`]; the display modules use
//! it directly so the terminal renders property values in exactly the
//! export format.

use std::fmt::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;

/// Renders repository-controlled text for plain terminal output. Terminal
/// controls and Unicode bidirectional formatting characters become visible
/// `\u{..}` escapes, so hostile text cannot inject ANSI/OSC sequences or
/// visually reorder a destructive plan. JSON output paths escape through
/// [`froe_export::json::append_json_string`] instead.
pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    if !text.chars().any(requires_terminal_escape) {
        return text.to_owned();
    }
    let mut sanitized = String::with_capacity(text.len());
    for character in text.chars() {
        if requires_terminal_escape(character) {
            let _ = write!(sanitized, "\\u{{{:x}}}", character as u32);
        } else {
            sanitized.push(character);
        }
    }
    sanitized
}

/// Renders a platform path as an unambiguous, terminal-safe literal. Valid
/// Unicode paths use a quoted string; on Unix, a path containing invalid UTF-8
/// uses an exact ASCII `b"..."` byte literal instead of replacement characters.
/// The underlying filesystem path is never changed; this is display-only.
pub(crate) fn sanitize_terminal_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        match path.to_str() {
            Some(text) => quote_terminal_text(text),
            None => format!(
                "b\"{}\"",
                escape_terminal_bytes(path.as_os_str().as_bytes())
            ),
        }
    }
    #[cfg(not(unix))]
    {
        quote_terminal_text(&path.to_string_lossy())
    }
}

/// Escapes an already-formatted multi-line diagnostic while preserving its
/// line structure. Clap diagnostics pass through here before reaching a
/// terminal, so bidi controls in invalid argument values cannot reorder them.
pub(crate) fn sanitize_terminal_diagnostic(diagnostic: &str) -> String {
    let mut sanitized = String::with_capacity(diagnostic.len());
    for line in diagnostic.split_inclusive('\n') {
        if let Some(content) = line.strip_suffix('\n') {
            sanitized.push_str(&sanitize_terminal_text(content));
            sanitized.push('\n');
        } else {
            sanitized.push_str(&sanitize_terminal_text(line));
        }
    }
    sanitized
}

/// Renders arbitrary bytes as the inside of an ASCII byte-string literal.
/// Printable ASCII stays readable; quotes, backslashes, controls, invalid
/// UTF-8, and every non-ASCII byte are escaped exactly.
pub(crate) fn escape_terminal_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'\\' => escaped.push_str("\\\\"),
            b'"' => escaped.push_str("\\\""),
            b' '..=b'~' => escaped.push(char::from(byte)),
            _ => {
                let _ = write!(escaped, "\\x{byte:02x}");
            }
        }
    }
    escaped
}

fn requires_terminal_escape(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// Renders repository-controlled Unicode text as an injective, terminal-safe
/// quoted literal. Unlike sanitizing followed by `Debug`, this escapes the
/// original text in one pass, so a raw control character cannot collide with
/// a literal `\\u{..}` sequence in a destructive confirmation.
pub(crate) fn quote_terminal_text(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            _ if requires_terminal_escape(character) => {
                let _ = write!(quoted, "\\u{{{:x}}}", character as u32);
            }
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
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
    use super::{
        escape_terminal_bytes, format_timestamp, quote_terminal_text, sanitize_terminal_diagnostic,
        sanitize_terminal_path, sanitize_terminal_text,
    };

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

    #[test]
    fn escapes_terminal_controls_and_bidirectional_formatting() {
        let hostile = "safe\u{1b}]8;;https://example.invalid\u{7}link\u{202e}txt";
        let rendered = sanitize_terminal_text(hostile);
        assert_eq!(
            rendered,
            "safe\\u{1b}]8;;https://example.invalid\\u{7}link\\u{202e}txt"
        );
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{202e}'));
    }

    #[test]
    fn quoted_text_distinguishes_controls_from_literal_escape_text() {
        let control = quote_terminal_text("checkpoint-\u{1b}-\u{202e}");
        let literal = quote_terminal_text(r"checkpoint-\u{1b}-\u{202e}");

        assert_eq!(control, r#""checkpoint-\u{1b}-\u{202e}""#);
        assert_eq!(literal, r#""checkpoint-\\u{1b}-\\u{202e}""#);
        assert_ne!(control, literal);
    }

    #[test]
    fn byte_preview_is_exact_ascii_and_terminal_safe() {
        assert_eq!(
            escape_terminal_bytes(b"ok \\\"\x00\xff\xe2\x80\xae"),
            r#"ok \\\"\x00\xff\xe2\x80\xae"#
        );
    }

    #[test]
    fn diagnostic_sanitizing_preserves_lines_and_escapes_bidi() {
        let rendered = sanitize_terminal_diagnostic("first\ninvalid \u{202e}value\n");
        assert_eq!(rendered, "first\ninvalid \\u{202e}value\n");
    }

    #[test]
    fn valid_paths_are_quoted_and_terminal_safe() {
        let rendered = sanitize_terminal_path(std::path::Path::new(
            "/tmp/quote-\"-slash-\\-escape-\u{1b}-bidi-\u{202e}",
        ));
        assert_eq!(
            rendered,
            r#""/tmp/quote-\"-slash-\\-escape-\u{1b}-bidi-\u{202e}""#
        );
    }

    #[cfg(unix)]
    #[test]
    fn invalid_utf8_paths_are_exact_ascii_byte_literals() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/invalid-\xff-\x1b".to_vec(),
        ));
        let rendered = sanitize_terminal_path(&path);
        assert_eq!(rendered, r#"b"/tmp/invalid-\xff-\x1b""#);
        assert!(rendered.is_ascii());
    }
}
