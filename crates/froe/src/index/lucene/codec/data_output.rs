//! Lucene's output primitives.
//!
//! `docs/analysis/lucene-4-7-codec.md` §1, from
//! `store/DataOutput.java` and `codecs/CodecUtil.java`.
//!
//! Everything the codec writes goes through these. They are small, and that
//! is the hazard: a variable-length integer written big-endian, or a count
//! written as a `VInt` where the format wants four bytes, produces a file
//! that round-trips through froe's own reader and that Lucene misparses. The
//! vectors in `tests/fixtures/lucene-4-7-primitive-vectors.tsv` come from
//! Lucene's own writers for exactly that reason.

use std::io::Write;

use crate::error::Result;

/// `CodecUtil.CODEC_MAGIC`.
pub const CODEC_MAGIC: i32 = 0x3fd7_6c17;

/// How long a codec header is: `9 + codec.length()`.
///
/// Four bytes of magic, one length byte, the name's bytes, four of version.
/// The single length byte is why a codec name must stay under 128
/// characters — a longer one would take a two-byte `VInt` and every offset
/// derived from this would be wrong.
#[must_use]
pub fn header_length(codec_name: &str) -> usize {
    9 + codec_name.len()
}

/// Writes Lucene's encodings to `output`.
///
/// A thin wrapper rather than an extension trait, so the write methods are
/// namespaced together and a caller cannot reach them on an arbitrary
/// `Write` by accident.
pub struct CodecOutput<Sink: Write> {
    sink: Sink,
    written: u64,
}

impl<Sink: Write> CodecOutput<Sink> {
    /// Wraps `sink`, counting from zero.
    pub const fn new(sink: Sink) -> Self {
        Self { sink, written: 0 }
    }

    /// How many bytes have been written.
    ///
    /// Several formats record a file pointer — `.fdx`'s per-document offset,
    /// the terms directory's start — so the count is part of the writer's
    /// contract rather than a convenience.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.written
    }

    /// The sink, once writing is done.
    pub fn into_inner(self) -> Sink {
        self.sink
    }

    /// One byte.
    pub fn write_byte(&mut self, byte: u8) -> Result<()> {
        self.sink.write_all(&[byte])?;
        self.written += 1;
        Ok(())
    }

    /// Raw bytes.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.sink.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// A fixed four-byte **big-endian** integer.
    pub fn write_int(&mut self, value: i32) -> Result<()> {
        self.write_bytes(&value.to_be_bytes())
    }

    /// A fixed eight-byte **big-endian** long.
    pub fn write_long(&mut self, value: i64) -> Result<()> {
        self.write_bytes(&value.to_be_bytes())
    }

    /// A variable-length integer: seven bits per byte, **little-endian by
    /// groups**, continuation in the high bit.
    ///
    /// The shift is Java's `>>>`, so a negative value is written as five
    /// bytes with its sign surviving into the last group: `-1` is
    /// `FF FF FF FF 0F`. Several formats use a negative value as a
    /// sentinel, so this is a case to preserve rather than to reject.
    pub fn write_vint(&mut self, value: i32) -> Result<()> {
        let mut remaining = value as u32;
        while remaining & !0x7F != 0 {
            self.write_byte(((remaining & 0x7F) | 0x80) as u8)?;
            remaining >>= 7;
        }
        self.write_byte(remaining as u8)
    }

    /// A variable-length long.
    ///
    /// Lucene asserts this non-negative, and the assertion is disabled in a
    /// production JVM — so a negative value is never *rejected* there, and
    /// never written either. froe refuses it, because the only way one
    /// arrives is a caller that meant [`Self::write_vlong_signed`].
    pub fn write_vlong(&mut self, value: i64) -> Result<()> {
        if value < 0 {
            return Err(crate::error::Error::InvalidFormat {
                details: format!(
                    "a Lucene vlong is non-negative and {value} is not; the block-packed \
                     writer's own signed form is the one that accepts a negative value"
                ),
            });
        }
        self.write_vlong_signed(value)
    }

    /// `AbstractBlockPackedWriter.writeVLong`: the same encoding, capped at
    /// nine bytes, tolerating a negative value.
    ///
    /// The block-packed writer carries its own because a block's minimum can
    /// be negative. The cap is what keeps a negative value to nine bytes
    /// rather than ten.
    pub fn write_vlong_signed(&mut self, value: i64) -> Result<()> {
        let mut remaining = value as u64;
        let mut written = 0;
        while remaining & !0x7F != 0 && written < 8 {
            self.write_byte(((remaining & 0x7F) | 0x80) as u8)?;
            remaining >>= 7;
            written += 1;
        }
        self.write_byte(remaining as u8)
    }

    /// A string: a `VInt` **byte** length, then UTF-8.
    pub fn write_string(&mut self, value: &str) -> Result<()> {
        let bytes = value.as_bytes();
        self.write_vint(i32::try_from(bytes.len()).map_err(|_| {
            crate::error::Error::InvalidFormat {
                details: format!("a string of {} bytes does not fit a vint", bytes.len()),
            }
        })?)?;
        self.write_bytes(bytes)
    }

    /// A string-to-string map: a **four-byte** count, then the pairs.
    ///
    /// The count is `writeInt`, not `writeVInt` — the one place in the codec
    /// where a count is fixed-width. A writer that reaches for a `VInt` here
    /// produces a file that parses for counts below 128 and diverges after.
    pub fn write_string_map<'entry>(
        &mut self,
        entries: impl IntoIterator<Item = (&'entry str, &'entry str)>,
    ) -> Result<()> {
        let entries: Vec<(&str, &str)> = entries.into_iter().collect();
        self.write_int(i32::try_from(entries.len()).unwrap_or(i32::MAX))?;
        for (key, value) in entries {
            self.write_string(key)?;
            self.write_string(value)?;
        }
        Ok(())
    }

    /// A string set: a **four-byte** count, then the values.
    ///
    /// Iteration order is the caller's and Lucene does not sort, so where
    /// byte identity with Lucene matters the caller supplies the order the
    /// format's own producer would.
    pub fn write_string_set<'value>(
        &mut self,
        values: impl IntoIterator<Item = &'value str>,
    ) -> Result<()> {
        let values: Vec<&str> = values.into_iter().collect();
        self.write_int(i32::try_from(values.len()).unwrap_or(i32::MAX))?;
        for value in values {
            self.write_string(value)?;
        }
        Ok(())
    }

    /// `CodecUtil.writeHeader`: magic, name, version.
    pub fn write_header(&mut self, codec_name: &str, version: i32) -> Result<()> {
        if !codec_name.is_ascii() || codec_name.len() >= 128 {
            return Err(crate::error::Error::InvalidFormat {
                details: format!(
                    "a codec name is ASCII and under 128 characters; {codec_name:?} is not, \
                     and header_length would be wrong for it"
                ),
            });
        }
        self.write_int(CODEC_MAGIC)?;
        self.write_string(codec_name)?;
        self.write_int(version)
    }
}
