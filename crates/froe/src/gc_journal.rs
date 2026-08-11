//! The garbage-collection journal (`gc.log`).
//!
//! Oak appends one comma-separated line after each successful compaction
//! and cleanup cycle. This module deliberately preserves Oak's tolerant
//! `GCJournalEntry.fromString` behavior: malformed fields become sentinel
//! values, six-field Oak 1.6 entries copy their generation into the absent
//! full-generation field, and only a line with exactly seven fields uses the
//! newer layout. Reading is informational and never creates, locks, or
//! modifies repository files.
//!
//! Decimal parsing follows current Oak's Java 11+ `Character.digit(char, 10)`
//! table. Oak itself always writes ASCII digits. Consequently, the only
//! historical-Java difference is for manually authored Unicode digits added
//! after Java 8's Unicode table; supplementary-plane digits remain invalid
//! because Java's parser examines their UTF-16 surrogate code units.
//!
//! Format evidence: `docs/analysis/filestore-layer.md` section 7, extracted
//! from Oak's `GCJournal.java` and `LocalGCJournalFile.java`.

use std::path::Path;

use crate::journal::parse_record_identifier_text;
use crate::segment::{GarbageCollectionGeneration, RecordIdentifier};

/// Oak's textual representation of its null record identifier.
const NULL_RECORD_IDENTIFIER_TEXT: &str = "00000000-0000-0000-0000-000000000000:0";

/// One parsed line of `gc.log`.
///
/// Parsing is deliberately total. A malformed or absent numeric field is
/// represented by `-1`, while an absent root uses Oak's null record-id text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GarbageCollectionJournalEntry {
    /// Repository size after cleanup, in bytes, or `-1` when unavailable.
    pub repository_size: i64,
    /// Bytes reclaimed by cleanup, or `-1` when unavailable.
    pub reclaimed_size: i64,
    /// Entry timestamp in milliseconds since the Unix epoch, or `-1` when
    /// unavailable.
    pub timestamp_milliseconds: i64,
    /// Garbage-collection generation recorded by Oak. Journal parsing always
    /// reconstructs it with `is_compacted == false` because that flag is not
    /// stored in `gc.log`.
    pub generation: GarbageCollectionGeneration,
    /// Number of nodes compacted, or `-1` when unavailable.
    pub compacted_nodes: i64,
    /// Root record identifier exactly as stored, or Oak's null record-id text
    /// when the field is absent.
    pub root_record_identifier_text: String,
}

impl GarbageCollectionJournalEntry {
    /// Returns Oak's EMPTY entry, used when there is no readable journal line.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            repository_size: -1,
            reclaimed_size: -1,
            timestamp_milliseconds: -1,
            generation: GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            },
            compacted_nodes: -1,
            root_record_identifier_text: NULL_RECORD_IDENTIFIER_TEXT.to_owned(),
        }
    }

    /// Parses the root field when it is a valid Oak record identifier, in
    /// either journal decimal or diagnostic eight-hex-digit form.
    ///
    /// Oak preserves this field as text while reading `gc.log`; consequently,
    /// an invalid value does not invalidate the entry and is reported here as
    /// `None` only when a caller asks for its structured form.
    #[must_use]
    pub fn root_record_identifier(&self) -> Option<RecordIdentifier> {
        parse_record_identifier_text(&self.root_record_identifier_text)
    }
}

impl Default for GarbageCollectionJournalEntry {
    fn default() -> Self {
        Self::empty()
    }
}

