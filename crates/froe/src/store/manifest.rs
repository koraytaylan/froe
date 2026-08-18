//! Reading `store.version` out of the manifest, which is a Java
//! properties file: its line continuations, escapes, and duplicate-key
//! rule are Java's, reproduced rather than approximated.

use super::{ArchivePresence, Error, Path, Result};

/// The highest store version this reader understands
/// (`store.version` in the manifest; 2 since Oak 1.8).
pub(crate) const MAXIMUM_STORE_VERSION: i64 = 2;

/// Validates the `manifest` file with read-only semantics: never writes,
/// accepts store versions 1 and 2, and rejects a store that has archives
/// but no manifest (that is the legacy pre-tar format).
pub(crate) fn check_manifest(directory: &Path, archives: ArchivePresence) -> Result<()> {
    let manifest_path = directory.join("manifest");
    if !manifest_path.exists() {
        if archives == ArchivePresence::Present {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} has segment archives but no manifest; this is the legacy \
                     oak-segment format, not segment-tar",
                    directory.display()
                ),
            });
        }
        return Ok(());
    }
    let store_version = read_manifest_store_version(&manifest_path)?;
    if store_version <= 0 {
        return Err(Error::InvalidFormat {
            details: format!("invalid store version {store_version} in manifest"),
        });
    }
    if store_version > MAXIMUM_STORE_VERSION {
        return Err(Error::InvalidFormat {
            details: format!(
                "store version {store_version} is newer than this reader supports \
                 (up to {MAXIMUM_STORE_VERSION})"
            ),
        });
    }
    Ok(())
}

/// Reads the `store.version` key from the manifest, a Java properties file.
/// Logical-line continuation, separators, and character escapes follow
/// `Properties.load(Reader)`, including its last-duplicate-wins behavior. The
/// version is parsed as a Java `int`; an absent or unparseable value defaults
/// to the maximum supported version, like the Java reader.
pub(crate) fn read_manifest_store_version(manifest_path: &Path) -> Result<i64> {
    let content = std::fs::read_to_string(manifest_path)?;
    parse_manifest_store_version(&content)
}

pub(crate) fn parse_manifest_store_version(content: &str) -> Result<i64> {
    let mut version = MAXIMUM_STORE_VERSION;
    let store_version_key = java_property_ascii("store.version");
    for (key, value) in parse_java_properties(content)? {
        if key != store_version_key {
            continue;
        }
        version = parse_java_i32(&value).map_or(MAXIMUM_STORE_VERSION, i64::from);
    }
    Ok(version)
}

/// Decodes the key/value entries produced by `Properties.load(Reader)`. Java
/// strings are represented as UTF-16 code units so even escaped surrogate code
/// units can be retained without manufacturing invalid Rust `char`s.
pub(crate) fn parse_java_properties(content: &str) -> Result<Vec<(Vec<u16>, Vec<u16>)>> {
    let mut properties = Vec::new();
    for line in java_property_logical_lines(content) {
        let (key, value) = split_java_property(&line);
        properties.push((
            decode_java_property_component(key)?,
            decode_java_property_component(value)?,
        ));
    }
    Ok(properties)
}

pub(crate) fn java_property_logical_lines(content: &str) -> Vec<Vec<char>> {
    let characters: Vec<char> = content.chars().collect();
    let mut lines = Vec::new();
    let mut logical = Vec::new();
    let mut continuation = false;
    let mut cursor = 0usize;

    while cursor < characters.len() {
        let start = cursor;
        while cursor < characters.len() && !matches!(characters[cursor], '\r' | '\n') {
            cursor += 1;
        }
        let line_terminated = cursor < characters.len();
        let end = cursor;
        if line_terminated {
            let terminator = characters[cursor];
            cursor += 1;
            if terminator == '\r' && cursor < characters.len() && characters[cursor] == '\n' {
                cursor += 1;
            }
        }

        let natural = &characters[start..end];
        let first_content = natural
            .iter()
            .position(|character| !is_java_property_whitespace(*character))
            .unwrap_or(natural.len());
        let natural = &natural[first_content..];
        if !continuation && logical.is_empty() {
            if natural.is_empty() {
                continue;
            }
            if natural
                .first()
                .is_some_and(|character| matches!(*character, '#' | '!'))
            {
                continue;
            }
        }

        let trailing_backslashes = natural
            .iter()
            .rev()
            .take_while(|character| **character == '\\')
            .count();
        let continues = trailing_backslashes % 2 == 1;
        let append_end = natural.len() - usize::from(continues);
        logical.extend_from_slice(&natural[..append_end]);
        if continues && line_terminated {
            continuation = true;
            continue;
        }

        if !logical.is_empty() {
            lines.push(std::mem::take(&mut logical));
        } else if continues && !line_terminated {
            // LineReader tests for an empty buffer before removing the final
            // continuation marker, so a lone backslash at EOF produces one
            // empty-key/empty-value property.
            lines.push(Vec::new());
        }
        continuation = false;
    }

    // Java removes an odd terminal backslash and returns the accumulated line
    // when EOF follows a continuation marker without another physical line.
    if continuation || !logical.is_empty() {
        lines.push(logical);
    }
    lines
}

