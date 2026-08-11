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

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::journal::parse_record_identifier_text;
use crate::segment::{GarbageCollectionGeneration, RecordIdentifier};

/// Oak's textual representation of its null record identifier.
const NULL_RECORD_IDENTIFIER_TEXT: &str = "00000000-0000-0000-0000-000000000000:0";

/// Default maximum accepted length of one `gc.log` file.
///
/// This limit includes line terminators and also bounds byte-oriented scanning
/// work. Limit enforcement may read one additional probe byte when metadata
/// understates the length or the file grows during the scan. Sixty-four MiB
/// leaves room for a maintenance history far larger than an ordinary Oak
/// journal without trusting a sparse or hostile file size.
pub const DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Default maximum UTF-8 bytes in one `gc.log` line.
///
/// Line terminators are excluded. Oak-written entries are normally well below
/// one KiB; the larger default accommodates manually annotated root fields
/// while bounding the parser's only input-sized scratch allocation.
pub const DEFAULT_MAXIMUM_GC_JOURNAL_LINE_BYTES: usize = 1024 * 1024;

/// Default maximum number of entries read from one `gc.log` file.
///
/// The limit bounds both parsing work and the fixed-size portion of the vector
/// returned by [`read_all_gc_journal_with_options`].
pub const DEFAULT_MAXIMUM_GC_JOURNAL_ENTRIES: usize = 250_000;

/// Resource limits for reading a garbage-collection journal.
///
/// Together, the file-byte and entry limits bound parsing work by the bytes
/// scanned plus the lines parsed. The line and file limits bound transient
/// text, while the entry and file limits bound an all-entry result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct GarbageCollectionJournalReadOptions {
    /// Maximum accepted file length, including CR and LF line terminators.
    /// The reader may consume one additional probe byte to detect growth.
    pub maximum_file_bytes: u64,
    /// Maximum UTF-8 bytes retained for one line, excluding its terminator.
    pub maximum_line_bytes: usize,
    /// Maximum physical lines parsed. This also bounds returned entries for an
    /// all-entry read.
    pub maximum_entries: usize,
}

impl Default for GarbageCollectionJournalReadOptions {
    fn default() -> Self {
        Self {
            maximum_file_bytes: DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES,
            maximum_line_bytes: DEFAULT_MAXIMUM_GC_JOURNAL_LINE_BYTES,
            maximum_entries: DEFAULT_MAXIMUM_GC_JOURNAL_ENTRIES,
        }
    }
}

/// Failure from a bounded garbage-collection journal read.
#[derive(Debug)]
#[non_exhaustive]
pub enum GarbageCollectionJournalReadError {
    /// Opening, inspecting, or reading the journal failed.
    InputOutput(std::io::Error),
    /// The file exceeded the configured byte limit.
    FileByteLimitExceeded {
        /// Configured maximum bytes.
        maximum_file_bytes: u64,
        /// Size reported by metadata, or the smallest size observed if the
        /// file grew while it was read.
        observed_file_bytes: u64,
    },
    /// A physical line exceeded the configured byte limit.
    LineByteLimitExceeded {
        /// One-based physical line number.
        line_number: usize,
        /// Configured maximum line bytes.
        maximum_line_bytes: usize,
        /// Length that the rejected byte would have produced.
        attempted_line_bytes: usize,
    },
    /// The file contained more physical lines than configured.
    EntryLimitExceeded {
        /// Configured maximum entries.
        maximum_entries: usize,
        /// Entry count that the rejected line would have produced.
        attempted_entries: usize,
    },
    /// A physical line was not valid UTF-8.
    InvalidUtf8 {
        /// One-based physical line number.
        line_number: usize,
        /// Zero-based byte offset of the first invalid sequence in the file.
        invalid_byte_offset: u64,
        /// Length of the invalid sequence, or `None` for an incomplete
        /// sequence at the end of the physical line.
        error_length: Option<usize>,
    },
}

