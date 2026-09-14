//! The `Lucene41` postings writer: `.doc`, `.pos` and `.pay`.
//!
//! `docs/analysis/lucene-4-7-codec.md` §6, from
//! `codecs/lucene41/Lucene41PostingsWriter.java`,
//! `codecs/lucene41/ForUtil.java`, `codecs/lucene41/Lucene41SkipWriter.java`
//! and `codecs/MultiLevelSkipListWriter.java`.
//!
//! This is the one format `oakCodec` keeps unchanged from `Lucene46`, and
//! the one the terms dictionary (§7, task 0905) drives: the terms writer
//! calls [`PostingsWriter::set_field`] once per field and then, per term,
//! [`PostingsWriter::start_term`] through
//! [`PostingsWriter::finish_term`], and embeds the [`TermMetadata`] it gets
//! back.
//!
//! # Three files, decided once for the segment
//!
//! `.doc` always, `.pos` when **any** field in the segment has positions,
//! `.pay` when any has offsets — Lucene creates all three in its
//! constructor from the completed field infos, so a file exists even when
//! no posting is ever written into it, and a segment whose files were
//! created per field would carry a different set. [`SegmentShape`] is that
//! decision.
//!
//! # No payloads
//!
//! Lucene buffers payload lengths and bytes alongside the positions, and
//! `.pay` exists for payloads *or* offsets. **No Oak field carries a
//! payload**, so froe has no payload input and every payload branch is
//! absent rather than dead: `.pay` here means offsets, and the position
//! delta in the `VInt` tail is written unshifted, which is what a field
//! without payloads gets from Lucene's own writer. §6.5 records both
//! conventions.

use std::io::Write;

use crate::error::{Error, Result};
use crate::index::lucene::codec::data_output::CodecOutput;
use crate::index::lucene::codec::packed::{
    FORMAT_PACKED, PACKED_VERSION_CURRENT, bits_required, write_packed,
};

/// `Lucene41PostingsFormat.BLOCK_SIZE`.
pub const BLOCK_SIZE: usize = 128;

/// `Lucene41PostingsWriter.VERSION_CURRENT` (`VERSION_META_ARRAY`).
pub const VERSION_CURRENT: i32 = 1;

/// `Lucene41PostingsWriter.TERMS_CODEC`, written into `.tim` by §7.
pub const TERMS_CODEC: &str = "Lucene41PostingsWriterTerms";

/// `Lucene41PostingsWriter.DOC_CODEC`.
const DOC_CODEC: &str = "Lucene41PostingsWriterDoc";

/// `Lucene41PostingsWriter.POS_CODEC`.
const POS_CODEC: &str = "Lucene41PostingsWriterPos";

/// `Lucene41PostingsWriter.PAY_CODEC`.
const PAY_CODEC: &str = "Lucene41PostingsWriterPay";

/// `ForUtil.ALL_VALUES_EQUAL`: a bits-per-value of zero, escaping to one
/// `VInt` for the whole block.
const ALL_VALUES_EQUAL: u8 = 0;

/// `Lucene41PostingsFormat.maxSkipLevels`.
const MAX_SKIP_LEVELS: usize = 10;

/// `MultiLevelSkipListWriter`'s multiplier, from
/// `Lucene41SkipWriter`'s `super(blockSize, 8, maxSkipLevels, docCount)`.
const SKIP_MULTIPLIER: usize = 8;

/// `FieldInfo.IndexOptions`, in Lucene's own order — the writer compares
/// them with `compareTo`, so the order is part of the format's meaning.
///
/// Oak's own fields never omit frequencies, but the postings writer's
/// branches all key off these, and §6.2's `VInt` tail changes shape
/// without them, so all four are here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexOptions {
    /// `DOCS_ONLY`.
    Documents,
    /// `DOCS_AND_FREQS`.
    DocumentsAndFrequencies,
    /// `DOCS_AND_FREQS_AND_POSITIONS`.
    DocumentsAndFrequenciesAndPositions,
    /// `DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS`.
    DocumentsAndFrequenciesAndPositionsAndOffsets,
}

impl IndexOptions {
    /// Whether a frequency accompanies each document.
    #[must_use]
    pub fn has_frequencies(self) -> bool {
        self >= Self::DocumentsAndFrequencies
    }

    /// Whether positions accompany each occurrence.
    #[must_use]
    pub fn has_positions(self) -> bool {
        self >= Self::DocumentsAndFrequenciesAndPositions
    }

