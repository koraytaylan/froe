//! Stored fields: `.fdt` and `.fdx`.
//!
//! `docs/analysis/lucene-4-7-codec.md` §5, from
//! `codecs/lucene40/Lucene40StoredFieldsWriter.java`.
//!
//! `OakCodec` sets this format explicitly — `new Lucene40StoredFieldsFormat()`
//! in its constructor, where `Lucene46Codec`'s own are `Lucene41`'s
//! LZ4-compressed ones (§3.4). It is older than the `Lucene46` two other
//! members of the composition carry, with its own version numbering that
//! restarts at zero, and **uncompressed**, which is the whole reason Oak
//! chooses it.
//!
//! What Oak stores: `:path` on every document, the property text for a
//! `useInExcerpt` property, and for a binary the extracted text under
//! `:fulltext` or a `fullnode:` value.

use std::io::Write;

use crate::error::{Error, Result};
use crate::index::lucene::codec::data_output::{CodecOutput, header_length};

/// `Lucene40StoredFieldsWriter.CODEC_NAME_DAT`.
const CODEC_NAME_DATA: &str = "Lucene40StoredFieldsData";

/// `Lucene40StoredFieldsWriter.CODEC_NAME_IDX`.
const CODEC_NAME_INDEX: &str = "Lucene40StoredFieldsIndex";

/// `VERSION_CURRENT`, which is `VERSION_START` — this format's numbering
/// is its own and starts again at zero.
const VERSION_CURRENT: i32 = 0;

/// `FIELD_IS_BINARY`. Bit 0 is unused, and so is bit 2: the numeric mask
/// begins at bit 3.
const FIELD_IS_BINARY: u8 = 1 << 1;

/// `_NUMERIC_BIT_SHIFT`.
const NUMERIC_BIT_SHIFT: u8 = 3;

/// One stored value.
///
/// A stored field with none of these is an `IllegalArgumentException` in
/// Lucene rather than an empty record, so froe has no variant for it.
/// Codes 5 and 6 — a short and a byte — exist only as comments in the
/// source and are never written; both widen to [`Self::Integer`].
#[derive(Clone, Copy, Debug)]
pub enum StoredValue<'value> {
    /// No bit at all: the case where neither binary nor numeric is set.
    Text(&'value str),
    /// `FIELD_IS_BINARY`: a `VInt` length, then the bytes.
    Binary(&'value [u8]),
    /// `FIELD_IS_NUMERIC_INT`, as a fixed four-byte big-endian value.
    Integer(i32),
    /// `FIELD_IS_NUMERIC_LONG`, as a fixed eight-byte big-endian value.
    Long(i64),
    /// `FIELD_IS_NUMERIC_FLOAT`, as `Float.floatToIntBits` in four bytes.
    Float(f32),
    /// `FIELD_IS_NUMERIC_DOUBLE`, as `Double.doubleToLongBits` in eight.
    Double(f64),
}

impl StoredValue<'_> {
    /// The bits byte this value's type sets.
    const fn bits(self) -> u8 {
        match self {
            Self::Text(_) => 0,
            Self::Binary(_) => FIELD_IS_BINARY,
            Self::Integer(_) => 1 << NUMERIC_BIT_SHIFT,
            Self::Long(_) => 2 << NUMERIC_BIT_SHIFT,
            Self::Float(_) => 3 << NUMERIC_BIT_SHIFT,
            Self::Double(_) => 4 << NUMERIC_BIT_SHIFT,
        }
    }
}

/// The two files, once every document is written.
#[derive(Debug)]
pub struct StoredFieldsFiles<Sink> {
    /// `.fdt`.
    pub data: Sink,
    /// `.fdx`.
    pub index: Sink,
}

/// The `Lucene40` stored-fields writer.
///
/// One per segment, driven document by document in document order:
/// [`Self::start_document`], then [`Self::write_field`] for each stored
/// field, then [`Self::finish_document`].
pub struct StoredFieldsWriter<Sink: Write> {
    data: CodecOutput<Sink>,
    index: CodecOutput<Sink>,
    open_document: Option<usize>,
    written_fields: usize,
    documents: u64,
}

impl<Sink: Write> StoredFieldsWriter<Sink> {
    /// Opens both files and writes their headers.
    pub fn new(data: Sink, index: Sink) -> Result<Self> {
        let mut data = CodecOutput::new(data);
        let mut index = CodecOutput::new(index);
        data.write_header(CODEC_NAME_DATA, VERSION_CURRENT)?;
        index.write_header(CODEC_NAME_INDEX, VERSION_CURRENT)?;
        Ok(Self {
            data,
            index,
            open_document: None,
            written_fields: 0,
            documents: 0,
        })
    }

