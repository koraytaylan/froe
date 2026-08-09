//! Output helpers: terminal sanitization and timestamp formatting.
//!
//! JSON encoding lives in [`froe_export::json`]; the display modules use
//! it directly so the terminal renders property values in exactly the
//! export format.

use std::fmt::Write;

/// Renders repository-controlled text for plain terminal output: C0
/// control characters and DEL become visible `\u{..}` escapes, so a
/// hostile node name or value cannot inject ANSI or OSC terminal control
/// sequences. JSON output paths escape through
/// [`froe_export::json::append_json_string`] instead.
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
    use super::format_timestamp;

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
