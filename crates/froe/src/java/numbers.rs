//! Java's integer parsing: `Long.parseLong` and `Integer.parseInt` over
//! every Unicode BMP decimal digit, not just ASCII, with Java's own
//! overflow behaviour at the signed boundaries.

use super::split::JavaSplitFields;

/// Zero code units of the BMP `Nd` blocks recognized by
/// `Character.digit(char, 10)`. Letter digits never have a value below ten at
/// radix ten and therefore do not apply here.
pub(crate) const JAVA_BMP_DECIMAL_ZEROES: [u16; 37] = [
    0x0030, 0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6, 0x0c66, 0x0ce6,
    0x0d66, 0x0de6, 0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80,
    0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
    0xff10,
];

pub(crate) fn java_decimal_digit(unit: u16) -> Option<u16> {
    JAVA_BMP_DECIMAL_ZEROES.iter().find_map(|&zero| {
        let digit = unit.wrapping_sub(zero);
        (digit < 10).then_some(digit)
    })
}

/// Mirrors the decimal subset of Java's `Long.parseLong` and
/// `Integer.parseInt`. Those methods consume UTF-16 code units through
/// `Character.digit(char, 10)`, so all BMP decimal-digit blocks are accepted
/// while supplementary-code-point digits (seen as surrogate pairs) are not.
pub(crate) fn parse_java_signed_decimal(value: &str, minimum: i128, maximum: i128) -> Option<i128> {
    let mut units = value.encode_utf16();
    let first = units.next()?;
    let negative = first == u16::from(b'-');
    let has_sign = negative || first == u16::from(b'+');
    let mut magnitude = 0i128;
    let mut has_digit = false;
    if !has_sign {
        magnitude = i128::from(java_decimal_digit(first)?);
        has_digit = true;
    }
    for unit in units {
        let digit = i128::from(java_decimal_digit(unit)?);
        magnitude = magnitude.checked_mul(10)?.checked_add(digit)?;
        has_digit = true;
    }
    if !has_digit {
        return None;
    }
    let parsed = if negative { -magnitude } else { magnitude };
    (minimum..=maximum).contains(&parsed).then_some(parsed)
}

pub(crate) fn parse_i64_field(fields: &JavaSplitFields<'_>, index: usize) -> i64 {
    fields
        .get(index)
        .and_then(|field| parse_java_signed_decimal(field, i64::MIN.into(), i64::MAX.into()))
        .and_then(|value| i64::try_from(value).ok())
        .unwrap_or(-1)
}

pub(crate) fn parse_i32_field(fields: &JavaSplitFields<'_>, index: usize) -> i32 {
    fields
        .get(index)
        .and_then(|field| parse_java_signed_decimal(field, i32::MIN.into(), i32::MAX.into()))
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(-1)
}

pub(crate) fn parse_java_i32(value: &[u16]) -> Option<i32> {
    let (&first, _) = value.split_first()?;
    let (negative, mut cursor, limit) = match first {
        character if character == u16::from(b'-') => (true, 1, i32::MIN),
        character if character == u16::from(b'+') => (false, 1, -i32::MAX),
        _ => (false, 0, -i32::MAX),
    };
    if cursor == value.len() {
        return None;
    }

    // Accumulate negatively, like Integer.parseInt, so MIN remains representable.
    let multiplication_limit = limit / 10;
    let mut result = 0i32;
    while cursor < value.len() {
        let digit = i32::from(java_decimal_digit(value[cursor])?);
        if result < multiplication_limit {
            return None;
        }
        result *= 10;
        if result < limit + digit {
            return None;
        }
        result -= digit;
        cursor += 1;
    }
    Some(if negative { result } else { -result })
}