    /// Whether character offsets accompany each position.
    #[must_use]
    pub fn has_offsets(self) -> bool {
        self == Self::DocumentsAndFrequenciesAndPositionsAndOffsets
    }
}

/// What the segment's completed field infos decide for the whole postings
/// writer: which files exist, and how deep the skip list may climb.
#[derive(Clone, Copy, Debug)]
pub struct SegmentShape {
    /// `SegmentInfo.getDocCount()`. The skip list's level count comes from
    /// this and not from any term's document frequency.
    pub document_count: i32,
    /// `FieldInfos.hasProx()`: some field in the segment has positions.
    pub any_field_has_positions: bool,
    /// `FieldInfos.hasOffsets()`: some field in the segment has offsets.
    pub any_field_has_offsets: bool,
}

/// How many file pointers a term's metadata carries — `longsSize` in the
/// terms dictionary's field directory (§7), which Lucene's own postings
/// reader reads positionally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LongCount(u8);

impl LongCount {
    /// The count, as an index bound.
    #[must_use]
    pub const fn value(self) -> usize {
        self.0 as usize
    }
}

/// What [`PostingsWriter::finish_term`] hands the terms dictionary.
///
/// §6.4 splits this in two when it is written: the three pointers become
/// the term's `longs`, as deltas against the previous term in the block,
/// and the three options become a `VInt`/`VLong` byte stream in that order.
/// **Both halves are the terms writer's to emit**, which is why this is a
/// record of values rather than bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermMetadata {
    /// `docStartFP`.
    pub document_start: u64,
    /// `posStartFP`. Meaningful only for a field with positions.
    pub position_start: u64,
    /// `payStartFP`. Meaningful only for a field with offsets.
    pub payload_start: u64,
    /// `singletonDocID`: the pulsed document of a single-document term,
    /// which wrote nothing at all to `.doc` (§6.3).
    pub singleton_document: Option<i32>,
    /// `lastPosBlockOffset`: where the partial position block begins,
    /// relative to the term's `.pos` start, for a term with more than
    /// [`BLOCK_SIZE`] positions.
    pub last_position_block_offset: Option<u64>,
    /// `skipOffset`: where the skip list begins, relative to the term's
    /// `.doc` start, for a term with more than [`BLOCK_SIZE`] documents.
    pub skip_offset: Option<u64>,
    /// `docFreq`, which the terms dictionary writes.
    pub document_frequency: i32,
    /// `totalTermFreq`, which the terms dictionary writes, and `-1` for a
    /// field without frequencies.
    pub total_term_frequency: i64,
}

/// The three sinks, handed back when the segment's postings are done.
#[derive(Debug)]
pub struct PostingsFiles<Sink> {
    /// `.doc`.
    pub document: Sink,
    /// `.pos`, when the segment has positions.
    pub position: Option<Sink>,
    /// `.pay`, when the segment has offsets.
    pub payload: Option<Sink>,
}

/// `CodecUtil.writeHeader` plus the block size, which
/// `Lucene41PostingsWriter.init` writes at the head of `.tim`.
///
/// It lives here because the constants are §6's; the file is §7's.
pub fn write_terms_header<Sink: Write>(output: &mut CodecOutput<Sink>) -> Result<()> {
    output.write_header(TERMS_CODEC, VERSION_CURRENT)?;
    output.write_vint(BLOCK_SIZE as i32)
}

/// `ForUtil`'s constructor: the packed-integer version, then one `VInt` per
/// bit width saying how a block of that width is packed.
///
/// froe records `PACKED` for every width (§10.3), so each entry is simply
/// the width less one. The encoded byte sizes are **not** written; a reader
/// recomputes them.
fn write_format_table<Sink: Write>(output: &mut CodecOutput<Sink>) -> Result<()> {
    output.write_vint(PACKED_VERSION_CURRENT)?;
    for bits_per_value in 1..=32i32 {
        output.write_vint((FORMAT_PACKED << 5) | (bits_per_value - 1))?;
    }
    Ok(())
}

