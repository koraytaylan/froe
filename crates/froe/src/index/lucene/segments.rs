//! The commit file, the per-segment descriptor, and the generation hint.
//!
//! `docs/analysis/index-lucene-storage.md` §8.3, §8.4 and §8.5 specify all
//! three, from `org/apache/lucene/index/SegmentInfos.java` and
//! `org/apache/lucene/codecs/lucene46/Lucene46SegmentInfoReader.java`.

use std::io::{Read, Seek};

use crate::index::lucene::codec_header::{
    CODEC_MAGIC, read_codec_header, read_codec_header_without_magic,
};
use crate::index::lucene::read::{LuceneReadError, LuceneResult, Reader};

/// `SegmentInfos.FORMAT_SEGMENTS_GEN_CURRENT`.
pub const SEGMENTS_GEN_FORMAT: i32 = -2;

/// The codec name `segments_N` carries.
pub const SEGMENTS_CODEC_NAME: &str = "segments";

/// `SegmentInfos.VERSION_40`.
pub const SEGMENTS_VERSION_40: i32 = 0;

/// `SegmentInfos.VERSION_46`, from which the field-infos generation and the
/// generation update files are present.
pub const SEGMENTS_VERSION_46: i32 = 1;

/// The codec name a `.si` carries.
pub const SEGMENT_INFO_CODEC_NAME: &str = "Lucene46SegmentInfo";

/// The only `.si` format version 4.7.2 writes or reads.
pub const SEGMENT_INFO_VERSION: i32 = 0;

/// The name of the generation hint file.
pub const SEGMENTS_GEN_FILE_NAME: &str = "segments.gen";

/// The prefix every commit file's name carries.
pub const SEGMENTS_FILE_PREFIX: &str = "segments";

/// A generous bound for any single string in these files.
///
/// A file name, a codec name, a Lucene version string or a diagnostics
/// value; none is anywhere near this, and the reader additionally clamps
/// every bound to the bytes that remain.
const STRING_BOUND: usize = 32 * 1024;

/// One segment, as `segments_N` and its `.si` describe it together.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SegmentEntry {
    /// The segment's name, such as `_0`.
    pub name: String,
    /// The codec that wrote it, whatever name the definition selected.
    pub codec_name: String,
    /// The deletion generation, `-1` when there are no deletions.
    pub deletion_generation: i64,
    /// How many documents are deleted.
    pub deletion_count: i32,
    /// The field-infos generation, `-1` before format version 46.
    pub field_infos_generation: i64,
    /// The `.si`'s own content.
    pub info: SegmentInfo,
}

impl SegmentEntry {
    /// Documents that are neither deleted nor absent.
    ///
    /// How Oak's own document count over a directory computes it.
    #[must_use]
    pub fn live_document_count(&self) -> i64 {
        i64::from(self.info.document_count) - i64::from(self.deletion_count)
    }

    /// The deletions file this segment's generation names, if any.
    ///
    /// `Lucene40LiveDocsFormat.files` through
    /// `IndexFileNames.fileNameFromGeneration`
    /// (`docs/analysis/index-lucene-storage.md` §8.8): nothing at `-1`,
    /// `<segment>.del` at `0`, and `<segment>_<generation in base 36>.del`
    /// above that. `Character.MAX_RADIX` is 36, the same radix the commit
    /// file's own generation uses.
    #[must_use]
    pub fn deletions_file_name(&self) -> Option<String> {
        match self.deletion_generation {
            // `hasDeletions()` is `delGen != -1`.
            -1 => None,
            0 => Some(format!("{}.{DELETIONS_EXTENSION}", self.name)),
            generation if generation > 0 => Some(format!(
                "{}_{}.{DELETIONS_EXTENSION}",
                self.name,
                to_base_36(generation)
            )),
            // Lucene asserts `gen > 0` on that branch, so any other
            // negative generation is malformed rather than nameable.
            _ => None,
        }
    }
}

/// `Lucene40LiveDocsFormat.DELETES_EXTENSION`.
const DELETIONS_EXTENSION: &str = "del";

/// `Long.toString(value, Character.MAX_RADIX)` for a positive value.
fn to_base_36(mut value: i64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rendered = Vec::new();
    while value > 0 {
        rendered.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    rendered.reverse();
    String::from_utf8(rendered).expect("every digit is ASCII")
}

/// The `.si` file's content.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SegmentInfo {
    /// The Lucene version that wrote the segment.
    pub lucene_version: String,
    /// How many documents it holds, deletions included.
    pub document_count: i32,
    /// Whether its files live inside a compound file.
    pub compound: bool,
    /// The diagnostics map, in file order.
    pub diagnostics: Vec<(String, String)>,
    /// The segment's own files, by full name.
    pub files: Vec<String>,
}

