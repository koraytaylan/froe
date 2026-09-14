//! The compound file's table of contents.
//!
//! `docs/analysis/index-lucene-storage.md` §8.6 specifies it, from
//! `org/apache/lucene/store/CompoundFileDirectory.java`. The `.cfs` holds
//! the bytes; the `.cfe` beside it says where each sub-file starts.

use std::io::{Read, Seek};

use crate::index::lucene::codec_header::read_codec_header;
use crate::index::lucene::read::{LuceneReadError, LuceneResult, Reader};

/// `CompoundFileWriter.DATA_CODEC`.
pub const COMPOUND_DATA_CODEC_NAME: &str = "CompoundFileWriterData";

/// `CompoundFileWriter.ENTRY_CODEC`.
pub const COMPOUND_ENTRY_CODEC_NAME: &str = "CompoundFileWriterEntries";

/// The only version 4.7.2 writes or reads, for both.
pub const COMPOUND_VERSION: i32 = 0;

/// A generous bound for an entry name.
const NAME_BOUND: usize = 4 * 1024;

/// One sub-file inside a `.cfs`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompoundEntry {
    /// The **segment-stripped** name: `.fdt`, never `_0.fdt`.
    pub name: String,
    /// Where its bytes start in the `.cfs`.
    pub offset: i64,
    /// How many bytes they occupy.
    pub length: i64,
}

/// A parsed `.cfe`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompoundTableOfContents {
    /// The entries, in file order — which is **not** offset order.
    pub entries: Vec<CompoundEntry>,
}

impl CompoundTableOfContents {
    /// Looks one up by a name that may or may not carry the segment prefix.
    ///
    /// `CompoundFileDirectory` looks up by
    /// `IndexFileNames.stripSegmentName(name)`, so a caller holding
    /// `_0.fdt` and a caller holding `.fdt` must reach the same entry.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&CompoundEntry> {
        let stripped = strip_segment_name(name);
        self.entries.iter().find(|entry| entry.name == stripped)
    }
}

/// `IndexFileNames.stripSegmentName`: everything up to and including the
/// segment prefix is cut.
///
/// The prefix is `_` followed by the segment's base-36 name, ending at the
/// first `.` or `_` that begins the rest.
#[must_use]
pub fn strip_segment_name(name: &str) -> &str {
    if !name.starts_with('_') {
        return name;
    }
    // Lucene's own implementation scans for the first `.` or `_` after the
    // leading `_`; whichever comes first begins the stripped remainder.
    let rest = &name[1..];
    match rest.find(['.', '_']) {
        Some(position) => &rest[position..],
        None => name,
    }
}

/// Reads a `.cfe`, given the length of the `.cfs` it describes.
///
/// Every entry is validated against that length before it is returned, so a
/// caller can read any entry without re-checking it.
pub fn read_table_of_contents<Source: Read + Seek>(
    reader: &mut Reader<Source>,
    data_length: i64,
) -> LuceneResult<CompoundTableOfContents> {
    read_codec_header(
        reader,
        COMPOUND_ENTRY_CODEC_NAME,
        COMPOUND_VERSION,
        COMPOUND_VERSION,
    )?;
    let count_offset = reader.position()?;
    let count = reader.read_variable_int()?;
    if count < 0 {
        return Err(LuceneReadError::Malformed {
            file: reader.file_name().to_owned(),
            offset: count_offset,
            details: format!("entry count {count} is negative"),
        });
    }
    // Each entry costs at least 17 bytes — a one-byte length prefix and two
    // eight-byte numbers — so a count above what remains cannot be honest,
    // and this is checked before anything is reserved.
    let remaining = reader.remaining()?;
    let count = u64::from(count.unsigned_abs());
    if count.saturating_mul(17) > remaining {
        return Err(LuceneReadError::ImplausibleLength {
            file: reader.file_name().to_owned(),
            offset: count_offset,
            length: count,
            bound: remaining / 17,
        });
    }

    let mut entries: Vec<CompoundEntry> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name_offset = reader.position()?;
        let name = reader.read_string(NAME_BOUND)?;

        // A name carrying its segment prefix cannot have come from Lucene's
        // own writer, which strips it. Accepting one would let a crafted
        // directory address the same bytes under two names.
        if name.starts_with('_') {
            return Err(LuceneReadError::Malformed {
                file: reader.file_name().to_owned(),
                offset: name_offset,
                details: format!(
                    "entry name {name:?} carries a segment prefix; Lucene's own writer \
                     strips it, and two names for one file is not a namespace froe reads"
                ),
            });
        }
        if entries.iter().any(|entry| entry.name == name) {
            return Err(LuceneReadError::Malformed {
                file: reader.file_name().to_owned(),
                offset: name_offset,
                details: format!("duplicate entry {name:?}"),
            });
        }

        let bounds_offset = reader.position()?;
        let offset = reader.read_long()?;
        let length = reader.read_long()?;
        if offset < 0 || length < 0 || offset.saturating_add(length) > data_length {
            return Err(LuceneReadError::Malformed {
                file: reader.file_name().to_owned(),
                offset: bounds_offset,
                details: format!(
                    "entry {name:?} spans {offset}..{} of a compound file {data_length} \
                     bytes long",
                    offset.saturating_add(length)
                ),
            });
        }
        entries.push(CompoundEntry {
            name,
            offset,
            length,
        });
    }

    Ok(CompoundTableOfContents { entries })
}

/// Reads the `.cfs`'s own header, which every compound file opens with.
pub fn read_compound_data_header<Source: Read + Seek>(
    reader: &mut Reader<Source>,
) -> LuceneResult<()> {
    read_codec_header(
        reader,
        COMPOUND_DATA_CODEC_NAME,
        COMPOUND_VERSION,
        COMPOUND_VERSION,
    )
    .map(|_| ())
}
