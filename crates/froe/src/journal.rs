//! The journal: the repository's sequence of head states.
//!
//! `journal.log` is an append-only text file. Each line records one
//! persisted head state:
//!
//! ```text
//! f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:270976 root 1754556000123
//! ```
//!
//! The first field is the record identifier of the head node record (the
//! *super-root*), the literal `root` is a historical tag, and the third
//! field is the timestamp in milliseconds since the Unix epoch. The last
//! line is the most recent head; readers scan backwards and skip over
//! malformed lines, which lets a reader recover from a torn write by
//! falling back to an older revision.

use std::path::Path;

use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;

/// One line of the journal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JournalEntry {
    /// The revision field exactly as stored (not yet validated).
    pub revision_text: String,
    /// The timestamp in milliseconds since the Unix epoch, or -1 when the
    /// line carries none.
    pub timestamp_milliseconds: i64,
}

impl JournalEntry {
    /// Parses the revision field into a record identifier, when valid.
    #[must_use]
    pub fn record_identifier(&self) -> Option<RecordIdentifier> {
        parse_record_identifier_text(&self.revision_text)
    }
}

/// Reads the journal file, returning entries **newest first** (the reverse
/// of file order). Lines without a space are skipped; missing or
/// malformed timestamps become -1. A missing journal file is an error:
/// a repository without a journal has no readable state.
pub fn read_journal(journal_path: &Path) -> Result<Vec<JournalEntry>> {
    let content = std::fs::read(journal_path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::InvalidFormat {
                details: format!("journal file {} does not exist", journal_path.display()),
            }
        } else {
            Error::InputOutput(source)
        }
    })?;
    let content = String::from_utf8_lossy(&content);
    let mut entries = Vec::new();
    for line in content.split(['\n', '\r']).rev() {
        if !line.contains(' ') {
            // Also skips empty lines, including those produced by
            // splitting `\r\n` on both characters.
            continue;
        }
        let fields: Vec<&str> = line.split(' ').collect();
        let timestamp_milliseconds = if fields.len() > 2 {
            fields[2].parse().unwrap_or(-1)
        } else {
            -1
        };
        entries.push(JournalEntry {
            revision_text: fields[0].to_owned(),
            timestamp_milliseconds,
        });
    }
    Ok(entries)
}

/// Parses a record identifier string in either of the two forms Oak
/// writes: `<uuid>:<record number in decimal>` (the journal form) or
/// `<uuid>.<record number as exactly eight lowercase hex digits>` (the
/// diagnostic form). The UUID must be lowercase.
#[must_use]
pub fn parse_record_identifier_text(text: &str) -> Option<RecordIdentifier> {
    // Guard the split against multi-byte characters: a corrupt journal
    // line may contain arbitrary UTF-8 and must be skipped, not panic on.
    if text.len() < 38 || !text.is_char_boundary(36) {
        return None;
    }
    let (uuid_text, rest) = text.split_at(36);
    let segment: SegmentIdentifier = uuid_text.parse().ok()?;
    let record_number = if let Some(decimal) = rest.strip_prefix(':') {
        // Decimal without leading zeros (a bare `0` is allowed).
        if decimal.is_empty()
            || !decimal.bytes().all(|byte| byte.is_ascii_digit())
            || (decimal.len() > 1 && decimal.starts_with('0'))
        {
            return None;
        }
        decimal.parse::<u32>().ok()?
    } else {
        let hexadecimal = rest.strip_prefix('.')?;
        if hexadecimal.len() != 8
            || !hexadecimal
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        u32::from_str_radix(hexadecimal, 16).ok()?
    };
    // Record numbers are signed 32-bit in the format; Java parses them
    // with Integer.parseInt and rejects anything above i32::MAX, causing
    // the journal scan to rewind. Match that.
    if record_number > i32::MAX as u32 {
        return None;
    }
    Some(RecordIdentifier::new(segment, record_number))
}

#[cfg(test)]
mod tests {
    use super::{parse_record_identifier_text, read_journal};
    use crate::segment::identifier::SegmentIdentifier;

    #[test]
    fn parses_record_identifier_forms() {
        let expected_segment = SegmentIdentifier::new(0xF81A_D1AC_E73E_4DB0, 0xA4B6_B1C8_AA5C_F303);

        let decimal = parse_record_identifier_text("f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:270976")
            .expect("decimal form");
        assert_eq!(decimal.segment, expected_segment);
        assert_eq!(decimal.record_number, 270_976);

        let hexadecimal =
            parse_record_identifier_text("f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303.00042280")
                .expect("hexadecimal form");
        assert_eq!(hexadecimal.record_number, 0x0004_2280);

        assert_eq!(
            parse_record_identifier_text("f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:0")
                .expect("zero record number")
                .record_number,
            0
        );
    }

    #[test]
    fn rejects_malformed_record_identifiers() {
        for text in [
            "",
            "not-an-identifier",
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303", // no record number
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:", // empty record number
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:007", // leading zeros
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:1x", // trailing garbage
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303.42280", // hex form too short
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303.0004228G", // invalid hex digit
            "F81AD1AC-E73E-4DB0-A4B6-B1C8AA5CF303:1", // uppercase UUID
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:3000000000", // above i32::MAX
            "f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303.ffffffff", // above i32::MAX
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\u{e9}xx:1", // multi-byte at index 36
        ] {
            assert!(
                parse_record_identifier_text(text).is_none(),
                "{text:?} must be rejected"
            );
        }
    }

    /// Removes the test directory even when an assertion panics.
    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn reads_journal_newest_first_with_tolerant_parsing() {
        let directory = TestDirectory {
            path: std::env::temp_dir().join(format!("froe-journal-test-{}", std::process::id())),
        };
        std::fs::create_dir_all(&directory.path).expect("create test directory");
        let journal_path = directory.path.join("journal.log");
        std::fs::write(
            &journal_path,
            "11111111-1111-4111-a111-111111111111:100 root 1000\n\
             garbage-line-without-spaces\n\
             22222222-2222-4222-a222-222222222222:200 root\n\
             33333333-3333-4333-a333-333333333333:300 root not-a-number\n\
             44444444-4444-4444-a444-444444444444:400 root 4000\n",
        )
        .expect("write journal");

        let entries = read_journal(&journal_path).expect("read journal");
        assert_eq!(entries.len(), 4, "the garbage line is skipped");
        assert_eq!(
            entries[0].revision_text, "44444444-4444-4444-a444-444444444444:400",
            "entries come newest first"
        );
        assert_eq!(entries[0].timestamp_milliseconds, 4000);
        assert_eq!(
            entries[1].timestamp_milliseconds, -1,
            "malformed timestamp becomes -1"
        );
        assert_eq!(
            entries[2].timestamp_milliseconds, -1,
            "missing timestamp becomes -1"
        );
        assert_eq!(entries[3].timestamp_milliseconds, 1000);
        assert!(entries[0].record_identifier().is_some());
    }

    #[test]
    fn missing_journal_is_an_error() {
        let missing = std::path::Path::new("/nonexistent-froe-test/journal.log");
        assert!(read_journal(missing).is_err());
    }
}
