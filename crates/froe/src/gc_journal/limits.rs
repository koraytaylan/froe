//! What a `gc.log` may cost to read: the file, line, and entry ceilings,
//! and the typed error each one raises before the byte that would exceed
//! it is parsed.

use super::fmt;

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

impl GarbageCollectionJournalReadOptions {
    /// Creates explicit resource limits for a journal read.
    ///
    /// Zero is a valid limit for every dimension. [`Default::default`]
    /// implementation supplies the conservative library defaults, and its
    /// public fields may also be adjusted after construction.
    #[must_use]
    pub const fn new(
        maximum_file_bytes: u64,
        maximum_line_bytes: usize,
        maximum_entries: usize,
    ) -> Self {
        Self {
            maximum_file_bytes,
            maximum_line_bytes,
            maximum_entries,
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES, GarbageCollectionJournalReadError,
        GarbageCollectionJournalReadOptions,
    };
    use crate::gc_journal::test_support::TestDirectory;
    use crate::gc_journal::{
        read_all_gc_journal, read_all_gc_journal_with_options, read_gc_journal,
    };

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
        assert!(matches!(
            read_gc_journal(&path),
            Err(GarbageCollectionJournalReadError::FileByteLimitExceeded {
                maximum_file_bytes: DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES,
                observed_file_bytes,
            }) if observed_file_bytes == DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1
        ));
        assert!(matches!(
            read_all_gc_journal(&path),
            Err(GarbageCollectionJournalReadError::FileByteLimitExceeded {
                maximum_file_bytes: DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES,
                observed_file_bytes,
            }) if observed_file_bytes == DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1
        ));
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
}
