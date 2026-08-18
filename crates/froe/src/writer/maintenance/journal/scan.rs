//! Reading a journal without interpreting it: every line's raw bytes, its
//! classification, and the record and timestamp it does or does not
//! resolve to.

use super::{
    Metadata, Path, PathBuf, Read, RecordIdentifier, Result, open_regular_journal,
    parse_record_identifier_text,
};

/// A byte-exact snapshot of an existing `journal.log`.
#[derive(Debug)]
pub(in crate::writer::maintenance) struct RawJournal {
    pub(in crate::writer::maintenance) path: PathBuf,
    pub(in crate::writer::maintenance) source_bytes: Vec<u8>,
    pub(in crate::writer::maintenance) metadata: Metadata,
    pub(in crate::writer::maintenance) lines: Vec<RawJournalLine>,
}

impl RawJournal {
    /// Returns the physical lines in their original, oldest-first order.
    pub(in crate::writer::maintenance) fn lines(&self) -> &[RawJournalLine] {
        &self.lines
    }

    /// Returns every source byte exactly as it appeared on disk.
    pub(in crate::writer::maintenance) fn source_bytes(&self) -> &[u8] {
        &self.source_bytes
    }
}

/// One physical journal line, including its original line terminator.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::writer::maintenance) struct RawJournalLine {
    pub(in crate::writer::maintenance) raw_bytes: Vec<u8>,
    pub(in crate::writer::maintenance) content_length: usize,
    pub(in crate::writer::maintenance) classification: RawJournalLineClassification,
}

impl RawJournalLine {
    /// Returns the complete line, including `LF`, `CRLF`, or bare `CR`.
    #[allow(
        dead_code,
        reason = "the byte-exact line accessor is part of the cleanup scanner's internal contract"
    )]
    pub(in crate::writer::maintenance) fn raw_bytes(&self) -> &[u8] {
        &self.raw_bytes
    }

    /// Returns the line without its terminator.
    #[allow(
        dead_code,
        reason = "callers inspecting unusual journal syntax need the un-terminated bytes"
    )]
    pub(in crate::writer::maintenance) fn content_bytes(&self) -> &[u8] {
        &self.raw_bytes[..self.content_length]
    }

    /// Returns the reader-compatible classification of this line.
    pub(in crate::writer::maintenance) fn classification(&self) -> &RawJournalLineClassification {
        &self.classification
    }
}

/// Why the ordinary journal reader would retain or ignore a physical line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::writer::maintenance) enum RawJournalLineClassification {
    /// The line contains no ASCII space and is skipped before field parsing.
    ParserSkippedNoSpace,
    /// The line has fields, but its first field is not a record identifier.
    InvalidRecordIdentifier {
        /// The exact, possibly non-UTF-8 first field.
        revision_text: Vec<u8>,
    },
    /// The line is a syntactic journal record, even if its timestamp is bad.
    Record(RawJournalRecord),
}

/// Parsed fields needed by cleanup while the enclosing line retains the bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::writer::maintenance) struct RawJournalRecord {
    /// The validated record identifier from the first field.
    pub(in crate::writer::maintenance) record_identifier: RecordIdentifier,
    /// The exact bytes of the first field.
    pub(in crate::writer::maintenance) revision_text: Vec<u8>,
    /// The exact bytes of the second (historical tag) field.
    pub(in crate::writer::maintenance) tag: Vec<u8>,
    /// The third-field timestamp classification.
    pub(in crate::writer::maintenance) timestamp: RawJournalTimestamp,
}

/// Timestamp parsing that preserves the distinction between absent and bad.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::writer::maintenance) enum RawJournalTimestamp {
    /// The line has no third space-separated field.
    Missing,
    /// A third field exists but is not an `i64` timestamp.
    Malformed {
        /// The exact third-field bytes.
        raw: Vec<u8>,
    },
    /// The third field is a valid millisecond timestamp.
    Milliseconds {
        /// The exact third-field bytes, before numeric parsing.
        raw: Vec<u8>,
        /// The parsed signed millisecond value.
        value: i64,
    },
}

/// Scans `journal.log` without creating, changing, or following any file.
///
/// The returned line indexes are stable for the lifetime of the snapshot and
/// are the indexes accepted by [`rewrite_journal_atomically`].
pub(in crate::writer::maintenance) fn scan_raw_journal(directory: &Path) -> Result<RawJournal> {
    scan_raw_journal_file(&directory.join("journal.log"))
}

/// Scans an explicitly named journal-shaped staging file without following
/// links. Cleanup uses this to prove that every physical staging line is
/// already represented by the canonical journal before deleting it.
pub(in crate::writer::maintenance) fn scan_raw_journal_file(path: &Path) -> Result<RawJournal> {
    let path = path.to_owned();
    let (mut file, metadata) = open_regular_journal(&path)?;
    let mut source_bytes = Vec::new();
    file.read_to_end(&mut source_bytes)?;
    let lines = split_and_classify_lines(&source_bytes);
    Ok(RawJournal {
        path,
        source_bytes,
        metadata,
        lines,
    })
}