/// `ForUtil.writeBlock`: one byte of width, then the packed values — or the
/// all-equal escape.
fn write_block<Sink: Write>(output: &mut CodecOutput<Sink>, values: &[i32]) -> Result<()> {
    debug_assert_eq!(values.len(), BLOCK_SIZE);
    let first = values[0];
    if values.iter().all(|value| *value == first) {
        output.write_byte(ALL_VALUES_EQUAL)?;
        return output.write_vint(first);
    }
    // `ForUtil.bitsRequired` ors the whole block and asks for the width of
    // the result, which is the width of the largest value.
    let mut combined = 0u64;
    let mut packed = Vec::with_capacity(BLOCK_SIZE);
    for value in values {
        let value = u64::from(*value as u32);
        combined |= value;
        packed.push(value);
    }
    let bits_per_value = bits_required(combined);
    output.write_byte(bits_per_value as u8)?;
    write_packed(output, &packed, bits_per_value)
}

/// One buffered skip point: the state at the end of a filled document
/// block, which the *next* document's `startDoc` records.
#[derive(Clone, Copy)]
struct SkipPoint {
    document: i32,
    document_pointer: u64,
    position_pointer: u64,
    payload_pointer: u64,
    position_buffer_upto: usize,
}

/// `Lucene41SkipWriter` over `MultiLevelSkipListWriter` (§6.6).
struct SkipWriter {
    level_count: usize,
    has_positions: bool,
    has_offsets: bool,
    buffers: Vec<Vec<u8>>,
    last_document: Vec<i32>,
    last_document_pointer: Vec<u64>,
    last_position_pointer: Vec<u64>,
    last_payload_pointer: Vec<u64>,
}

impl SkipWriter {
    /// `MultiLevelSkipListWriter`'s constructor, whose `df` is the
    /// **segment's** document count.
    fn new(document_count: i32) -> Self {
        let level_count = Self::level_count(document_count);
        Self {
            level_count,
            has_positions: false,
            has_offsets: false,
            buffers: vec![Vec::new(); level_count],
            last_document: vec![0; level_count],
            last_document_pointer: vec![0; level_count],
            last_position_pointer: vec![0; level_count],
            last_payload_pointer: vec![0; level_count],
        }
    }

    /// `1 + MathUtil.log(df / skipInterval, skipMultiplier)`, capped.
    fn level_count(document_count: i32) -> usize {
        if document_count <= BLOCK_SIZE as i32 {
            return 1;
        }
        let mut remaining = document_count as usize / BLOCK_SIZE;
        let mut levels = 1;
        while remaining >= SKIP_MULTIPLIER {
            remaining /= SKIP_MULTIPLIER;
            levels += 1;
        }
        levels.min(MAX_SKIP_LEVELS)
    }

    fn set_field(&mut self, has_positions: bool, has_offsets: bool) {
        self.has_positions = has_positions;
        self.has_offsets = has_offsets;
    }

    /// `resetSkip`: every level's previous document to zero and every
    /// level's previous pointers to where the term starts.
    fn reset(&mut self, document_pointer: u64, position_pointer: u64, payload_pointer: u64) {
        for buffer in &mut self.buffers {
            buffer.clear();
        }
        self.last_document.fill(0);
        self.last_document_pointer.fill(document_pointer);
        if self.has_positions {
            self.last_position_pointer.fill(position_pointer);
            if self.has_offsets {
                self.last_payload_pointer.fill(payload_pointer);
            }
        }
    }

    /// `MultiLevelSkipListWriter.bufferSkip`: how high a point climbs is
    /// how many times eight divides the block number.
    fn buffer_skip(&mut self, document_count: i32, point: &SkipPoint) -> Result<()> {
        debug_assert_eq!(
            document_count as usize % BLOCK_SIZE,
            0,
            "a skip point falls on a block boundary"
        );
        let mut levels = 1usize;
        let mut remaining = document_count as usize / BLOCK_SIZE;
        while remaining.is_multiple_of(SKIP_MULTIPLIER) && levels < self.level_count {
            levels += 1;
            remaining /= SKIP_MULTIPLIER;
        }
        let mut child_pointer = 0u64;
        for level in 0..levels {
            self.write_skip_data(level, point)?;
            let new_child_pointer = self.buffers[level].len() as u64;
            if level != 0 {
                // The offset just past the entry this call wrote one level
                // down, within that level's own buffer; the reader rebases
                // it onto where the level landed in the file.
                let mut output = CodecOutput::new(&mut self.buffers[level]);
                output.write_vlong(child_pointer as i64)?;
            }
            child_pointer = new_child_pointer;
        }
        Ok(())
    }