/// A parsed commit file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommitFile {
    /// The commit file's own name.
    pub file_name: String,
    /// Its generation, from the name's suffix.
    pub generation: i64,
    /// The format version it carries.
    pub format_version: i32,
    /// The commit's version counter.
    pub version: i64,
    /// The next segment name to allocate.
    pub counter: i32,
    /// One entry per segment, in file order.
    pub segments: Vec<SegmentEntry>,
    /// The user data map, in file order.
    pub user_data: Vec<(String, String)>,
    /// Files the commit itself names beyond the segments' own — the `.del`
    /// and generation files listed per segment.
    pub generation_files: Vec<String>,
}

impl CommitFile {
    /// Every file this commit refers to, the commit file itself included.
    ///
    /// This is `SegmentCommitInfo.files()`, not `SegmentInfo.files()`: a
    /// segment's deletions file is **derived from its deletion generation**
    /// and named nowhere in the commit file's strings
    /// (`docs/analysis/index-lucene-storage.md` §8.8). A reader that
    /// collected only the strings would call a real Oak index incoherent,
    /// which is exactly what froe's did against the interop fixture.
    #[must_use]
    pub fn referenced_files(&self) -> Vec<String> {
        let mut files = vec![self.file_name.clone()];
        for segment in &self.segments {
            files.extend(segment.info.files.iter().cloned());
            if let Some(deletions) = segment.deletions_file_name() {
                files.push(deletions);
            }
        }
        files.extend(self.generation_files.iter().cloned());
        files.sort();
        files.dedup();
        files
    }

    /// Documents that are neither deleted nor absent, over every segment.
    #[must_use]
    pub fn live_document_count(&self) -> i64 {
        self.segments
            .iter()
            .map(SegmentEntry::live_document_count)
            .sum()
    }
}

/// The generation a `segments_N` name carries.
///
/// `SegmentInfos.generationFromSegmentsFileName`: the bare name `segments`
/// is generation 0, and `segments_N` carries `N` in base 36.
#[must_use]
pub fn generation_from_commit_file_name(name: &str) -> Option<i64> {
    if name == SEGMENTS_FILE_PREFIX {
        return Some(0);
    }
    let suffix = name.strip_prefix("segments_")?;
    i64::from_str_radix(suffix, 36).ok()
}

/// Whether a name is a commit file's.
#[must_use]
pub fn is_commit_file_name(name: &str) -> bool {
    name != SEGMENTS_GEN_FILE_NAME && generation_from_commit_file_name(name).is_some()
}

/// The commit generation, from the listing and the optional hint.
///
/// §8.3: the maximum of the listing's highest suffix and the hint's value,
/// where the hint counts **only when its two copies agree**. The hint's
/// absence, truncation or disagreement is never a fault — Oak writes it
/// best-effort and deletes it on any failure.
#[must_use]
pub fn commit_generation(listing: &[String], hint: Option<i64>) -> Option<i64> {
    let from_listing = listing
        .iter()
        .filter(|name| is_commit_file_name(name))
        .filter_map(|name| generation_from_commit_file_name(name))
        .max();
    match (from_listing, hint) {
        (Some(listed), Some(hinted)) => Some(listed.max(hinted)),
        (Some(listed), None) => Some(listed),
        (None, hinted) => hinted,
    }
}

/// Reads `segments.gen`, returning its generation only when it is coherent.
///
/// Never an error: every way this file can be wrong is one Oak itself
/// tolerates, so a caller gets `None` and carries on with the listing.
pub fn read_segments_gen<Source: Read + Seek>(reader: &mut Reader<Source>) -> Option<i64> {
    let format = reader.read_int().ok()?;
    if format != SEGMENTS_GEN_FORMAT {
        return None;
    }
    let first = reader.read_long().ok()?;
    let second = reader.read_long().ok()?;
    (first == second).then_some(first)
}

/// Reads a `.si`.
///
/// The file must be consumed exactly: `Lucene46SegmentInfoReader.read`
/// requires `getFilePointer() == length()` and refuses otherwise, so a `.si`
/// with trailing bytes is one Lucene itself will not open.
pub fn read_segment_info<Source: Read + Seek>(
    reader: &mut Reader<Source>,
) -> LuceneResult<SegmentInfo> {
    read_codec_header(
        reader,
        SEGMENT_INFO_CODEC_NAME,
        SEGMENT_INFO_VERSION,
        SEGMENT_INFO_VERSION,
    )?;
    let lucene_version = reader.read_string(STRING_BOUND)?;
    let offset = reader.position()?;
    let document_count = reader.read_int()?;
    if document_count < 0 {
        return Err(LuceneReadError::Malformed {
            file: reader.file_name().to_owned(),
            offset,
            details: format!("document count {document_count} is negative"),
        });
    }
    let compound = reader.read_byte()? == 1;
    let diagnostics = reader.read_string_map(STRING_BOUND)?;
    let files = reader.read_string_set(STRING_BOUND)?;

    let position = reader.position()?;
    if position != reader.length() {
        return Err(LuceneReadError::Malformed {
            file: reader.file_name().to_owned(),
            offset: position,
            details: format!(
                "{} trailing bytes; Lucene's own reader requires the file to be consumed \
                 exactly",
                reader.length().saturating_sub(position)
            ),
        });
    }

    Ok(SegmentInfo {
        lucene_version,
        document_count,
        compound,
        diagnostics,
        files,
    })
}

