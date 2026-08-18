//! Java's own parsing, reproduced exactly: `Long.parseLong` over Unicode
//! decimal digits, and `String.split` with its trailing-empty-field rule.

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

pub(crate) struct JavaSplitFields<'line> {
    pub(crate) first_fields: [Option<&'line str>; 7],
    pub(crate) length: usize,
}

impl<'line> JavaSplitFields<'line> {
    pub(crate) fn get(&self, index: usize) -> Option<&'line str> {
        if index < self.length {
            self.first_fields.get(index).copied().flatten()
        } else {
            None
        }
    }
}

pub(crate) fn split_like_java(line: &str) -> JavaSplitFields<'_> {
    let mut first_fields = [None; 7];
    let mut split_field_count = 0usize;
    let mut last_nonempty_field_count = 0usize;
    for field in line.split(',') {
        if split_field_count < first_fields.len() {
            first_fields[split_field_count] = Some(field);
        }
        split_field_count = split_field_count.saturating_add(1);
        if !field.is_empty() {
            last_nonempty_field_count = split_field_count;
        }
    }
    JavaSplitFields {
        first_fields,
        length: if line.is_empty() {
            1
        } else {
            last_nonempty_field_count
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::gc_journal::test_support::TestDirectory;
    use crate::gc_journal::{
        NULL_RECORD_IDENTIFIER_TEXT, parse_gc_journal_entry, read_all_gc_journal,
    };

    #[test]
    fn numeric_fields_match_java_unicode_decimal_and_overflow_rules() {
        let unicode = parse_gc_journal_entry("١٢,３४,-٥,६,７,+٨,root");
        assert_eq!(unicode.repository_size, 12);
        assert_eq!(unicode.reclaimed_size, 34);
        assert_eq!(unicode.timestamp_milliseconds, -5);
        assert_eq!(unicode.generation.generation, 6);
        assert_eq!(unicode.generation.full_generation, 7);
        assert_eq!(unicode.compacted_nodes, 8);

        let overflow = parse_gc_journal_entry("٩٢٢٣٣٧٢٠٣٦٨٥٤٧٧٥٨٠٨,٠,٠,٢١٤٧٤٨٣٦٤٨,٠,٠,root");
        assert_eq!(overflow.repository_size, -1, "i64 overflow falls back");
        assert_eq!(
            overflow.generation.generation, -1,
            "i32 overflow falls back"
        );

        let supplementary_digit = parse_gc_journal_entry("𞥑,0,0,0,0,0,root");
        assert_eq!(
            supplementary_digit.repository_size, -1,
            "Java examines a supplementary digit as two invalid surrogate code units"
        );
    }

    #[test]
    fn matches_java_trailing_empty_and_surplus_field_layout_selection() {
        let delimiter_only = parse_gc_journal_entry(",,,,,,");
        assert_eq!(delimiter_only.repository_size, -1);
        assert_eq!(delimiter_only.generation.generation, -1);
        assert_eq!(
            delimiter_only.root_record_identifier_text,
            NULL_RECORD_IDENTIFIER_TEXT
        );

        let leading_empty = parse_gc_journal_entry(",2,3,4,5,6,root");
        assert_eq!(leading_empty.repository_size, -1);
        assert_eq!(leading_empty.reclaimed_size, 2);
        assert_eq!(leading_empty.generation.generation, 4);
        assert_eq!(leading_empty.generation.full_generation, 5);
        assert_eq!(leading_empty.compacted_nodes, 6);
        assert_eq!(leading_empty.root_record_identifier_text, "root");

        let trailing_empty = parse_gc_journal_entry("1,2,3,4,5,6,");
        assert_eq!(trailing_empty.generation.full_generation, 4);
        assert_eq!(trailing_empty.compacted_nodes, 5);
        assert_eq!(trailing_empty.root_record_identifier_text, "6");

        let current_with_trailing_delimiter = parse_gc_journal_entry("1,2,3,4,5,6,root,");
        assert_eq!(
            current_with_trailing_delimiter.generation.full_generation,
            5
        );
        assert_eq!(current_with_trailing_delimiter.compacted_nodes, 6);
        assert_eq!(
            current_with_trailing_delimiter.root_record_identifier_text,
            "root"
        );

        let surplus = parse_gc_journal_entry("1,2,3,4,5,6,root,surplus");
        assert_eq!(surplus.generation.full_generation, 4);
        assert_eq!(surplus.compacted_nodes, 5);
        assert_eq!(surplus.root_record_identifier_text, "6");
    }

    #[test]
    fn comma_heavy_lines_preserve_java_trailing_field_semantics() {
        let directory = TestDirectory::new("comma-heavy");
        let path = directory.path.join("gc.log");
        let mut current_entry = b"1,2,3,4,5,6,root".to_vec();
        current_entry.resize(200_000, b',');
        current_entry.push(b'\n');
        let mut delimiter_only = vec![b','; 200_000];
        delimiter_only.push(b'\n');
        current_entry.extend_from_slice(&delimiter_only);
        std::fs::write(&path, current_entry).expect("write comma-heavy fixture");

        let entries = read_all_gc_journal(&path).expect("read comma-heavy fixture");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].generation.generation, 4);
        assert_eq!(entries[0].generation.full_generation, 5);
        assert_eq!(entries[0].root_record_identifier_text, "root");
        assert_eq!(entries[1], parse_gc_journal_entry(""));
    }
}