pub(in crate::writer::maintenance) fn split_and_classify_lines(
    source: &[u8],
) -> Vec<RawJournalLine> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    while cursor < source.len() {
        let terminator_length = match source[cursor] {
            b'\r' if source.get(cursor + 1) == Some(&b'\n') => 2,
            b'\n' | b'\r' => 1,
            _ => {
                cursor += 1;
                continue;
            }
        };
        let end = cursor + terminator_length;
        lines.push(make_line(&source[start..end], cursor - start));
        start = end;
        cursor = end;
    }
    if start < source.len() {
        lines.push(make_line(&source[start..], source.len() - start));
    }
    lines
}

pub(in crate::writer::maintenance) fn make_line(
    raw: &[u8],
    content_length: usize,
) -> RawJournalLine {
    let content = &raw[..content_length];
    RawJournalLine {
        raw_bytes: raw.to_vec(),
        content_length,
        classification: classify_line(content),
    }
}

pub(in crate::writer::maintenance) fn classify_line(
    content: &[u8],
) -> RawJournalLineClassification {
    if !content.contains(&b' ') {
        return RawJournalLineClassification::ParserSkippedNoSpace;
    }
    let mut fields = content.split(|byte| *byte == b' ');
    let revision_text = fields.next().unwrap_or_default().to_vec();
    let tag = fields.next().unwrap_or_default().to_vec();
    let record_identifier = std::str::from_utf8(&revision_text)
        .ok()
        .and_then(parse_record_identifier_text);
    let Some(record_identifier) = record_identifier else {
        return RawJournalLineClassification::InvalidRecordIdentifier { revision_text };
    };
    let timestamp = match fields.next() {
        None => RawJournalTimestamp::Missing,
        Some(raw) => match std::str::from_utf8(raw)
            .ok()
            .and_then(|text| text.parse::<i64>().ok())
        {
            Some(value) => RawJournalTimestamp::Milliseconds {
                raw: raw.to_vec(),
                value,
            },
            None => RawJournalTimestamp::Malformed { raw: raw.to_vec() },
        },
    };
    RawJournalLineClassification::Record(RawJournalRecord {
        record_identifier,
        revision_text,
        tag,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{RawJournalLineClassification, RawJournalTimestamp, scan_raw_journal};
    use crate::writer::maintenance::journal::test_support::{FIRST, SECOND, TestDirectory};

    #[test]
    fn scanner_preserves_every_raw_byte_and_classifies_ignored_lines() {
        let directory = TestDirectory::new("raw");
        let bytes = format!("{FIRST} root 1\nno-space\rnot-a-record root 2\n{SECOND} custom 3");
        directory.write_journal(bytes.as_bytes());

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        assert_eq!(journal.source_bytes(), bytes.as_bytes());
        assert_eq!(
            journal
                .lines()
                .iter()
                .flat_map(crate::writer::maintenance::journal::RawJournalLine::raw_bytes)
                .copied()
                .collect::<Vec<_>>(),
            bytes.as_bytes()
        );
        assert!(matches!(
            journal.lines()[0].classification(),
            RawJournalLineClassification::Record(_)
        ));
        assert!(matches!(
            journal.lines()[1].classification(),
            RawJournalLineClassification::ParserSkippedNoSpace
        ));
        assert!(matches!(
            journal.lines()[2].classification(),
            RawJournalLineClassification::InvalidRecordIdentifier { .. }
        ));
        assert_eq!(
            journal.lines()[3].content_bytes(),
            format!("{SECOND} custom 3").as_bytes()
        );
    }

    #[test]
    fn malformed_or_missing_timestamp_does_not_invalidate_a_record() {
        let directory = TestDirectory::new("timestamp");
        let bytes = format!("{FIRST} tag not-a-number\n{SECOND} tag\n");
        directory.write_journal(bytes.as_bytes());

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        let RawJournalLineClassification::Record(first) = journal.lines()[0].classification()
        else {
            panic!("malformed timestamp must remain a record");
        };
        assert_eq!(first.tag, b"tag");
        assert_eq!(
            first.timestamp,
            RawJournalTimestamp::Malformed {
                raw: b"not-a-number".to_vec()
            }
        );
        let RawJournalLineClassification::Record(second) = journal.lines()[1].classification()
        else {
            panic!("missing timestamp must remain a record");
        };
        assert_eq!(second.timestamp, RawJournalTimestamp::Missing);
    }

    #[test]
    fn invalid_identifiers_are_distinct_from_parser_skipped_lines() {
        let directory = TestDirectory::new("invalid-id");
        directory.write_journal(b"garbage\ngarbage root 12\n");

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        assert!(matches!(
            journal.lines()[0].classification(),
            RawJournalLineClassification::ParserSkippedNoSpace
        ));
        assert_eq!(
            journal.lines()[1].classification(),
            &RawJournalLineClassification::InvalidRecordIdentifier {
                revision_text: b"garbage".to_vec()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn scanner_rejects_a_symlinked_journal() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("symlink");
        std::fs::write(directory.path.join("target"), b"not the journal").expect("write target");
        symlink("target", directory.path.join("journal.log")).expect("create symlink");

        assert!(scan_raw_journal(&directory.path).is_err());
    }
}
