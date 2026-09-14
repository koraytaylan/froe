//! Jackrabbit's `ISO8601`, which Oak parses every date through.
//!
//! `docs/analysis/lucene-oak-analysis.md` §0.4 pins the artifact and §8.4
//! states the rules, all six of which are here. Two callers share it: the
//! Lucene document maker, whose `DATE` fields are the epoch millisecond of
//! the parse, and the version-storage planner, which ages a version
//! history by the `jcr:created` of its newest version.
//!
//! It is not the ISO 8601 a reasonable reader would write, and the ways it
//! differs are the point of reproducing it rather than parsing dates some
//! other way: the fraction is mandatory and exactly three digits, the
//! time-zone designator is whatever Java's `TimeZone.getTimeZone` accepts
//! *and answers to by the same name*, the fields go through
//! `Integer.parseInt` and so take every BMP decimal digit, and the
//! calendar is the historical one, with ten days missing in October 1582.

use super::parse_java_i32;

/// Days from the epoch to 1582-10-15, `GregorianCalendar`'s default
/// cutover: -12,219,292,800,000 milliseconds.
const CUTOVER_DAYS: i64 = -141_427;

/// Milliseconds since the Unix epoch of a date in the one form Jackrabbit
/// accepts: `±YYYY-MM-DDThh:mm:ss.SSSTZD`. `None` for anything else,
/// which is `ISO8601.parse` returning `null`.
pub(crate) fn parse_epoch_milliseconds(text: &str) -> Option<i64> {
    // Java indexes a `String` by UTF-16 code unit, and an astral character
    // would shift every field that follows it by one.
    let units: Vec<u16> = text.encode_utf16().collect();
    let (negative_era, mut at) = match units.first().copied() {
        Some(unit) if unit == u16::from(b'-') => (true, 1),
        Some(unit) if unit == u16::from(b'+') => (false, 1),
        _ => (false, 0),
    };
    let year = field(&units, at, 4)?;
    at += 4;
    at = delimiter(&units, at, b'-')?;
    let month = field(&units, at, 2)?;
    at += 2;
    at = delimiter(&units, at, b'-')?;
    let day = field(&units, at, 2)?;
    at += 2;
    at = delimiter(&units, at, b'T')?;
    let hour = field(&units, at, 2)?;
    at += 2;
    at = delimiter(&units, at, b':')?;
    let minute = field(&units, at, 2)?;
    at += 2;
    at = delimiter(&units, at, b':')?;
    let second = field(&units, at, 2)?;
    at += 2;
    at = delimiter(&units, at, b'.')?;
    let millisecond = field(&units, at, 3)?;
    at += 3;
    let offset_minutes = zone_offset_minutes(units.get(at..)?)?;

    // The era comes from the leading sign, and from a year of `0000`:
    // either sets the era to BC with the year one higher, so the
    // astronomical year is the negation of the digits.
    let (calendar_year, astronomical_year) = if negative_era || year == 0 {
        (year + 1, -year)
    } else {
        (year, year)
    };
    if calendar_year < 1 || !(-9999..=9999).contains(&astronomical_year) {
        return None;
    }
    if !(1..=12).contains(&month)
        || day < 1
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
        || !(0..=999).contains(&millisecond)
    {
        return None;
    }
    let days = days_since_epoch(
        i64::from(astronomical_year),
        i64::from(month),
        i64::from(day),
    )?;
    let seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second)
            - i64::from(offset_minutes) * 60;
    Some(seconds * 1_000 + i64::from(millisecond))
}

/// One fixed-width field, through `Integer.parseInt`: every BMP decimal
/// digit, and an out-of-bounds slice is the `IndexOutOfBoundsException`
/// the parser catches.
fn field(units: &[u16], at: usize, width: usize) -> Option<i32> {
    parse_java_i32(units.get(at..at + width)?)
}

/// One literal delimiter, and where it leaves the cursor.
fn delimiter(units: &[u16], at: usize, expected: u8) -> Option<usize> {
    (*units.get(at)? == u16::from(expected)).then_some(at + 1)
}

/// The time-zone designator, which is the whole of the rest of the string.
///
/// `Z`, `+00:00` and `-00:00` come from the parser's own map; everything
/// else is `TimeZone.getTimeZone("GMT" + designator)` and is accepted only
/// if the zone answers to that same name. An **empty** designator is
/// therefore UTC, because `getTimeZone("GMT")` answers to `GMT`, and so is
/// `0`, `GMT0` being a zone of its own in the database — while `+0`, `+00`
/// and `+0100` are all refused, Java normalizing each to something else.
fn zone_offset_minutes(designator: &[u16]) -> Option<i32> {
    let text = String::from_utf16(designator).ok()?;
    if matches!(text.as_str(), "" | "Z" | "0") {
        return Some(0);
    }
    // A custom zone is `GMT±hh:mm` and nothing else: Java's own parser
    // takes ASCII digits alone, and a one-digit hour or a missing colon
    // comes back normalized under a different name.
    let bytes = text.as_bytes();
    if bytes.len() != 6 || bytes[3] != b':' {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digit = |at: usize| -> Option<i32> {
        bytes[at]
            .is_ascii_digit()
            .then(|| i32::from(bytes[at] - b'0'))
    };
    let hours = digit(1)? * 10 + digit(2)?;
    let minutes = digit(4)? * 10 + digit(5)?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

/// Days since the epoch, in the calendar `GregorianCalendar` would have
/// used, or `None` for a date that does not exist in it.
///
/// Three ways a date fails: a day past the end of its month, in whichever
/// calendar applies; one of the ten days the cutover swallowed; or a month
/// outside 1 to 12, which the caller has already refused.
fn days_since_epoch(year: i64, month: i64, day: i64) -> Option<i64> {
    let gregorian = day_number(year, month, day, true);
    if gregorian >= CUTOVER_DAYS {
        return (day <= days_in_month(year, month, true)).then_some(gregorian);
    }
    let julian = day_number(year, month, day, false);
    if julian >= CUTOVER_DAYS {
        // 1582-10-05 through 1582-10-14 read back as a different day, which
        // is what the non-lenient calendar throws on.
        return None;
    }
    (day <= days_in_month(year, month, false)).then_some(julian)
}

/// The day number of a civil date, from the Julian day number, in either
/// calendar.
fn day_number(year: i64, month: i64, day: i64, gregorian: bool) -> i64 {
    let leap_shift = (14 - month).div_euclid(12);
    let shifted_year = year + 4800 - leap_shift;
    let shifted_month = month + 12 * leap_shift - 3;
    let base = day
        + (153 * shifted_month + 2).div_euclid(5)
        + 365 * shifted_year
        + shifted_year.div_euclid(4);
    let julian_day = if gregorian {
        base - shifted_year.div_euclid(100) + shifted_year.div_euclid(400) - 32_045
    } else {
        base - 32_083
    };
    // The Julian day number of 1970-01-01.
    julian_day - 2_440_588
}

/// How many days a month has, under either calendar's leap rule.
fn days_in_month(year: i64, month: i64, gregorian: bool) -> i64 {
    const LENGTHS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if month == 2 && is_leap_year(year, gregorian) {
        return 29;
    }
    LENGTHS[(month - 1) as usize]
}

/// The Julian rule is every fourth year, including the negative ones; the
/// Gregorian rule drops three of every four centuries.
fn is_leap_year(year: i64, gregorian: bool) -> bool {
    if year.rem_euclid(4) != 0 {
        return false;
    }
    !gregorian || year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0
}