    /// `Lucene41SkipWriter.writeSkipData`: every delta is against this
    /// level's own previous value, so a level-1 entry spans the eight
    /// level-0 entries beneath it.
    fn write_skip_data(&mut self, level: usize, point: &SkipPoint) -> Result<()> {
        let document_delta = point.document - self.last_document[level];
        self.last_document[level] = point.document;
        let document_pointer_delta = point.document_pointer - self.last_document_pointer[level];
        self.last_document_pointer[level] = point.document_pointer;

        let position_pointer_delta = point.position_pointer - self.last_position_pointer[level];
        let payload_pointer_delta = point.payload_pointer - self.last_payload_pointer[level];
        if self.has_positions {
            self.last_position_pointer[level] = point.position_pointer;
            if self.has_offsets {
                self.last_payload_pointer[level] = point.payload_pointer;
            }
        }

        let has_positions = self.has_positions;
        let has_offsets = self.has_offsets;
        let position_buffer_upto = point.position_buffer_upto as i32;
        let mut output = CodecOutput::new(&mut self.buffers[level]);
        output.write_vint(document_delta)?;
        output.write_vint(document_pointer_delta as i32)?;
        if has_positions {
            output.write_vint(position_pointer_delta as i32)?;
            output.write_vint(position_buffer_upto)?;
            if has_offsets {
                output.write_vint(payload_pointer_delta as i32)?;
            }
        }
        Ok(())
    }

    /// `MultiLevelSkipListWriter.writeSkip`: highest level first, each
    /// above level 0 prefixed by its `VLong` length and omitted entirely
    /// when empty, level 0 last and unprefixed.
    fn write_skip<Sink: Write>(&self, output: &mut CodecOutput<Sink>) -> Result<u64> {
        let pointer = output.position();
        for level in (1..self.level_count).rev() {
            let length = self.buffers[level].len();
            if length > 0 {
                output.write_vlong(length as i64)?;
                output.write_bytes(&self.buffers[level])?;
            }
        }
        output.write_bytes(&self.buffers[0])?;
        Ok(pointer)
    }
}

/// The `Lucene41` postings writer.
///
/// One per segment, driven by the terms dictionary: [`Self::set_field`]
/// once per field, then per term [`Self::start_term`], and per document
/// [`Self::start_document`], [`Self::add_position`] and
/// [`Self::finish_document`], closing with [`Self::finish_term`].
///
/// Nothing larger than one block per file is held: the three buffers are
/// [`BLOCK_SIZE`] entries each, and the skip buffers hold only the points
/// of the term in hand.
pub struct PostingsWriter<Sink: Write> {
    document_output: CodecOutput<Sink>,
    position_output: Option<CodecOutput<Sink>>,
    payload_output: Option<CodecOutput<Sink>>,

    options: IndexOptions,

    document_deltas: Vec<i32>,
    frequencies: Vec<i32>,
    document_buffer_upto: usize,

    position_deltas: Vec<i32>,
    offset_start_deltas: Vec<i32>,
    offset_lengths: Vec<i32>,
    position_buffer_upto: usize,

    last_document: i32,
    last_position: i32,
    last_start_offset: i32,
    document_count: i32,
    frequency_total: i64,
    position_count: i64,

    last_block_document: i32,
    last_block_position_pointer: u64,
    last_block_payload_pointer: u64,
    last_block_position_buffer_upto: usize,

    document_start: u64,
    position_start: u64,
    payload_start: u64,

    skip: SkipWriter,
}