pub(crate) fn split_java_property(line: &[char]) -> (&[char], &[char]) {
    let mut key_length = 0usize;
    let mut value_start = line.len();
    let mut has_separator = false;
    let mut preceding_backslash = false;
    while key_length < line.len() {
        let character = line[key_length];
        if matches!(character, '=' | ':') && !preceding_backslash {
            value_start = key_length + 1;
            has_separator = true;
            break;
        }
        if is_java_property_whitespace(character) && !preceding_backslash {
            value_start = key_length + 1;
            break;
        }
        if character == '\\' {
            preceding_backslash = !preceding_backslash;
        } else {
            preceding_backslash = false;
        }
        key_length += 1;
    }

    while value_start < line.len() {
        let character = line[value_start];
        if !is_java_property_whitespace(character) {
            if !has_separator && matches!(character, '=' | ':') {
                has_separator = true;
            } else {
                break;
            }
        }
        value_start += 1;
    }
    (&line[..key_length], &line[value_start..])
}

pub(crate) fn decode_java_property_component(component: &[char]) -> Result<Vec<u16>> {
    let mut decoded = Vec::with_capacity(component.len());
    let mut cursor = 0usize;
    while cursor < component.len() {
        let character = component[cursor];
        cursor += 1;
        if character != '\\' {
            push_java_property_character(&mut decoded, character);
            continue;
        }
        let Some(&escaped) = component.get(cursor) else {
            return Err(Error::InvalidFormat {
                details: "malformed trailing escape in manifest properties".to_owned(),
            });
        };
        cursor += 1;
        match escaped {
            't' => decoded.push('\t' as u16),
            'n' => decoded.push('\n' as u16),
            'r' => decoded.push('\r' as u16),
            'f' => decoded.push('\u{c}' as u16),
            'u' => {
                let mut value = 0u32;
                for _ in 0..4 {
                    let Some(&digit) = component.get(cursor) else {
                        return Err(malformed_java_unicode_escape());
                    };
                    cursor += 1;
                    let Some(digit) = java_hex_digit(digit) else {
                        return Err(malformed_java_unicode_escape());
                    };
                    value = value * 16 + digit;
                }
                decoded.push(value as u16);
            }
            _ => push_java_property_character(&mut decoded, escaped),
        }
    }
    Ok(decoded)
}

pub(crate) fn push_java_property_character(decoded: &mut Vec<u16>, character: char) {
    let mut encoded = [0u16; 2];
    decoded.extend_from_slice(character.encode_utf16(&mut encoded));
}

pub(crate) fn malformed_java_unicode_escape() -> Error {
    Error::InvalidFormat {
        details: "malformed \\uXXXX escape in manifest properties".to_owned(),
    }
}

pub(crate) fn java_hex_digit(character: char) -> Option<u32> {
    match character {
        '0'..='9' => Some(character as u32 - '0' as u32),
        'a'..='f' => Some(character as u32 - 'a' as u32 + 10),
        'A'..='F' => Some(character as u32 - 'A' as u32 + 10),
        _ => None,
    }
}

pub(crate) fn java_property_ascii(value: &str) -> Vec<u16> {
    value.bytes().map(u16::from).collect()
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
        let digit = java_decimal_digit(value[cursor])?;
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

/// The zero code unit of every BMP `Nd` block recognized by
/// `Character.digit(char, 10)`. Its letter-to-digit cases cannot produce a
/// value below ten and therefore do not apply at radix ten.
pub(crate) const JAVA_BMP_DECIMAL_ZEROES: [u16; 37] = [
    0x0030, 0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6, 0x0c66, 0x0ce6,
    0x0d66, 0x0de6, 0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80,
    0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
    0xff10,
];

pub(crate) fn java_decimal_digit(character: u16) -> Option<i32> {
    JAVA_BMP_DECIMAL_ZEROES.iter().find_map(|&zero| {
        let digit = character.wrapping_sub(zero);
        (digit < 10).then(|| i32::from(digit))
    })
}

pub(crate) fn is_java_property_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\u{c}')
}

#[cfg(test)]
mod tests {
    use super::{
        MAXIMUM_STORE_VERSION, parse_java_i32, parse_java_properties, parse_manifest_store_version,
    };