impl fmt::Display for GarbageCollectionJournalReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputOutput(source) => write!(formatter, "input/output error: {source}"),
            Self::FileByteLimitExceeded {
                maximum_file_bytes,
                observed_file_bytes,
            } => write!(
                formatter,
                "garbage-collection journal contains at least {observed_file_bytes} bytes, \
                 exceeding the {maximum_file_bytes}-byte limit"
            ),
            Self::LineByteLimitExceeded {
                line_number,
                maximum_line_bytes,
                attempted_line_bytes,
            } => write!(
                formatter,
                "garbage-collection journal line {line_number} would contain \
                 {attempted_line_bytes} bytes, exceeding the {maximum_line_bytes}-byte limit"
            ),
            Self::EntryLimitExceeded {
                maximum_entries,
                attempted_entries,
            } => write!(
                formatter,
                "garbage-collection journal would contain {attempted_entries} entries, \
                 exceeding the {maximum_entries}-entry limit"
            ),
            Self::InvalidUtf8 {
                line_number,
                invalid_byte_offset,
                ..
            } => write!(
                formatter,
                "garbage-collection journal line {line_number} contains invalid UTF-8 at byte \
                 offset {invalid_byte_offset}"
            ),
        }
    }
}

impl std::error::Error for GarbageCollectionJournalReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InputOutput(source) => Some(source),
            Self::FileByteLimitExceeded { .. }
            | Self::LineByteLimitExceeded { .. }
            | Self::EntryLimitExceeded { .. }
            | Self::InvalidUtf8 { .. } => None,
        }
    }
}

impl From<std::io::Error> for GarbageCollectionJournalReadError {
    fn from(source: std::io::Error) -> Self {
        Self::InputOutput(source)
    }
}

