//! Norms: `.nvd` and `.nvm`.
//!
//! `docs/analysis/lucene-4-7-codec.md` §8.2, from
//! `codecs/lucene42/Lucene42NormsConsumer.java`,
//! `search/similarities/DefaultSimilarity.java` and
//! `util/SmallFloat.java`.
//!
//! One byte per document per field with norms, quantized from the field's
//! boost on that document and its term count.
//!
//! # Which Oak fields have them
//!
//! The analyzed fields Oak builds with norms: `:fulltext`, the
//! relative-node `fullnode:<path>` fields aggregates produce, `:ancestors`,
//! and the out-of-scope `sim:*` and `simtags`. **Not** the norm-omitting
//! analyzed fields it builds for `full:<name>` and `:spellcheck`.
//!
//! # Two formats, four shared names, no shared values
//!
//! This is the **`Lucene42`** format, and its numeric-format codes are not
//! §8.1's: `TABLE_COMPRESSED` is 1 here and 2 there, `GCD_COMPRESSED` is 3
//! here and 1 there, and the block size is 4,096 rather than 16,384. Using
//! one's codes in the other is the mistake this module's constants exist to
//! prevent.

use std::io::Write;

use crate::error::{Error, Result};
use crate::external_sort::{SortedPasses, SpillRecord};
use crate::index::lucene::codec::data_output::CodecOutput;

/// `Lucene42NormsFormat.DATA_CODEC` — **41**, not 42. The format was
/// renamed and the header strings were not; a writer that "corrects" them
/// produces files the reader refuses with `CorruptIndexException`.
const DATA_CODEC: &str = "Lucene41NormsData";

/// `Lucene42NormsFormat.METADATA_CODEC`, renamed the same way.
const META_CODEC: &str = "Lucene41NormsMetadata";

/// `Lucene42NormsConsumer.VERSION_CURRENT` (`VERSION_GCD_COMPRESSION`).
const VERSION_CURRENT: i32 = 1;

/// `Lucene42NormsConsumer.NUMBER`, the only type byte this format has.
const NUMBER: u8 = 0;

/// `Lucene42NormsConsumer.UNCOMPRESSED`. **Not** §8.1's value 0.
const UNCOMPRESSED: u8 = 2;

/// One document's norm byte, in document order.
///
/// The stream is **dense**: one record per document of the segment. A
/// document that does not carry the field takes the byte `0`, which is what
/// Lucene's own norms accumulation pads a missing document with — a stream
/// holding only the documents that carried the field would leave the reader
/// taking the following bytes as norms.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct NormRecord {
    /// The document, which is also the sort key.
    pub document: i32,
    /// The quantized norm, from [`norm_byte`].
    pub norm: u8,
}

impl SpillRecord for NormRecord {
    fn encode(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(&self.document.to_be_bytes());
        buffer.push(self.norm);
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 5 {
            return Err(Error::InvalidFormat {
                details: format!("a norm record is five bytes, not {}", bytes.len()),
            });
        }
        Ok(Self {
            document: i32::from_be_bytes(bytes[..4].try_into().expect("four bytes")),
            norm: bytes[4],
        })
    }

    fn resident_size(&self) -> usize {
        5
    }
}

/// The two files, once every field is written.
#[derive(Debug)]
pub struct NormsFiles<Sink> {
    /// `.nvd`.
    pub data: Sink,
    /// `.nvm`.
    pub metadata: Sink,
}

/// `SmallFloat.floatToByte315`: the eight-bit quantization norms use.
///
/// Three mantissa bits and five exponent bits, taken straight out of the
/// float's raw bits. The two saturating ends are not symmetric: anything at
/// or below the low threshold becomes `0` when the value is zero or
/// negative and `1` otherwise, and anything at or above the high one
/// becomes `-1` — the byte `0xff`, which is what an infinite value gives.
#[must_use]
pub fn float_to_byte_315(value: f32) -> u8 {
    // `(63 - 15) << 3`, the zero point of the five-bit exponent.
    const ZERO_POINT: i32 = (63 - 15) << 3;
    let bits = value.to_bits() as i32;
    let small = bits >> (24 - 3);
    if small <= ZERO_POINT {
        return u8::from(bits > 0);
    }
    if small >= ZERO_POINT + 0x100 {
        return 0xff;
    }
    (small - ZERO_POINT) as u8
}