impl<Sink: Write> PostingsWriter<Sink> {
    /// Opens the segment's postings files and writes their headers, and the
    /// format table `.doc` carries directly after its own.
    ///
    /// The sinks must match `shape`: `.pos` exactly when the segment has
    /// positions, `.pay` exactly when it has offsets. A mismatch is
    /// refused rather than silently ignored, because the file set is what
    /// §9's compound file and §3's descriptor both enumerate.
    pub fn new(
        shape: SegmentShape,
        document: Sink,
        position: Option<Sink>,
        payload: Option<Sink>,
    ) -> Result<Self> {
        if shape.any_field_has_positions != position.is_some() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a segment with positions = {} needs a .pos sink and only then; \
                     one was {}given",
                    shape.any_field_has_positions,
                    if position.is_some() { "" } else { "not " }
                ),
            });
        }
        if shape.any_field_has_offsets != payload.is_some() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a segment with offsets = {} needs a .pay sink and only then; \
                     one was {}given",
                    shape.any_field_has_offsets,
                    if payload.is_some() { "" } else { "not " }
                ),
            });
        }
        if shape.any_field_has_offsets && !shape.any_field_has_positions {
            return Err(Error::InvalidFormat {
                details: "offsets without positions is not an index option Lucene has; \
                          .pay is created inside the branch that creates .pos"
                    .to_owned(),
            });
        }

        let mut document_output = CodecOutput::new(document);
        document_output.write_header(DOC_CODEC, VERSION_CURRENT)?;
        write_format_table(&mut document_output)?;

        let position_output = match position {
            Some(sink) => {
                let mut output = CodecOutput::new(sink);
                output.write_header(POS_CODEC, VERSION_CURRENT)?;
                Some(output)
            }
            None => None,
        };
        let payload_output = match payload {
            Some(sink) => {
                let mut output = CodecOutput::new(sink);
                output.write_header(PAY_CODEC, VERSION_CURRENT)?;
                Some(output)
            }
            None => None,
        };

        Ok(Self {
            document_output,
            position_output,
            payload_output,
            options: IndexOptions::Documents,
            document_deltas: vec![0; BLOCK_SIZE],
            frequencies: vec![0; BLOCK_SIZE],
            document_buffer_upto: 0,
            position_deltas: vec![0; BLOCK_SIZE],
            offset_start_deltas: vec![0; BLOCK_SIZE],
            offset_lengths: vec![0; BLOCK_SIZE],
            position_buffer_upto: 0,
            last_document: 0,
            last_position: 0,
            last_start_offset: 0,
            document_count: 0,
            frequency_total: 0,
            position_count: 0,
            last_block_document: -1,
            last_block_position_pointer: 0,
            last_block_payload_pointer: 0,
            last_block_position_buffer_upto: 0,
            document_start: 0,
            position_start: 0,
            payload_start: 0,
            skip: SkipWriter::new(shape.document_count),
        })
    }

    /// `setField`: fixes the field's index options and reports how many
    /// file pointers its terms' metadata will carry.
    pub fn set_field(&mut self, options: IndexOptions) -> Result<LongCount> {
        if options.has_positions() && self.position_output.is_none() {
            return Err(Error::InvalidFormat {
                details: "a field with positions in a segment whose field infos said it had \
                          none: there is no .pos to write into"
                    .to_owned(),
            });
        }
        if options.has_offsets() && self.payload_output.is_none() {
            return Err(Error::InvalidFormat {
                details: "a field with offsets in a segment whose field infos said it had \
                          none: there is no .pay to write into"
                    .to_owned(),
            });
        }
        self.options = options;
        self.skip
            .set_field(options.has_positions(), options.has_offsets());
        Ok(LongCount(if options.has_positions() {
            if options.has_offsets() { 3 } else { 2 }
        } else {
            1
        }))
    }

    /// `startTerm`: latches where this term begins in each file and clears
    /// the skip list.
    ///
    /// Lucene latches the `.pos` and `.pay` pointers only for a field that
    /// has them, leaving the previous field's values in place otherwise;
    /// this latches whenever the file exists. Neither is observable —
    /// `longsSize` keeps the terms writer from reading a pointer a field
    /// does not have — and a stale value in [`TermMetadata`] would be a
    /// trap for a reader of this crate.
    pub fn start_term(&mut self) {
        self.document_start = self.document_output.position();
        if let Some(output) = self.position_output.as_ref() {
            self.position_start = output.position();
        }
        if let Some(output) = self.payload_output.as_ref() {
            self.payload_start = output.position();
        }
        self.last_document = 0;
        self.last_block_document = -1;
        self.skip
            .reset(self.document_start, self.position_start, self.payload_start);
    }

    /// `startDoc`: buffers the previous block's skip point, then the
    /// document's delta and frequency.
    ///
    /// `frequency` is ignored by a field without frequencies, which is what
    /// Lucene does with it — nothing reads `freqBuffer` in that case.
    pub fn start_document(&mut self, document: i32, frequency: i32) -> Result<()> {
        if self.last_block_document != -1 && self.document_buffer_upto == 0 {
            let point = SkipPoint {
                document: self.last_block_document,
                document_pointer: self.document_output.position(),
                position_pointer: self.last_block_position_pointer,
                payload_pointer: self.last_block_payload_pointer,
                position_buffer_upto: self.last_block_position_buffer_upto,
            };
            self.skip.buffer_skip(self.document_count, &point)?;
        }

        let delta = document - self.last_document;
        if document < 0 || (self.document_count > 0 && delta <= 0) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "postings are written in increasing document order and {document} does \
                     not follow {}",
                    self.last_document
                ),
            });
        }
        if self.options.has_frequencies() && frequency < 1 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a document in the postings of a term occurs at least once, so a \
                     frequency of {frequency} cannot be encoded"
                ),
            });
        }

        self.document_deltas[self.document_buffer_upto] = delta;
        if self.options.has_frequencies() {
            self.frequencies[self.document_buffer_upto] = frequency;
            self.frequency_total += i64::from(frequency);
        }
        self.document_buffer_upto += 1;
        self.document_count += 1;

        if self.document_buffer_upto == BLOCK_SIZE {
            write_block(&mut self.document_output, &self.document_deltas)?;
            if self.options.has_frequencies() {
                write_block(&mut self.document_output, &self.frequencies)?;
            }
            // Deliberately *not* clearing the buffer: `finish_document`
            // needs to see that the block filled, so it can latch the skip
            // point's pointers first.
        }

        self.last_document = document;
        self.last_position = 0;
        self.last_start_offset = 0;
        Ok(())
    }

    /// `addPosition`, without the payload: the position delta within the
    /// document, and the offset start delta and length.
    pub fn add_position(
        &mut self,
        position: i32,
        start_offset: i32,
        end_offset: i32,
    ) -> Result<()> {
        if !self.options.has_positions() {
            return Err(Error::InvalidFormat {
                details: "a position in a field whose index options have none".to_owned(),
            });
        }
        let position_delta = position - self.last_position;
        if position_delta < 0 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "positions within a document increase, and {position} does not follow {}",
                    self.last_position
                ),
            });
        }
        self.position_deltas[self.position_buffer_upto] = position_delta;

        if self.options.has_offsets() {
            if start_offset < self.last_start_offset || end_offset < start_offset {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "offsets increase and enclose their term: {start_offset}..{end_offset} \
                         does not follow a start of {}",
                        self.last_start_offset
                    ),
                });
            }
            self.offset_start_deltas[self.position_buffer_upto] =
                start_offset - self.last_start_offset;
            self.offset_lengths[self.position_buffer_upto] = end_offset - start_offset;
            self.last_start_offset = start_offset;
        }

        self.position_buffer_upto += 1;
        self.position_count += 1;
        self.last_position = position;

        if self.position_buffer_upto == BLOCK_SIZE {
            let position_output =
                self.position_output
                    .as_mut()
                    .ok_or_else(|| Error::InvalidFormat {
                        details: "a field with positions but no .pos sink".to_owned(),
                    })?;
            write_block(position_output, &self.position_deltas)?;
            if self.options.has_offsets() {
                let payload_output =
                    self.payload_output
                        .as_mut()
                        .ok_or_else(|| Error::InvalidFormat {
                            details: "a field with offsets but no .pay sink".to_owned(),
                        })?;
                write_block(payload_output, &self.offset_start_deltas)?;
                write_block(payload_output, &self.offset_lengths)?;
            }
            // Cleared here, unlike the document buffer: nothing downstream
            // needs to know the position block just filled.
            self.position_buffer_upto = 0;
        }
        Ok(())
    }

    /// `finishDoc`: latches the skip point's pointers for a filled block
    /// and only then clears the document buffer.
    ///
    /// Skipping this leaves the buffer full, and a term whose document
    /// count is a multiple of [`BLOCK_SIZE`] then writes its last block's
    /// deltas a second time as a `VInt` tail.
    pub fn finish_document(&mut self) {
        if self.document_buffer_upto == BLOCK_SIZE {
            self.last_block_document = self.last_document;
            if let Some(output) = self.position_output.as_ref() {
                if let Some(payload) = self.payload_output.as_ref() {
                    self.last_block_payload_pointer = payload.position();
                }
                self.last_block_position_pointer = output.position();
                self.last_block_position_buffer_upto = self.position_buffer_upto;
            }
            self.document_buffer_upto = 0;
        }
    }

    /// `finishTerm`: the `VInt` tails, the skip list, and the metadata the
    /// terms dictionary embeds.
    pub fn finish_term(&mut self) -> Result<TermMetadata> {
        if self.document_count == 0 {
            return Err(Error::InvalidFormat {
                details: "a term with no documents has no postings to finish".to_owned(),
            });
        }

        let singleton_document = if self.document_count == 1 {
            // §6.3: nothing at all goes to `.doc`; the document id rides in
            // the term's metadata and the frequency is `totalTermFreq`.
            Some(self.document_deltas[0])
        } else {
            self.write_document_tail()?;
            None
        };

        let total_term_frequency = if self.options.has_frequencies() {
            self.frequency_total
        } else {
            -1
        };
        let last_position_block_offset = self.finish_positions(total_term_frequency)?;

        let skip_offset = if self.document_count > BLOCK_SIZE as i32 {
            Some(self.skip.write_skip(&mut self.document_output)? - self.document_start)
        } else {
            None
        };

        let metadata = TermMetadata {
            document_start: self.document_start,
            position_start: self.position_start,
            payload_start: self.payload_start,
            singleton_document,
            last_position_block_offset,
            skip_offset,
            document_frequency: self.document_count,
            total_term_frequency,
        };

        self.document_buffer_upto = 0;
        self.position_buffer_upto = 0;
        self.last_document = 0;
        self.document_count = 0;
        self.frequency_total = 0;
        self.position_count = 0;
        Ok(metadata)
    }

    /// The `VInt` tail of `.doc` (§6.2): the delta and the frequency share
    /// one `VInt` when the field has frequencies.
    fn write_document_tail(&mut self) -> Result<()> {
        for index in 0..self.document_buffer_upto {
            let delta = self.document_deltas[index];
            if !self.options.has_frequencies() {
                self.document_output.write_vint(delta)?;
            } else if self.frequencies[index] == 1 {
                self.document_output.write_vint((delta << 1) | 1)?;
            } else {
                self.document_output.write_vint(delta << 1)?;
                self.document_output.write_vint(self.frequencies[index])?;
            }
        }
        Ok(())
    }

    /// The `.pos` tail (§6.5) and `lastPosBlockOffset`, which is read
    /// before the tail is written.
    fn finish_positions(&mut self, total_term_frequency: i64) -> Result<Option<u64>> {
        if !self.options.has_positions() {
            return Ok(None);
        }
        if self.position_count != total_term_frequency {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a field with positions records one position per occurrence, so \
                     {} positions cannot belong to a term of total frequency \
                     {total_term_frequency}",
                    self.position_count
                ),
            });
        }
        let has_offsets = self.options.has_offsets();
        let position_start = self.position_start;
        let buffer_upto = self.position_buffer_upto;
        let position_deltas = &self.position_deltas;
        let offset_start_deltas = &self.offset_start_deltas;
        let offset_lengths = &self.offset_lengths;
        let output = self
            .position_output
            .as_mut()
            .ok_or_else(|| Error::InvalidFormat {
                details: "a field with positions but no .pos sink".to_owned(),
            })?;

        let last_position_block_offset = if total_term_frequency > BLOCK_SIZE as i64 {
            Some(output.position() - position_start)
        } else {
            None
        };

        // `lastOffsetLength` starts below zero so the first entry of every
        // tail carries its length, and it is local to the tail: the full
        // blocks wrote every length unconditionally.
        let mut last_offset_length = -1i32;
        for index in 0..buffer_upto {
            // Unshifted, because the field has no payloads.
            output.write_vint(position_deltas[index])?;
            if has_offsets {
                let start_delta = offset_start_deltas[index];
                let length = offset_lengths[index];
                if length == last_offset_length {
                    output.write_vint(start_delta << 1)?;
                } else {
                    output.write_vint((start_delta << 1) | 1)?;
                    output.write_vint(length)?;
                    last_offset_length = length;
                }
            }
        }
        Ok(last_position_block_offset)
    }

    /// The three sinks, once the segment's terms are all written.
    ///
    /// Nothing is flushed here: the sinks are handed back so the caller,
    /// which knows whether they are files and what else belongs beside
    /// them, closes them in the order the segment's durability needs.
    pub fn finish(self) -> PostingsFiles<Sink> {
        PostingsFiles {
            document: self.document_output.into_inner(),
            position: self.position_output.map(CodecOutput::into_inner),
            payload: self.payload_output.map(CodecOutput::into_inner),
        }
    }
}
