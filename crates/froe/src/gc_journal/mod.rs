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

mod java;
mod limits;
#[cfg(test)]
mod test_support;

pub(crate) use java::*;
pub use limits::*;

/// Oak's textual representation of its null record identifier.
pub(crate) const NULL_RECORD_IDENTIFIER_TEXT: &str = "00000000-0000-0000-0000-000000000000:0";

/// One parsed line of `gc.log`.
///
/// Parsing is deliberately total. A malformed or absent numeric field is
/// represented by `-1`, while an absent root uses Oak's null record-id text.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
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
/// A missing, unreadable, or non-UTF-8 file, and a readable file with no lines,
/// succeeds with [`GarbageCollectionJournalEntry::empty`], matching Oak's
/// optional-journal fallback. Unlike Oak, this reader applies resource limits;
/// exceeding a default limit returns its typed error rather than silently
/// replacing data Oak would have returned. Use [`read_gc_journal_with_options`]
/// to diagnose input/output and UTF-8 failures or to select different limits.
///
/// This is a stateless helper and rereads the path on every call; Oak's
/// long-lived `GCJournal` object caches its latest entry.
pub fn read_gc_journal(
    gc_journal_path: &Path,
) -> GarbageCollectionJournalReadResult<GarbageCollectionJournalEntry> {
    oak_optional_read_fallback(read_gc_journal_with_options(
        gc_journal_path,
        GarbageCollectionJournalReadOptions::default(),
    ))
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
/// fallbacks. A missing, unreadable, or non-UTF-8 file yields an empty vector,
/// matching Oak's optional-journal fallback. Exceeding a froe resource limit
/// instead returns its typed error so successfully readable Oak data is never
/// silently replaced. Use [`read_all_gc_journal_with_options`] to diagnose
/// input/output and UTF-8 failures or to select different limits.
pub fn read_all_gc_journal(
    gc_journal_path: &Path,
) -> GarbageCollectionJournalReadResult<Vec<GarbageCollectionJournalEntry>> {
    oak_optional_read_fallback(read_all_gc_journal_with_options(
        gc_journal_path,
        GarbageCollectionJournalReadOptions::default(),
    ))
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

pub(crate) fn oak_optional_read_fallback<Value: Default>(
    result: GarbageCollectionJournalReadResult<Value>,
) -> GarbageCollectionJournalReadResult<Value> {
    match result {
        Ok(value) => Ok(value),
        Err(
            GarbageCollectionJournalReadError::InputOutput(_)
            | GarbageCollectionJournalReadError::InvalidUtf8 { .. },
        ) => Ok(Value::default()),
        Err(error) => Err(error),
    }
}

pub(crate) const GC_JOURNAL_READ_BUFFER_BYTES: usize = 8 * 1024;

pub(crate) fn scan_gc_journal_lines(
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

pub(crate) fn finish_gc_journal_line(
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
    use super::{
        GC_JOURNAL_READ_BUFFER_BYTES, GarbageCollectionJournalEntry, NULL_RECORD_IDENTIFIER_TEXT,
        parse_gc_journal_entry, read_all_gc_journal, read_all_gc_journal_with_options,
        read_gc_journal, read_gc_journal_with_options,
    };
    use crate::gc_journal::limits::{
        GarbageCollectionJournalReadError, GarbageCollectionJournalReadOptions,
    };
    use crate::gc_journal::test_support::TestDirectory;

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
    fn parses_every_field_at_its_signed_extreme() {
        let entry = parse_gc_journal_entry(&format!(
            "{},{},-1,{},{},+7,root",
            i64::MIN,
            i64::MAX,
            i32::MIN,
            i32::MAX
        ));

        assert_eq!(entry.repository_size, i64::MIN);
        assert_eq!(entry.reclaimed_size, i64::MAX);
        assert_eq!(entry.timestamp_milliseconds, -1);
        assert_eq!(entry.generation.generation, i32::MIN);
        assert_eq!(entry.generation.full_generation, i32::MAX);
        assert_eq!(entry.compacted_nodes, 7);
        assert_eq!(entry.root_record_identifier_text, "root");
    }

    /// One malformed field defaults on its own: a leading space, a
    /// non-numeric value, an i32 overflow, and a bare sign each fall back
    /// without disturbing the fields around them.
    #[test]
    fn defaults_each_malformed_field_independently() {
        let entry = parse_gc_journal_entry(" 1,2,x,2147483648,5,+,not-a-record");

        assert_eq!(entry.repository_size, -1, "Java does not trim fields");
        assert_eq!(entry.reclaimed_size, 2);
        assert_eq!(entry.timestamp_milliseconds, -1);
        assert_eq!(entry.generation.generation, -1);
        assert_eq!(entry.generation.full_generation, 5);
        assert_eq!(entry.compacted_nodes, -1);
        assert_eq!(entry.root_record_identifier_text, "not-a-record");
        assert!(entry.root_record_identifier().is_none());
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
    fn reads_all_java_line_endings_in_file_order_and_read_uses_last_line() {
        let directory = TestDirectory::new("line-endings");
        let path = directory.path.join("gc.log");
        std::fs::write(
            &path,
            "1,2,3,4,5,first\r2,3,4,5,6,second\r\n3,4,5,6,7,third\n",
        )
        .expect("write fixture");

        let all = read_all_gc_journal(&path).expect("read all line-ending variants");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].root_record_identifier_text, "first");
        assert_eq!(all[1].root_record_identifier_text, "second");
        assert_eq!(all[2].root_record_identifier_text, "third");
        assert_eq!(read_gc_journal(&path).expect("read latest line"), all[2]);

        std::fs::write(&path, "1,2,3,4,5,first\n\n").expect("write empty last line");
        let all = read_all_gc_journal(&path).expect("read trailing empty line");
        assert_eq!(all.len(), 2, "a physical empty line remains an entry");
        let parsed_empty_line = parse_gc_journal_entry("");
        assert_eq!(parsed_empty_line.generation.generation, -1);
        assert_eq!(all[1], parsed_empty_line);
        assert_eq!(
            read_gc_journal(&path).expect("read latest empty line"),
            parsed_empty_line
        );
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

        let entries = read_all_gc_journal(&path).expect("read UTF-8 boundary fixture");
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
                read_gc_journal(&path).expect("Oak-style invalid UTF-8 fallback"),
                GarbageCollectionJournalEntry::empty()
            );
            assert!(
                read_all_gc_journal(&path)
                    .expect("Oak-style invalid UTF-8 fallback")
                    .is_empty()
            );
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
    fn missing_unreadable_and_non_utf8_files_use_oak_fallbacks() {
        let directory = TestDirectory::new("read-failures");
        let missing = directory.path.join("missing.log");
        assert_eq!(
            read_gc_journal(&missing).expect("Oak-style missing-file fallback"),
            GarbageCollectionJournalEntry::empty()
        );
        assert!(
            read_all_gc_journal(&missing)
                .expect("Oak-style missing-file fallback")
                .is_empty()
        );

        let empty = directory.path.join("empty.log");
        std::fs::write(&empty, []).expect("write empty journal");
        assert_eq!(
            read_gc_journal(&empty).expect("read empty journal"),
            GarbageCollectionJournalEntry::empty()
        );
        assert!(
            read_all_gc_journal(&empty)
                .expect("read empty journal")
                .is_empty()
        );

        assert_eq!(
            read_gc_journal(&directory.path).expect("Oak-style unreadable-file fallback"),
            GarbageCollectionJournalEntry::empty(),
            "a directory is not a readable journal file"
        );
        assert!(
            read_all_gc_journal(&directory.path)
                .expect("Oak-style unreadable-file fallback")
                .is_empty()
        );

        let invalid_utf8 = directory.path.join("invalid-utf8.log");
        std::fs::write(&invalid_utf8, [b'1', b',', 0xff, b'\n']).expect("write invalid UTF-8");
        assert_eq!(
            read_gc_journal(&invalid_utf8).expect("Oak-style invalid UTF-8 fallback"),
            GarbageCollectionJournalEntry::empty()
        );
        assert!(
            read_all_gc_journal(&invalid_utf8)
                .expect("Oak-style invalid UTF-8 fallback")
                .is_empty()
        );

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
