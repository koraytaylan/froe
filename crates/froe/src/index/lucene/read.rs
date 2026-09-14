//! Bounded primitive reads over a Lucene file.
//!
//! `docs/analysis/index-lucene-storage.md` §8.1 specifies the encodings.
//! Everything here works over any `Read + Seek`, so the same readers serve
//! a file inside `:data` through `OakDirectory` and a file on disk under a
//! dump directory.
//!
//! **Every length is attacker-controlled.** A `String`'s length prefix is a
//! `VInt` of *bytes*, and a `StringSet`'s count is a raw `Int`; a reader
//! that allocates either before checking it against the bytes that remain
//! turns a truncated file into an out-of-memory. So every read that would
//! allocate takes a bound and is checked against the file's remaining
//! length first.

use std::io::{Read, Seek, SeekFrom};

/// What a Lucene file can be wrong about.
///
/// Every variant names the file and the offset, because a structural
/// failure an operator cannot locate is one they cannot act on.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum LuceneReadError {
    /// The file ended before the structure did.
    Truncated {
        /// The file.
        file: String,
        /// Where the read started.
        offset: u64,
        /// How many bytes it needed.
        needed: u64,
        /// How many remained.
        remaining: u64,
    },
    /// The first four bytes are not Lucene's codec magic, so this is not a
    /// Lucene 4.x file at all.
    NotALuceneFile {
        /// The file.
        file: String,
        /// Where the magic was expected.
        offset: u64,
        /// What was there instead.
        found: i32,
    },
    /// A Lucene file, but not the kind expected here.
    WrongCodecName {
        /// The file.
        file: String,
        /// Where the name was read.
        offset: u64,
        /// The name this reader wanted.
        expected: String,
        /// The name the file carries.
        found: String,
    },
    /// A version this froe does not read.
    UnsupportedFormatVersion {
        /// The file.
        file: String,
        /// Where the version was read.
        offset: u64,
        /// The codec whose version it is.
        codec: String,
        /// The version the file carries.
        version: i32,
        /// The lowest this reader accepts.
        minimum: i32,
        /// The highest.
        maximum: i32,
    },
    /// A length or count that cannot be right for this file.
    ///
    /// Raised *before* the allocation it would have caused, which is the
    /// whole point of carrying a bound into every read.
    ImplausibleLength {
        /// The file.
        file: String,
        /// Where the length was read.
        offset: u64,
        /// What it said.
        length: u64,
        /// The most that could be valid here.
        bound: u64,
    },
    /// A `VInt` whose continuation bits never ended.
    MalformedVariableInteger {
        /// The file.
        file: String,
        /// Where it started.
        offset: u64,
    },
    /// A string that is not UTF-8, which Lucene's own writer cannot produce.
    NotUtf8 {
        /// The file.
        file: String,
        /// Where the string started.
        offset: u64,
    },
    /// The structure is self-contradictory in a way its own format forbids.
    Malformed {
        /// The file.
        file: String,
        /// Where the contradiction was found.
        offset: u64,
        /// What it is.
        details: String,
    },
    /// Reading the underlying source failed.
    Source {
        /// The file.
        file: String,
        /// What the source said.
        details: String,
    },
}

impl std::fmt::Display for LuceneReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated {
                file,
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "{file} ends early: {needed} bytes were needed at offset {offset} and \
                 {remaining} remain"
            ),
            Self::NotALuceneFile {
                file,
                offset,
                found,
            } => write!(
                formatter,
                "{file} does not open with Lucene's codec magic at offset {offset}: found \
                 {found:#010x}, expected {CODEC_MAGIC_TEXT}"
            ),
            Self::WrongCodecName {
                file,
                offset,
                expected,
                found,
            } => write!(
                formatter,
                "{file} carries codec {found:?} at offset {offset} where {expected:?} was \
                 expected"
            ),
            Self::UnsupportedFormatVersion {
                file,
                offset,
                codec,
                version,
                minimum,
                maximum,
            } => write!(
                formatter,
                "{file} carries {codec} format version {version} at offset {offset}, \
                 outside the {minimum}..={maximum} this froe reads"
            ),
            Self::ImplausibleLength {
                file,
                offset,
                length,
                bound,
            } => write!(
                formatter,
                "{file} declares a length of {length} at offset {offset}, above the \
                 {bound} that could be valid there"
            ),
            Self::MalformedVariableInteger { file, offset } => write!(
                formatter,
                "{file} holds a variable-length integer at offset {offset} whose \
                 continuation never ended"
            ),
            Self::NotUtf8 { file, offset } => write!(
                formatter,
                "{file} holds a string at offset {offset} that is not UTF-8, which \
                 Lucene's own writer cannot produce"
            ),
            Self::Malformed {
                file,
                offset,
                details,
            } => write!(
                formatter,
                "{file} is malformed at offset {offset}: {details}"
            ),
            Self::Source { file, details } => {
                write!(formatter, "reading {file} failed: {details}")
            }
        }
    }
}

/// The magic, rendered for the message above.
const CODEC_MAGIC_TEXT: &str = "0x3fd76c17";

impl std::error::Error for LuceneReadError {}

/// The result of every read here.
pub type LuceneResult<T> = std::result::Result<T, LuceneReadError>;

/// A bounded reader over one Lucene file.
pub struct Reader<Source: Read + Seek> {
    source: Source,
    file_name: String,
    length: u64,
}

impl<Source: Read + Seek> Reader<Source> {
    /// Opens a reader over `source`, whose length is `length`.
    ///
    /// The length is taken rather than measured, because a file inside a
    /// compound file is a slice of a larger one and its bound is the
    /// slice's, not the container's.
    pub fn new(source: Source, file_name: impl Into<String>, length: u64) -> Self {
        Self {
            source,
            file_name: file_name.into(),
            length,
        }
    }