/// `DefaultSimilarity.lengthNorm` and `encodeNormValue`, together.
///
/// ```text
/// return state.getBoost() * ((float) (1.0 / Math.sqrt(numTerms)));
/// ```
///
/// **The reciprocal square root is computed in double and narrowed to float
/// once**, and only then does the float boost multiply it. A port that
/// works in float throughout takes the square root of a rounded count and
/// rounds the reciprocal a second time, which lands on the other side of a
/// quantization boundary at a term count of 3,081,530 — a length the
/// vectors carry for exactly that reason.
///
/// `term_count` is the field's length on this document **less its
/// overlapping positions**, which the default similarity discounts. A count
/// of zero — a norms-bearing field whose analyzed value yielded no token —
/// makes the reciprocal infinite, which quantizes to `0xff` rather than
/// saturating at the top of the ordinary range.
///
/// # The one input whose answer the *machine* would otherwise decide
///
/// A zero boost on a field with no terms multiplies zero by that infinity,
/// and the product is a NaN. `SmallFloat.floatToByte315` reads its
/// argument through `Float.floatToRawIntBits`, which — unlike
/// `floatToIntBits` — does **not** canonicalize, so the sign bit it sees is
/// whatever the hardware put there: `x86-64` writes the default quiet NaN
/// with the sign **set** (`0xffc00000`) and `AArch64` writes it **clear**
/// (`0x7fc00000`). Through the quantization that is the difference between
/// `0x00` and `0xff` — the two ends of the range — and Java has the same
/// split, so a real Oak on an Apple-silicon machine disagrees with a real
/// Oak on an Intel one about this one input.
///
/// froe answers `0`, the byte the `x86-64` JVM produces: it is the JVM the
/// norm vectors were generated on and the one the interoperability suite
/// compares against, and an index whose bytes depend on the machine that
/// wrote it is not something this port is willing to ship.
#[must_use]
pub fn norm_byte(boost: f32, term_count: u32) -> u8 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the single narrowing to float is the rule, not an approximation of it"
    )]
    let reciprocal = (1.0 / f64::from(term_count).sqrt()) as f32;
    let value = boost * reciprocal;
    if value.is_nan() {
        return 0;
    }
    float_to_byte_315(value)
}

/// The `Lucene42` norms consumer.
///
/// One per segment, one call per field with norms, closing with
/// [`Self::finish`].
pub struct NormsConsumer<Sink: Write> {
    data: CodecOutput<Sink>,
    metadata: CodecOutput<Sink>,
    document_count: i64,
}

impl<Sink: Write> NormsConsumer<Sink> {
    /// Opens both files and writes their headers.
    pub fn new(data: Sink, metadata: Sink, document_count: i64) -> Result<Self> {
        let mut data = CodecOutput::new(data);
        let mut metadata = CodecOutput::new(metadata);
        data.write_header(DATA_CODEC, VERSION_CURRENT)?;
        metadata.write_header(META_CODEC, VERSION_CURRENT)?;
        Ok(Self {
            data,
            metadata,
            document_count,
        })
    }

    /// One field's norms, one byte per document of the segment.
    ///
    /// **Always `UNCOMPRESSED`.** The format asks packed integers for the
    /// fastest decode, which rounds any width up to eight, and a norm byte
    /// already fills eight bits and fits a signed byte — so the branch that
    /// would choose otherwise never fires. The entry is therefore the field
    /// number, the type byte, the data pointer and the format byte, with no
    /// packed-integer version and no block size behind it.
    pub fn add_field(
        &mut self,
        field_number: i32,
        norms: &mut SortedPasses<NormRecord>,
    ) -> Result<()> {
        self.metadata.write_vint(field_number)?;
        self.metadata.write_byte(NUMBER)?;
        self.metadata.write_long(self.data.position() as i64)?;
        self.metadata.write_byte(UNCOMPRESSED)?;

        let mut expected = 0i32;
        for record in norms.pass()? {
            let record = record?;
            if record.document != expected {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "a norms stream is dense and in document order: document {expected} \
                         was expected and {} came",
                        record.document
                    ),
                });
            }
            self.data.write_byte(record.norm)?;
            expected += 1;
        }
        if i64::from(expected) != self.document_count {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a norms stream holds one byte per document: {expected} for a segment of \
                     {} documents",
                    self.document_count
                ),
            });
        }
        Ok(())
    }

    /// Writes `.nvm`'s end-of-fields marker and hands back both files.
    pub fn finish(mut self) -> Result<NormsFiles<Sink>> {
        self.metadata.write_vint(-1)?;
        Ok(NormsFiles {
            data: self.data.into_inner(),
            metadata: self.metadata.into_inner(),
        })
    }
}