/// Reads a `segments_N`, resolving each segment's `.si` through `open_info`.
///
/// The `.si` is opened **mid-record**, between the codec name and the
/// deletion generation, exactly as `SegmentInfos.read` does. A reader that
/// treated the per-segment record as one run of fields would misparse
/// everything after the first segment.
pub fn read_commit_file<Source, Open, InfoSource>(
    reader: &mut Reader<Source>,
    file_name: &str,
    mut open_info: Open,
) -> LuceneResult<CommitFile>
where
    Source: Read + Seek,
    Open: FnMut(&str) -> LuceneResult<Reader<InfoSource>>,
    InfoSource: Read + Seek,
{
    let generation =
        generation_from_commit_file_name(file_name).ok_or_else(|| LuceneReadError::Malformed {
            file: file_name.to_owned(),
            offset: 0,
            details: "the name carries no commit generation".to_owned(),
        })?;

    // The first `Int` is read raw and compared to the magic, because that is
    // how `SegmentInfos.read` tells a 4.x commit file from a 3.x one.
    let offset = reader.position()?;
    let magic = reader.read_int()?;
    if magic != CODEC_MAGIC {
        return Err(LuceneReadError::Malformed {
            file: file_name.to_owned(),
            offset,
            details: format!(
                "first word {magic:#010x} is not Lucene's codec magic, so this is a \
                 Lucene 3.x commit file this froe does not read"
            ),
        });
    }
    let header = read_codec_header_without_magic(
        reader,
        SEGMENTS_CODEC_NAME,
        SEGMENTS_VERSION_40,
        SEGMENTS_VERSION_46,
    )?;

    let version = reader.read_long()?;
    let counter = reader.read_int()?;
    let count_offset = reader.position()?;
    let segment_count = reader.read_int()?;
    if segment_count < 0 {
        return Err(LuceneReadError::Malformed {
            file: file_name.to_owned(),
            offset: count_offset,
            details: format!("segment count {segment_count} is negative"),
        });
    }

    let mut segments = Vec::new();
    let mut generation_files = Vec::new();
    for _ in 0..segment_count {
        let name = reader.read_string(STRING_BOUND)?;
        let codec_name = reader.read_string(STRING_BOUND)?;

        // Mid-record, as `SegmentInfos.read` does.
        let info_name = format!("{name}.si");
        let mut info_reader = open_info(&info_name)?;
        let info = read_segment_info(&mut info_reader)?;

        let deletion_generation = reader.read_long()?;
        let deletion_offset = reader.position()?;
        let deletion_count = reader.read_int()?;
        if deletion_count < 0 || deletion_count > info.document_count {
            return Err(LuceneReadError::Malformed {
                file: file_name.to_owned(),
                offset: deletion_offset,
                details: format!(
                    "deletion count {deletion_count} is outside 0..={} for segment {name}",
                    info.document_count
                ),
            });
        }

        let mut field_infos_generation = -1;
        if header.version >= SEGMENTS_VERSION_46 {
            field_infos_generation = reader.read_long()?;
            let update_count = reader.read_counted_length()?;
            for _ in 0..update_count {
                let _generation = reader.read_long()?;
                generation_files.extend(reader.read_string_set(STRING_BOUND)?);
            }
        }

        segments.push(SegmentEntry {
            name,
            codec_name,
            deletion_generation,
            deletion_count,
            field_infos_generation,
            info,
        });
    }

    let user_data = reader.read_string_map(STRING_BOUND)?;
    // The trailing checksum is read so the structure is consumed, but its
    // value is not recomputed here: the running checksum Lucene keeps is
    // over every byte read through its own `ChecksumIndexInput`, and froe's
    // structural check reports file-set coherence rather than re-deriving
    // Lucene's checksum.
    let _checksum = reader.read_long()?;

    Ok(CommitFile {
        file_name: file_name.to_owned(),
        generation,
        format_version: header.version,
        version,
        counter,
        segments,
        user_data,
        generation_files,
    })
}
