//! Field infos: `.fnm`.
//!
//! `docs/analysis/lucene-4-7-codec.md` §4, from
//! `codecs/lucene46/Lucene46FieldInfosFormat.java` and its writer.
//!
//! One record per field of the segment: its name, its number, a bits byte,
//! the doc-value and norms types packed into a second byte, a generation
//! and an attribute map.

use std::io::Write;

use crate::error::Result;
use crate::index::lucene::codec::data_output::CodecOutput;
use crate::index::lucene::codec::postings::IndexOptions;

/// `Lucene46FieldInfosFormat.CODEC_NAME`.
const CODEC_NAME: &str = "Lucene46FieldInfos";

/// `FORMAT_CURRENT`, which is `FORMAT_START`.
const FORMAT_CURRENT: i32 = 0;

const IS_INDEXED: u8 = 0x1;
const STORE_OFFSETS_IN_POSTINGS: u8 = 0x4;
const OMIT_NORMS: u8 = 0x10;
const OMIT_TERM_FREQ_AND_POSITIONS: u8 = 0x40;
// `STORE_TERM_VECTOR` (0x2) and `STORE_PAYLOADS` (0x20) are the other two
// bits §4.1 lists. froe sets neither: it writes no term vectors, and no Oak
// field carries a payload — §6's writer has no input for one. They are named
// here and not defined, because a constant nothing sets is a constant
// nothing tests.
/// `OMIT_POSITIONS` is Java's `-128`, which is this bit pattern. A reader
/// with an unsigned type compares against `0x80`; `0x8` is unused.
const OMIT_POSITIONS: u8 = 0x80;

/// `FieldInfo.DocValuesType`, whose codes start at 1 because **zero means
/// absent** in the packed byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocValuesType {
    /// One number per document.
    Numeric,
    /// One byte string per document.
    Binary,
    /// A dictionary and one ordinal per document.
    Sorted,
    /// A dictionary and any number of ordinals per document.
    SortedSet,
}

impl DocValuesType {
    /// `Lucene46FieldInfosWriter.docValuesByte`.
    const fn code(self) -> u8 {
        match self {
            Self::Numeric => 1,
            Self::Binary => 2,
            Self::Sorted => 3,
            Self::SortedSet => 4,
        }
    }
}

/// One field of the segment.
///
/// §4.4's reconciliation — index options downgrading to the lesser,
/// `omitNorms` sticky once true, a doc-values type change refused — happens
/// while the segment is built, so what reaches here is already the
/// segment-wide answer.
#[derive(Clone, Debug)]
pub struct FieldInfo {
    /// The field's name.
    pub name: String,
    /// Its number, which every other format addresses it by.
    pub number: i32,
    /// Whether it is indexed at all. **The index-option bits are written
    /// only when it is**, whatever the options say.
    pub indexed: bool,
    /// Its index options, which matter only when `indexed`.
    pub options: IndexOptions,
    /// Whether norms are omitted for it.
    pub omits_norms: bool,
    /// Its doc-values type, if any.
    pub doc_values: Option<DocValuesType>,
    /// Its norms type, which is `NUMERIC` whenever present.
    pub norms: Option<DocValuesType>,
    /// `dvGen`, which is `-1` for a segment whose doc values were never
    /// updated in place.
    pub doc_values_generation: i64,
    /// The attribute map, in the order it will be written.
    pub attributes: Vec<(String, String)>,
}

impl FieldInfo {
    /// The bits byte of §4.2.
    fn bits(&self) -> u8 {
        let mut bits = 0u8;
        if self.omits_norms {
            bits |= OMIT_NORMS;
        }
        if self.indexed {
            bits |= IS_INDEXED;
            // Only three of the four options have a bit;
            // `DOCS_AND_FREQS_AND_POSITIONS` is their absence.
            bits |= match self.options {
                IndexOptions::Documents => OMIT_TERM_FREQ_AND_POSITIONS,
                IndexOptions::DocumentsAndFrequencies => OMIT_POSITIONS,
                IndexOptions::DocumentsAndFrequenciesAndPositions => 0,
                IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets => {
                    STORE_OFFSETS_IN_POSTINGS
                }
            };
        }
        bits
    }

    /// The packed type byte: **norms in the high nibble, doc values in the
    /// low**, with zero for absent.
    fn packed_types(&self) -> u8 {
        let doc_values = self.doc_values.map_or(0, DocValuesType::code);
        let norms = self.norms.map_or(0, DocValuesType::code);
        (norms << 4) | doc_values
    }
}

/// Writes `.fnm`.
pub fn write_field_infos<Sink: Write>(sink: Sink, fields: &[FieldInfo]) -> Result<Sink> {
    let mut output = CodecOutput::new(sink);
    output.write_header(CODEC_NAME, FORMAT_CURRENT)?;
    output.write_vint(fields.len() as i32)?;
    for field in fields {
        output.write_string(&field.name)?;
        output.write_vint(field.number)?;
        output.write_byte(field.bits())?;
        output.write_byte(field.packed_types())?;
        output.write_long(field.doc_values_generation)?;
        output.write_string_map(
            field
                .attributes
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )?;
    }
    Ok(output.into_inner())
}