    /// The file this reader is over.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Its length.
    #[must_use]
    pub fn length(&self) -> u64 {
        self.length
    }

    /// The current offset.
    pub fn position(&mut self) -> LuceneResult<u64> {
        self.source
            .stream_position()
            .map_err(|error| self.source_error(&error))
    }

    /// How many bytes remain.
    pub fn remaining(&mut self) -> LuceneResult<u64> {
        let position = self.position()?;
        Ok(self.length.saturating_sub(position))
    }

    /// Seeks to `offset`.
    pub fn seek(&mut self, offset: u64) -> LuceneResult<()> {
        self.source
            .seek(SeekFrom::Start(offset))
            .map(|_| ())
            .map_err(|error| self.source_error(&error))
    }

    /// Reads exactly `count` bytes.
    pub fn read_exact(&mut self, count: usize) -> LuceneResult<Vec<u8>> {
        let offset = self.position()?;
        let remaining = self.remaining()?;
        let needed = count as u64;
        if needed > remaining {
            return Err(LuceneReadError::Truncated {
                file: self.file_name.clone(),
                offset,
                needed,
                remaining,
            });
        }
        let mut bytes = vec![0u8; count];
        self.source
            .read_exact(&mut bytes)
            .map_err(|error| self.source_error(&error))?;
        Ok(bytes)
    }

    /// One byte.
    pub fn read_byte(&mut self) -> LuceneResult<u8> {
        Ok(self.read_exact(1)?[0])
    }

    /// A big-endian 32-bit integer.
    pub fn read_int(&mut self) -> LuceneResult<i32> {
        let bytes = self.read_exact(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// A big-endian 64-bit integer.
    pub fn read_long(&mut self) -> LuceneResult<i64> {
        let bytes = self.read_exact(8)?;
        Ok(i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// A variable-length integer, low group first.
    ///
    /// Five bytes is the most a 32-bit value can occupy; a sixth
    /// continuation byte is a malformed file rather than a larger number.
    pub fn read_variable_int(&mut self) -> LuceneResult<i32> {
        let offset = self.position()?;
        let mut value: u32 = 0;
        for group in 0..5u32 {
            let byte = self.read_byte()?;
            value |= u32::from(byte & 0x7f) << (group * 7);
            if byte & 0x80 == 0 {
                return Ok(value as i32);
            }
        }
        Err(LuceneReadError::MalformedVariableInteger {
            file: self.file_name.clone(),
            offset,
        })
    }

    /// A length-prefixed UTF-8 string, refused above `bound` bytes.
    ///
    /// The bound is checked against the file's own remaining length too, so
    /// a caller that passes a generous bound still cannot be made to
    /// allocate more than the file could hold.
    pub fn read_string(&mut self, bound: usize) -> LuceneResult<String> {
        let offset = self.position()?;
        let length = self.read_variable_int()?;
        let length = u64::try_from(length).map_err(|_| LuceneReadError::ImplausibleLength {
            file: self.file_name.clone(),
            offset,
            length: 0,
            bound: bound as u64,
        })?;
        let remaining = self.remaining()?;
        let effective_bound = (bound as u64).min(remaining);
        if length > effective_bound {
            return Err(LuceneReadError::ImplausibleLength {
                file: self.file_name.clone(),
                offset,
                length,
                bound: effective_bound,
            });
        }
        let bytes = self.read_exact(length as usize)?;
        String::from_utf8(bytes).map_err(|_| LuceneReadError::NotUtf8 {
            file: self.file_name.clone(),
            offset,
        })
    }

    /// An `Int`-counted set of strings.
    ///
    /// The count is checked against the bytes that remain before anything is
    /// reserved: every entry costs at least one byte for its length prefix,
    /// so a count above the remaining length cannot be honest.
    pub fn read_string_set(&mut self, bound: usize) -> LuceneResult<Vec<String>> {
        let count = self.read_counted_length()?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.read_string(bound)?);
        }
        Ok(values)
    }

    /// An `Int`-counted map of string to string, in file order.
    pub fn read_string_map(&mut self, bound: usize) -> LuceneResult<Vec<(String, String)>> {
        let count = self.read_counted_length()?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let key = self.read_string(bound)?;
            let value = self.read_string(bound)?;
            entries.push((key, value));
        }
        Ok(entries)
    }

    /// An `Int` count, refused when negative or beyond what the file could
    /// hold.
    pub fn read_counted_length(&mut self) -> LuceneResult<usize> {
        let offset = self.position()?;
        let count = self.read_int()?;
        let remaining = self.remaining()?;
        let count = u64::try_from(count).map_err(|_| LuceneReadError::ImplausibleLength {
            file: self.file_name.clone(),
            offset,
            length: 0,
            bound: remaining,
        })?;
        // Every entry costs at least one byte, so a count above the bytes
        // that remain cannot be honest — and this is checked before the
        // `with_capacity` it would otherwise drive.
        if count > remaining {
            return Err(LuceneReadError::ImplausibleLength {
                file: self.file_name.clone(),
                offset,
                length: count,
                bound: remaining,
            });
        }
        usize::try_from(count).map_err(|_| LuceneReadError::ImplausibleLength {
            file: self.file_name.clone(),
            offset,
            length: count,
            bound: remaining,
        })
    }

    /// Wraps a source failure with the file it happened on.
    fn source_error(&self, error: &std::io::Error) -> LuceneReadError {
        LuceneReadError::Source {
            file: self.file_name.clone(),
            details: error.to_string(),
        }
    }
}