    fn java_units(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn manifest_properties_accept_java_line_terminators_and_last_duplicate_wins() {
        let manifest = concat!(
            "\u{c}! ignored comment\r",
            "store.version=0\r\n",
            "store.version : 1\n",
        );

        assert_eq!(parse_manifest_store_version(manifest).unwrap(), 1);
    }

    #[test]
    fn manifest_properties_continue_odd_backslashes_and_skip_leading_whitespace() {
        assert_eq!(
            parse_manifest_store_version("store.version=\\\n 1").unwrap(),
            1,
        );

        for terminator in ["\n", "\r", "\r\n"] {
            let manifest = format!("store.version=\\{terminator} \t\u{c}1");
            assert_eq!(
                parse_manifest_store_version(&manifest).unwrap(),
                1,
                "terminator {terminator:?}",
            );
        }

        let properties = parse_java_properties("key=value\\\\\nnext=entry").unwrap();
        assert_eq!(properties[0].1, java_units("value\\"));
        assert_eq!(properties[1].0, java_units("next"));

        let three_backslashes = format!("key=value{}\n  continued", "\\".repeat(3));
        let properties = parse_java_properties(&three_backslashes).unwrap();
        assert_eq!(properties[0].1, java_units("value\\continued"));

        assert_eq!(
            parse_java_properties("\\\n").unwrap(),
            [(Vec::new(), Vec::new())],
            "a continued zero-length logical line at EOF is an empty Java property",
        );
    }

    #[test]
    fn manifest_properties_preserve_java_terminal_backslash_eof_behavior() {
        assert_eq!(
            parse_java_properties("\\").unwrap(),
            vec![(Vec::new(), Vec::new())],
        );
        assert_eq!(
            parse_java_properties("key=\\").unwrap(),
            vec![(java_units("key"), Vec::new())],
        );
        assert_eq!(
            parse_manifest_store_version("store.version=\\").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
    }

    #[test]
    fn java_i32_accepts_bmp_decimal_digits_and_checks_signed_overflow() {
        assert_eq!(parse_java_i32(&java_units("١")), Some(1));
        assert_eq!(parse_java_i32(&java_units("２")), Some(2));
        assert_eq!(parse_java_i32(&java_units("١2३")), Some(123));
        assert_eq!(parse_java_i32(&java_units("+١")), Some(1));
        assert_eq!(parse_java_i32(&java_units("-٢")), Some(-2));
        assert_eq!(parse_java_i32(&java_units("٢١٤٧٤٨٣٦٤٧")), Some(i32::MAX));
        assert_eq!(parse_java_i32(&java_units("٢١٤٧٤٨٣٦٤٨")), None);
        assert_eq!(parse_java_i32(&java_units("-٢١٤٧٤٨٣٦٤٨")), Some(i32::MIN));
        assert_eq!(parse_java_i32(&java_units("-٢١٤٧٤٨٣٦٤٩")), None);
        assert_eq!(
            parse_manifest_store_version(r"store.version=\u0661").unwrap(),
            1,
        );
    }

    #[test]
    fn manifest_properties_decode_escaped_keys_separators_and_characters() {
        let properties = parse_java_properties(r"escaped\ key\:\=\\tail=\t\n\r\f\\\u0031").unwrap();

        assert_eq!(properties.len(), 1);
        assert_eq!(properties[0].0, java_units("escaped key:=\\tail"));
        assert_eq!(properties[0].1, java_units("\t\n\r\u{c}\\1"));
        assert_eq!(
            parse_manifest_store_version(r"store\.version=\u0031").unwrap(),
            1,
        );
        assert_eq!(
            parse_manifest_store_version(r"store\u002eversion=\u0031").unwrap(),
            1,
        );
    }

    #[test]
    fn manifest_properties_reject_malformed_unicode_escapes() {
        assert!(parse_manifest_store_version(r"store.version=\u12x4").is_err());
        assert!(parse_manifest_store_version(r"unrelated=\u123").is_err());
    }

    #[test]
    fn manifest_properties_use_maximum_for_absent_or_last_unparseable_version() {
        assert_eq!(
            parse_manifest_store_version("unrelated=value\n").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=1\nstore.version=invalid\n").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=invalid\nstore.version=1\n").unwrap(),
            1,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=1 \n").unwrap(),
            MAXIMUM_STORE_VERSION,
            "Java Integer.parseInt does not trim the decoded value",
        );
        assert_eq!(
            parse_manifest_store_version("store.version=2147483648\n").unwrap(),
            MAXIMUM_STORE_VERSION,
            "values outside a Java int are unparseable",
        );
        assert_eq!(
            parse_manifest_store_version("store.version=-2147483648\n").unwrap(),
            i64::from(i32::MIN),
        );
    }
}