/// Parses one `gc.log` line with Oak's field-specific fallback behavior.
///
/// Java's `String.split(",")` drops trailing empty fields. Only a resulting
/// field count of exactly seven selects the Oak 1.8+ layout; every other count
/// uses the legacy offsets and ignores surplus fields.
#[must_use]
pub fn parse_gc_journal_entry(line: &str) -> GarbageCollectionJournalEntry {
    let fields = split_like_java(line);
    let repository_size = parse_i64_field(&fields, 0);
    let reclaimed_size = parse_i64_field(&fields, 1);
    let timestamp_milliseconds = parse_i64_field(&fields, 2);
    let generation_number = parse_i32_field(&fields, 3);
    let (full_generation, next_field) = if fields.len() == 7 {
        (parse_i32_field(&fields, 4), 5)
    } else {
        (generation_number, 4)
    };
    let compacted_nodes = parse_i64_field(&fields, next_field);
    let root_record_identifier_text = fields.get(next_field + 1).map_or_else(
        || NULL_RECORD_IDENTIFIER_TEXT.to_owned(),
        |field| (*field).to_owned(),
    );

    GarbageCollectionJournalEntry {
        repository_size,
        reclaimed_size,
        timestamp_milliseconds,
        generation: GarbageCollectionGeneration {
            generation: generation_number,
            full_generation,
            is_compacted: false,
        },
        compacted_nodes,
        root_record_identifier_text,
    }
}

/// Reads the last `gc.log` line with `GCJournal.read()`'s selection and
/// fallback behavior.
///
/// A missing, unreadable, or non-UTF-8 file, and a readable file with no
/// lines, yields [`GarbageCollectionJournalEntry::empty`]. I/O errors are
/// intentionally not exposed because Oak treats this journal as optional
/// informational state. This is a stateless helper and rereads the path on
/// every call; Oak's long-lived `GCJournal` object caches its latest entry.
#[must_use]
pub fn read_gc_journal(gc_journal_path: &Path) -> GarbageCollectionJournalEntry {
    std::fs::read_to_string(gc_journal_path)
        .ok()
        .and_then(|content| {
            java_lines(&content)
                .last()
                .map(|line| parse_gc_journal_entry(line))
        })
        .unwrap_or_default()
}

/// Reads and parses all `gc.log` lines in file order, matching
/// `GCJournal.readAll()`.
///
/// Malformed lines remain present as entries populated with field-specific
/// fallbacks. A missing, unreadable, or non-UTF-8 file yields an empty vector.
#[must_use]
pub fn read_all_gc_journal(gc_journal_path: &Path) -> Vec<GarbageCollectionJournalEntry> {
    std::fs::read_to_string(gc_journal_path).map_or_else(
        |_| Vec::new(),
        |content| {
            java_lines(&content)
                .into_iter()
                .map(parse_gc_journal_entry)
                .collect()
        },
    )
}

fn parse_i64_field(fields: &[&str], index: usize) -> i64 {
    fields
        .get(index)
        .and_then(|field| parse_java_signed_decimal(field, i64::MIN.into(), i64::MAX.into()))
        .and_then(|value| i64::try_from(value).ok())
        .unwrap_or(-1)
}