    /// `startDocument`: the document's `.fdt` offset into `.fdx`, then its
    /// stored-field count into `.fdt`.
    ///
    /// The index entry goes out **before** the data, which is what makes
    /// `.fdx` a fixed eight bytes per document whatever the document
    /// holds.
    pub fn start_document(&mut self, field_count: usize) -> Result<()> {
        if self.open_document.is_some() {
            return Err(Error::InvalidFormat {
                details: "a document is already open; finish it before starting another".to_owned(),
            });
        }
        self.index.write_long(self.data.position() as i64)?;
        self.data.write_vint(
            i32::try_from(field_count).map_err(|_| Error::InvalidFormat {
                details: format!("{field_count} stored fields does not fit a vint"),
            })?,
        )?;
        self.open_document = Some(field_count);
        self.written_fields = 0;
        Ok(())
    }

    /// `writeField`: the field number, the bits byte, then the value.
    pub fn write_field(&mut self, number: i32, value: StoredValue<'_>) -> Result<()> {
        let Some(field_count) = self.open_document else {
            return Err(Error::InvalidFormat {
                details: "a stored field outside any document".to_owned(),
            });
        };
        if self.written_fields == field_count {
            return Err(Error::InvalidFormat {
                details: format!(
                    "the document said it held {field_count} stored fields and this is one \
                     more; the count is written before the fields and cannot be revised"
                ),
            });
        }
        self.data.write_vint(number)?;
        self.data.write_byte(value.bits())?;
        match value {
            StoredValue::Text(text) => self.data.write_string(text)?,
            StoredValue::Binary(bytes) => {
                self.data
                    .write_vint(i32::try_from(bytes.len()).map_err(|_| {
                        Error::InvalidFormat {
                            details: format!("{} bytes does not fit a vint", bytes.len()),
                        }
                    })?)?;
                self.data.write_bytes(bytes)?;
            }
            StoredValue::Integer(value) => self.data.write_int(value)?,
            StoredValue::Long(value) => self.data.write_long(value)?,
            // The raw bits, not text and not a float-specific encoding:
            // §10.5, because a writer that formats these produces a `.fdt`
            // that parses cleanly and yields nonsense.
            StoredValue::Float(value) => self.data.write_int(value.to_bits() as i32)?,
            StoredValue::Double(value) => self.data.write_long(value.to_bits() as i64)?,
        }
        self.written_fields += 1;
        Ok(())
    }

    /// Closes the document's scope.
    ///
    /// **This writes no bytes**: the format has no per-document terminator,
    /// the count at the head of the record being enough. It exists so a
    /// document that wrote fewer fields than it promised is caught here
    /// rather than at the next document's first field — unlike §6's
    /// terminator, which is where a filled block latches its skip state.
    pub fn finish_document(&mut self) -> Result<()> {
        let Some(field_count) = self.open_document.take() else {
            return Err(Error::InvalidFormat {
                details: "no document is open".to_owned(),
            });
        };
        if self.written_fields != field_count {
            return Err(Error::InvalidFormat {
                details: format!(
                    "the document said it held {field_count} stored fields and wrote {}; \
                     the count is written before them and cannot be revised",
                    self.written_fields
                ),
            });
        }
        self.documents += 1;
        Ok(())
    }

    /// `finish`: checks the size invariant and hands back the files.
    ///
    /// `.fdx` is its own header length plus eight bytes per document, and
    /// nothing else. Lucene's comment blames a JRE bug for the check; the
    /// reason to keep it is simpler — it is the one cheap test that catches
    /// a document written without its index entry, or an entry written
    /// twice.
    pub fn finish(self, document_count: u64) -> Result<StoredFieldsFiles<Sink>> {
        if self.open_document.is_some() {
            return Err(Error::InvalidFormat {
                details: "a document is still open".to_owned(),
            });
        }
        let expected = header_length(CODEC_NAME_INDEX) as u64 + document_count * 8;
        if expected != self.index.position() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "fdx size mismatch: {document_count} documents make {expected} bytes and \
                     the file is {}",
                    self.index.position()
                ),
            });
        }
        if document_count != self.documents {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} documents were written and {document_count} were declared",
                    self.documents
                ),
            });
        }
        Ok(StoredFieldsFiles {
            data: self.data.into_inner(),
            index: self.index.into_inner(),
        })
    }
}
