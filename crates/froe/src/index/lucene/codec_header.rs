//! The codec header every Lucene 4.7.2 file opens with.
//!
//! `docs/analysis/index-lucene-storage.md` §8.2 specifies it, from
//! `org/apache/lucene/codecs/CodecUtil.java`. Nine bytes plus the codec
//! name: a magic word, the name, and a version this reader range-checks.
//!
//! The three failures are distinct types rather than one, because an
//! operator's next move differs for each. A wrong magic means the file is
//! not a Lucene file at all. A wrong name means it is the wrong *kind* of
//! Lucene file — a `.cfe` where a `.si` was expected, say. A version outside
//! the range means this froe does not read it.

use std::io::{Read, Seek};

use crate::index::lucene::read::{LuceneReadError, LuceneResult, Reader};

/// `CodecUtil.CODEC_MAGIC`.
pub const CODEC_MAGIC: i32 = 0x3fd7_6c17;

/// The longest codec name `CodecUtil.writeHeader` will write.
///
/// It refuses a name that is not simple ASCII or is 128 bytes or longer, so
/// a longer one cannot have come from Lucene's own writer.
pub const MAXIMUM_CODEC_NAME_BYTES: usize = 127;

/// A header that was read and validated.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CodecHeader {
    /// The codec name the file carries.
    pub name: String,
    /// The format version the file carries.
    pub version: i32,
}

/// Reads a codec header, requiring `name` and a version within the range.
///
/// The equivalent of `CodecUtil.checkHeader`, which reads the magic and then
/// delegates to `checkHeaderNoMagic` for the name and the version range.
pub fn read_codec_header<Source: Read + Seek>(
    reader: &mut Reader<Source>,
    expected_name: &str,
    minimum_version: i32,
    maximum_version: i32,
) -> LuceneResult<CodecHeader> {
    let offset = reader.position()?;
    let magic = reader.read_int()?;
    if magic != CODEC_MAGIC {
        return Err(LuceneReadError::NotALuceneFile {
            file: reader.file_name().to_owned(),
            offset,
            found: magic,
        });
    }
    read_codec_header_without_magic(reader, expected_name, minimum_version, maximum_version)
}

/// The same without the magic, which the caller has already consumed.
///
/// `SegmentInfos.read` needs this: it reads the first `Int` raw to tell a
/// 4.x commit file from a 3.x one, and only then checks the rest.
pub fn read_codec_header_without_magic<Source: Read + Seek>(
    reader: &mut Reader<Source>,
    expected_name: &str,
    minimum_version: i32,
    maximum_version: i32,
) -> LuceneResult<CodecHeader> {
    let offset = reader.position()?;
    let name = reader.read_string(MAXIMUM_CODEC_NAME_BYTES)?;
    if name != expected_name {
        return Err(LuceneReadError::WrongCodecName {
            file: reader.file_name().to_owned(),
            offset,
            expected: expected_name.to_owned(),
            found: name,
        });
    }
    let version = reader.read_int()?;
    if version < minimum_version || version > maximum_version {
        return Err(LuceneReadError::UnsupportedFormatVersion {
            file: reader.file_name().to_owned(),
            offset,
            codec: name,
            version,
            minimum: minimum_version,
            maximum: maximum_version,
        });
    }
    Ok(CodecHeader { name, version })
}

/// How many bytes a header with this name occupies.
///
/// `CodecUtil.headerLength`: nine bytes plus the name, which holds because
/// the name is simple ASCII and its length prefix is therefore one byte.
#[must_use]
pub fn header_length(codec_name: &str) -> usize {
    9 + codec_name.len()
}