/// Result type returned by bounded garbage-collection journal reads.
pub type GarbageCollectionJournalReadResult<Value> =
    std::result::Result<Value, GarbageCollectionJournalReadError>;

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
/// uses the legacy offsets and ignores surplus fields. Parsing uses constant
/// auxiliary space beyond the returned root string; because this entry point
/// accepts an already resident caller-owned string, the root clone is
/// necessarily proportional to the root field the caller supplied. File reads
/// apply [`GarbageCollectionJournalReadOptions::maximum_line_bytes`] before
/// calling this parser.
#[must_use]
pub fn parse_gc_journal_entry(line: &str) -> GarbageCollectionJournalEntry {
    let fields = split_like_java(line);
    let repository_size = parse_i64_field(&fields, 0);
    let reclaimed_size = parse_i64_field(&fields, 1);
    let timestamp_milliseconds = parse_i64_field(&fields, 2);
    let generation_number = parse_i32_field(&fields, 3);
    let (full_generation, next_field) = if fields.length == 7 {
        (parse_i32_field(&fields, 4), 5)
    } else {
        (generation_number, 4)
    };
    let compacted_nodes = parse_i64_field(&fields, next_field);
    let root_record_identifier_text = fields
        .get(next_field + 1)
        .map_or_else(|| NULL_RECORD_IDENTIFIER_TEXT.to_owned(), str::to_owned);

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
/// A missing, unreadable, non-UTF-8, or default-limit-exceeding file, and a
/// readable file with no lines, yields
/// [`GarbageCollectionJournalEntry::empty`]. Diagnostics are intentionally not
/// exposed because Oak treats this journal as optional informational state;
/// use [`read_gc_journal_with_options`] when the distinction matters. This is
/// a stateless helper and rereads the path on every call; Oak's long-lived
/// `GCJournal` object caches its latest entry.
#[must_use]
pub fn read_gc_journal(gc_journal_path: &Path) -> GarbageCollectionJournalEntry {
    read_gc_journal_with_options(
        gc_journal_path,
        GarbageCollectionJournalReadOptions::default(),
    )
    .unwrap_or_default()
}

/// Reads the last `gc.log` line with explicit resource limits and diagnostics.
///
/// Only the current line and latest parsed candidate are retained. The entire
/// file is nevertheless scanned so invalid UTF-8 anywhere is reported and no
/// partial result escapes. An empty file succeeds with
/// [`GarbageCollectionJournalEntry::empty`].
pub fn read_gc_journal_with_options(
    gc_journal_path: &Path,
    options: GarbageCollectionJournalReadOptions,
) -> GarbageCollectionJournalReadResult<GarbageCollectionJournalEntry> {
    let mut latest_entry = None;
    scan_gc_journal_lines(gc_journal_path, options, |line| {
        latest_entry = Some(parse_gc_journal_entry(line));
    })?;
    Ok(latest_entry.unwrap_or_default())
}

/// Reads and parses all `gc.log` lines in file order, matching
/// `GCJournal.readAll()`.
///
/// Malformed lines remain present as entries populated with field-specific
/// fallbacks. A missing, unreadable, non-UTF-8, or default-limit-exceeding file
/// yields an empty vector. Use [`read_all_gc_journal_with_options`] when the
/// distinction matters.
#[must_use]
pub fn read_all_gc_journal(gc_journal_path: &Path) -> Vec<GarbageCollectionJournalEntry> {
    read_all_gc_journal_with_options(
        gc_journal_path,
        GarbageCollectionJournalReadOptions::default(),
    )
    .unwrap_or_default()
}

/// Reads all `gc.log` lines with explicit resource limits and diagnostics.
///
/// Each valid UTF-8 line is parsed as it arrives, so the input file and a
/// separate line index are never retained. Any later decoding, I/O, or limit
/// failure discards the partial vector and returns an error.
pub fn read_all_gc_journal_with_options(
    gc_journal_path: &Path,
    options: GarbageCollectionJournalReadOptions,
) -> GarbageCollectionJournalReadResult<Vec<GarbageCollectionJournalEntry>> {
    let mut entries = Vec::new();
    scan_gc_journal_lines(gc_journal_path, options, |line| {
        entries.push(parse_gc_journal_entry(line));
    })?;
    Ok(entries)
}

fn parse_i64_field(fields: &JavaSplitFields<'_>, index: usize) -> i64 {
    fields
        .get(index)
        .and_then(|field| parse_java_signed_decimal(field, i64::MIN.into(), i64::MAX.into()))
        .and_then(|value| i64::try_from(value).ok())
        .unwrap_or(-1)
}

fn parse_i32_field(fields: &JavaSplitFields<'_>, index: usize) -> i32 {
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

struct JavaSplitFields<'line> {
    first_fields: [Option<&'line str>; 7],
    length: usize,
}

impl<'line> JavaSplitFields<'line> {
    fn get(&self, index: usize) -> Option<&'line str> {
        if index < self.length {
            self.first_fields.get(index).copied().flatten()
        } else {
            None
        }
    }
}

fn split_like_java(line: &str) -> JavaSplitFields<'_> {
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

const GC_JOURNAL_READ_BUFFER_BYTES: usize = 8 * 1024;

fn scan_gc_journal_lines(
    gc_journal_path: &Path,
    options: GarbageCollectionJournalReadOptions,
    mut consume_line: impl FnMut(&str),
) -> GarbageCollectionJournalReadResult<()> {
    let mut file = File::open(gc_journal_path)?;
    let metadata_file_bytes = file.metadata()?.len();
    if metadata_file_bytes > options.maximum_file_bytes {
        return Err(GarbageCollectionJournalReadError::FileByteLimitExceeded {
            maximum_file_bytes: options.maximum_file_bytes,
            observed_file_bytes: metadata_file_bytes,
        });
    }

    let mut read_buffer = [0u8; GC_JOURNAL_READ_BUFFER_BYTES];
    let mut line_bytes = Vec::new();
    let mut file_bytes_read = 0u64;
    let mut file_bytes_processed = 0u64;
    let mut line_start_byte_offset = 0u64;
    let mut entries_read = 0usize;
    let mut preceding_carriage_return = false;

    loop {
        let remaining_file_bytes = options.maximum_file_bytes.saturating_sub(file_bytes_read);
        let detectable_read_bytes = remaining_file_bytes.saturating_add(1);
        let requested_read_bytes = usize::try_from(detectable_read_bytes)
            .unwrap_or(usize::MAX)
            .min(read_buffer.len());
        let bytes_read = file.read(&mut read_buffer[..requested_read_bytes])?;
        if bytes_read == 0 {
            break;
        }
        let bytes_read_u64 = u64::try_from(bytes_read).unwrap_or(u64::MAX);
        let observed_file_bytes = file_bytes_read.saturating_add(bytes_read_u64);
        if bytes_read_u64 > remaining_file_bytes {
            return Err(GarbageCollectionJournalReadError::FileByteLimitExceeded {
                maximum_file_bytes: options.maximum_file_bytes,
                observed_file_bytes,
            });
        }
        file_bytes_read = observed_file_bytes;

        for &byte in &read_buffer[..bytes_read] {
            file_bytes_processed = file_bytes_processed.saturating_add(1);
            if preceding_carriage_return {
                preceding_carriage_return = false;
                if byte == b'\n' {
                    line_start_byte_offset = file_bytes_processed;
                    continue;
                }
            }

            if matches!(byte, b'\r' | b'\n') {
                finish_gc_journal_line(
                    &line_bytes,
                    line_start_byte_offset,
                    &mut entries_read,
                    options.maximum_entries,
                    &mut consume_line,
                )?;
                line_bytes.clear();
                line_start_byte_offset = file_bytes_processed;
                preceding_carriage_return = byte == b'\r';
            } else {
                if line_bytes.len() >= options.maximum_line_bytes {
                    return Err(GarbageCollectionJournalReadError::LineByteLimitExceeded {
                        line_number: entries_read.saturating_add(1),
                        maximum_line_bytes: options.maximum_line_bytes,
                        attempted_line_bytes: line_bytes.len().saturating_add(1),
                    });
                }
                line_bytes.push(byte);
            }
        }
    }

    if !line_bytes.is_empty() {
        finish_gc_journal_line(
            &line_bytes,
            line_start_byte_offset,
            &mut entries_read,
            options.maximum_entries,
            &mut consume_line,
        )?;
    }
    Ok(())
}

fn finish_gc_journal_line(
    line_bytes: &[u8],
    line_start_byte_offset: u64,
    entries_read: &mut usize,
    maximum_entries: usize,
    consume_line: &mut impl FnMut(&str),
) -> GarbageCollectionJournalReadResult<()> {
    let attempted_entries = entries_read.saturating_add(1);
    if *entries_read >= maximum_entries {
        return Err(GarbageCollectionJournalReadError::EntryLimitExceeded {
            maximum_entries,
            attempted_entries,
        });
    }
    let line_number = attempted_entries;
    let line = std::str::from_utf8(line_bytes).map_err(|source| {
        GarbageCollectionJournalReadError::InvalidUtf8 {
            line_number,
            invalid_byte_offset: line_start_byte_offset
                .saturating_add(u64::try_from(source.valid_up_to()).unwrap_or(u64::MAX)),
            error_length: source.error_len(),
        }
    })?;
    consume_line(line);
    *entries_read = attempted_entries;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES, GC_JOURNAL_READ_BUFFER_BYTES,
        GarbageCollectionJournalEntry, GarbageCollectionJournalReadError,
        GarbageCollectionJournalReadOptions, NULL_RECORD_IDENTIFIER_TEXT, parse_gc_journal_entry,
        read_all_gc_journal, read_all_gc_journal_with_options, read_gc_journal,
        read_gc_journal_with_options,
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
    fn streaming_state_handles_chunk_boundaries_and_unterminated_final_line() {
        let directory = TestDirectory::new("chunk-boundaries");
        let path = directory.path.join("gc.log");
        let first_prefix = b"1,2,3,4,5,";
        let mut fixture = Vec::new();
        fixture.extend_from_slice(first_prefix);
        fixture.resize(GC_JOURNAL_READ_BUFFER_BYTES - 1, b'x');
        fixture.push(b'\r');
        assert_eq!(fixture.len(), GC_JOURNAL_READ_BUFFER_BYTES);
        fixture.extend_from_slice(b"\n2,3,4,5,6,second\r3,4,5,6,7,third\n4,5,6,7,8,unterminated");
        std::fs::write(&path, fixture).expect("write boundary fixture");

        let entries =
            read_all_gc_journal_with_options(&path, GarbageCollectionJournalReadOptions::default())
                .expect("read boundary fixture");
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries[0].root_record_identifier_text.len(),
            GC_JOURNAL_READ_BUFFER_BYTES - 1 - first_prefix.len()
        );
        assert!(
            entries[0]
                .root_record_identifier_text
                .bytes()
                .all(|byte| byte == b'x')
        );
        assert_eq!(entries[1].root_record_identifier_text, "second");
        assert_eq!(entries[2].root_record_identifier_text, "third");
        assert_eq!(entries[3].root_record_identifier_text, "unterminated");
        assert_eq!(
            read_gc_journal_with_options(&path, GarbageCollectionJournalReadOptions::default())
                .expect("read last boundary entry"),
            entries[3]
        );
    }

    #[test]
    fn utf8_code_point_may_cross_a_fixed_read_buffer_boundary() {
        let directory = TestDirectory::new("utf8-chunk-boundary");
        let path = directory.path.join("gc.log");
        let prefix = b"1,2,3,4,5,";
        let mut fixture = Vec::new();
        fixture.extend_from_slice(prefix);
        fixture.resize(GC_JOURNAL_READ_BUFFER_BYTES - 1, b'x');
        fixture.extend_from_slice("３".as_bytes());
        fixture.push(b'\n');
        std::fs::write(&path, fixture).expect("write UTF-8 boundary fixture");

        let entries = read_all_gc_journal(&path);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].root_record_identifier_text.ends_with('３'));
    }

    #[test]
    fn invalid_utf8_anywhere_discards_every_default_result() {
        let directory = TestDirectory::new("invalid-utf8-anywhere");
        let path = directory.path.join("gc.log");
        let valid_line = b"1,2,3,4,5,valid\n";
        let fixtures = [
            [vec![0xff, b'\n'], valid_line.to_vec()].concat(),
            [valid_line.to_vec(), vec![b'2', b',', 0xff, b'\n']].concat(),
            [valid_line.to_vec(), vec![b'2', b',', 0xe2, 0x82]].concat(),
        ];

        for fixture in fixtures {
            std::fs::write(&path, fixture).expect("write invalid UTF-8 fixture");
            assert_eq!(
                read_gc_journal(&path),
                GarbageCollectionJournalEntry::empty()
            );
            assert!(read_all_gc_journal(&path).is_empty());
            assert!(matches!(
                read_all_gc_journal_with_options(
                    &path,
                    GarbageCollectionJournalReadOptions::default()
                ),
                Err(GarbageCollectionJournalReadError::InvalidUtf8 { .. })
            ));
        }
    }

    #[test]
    fn invalid_utf8_diagnostic_reports_line_and_file_offset() {
        let directory = TestDirectory::new("invalid-utf8-offset");
        let path = directory.path.join("gc.log");
        let first_line = b"1,2,3,4,5,first\r\n";
        let second_line_prefix = b"2,3,4,5,6,";
        let fixture = [
            first_line.as_slice(),
            second_line_prefix.as_slice(),
            &[0xff, b'\n'],
        ]
        .concat();
        std::fs::write(&path, fixture).expect("write invalid UTF-8 offset fixture");

        let error =
            read_all_gc_journal_with_options(&path, GarbageCollectionJournalReadOptions::default())
                .expect_err("invalid UTF-8 must fail the whole read");
        assert!(matches!(
            error,
            GarbageCollectionJournalReadError::InvalidUtf8 {
                line_number: 2,
                invalid_byte_offset,
                error_length: Some(1),
            } if invalid_byte_offset
                == u64::try_from(first_line.len() + second_line_prefix.len())
                    .expect("fixture length fits u64")
        ));
    }

    #[test]
    fn sparse_oversized_file_fails_from_the_default_metadata_limit() {
        let directory = TestDirectory::new("sparse-file-limit");
        let path = directory.path.join("gc.log");
        let file = std::fs::File::create(&path).expect("create sparse fixture");
        file.set_len(DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1)
            .expect("set sparse fixture length");
        drop(file);

        let error =
            read_all_gc_journal_with_options(&path, GarbageCollectionJournalReadOptions::default())
                .expect_err("sparse oversized file must fail before reading");
        assert!(matches!(
            error,
            GarbageCollectionJournalReadError::FileByteLimitExceeded {
                maximum_file_bytes: DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES,
                observed_file_bytes,
            } if observed_file_bytes == DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1
        ));
        assert_eq!(
            read_gc_journal(&path),
            GarbageCollectionJournalEntry::empty()
        );
        assert!(read_all_gc_journal(&path).is_empty());
        assert_eq!(
            std::fs::metadata(&path)
                .expect("metadata after bounded reads")
                .len(),
            DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1
        );
    }

    #[test]
    fn line_limit_rejects_the_next_byte_before_parsing() {
        let directory = TestDirectory::new("line-limit");
        let path = directory.path.join("gc.log");
        std::fs::write(&path, b"1234\n").expect("write overlong line fixture");
        let options = GarbageCollectionJournalReadOptions {
            maximum_file_bytes: 5,
            maximum_line_bytes: 3,
            maximum_entries: 1,
        };

        let error = read_all_gc_journal_with_options(&path, options)
            .expect_err("fourth line byte must exceed configured limit");
        assert!(matches!(
            error,
            GarbageCollectionJournalReadError::LineByteLimitExceeded {
                line_number: 1,
                maximum_line_bytes: 3,
                attempted_line_bytes: 4,
            }
        ));
    }

    #[test]
    fn custom_limits_accept_exact_boundaries_and_reject_the_next_unit() {
        let directory = TestDirectory::new("exact-resource-limits");
        let path = directory.path.join("gc.log");
        // CRLF consumes two file bytes, but neither byte belongs to a line.
        // The final U+00E9 consumes two UTF-8 bytes in the second line.
        let fixture = "x\r\né".as_bytes();
        std::fs::write(&path, fixture).expect("write exact-limit fixture");

        let exact = GarbageCollectionJournalReadOptions {
            maximum_file_bytes: u64::try_from(fixture.len()).expect("fixture length fits u64"),
            maximum_line_bytes: 2,
            maximum_entries: 2,
        };
        let entries = read_all_gc_journal_with_options(&path, exact)
            .expect("all three resource dimensions accept equality");
        assert_eq!(entries.len(), 2);

        let file_error = read_all_gc_journal_with_options(
            &path,
            GarbageCollectionJournalReadOptions {
                maximum_file_bytes: u64::try_from(fixture.len() - 1)
                    .expect("fixture length fits u64"),
                ..exact
            },
        )
        .expect_err("one byte below the file size must fail");
        assert!(matches!(
            file_error,
            GarbageCollectionJournalReadError::FileByteLimitExceeded {
                maximum_file_bytes: 4,
                observed_file_bytes: 5,
            }
        ));

        let line_error = read_all_gc_journal_with_options(
            &path,
            GarbageCollectionJournalReadOptions {
                maximum_line_bytes: 1,
                ..exact
            },
        )
        .expect_err("a two-byte UTF-8 value must not fit a one-byte line limit");
        assert!(matches!(
            line_error,
            GarbageCollectionJournalReadError::LineByteLimitExceeded {
                line_number: 2,
                maximum_line_bytes: 1,
                attempted_line_bytes: 2,
            }
        ));

        let entry_error = read_all_gc_journal_with_options(
            &path,
            GarbageCollectionJournalReadOptions {
                maximum_entries: 1,
                ..exact
            },
        )
        .expect_err("the second physical line must exceed a one-entry limit");
        assert!(matches!(
            entry_error,
            GarbageCollectionJournalReadError::EntryLimitExceeded {
                maximum_entries: 1,
                attempted_entries: 2,
            }
        ));
    }

    #[test]
    fn entry_limit_bounds_newline_heavy_input_work() {
        let directory = TestDirectory::new("entry-limit");
        let path = directory.path.join("gc.log");
        let fixture = vec![b'\n'; 10_000];
        std::fs::write(&path, fixture).expect("write newline-heavy fixture");
        let options = GarbageCollectionJournalReadOptions {
            maximum_file_bytes: 10_000,
            maximum_line_bytes: 0,
            maximum_entries: 32,
        };

        let error = read_all_gc_journal_with_options(&path, options)
            .expect_err("thirty-third line must exceed configured limit");
        assert!(matches!(
            error,
            GarbageCollectionJournalReadError::EntryLimitExceeded {
                maximum_entries: 32,
                attempted_entries: 33,
            }
        ));
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

        let entries = read_all_gc_journal(&path);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].generation.generation, 4);
        assert_eq!(entries[0].generation.full_generation, 5);
        assert_eq!(entries[0].root_record_identifier_text, "root");
        assert_eq!(entries[1], parse_gc_journal_entry(""));
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

        assert!(matches!(
            read_gc_journal_with_options(
                &missing,
                GarbageCollectionJournalReadOptions::default()
            ),
            Err(GarbageCollectionJournalReadError::InputOutput(source))
                if source.kind() == std::io::ErrorKind::NotFound
        ));
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
        let _ = read_gc_journal_with_options(&path, GarbageCollectionJournalReadOptions::default());
        let _ =
            read_all_gc_journal_with_options(&path, GarbageCollectionJournalReadOptions::default());

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
