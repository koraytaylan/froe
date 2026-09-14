//! The segment descriptor, the commit file, and the assembly that produces
//! them.
//!
//! `docs/analysis/lucene-4-7-codec.md` §3, from
//! `codecs/lucene46/Lucene46SegmentInfoWriter.java` and
//! `index/SegmentInfos.java`.
//!
//! This is where a segment becomes an index: `.fnm` goes into the compound
//! file with the rest, the compound file replaces them all in the
//! descriptor's file set, the descriptor adds its own name, and the commit
//! file names the segment and the codec that wrote it.

use std::io::{Read, Write};

use crate::checksum::crc32;
use crate::error::{Error, Result};
use crate::index::lucene::codec::compound::{CompoundFileWriter, write_entry_table};
use crate::index::lucene::codec::data_output::CodecOutput;
use crate::index::lucene::codec::field_infos::{FieldInfo, write_field_infos};

/// `Lucene46SegmentInfoFormat.CODEC_NAME`.
const SEGMENT_INFO_CODEC: &str = "Lucene46SegmentInfo";

/// `VERSION_CURRENT`, which is `VERSION_START`.
const SEGMENT_INFO_VERSION: i32 = 0;

/// `Constants.LUCENE_MAIN_VERSION` for the 4.7 line — **"4.7"**, not
/// "4.7.2". The segment descriptor of the committed sample index Oak wrote
/// carries exactly this string.
pub const LUCENE_VERSION: &str = "4.7";

/// `SegmentInfo.YES`.
const COMPOUND_YES: u8 = 1;

/// `SegmentInfo.NO`, which is `-1` and **not** zero.
const COMPOUND_NO: u8 = 0xff;

/// `IndexFileNames.SEGMENTS`, and the commit file's codec name.
const SEGMENTS: &str = "segments";

/// `SegmentInfos.VERSION_46`.
const COMMIT_VERSION: i32 = 1;

/// `SegmentInfos.FORMAT_SEGMENTS_GEN_CURRENT`.
const FORMAT_SEGMENTS_GEN: i32 = -2;

/// The codec name Oak records for a fulltext-enabled definition.
pub const OAK_CODEC: &str = "oakCodec";

/// The `.si` file's content.
#[derive(Clone, Debug)]
pub struct SegmentDescriptor {
    /// How many documents the segment holds.
    pub document_count: i32,
    /// Whether its files live inside a compound file.
    pub compound: bool,
    /// The diagnostics map, in the order it will be written. Nothing reads
    /// it back for meaning.
    pub diagnostics: Vec<(String, String)>,
    /// The segment's files, by **full** name.
    pub files: Vec<String>,
}

/// One segment of a commit.
#[derive(Clone, Debug)]
pub struct CommittedSegment {
    /// The segment's name, such as `_0`.
    pub name: String,
    /// The codec that wrote it.
    pub codec_name: String,
}

/// Writes `.si`.
pub fn write_segment_info<Sink: Write>(sink: Sink, descriptor: &SegmentDescriptor) -> Result<Sink> {
    let mut output = CodecOutput::new(sink);
    output.write_header(SEGMENT_INFO_CODEC, SEGMENT_INFO_VERSION)?;
    output.write_string(LUCENE_VERSION)?;
    // A fixed four-byte `Int`, not a vint.
    output.write_int(descriptor.document_count)?;
    output.write_byte(if descriptor.compound {
        COMPOUND_YES
    } else {
        COMPOUND_NO
    })?;
    output.write_string_map(
        descriptor
            .diagnostics
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )?;
    output.write_string_set(descriptor.files.iter().map(String::as_str))?;
    Ok(output.into_inner())
}

/// Writes a `segments_N` commit file, checksum and all.
///
/// **Neither `version` nor `counter` is unread.** `version` is the change
/// counter a directory reader compares against its own; `counter` is what
/// an index writer reopening this index consumes to derive its next segment
/// name — `_` plus the counter in radix 36 — so a counter of 0 beside an
/// existing `_0` segment would have Oak's next flush overwrite it.
pub fn write_commit<Sink: Write>(
    sink: Sink,
    version: i64,
    counter: i32,
    segments: &[CommittedSegment],
    user_data: &[(String, String)],
) -> Result<Sink> {
    // Built in memory because the file closes with a CRC32 of everything
    // before it, which is what `ChecksumIndexOutput` appends on close.
    let mut body = Vec::new();
    {
        let mut output = CodecOutput::new(&mut body);
        output.write_header(SEGMENTS, COMMIT_VERSION)?;
        output.write_long(version)?;
        output.write_int(counter)?;
        output.write_int(segments.len() as i32)?;
        for segment in segments {
            output.write_string(&segment.name)?;
            output.write_string(&segment.codec_name)?;
            // No deletions, no field-infos generation, no doc-value updates.
            output.write_long(-1)?;
            output.write_int(0)?;
            output.write_long(-1)?;
            output.write_int(0)?;
        }
        output.write_string_map(
            user_data
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )?;
    }
    let checksum = i64::from(crc32(&body));
    let mut output = CodecOutput::new(sink);
    output.write_bytes(&body)?;
    output.write_long(checksum)?;
    Ok(output.into_inner())
}

