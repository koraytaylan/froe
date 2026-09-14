//! Jackrabbit's `ISO8601`, which Oak parses every date through.
//!
//! `docs/analysis/lucene-oak-analysis.md` §0.4 pins the artifact and §8.4
//! states the rules. Two callers share it: the Lucene document maker,
//! whose `DATE` fields are the epoch millisecond of the parse, and the
//! version-storage planner, which ages a version history by the
//! `jcr:created` of its newest version.

/// Milliseconds since the Unix epoch of an ISO-8601 timestamp in the form
/// Oak serializes dates: `2012-03-01T12:30:45.678+01:00`, with `Z`
/// accepted for a zero offset and the fraction optional. Integer
/// arithmetic throughout; `None` for anything that does not parse.
pub(crate) fn parse_epoch_milliseconds(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = bytes.get(range)?;
        let mut value = 0i64;
        for byte in slice {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        Some(value)
    };
    let separator = |position: usize, expected: u8| bytes.get(position) == Some(&expected);
    if !(separator(4, b'-')
        && separator(7, b'-')
        && separator(10, b'T')
        && separator(13, b':')
        && separator(16, b':'))
    {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut cursor = 19;
    let mut fraction = 0i64;
    if separator(cursor, b'.') {
        cursor += 1;
        let opened = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            // Three digits are a millisecond; the rest are read and
            // dropped, as a millisecond-precision parser must.
            if cursor - opened < 3 {
                fraction = fraction * 10 + i64::from(bytes[cursor] - b'0');
            }
            cursor += 1;
        }
        for _ in (cursor - opened)..3 {
            fraction *= 10;
        }
    }
    let offset_seconds = match bytes.get(cursor) {
        Some(b'Z') if cursor + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            if cursor + 6 != bytes.len() || !separator(cursor + 3, b':') {
                return None;
            }
            let hours = digits(cursor + 1..cursor + 3)?;
            let minutes = digits(cursor + 4..cursor + 6)?;
            let magnitude = (hours * 60 + minutes) * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };
    // Days since the epoch by Howard Hinnant's civil-days algorithm.
    let year_adjusted = year - i64::from(month <= 2);
    let era = year_adjusted.div_euclid(400);
    let year_of_era = year_adjusted - era * 400;
    let month_shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds;
    Some(seconds * 1_000 + fraction)
}
