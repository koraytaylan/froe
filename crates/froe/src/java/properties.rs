//! `java.util.Properties`: logical lines spliced across odd backslashes,
//! the three separator characters, the escape decoding, and the
//! last-duplicate-wins rule.

use crate::error::{Error, Result};

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

pub(crate) fn is_java_property_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\u{c}')
}