fn parse_i32_field(fields: &[&str], index: usize) -> i32 {
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
fn parse_java_signed_decimal(value: &str, minimum: i128, maximum: i128) -> Option<i128> {
    let units: Vec<u16> = value.encode_utf16().collect();
    let (&first, rest) = units.split_first()?;
    let (negative, digits) = match first {
        unit if unit == u16::from(b'-') => (true, rest),
        unit if unit == u16::from(b'+') => (false, rest),
        _ => (false, units.as_slice()),
    };
    if digits.is_empty() {
        return None;
    }

    let mut magnitude = 0i128;
    for &unit in digits {
        let digit = i128::from(java_decimal_digit(unit)?);
        magnitude = magnitude.checked_mul(10)?.checked_add(digit)?;
    }
    let parsed = if negative { -magnitude } else { magnitude };
    (minimum..=maximum).contains(&parsed).then_some(parsed)
}

/// Zero code units of the BMP `Nd` blocks recognized by
/// `Character.digit(char, 10)`. Letter digits never have a value below ten at
/// radix ten and therefore do not apply here.
const JAVA_BMP_DECIMAL_ZEROES: [u16; 37] = [
    0x0030, 0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6, 0x0c66, 0x0ce6,
    0x0d66, 0x0de6, 0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80,
    0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
    0xff10,
];

fn java_decimal_digit(unit: u16) -> Option<u16> {
    JAVA_BMP_DECIMAL_ZEROES.iter().find_map(|&zero| {
        let digit = unit.wrapping_sub(zero);
        (digit < 10).then_some(digit)
    })
}

fn split_like_java(line: &str) -> Vec<&str> {
    let mut fields: Vec<&str> = line.split(',').collect();
    if !line.is_empty() {
        while fields.last() == Some(&"") {
            fields.pop();
        }
    }
    fields
}

fn java_lines(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = content.as_bytes();
    let mut line_start = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        if matches!(bytes[cursor], b'\n' | b'\r') {
            lines.push(&content[line_start..cursor]);
            if bytes[cursor] == b'\r' && bytes.get(cursor + 1) == Some(&b'\n') {
                cursor += 1;
            }
            line_start = cursor + 1;
        }
        cursor += 1;
    }
    if line_start < content.len() {
        lines.push(&content[line_start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        GarbageCollectionJournalEntry, NULL_RECORD_IDENTIFIER_TEXT, parse_gc_journal_entry,
        read_all_gc_journal, read_gc_journal,
    };

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "froe-gc-journal-{name}-{}-{timestamp}-{sequence}",
                std::process::id(),
            ));
            std::fs::create_dir(&path).expect("create test directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn parses_current_seven_field_entries() {
        let entry = parse_gc_journal_entry(
            "127469568,60295168,1754556010042,2,3,180042,\
             f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:270976",
        );

        assert_eq!(entry.repository_size, 127_469_568);
        assert_eq!(entry.reclaimed_size, 60_295_168);
        assert_eq!(entry.timestamp_milliseconds, 1_754_556_010_042);
        assert_eq!(entry.generation.generation, 2);
        assert_eq!(entry.generation.full_generation, 3);
        assert!(!entry.generation.is_compacted);
        assert_eq!(entry.compacted_nodes, 180_042);
        assert_eq!(
            entry.root_record_identifier_text,
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:270976"
        );
        assert_eq!(
            entry
                .root_record_identifier()
                .expect("valid root record identifier")
                .record_number,
            270_976
        );
    }

    #[test]
    fn parses_legacy_six_field_entries() {
        let entry =
            parse_gc_journal_entry("100,20,30,-4,50,00000000-0000-0000-0000-000000000000:0");

        assert_eq!(entry.generation.generation, -4);
        assert_eq!(entry.generation.full_generation, -4);
        assert!(!entry.generation.is_compacted);
        assert_eq!(entry.compacted_nodes, 50);
        assert_eq!(
            entry.root_record_identifier_text,
            NULL_RECORD_IDENTIFIER_TEXT
        );
    }

    #[test]
    fn parses_signed_boundaries_and_defaults_each_malformed_field() {
        let boundaries = parse_gc_journal_entry(&format!(
            "{},{},-1,{},{},+7,root",
            i64::MIN,
            i64::MAX,
            i32::MIN,
            i32::MAX
        ));
        assert_eq!(boundaries.repository_size, i64::MIN);
        assert_eq!(boundaries.reclaimed_size, i64::MAX);
        assert_eq!(boundaries.timestamp_milliseconds, -1);
        assert_eq!(boundaries.generation.generation, i32::MIN);
        assert_eq!(boundaries.generation.full_generation, i32::MAX);
        assert_eq!(boundaries.compacted_nodes, 7);
        assert_eq!(boundaries.root_record_identifier_text, "root");

        let malformed = parse_gc_journal_entry(" 1,2,x,2147483648,5,+,not-a-record");
        assert_eq!(malformed.repository_size, -1, "Java does not trim fields");
        assert_eq!(malformed.reclaimed_size, 2);
        assert_eq!(malformed.timestamp_milliseconds, -1);
        assert_eq!(malformed.generation.generation, -1);
        assert_eq!(malformed.generation.full_generation, 5);
        assert_eq!(malformed.compacted_nodes, -1);
        assert_eq!(malformed.root_record_identifier_text, "not-a-record");
        assert!(malformed.root_record_identifier().is_none());
    }

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
    fn defaults_absent_fields_without_discarding_the_line() {
        let entry = parse_gc_journal_entry("1,2,3,4");

        assert_eq!(entry.repository_size, 1);
        assert_eq!(entry.reclaimed_size, 2);
        assert_eq!(entry.timestamp_milliseconds, 3);
        assert_eq!(entry.generation.generation, 4);
        assert_eq!(entry.generation.full_generation, 4);
        assert_eq!(entry.compacted_nodes, -1);
        assert_eq!(
            entry.root_record_identifier_text,
            NULL_RECORD_IDENTIFIER_TEXT
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
    fn reads_all_java_line_endings_in_file_order_and_read_uses_last_line() {
        let directory = TestDirectory::new("line-endings");
        let path = directory.path.join("gc.log");
        std::fs::write(
            &path,
            "1,2,3,4,5,first\r2,3,4,5,6,second\r\n3,4,5,6,7,third\n",
        )
        .expect("write fixture");

        let all = read_all_gc_journal(&path);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].root_record_identifier_text, "first");
        assert_eq!(all[1].root_record_identifier_text, "second");
        assert_eq!(all[2].root_record_identifier_text, "third");
        assert_eq!(read_gc_journal(&path), all[2]);

        std::fs::write(&path, "1,2,3,4,5,first\n\n").expect("write empty last line");
        let all = read_all_gc_journal(&path);
        assert_eq!(all.len(), 2, "a physical empty line remains an entry");
        let parsed_empty_line = parse_gc_journal_entry("");
        assert_eq!(parsed_empty_line.generation.generation, -1);
        assert_eq!(all[1], parsed_empty_line);
        assert_eq!(read_gc_journal(&path), parsed_empty_line);
    }

    #[test]
    fn missing_unreadable_and_non_utf8_files_use_oak_fallbacks() {
        let directory = TestDirectory::new("read-failures");
        let missing = directory.path.join("missing.log");
        assert_eq!(
            read_gc_journal(&missing),
            GarbageCollectionJournalEntry::empty()
        );
        assert!(read_all_gc_journal(&missing).is_empty());

        let empty = directory.path.join("empty.log");
        std::fs::write(&empty, []).expect("write empty journal");
        assert_eq!(
            read_gc_journal(&empty),
            GarbageCollectionJournalEntry::empty()
        );
        assert!(read_all_gc_journal(&empty).is_empty());

        assert_eq!(
            read_gc_journal(&directory.path),
            GarbageCollectionJournalEntry::empty(),
            "a directory is not a readable journal file"
        );
        assert!(read_all_gc_journal(&directory.path).is_empty());

        let invalid_utf8 = directory.path.join("invalid-utf8.log");
        std::fs::write(&invalid_utf8, [b'1', b',', 0xff, b'\n']).expect("write invalid UTF-8");
        assert_eq!(
            read_gc_journal(&invalid_utf8),
            GarbageCollectionJournalEntry::empty()
        );
        assert!(read_all_gc_journal(&invalid_utf8).is_empty());
    }

    #[test]
    fn production_reads_do_not_modify_the_repository_directory() {
        let directory = TestDirectory::new("read-only");
        let path = directory.path.join("gc.log");
        let fixture = b"1,2,3,4,5,root\n";
        std::fs::write(&path, fixture).expect("write fixture");
        let names_before = directory_entries(&directory.path);
        let metadata_before = std::fs::metadata(&path).expect("metadata before read");

        let _ = read_gc_journal(&path);
        let _ = read_all_gc_journal(&path);

        assert_eq!(
            std::fs::read(&path).expect("read fixture after calls"),
            fixture
        );
        assert_eq!(directory_entries(&directory.path), names_before);
        let metadata_after = std::fs::metadata(&path).expect("metadata after read");
        assert_eq!(metadata_after.len(), metadata_before.len());
        assert_eq!(
            metadata_after.modified().expect("modified time after read"),
            metadata_before
                .modified()
                .expect("modified time before read")
        );
    }

    fn directory_entries(path: &std::path::Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .expect("read test directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect();
        names.sort();
        names
    }
}