/// Writes `segments.gen`: the format, then the generation **twice**, so a
/// torn write is detectable.
///
/// It is a hint. No commit's file set names it, its absence is never a
/// finding, and a reader that disagrees with it falls back to the directory
/// listing.
pub fn write_generation_file<Sink: Write>(sink: Sink, generation: i64) -> Result<Sink> {
    let mut output = CodecOutput::new(sink);
    output.write_int(FORMAT_SEGMENTS_GEN)?;
    output.write_long(generation)?;
    output.write_long(generation)?;
    Ok(output.into_inner())
}

/// Where an assembled index's files go.
///
/// One method rather than an open-and-close pair, so an implementation over
/// real files can create, write, fsync and close in one place — and so a
/// test can collect the bytes without owning a sink the assembly still
/// holds.
pub trait SegmentDirectory {
    /// Writes one file in full under `name`.
    fn write_file(
        &mut self,
        name: &str,
        write: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
    ) -> Result<()>;
}

/// The per-format files a segment's writers produced, in the order they go
/// into the compound file.
///
/// `.fnm` is **not** among them: the assembly writes it, from the same
/// field table the formats were driven with.
pub struct SegmentOutputs<'source> {
    /// The document count, which reaches `.si` and which every format's own
    /// count already agreed with. Lucene's index checker recomputes each
    /// format's count and refuses a segment whose `.si` disagrees.
    pub document_count: i32,
    /// The files, by full name, each as a source to copy from.
    pub files: Vec<(String, Box<dyn Read + 'source>)>,
}

/// Writes one segment's `.cfs`, `.cfe` and `.si` into `directory`.
///
/// The order is the one that makes the descriptor's file set right:
/// `.fnm` is built first and goes into the compound file with the rest, the
/// compound file replaces every inner name with its own two, and the
/// descriptor then adds its own — so `.si` lists exactly three names for a
/// segment whose directory holds three files. A `.si` naming the inner
/// files instead would make Oak's next open delete the compound files as
/// unreferenced.
pub fn assemble_segment(
    directory: &mut impl SegmentDirectory,
    segment: &str,
    fields: &[FieldInfo],
    outputs: SegmentOutputs<'_>,
) -> Result<SegmentDescriptor> {
    let field_infos = write_field_infos(Vec::new(), fields)?;
    let mut sources = outputs.files;
    let mut entries = Vec::new();
    let mut failure = None;
    directory.write_file(&format!("{segment}.cfs"), &mut |sink| {
        let mut writer = CompoundFileWriter::new(sink)?;
        writer.add(&format!("{segment}.fnm"), &mut field_infos.as_slice())?;
        for (name, source) in &mut sources {
            writer.add(name, source)?;
        }
        let (_, written) = writer.finish();
        entries = written;
        Ok(())
    })?;
    directory.write_file(&format!("{segment}.cfe"), &mut |sink| {
        if let Err(error) = write_entry_table(sink, &entries) {
            failure = Some(error);
        }
        Ok(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }

    let descriptor = SegmentDescriptor {
        document_count: outputs.document_count,
        compound: true,
        diagnostics: vec![
            ("source".to_owned(), "froe".to_owned()),
            ("lucene.version".to_owned(), LUCENE_VERSION.to_owned()),
        ],
        // The compound names, and then the descriptor's own — which the
        // Java adds before it writes itself.
        files: vec![
            format!("{segment}.cfs"),
            format!("{segment}.cfe"),
            format!("{segment}.si"),
        ],
    };
    let mut failure = None;
    directory.write_file(&format!("{segment}.si"), &mut |sink| {
        if let Err(error) = write_segment_info(sink, &descriptor) {
            failure = Some(error);
        }
        Ok(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(descriptor)
}

/// Writes `segments_1` and `segments.gen` for a fresh index.
///
/// The counter is the number of segment names consumed — 1 for a single
/// `_0`, and 0 only for a commit with no segment at all, which is what Oak
/// persists for an index whose writer received no document.
pub fn write_commit_files(
    directory: &mut impl SegmentDirectory,
    segments: &[CommittedSegment],
) -> Result<()> {
    let counter = i32::try_from(segments.len()).map_err(|_| Error::InvalidFormat {
        details: format!("{} segments does not fit the counter", segments.len()),
    })?;
    let mut failure = None;
    directory.write_file("segments_1", &mut |sink| {
        if let Err(error) = write_commit(sink, 0, counter, segments, &[]) {
            failure = Some(error);
        }
        Ok(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    directory.write_file("segments.gen", &mut |sink| {
        if let Err(error) = write_generation_file(sink, 1) {
            failure = Some(error);
        }
        Ok(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}
