//! Value records: strings and binaries with their length encodings.
//!
//! The first byte of a value record encodes its size class in its high
//! bits:
//!
//! | pattern    | class                                            |
//! |------------|--------------------------------------------------|
//! | `0xxxxxxx` | small value, 0–127 bytes inline                  |
//! | `10xxxxxx` | medium value, 128–16511 bytes inline             |
//! | `110xxxxx` | long value, stored as a list of 4 KiB blocks     |
//! | `1110xxxx` | external binary, short identifier inline         |
//! | `11110xxx` | external binary, identifier in a string record   |
//! | `11111xxx` | invalid                                          |
//!
//! Everything the content tree stores — property values, names, binary
//! data — flows through these encodings. Non-binary values are UTF-8
//! strings; binaries are either inline (in blocks) or references into an
//! external blob store, of which the segment store only knows the
//! identifier.

use std::io;

use crate::content::list::{MAXIMUM_LIST_SIZE, uncounted_list_entries, uncounted_list_entry};
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;

/// Lengths below this fit the one-byte small encoding.
pub const SMALL_VALUE_LIMIT: u64 = 128;

/// Lengths below this fit the two-byte medium encoding.
pub const MEDIUM_VALUE_LIMIT: u64 = (1 << 14) + 128;

/// The size of the block records a long value is split into.
pub const BLOCK_SIZE: u64 = 4096;

/// A binary property value as far as the segment store knows it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BinaryValue {
    /// The binary is stored inline in the segment store.
    Inline {
        /// The binary's length in bytes.
        length: u64,
        /// The value record; content is fetched on demand with
        /// [`read_binary_stream`] or materialized with
        /// [`read_binary_content`].
        record_identifier: RecordIdentifier,
    },
    /// The binary lives in an external blob store.
    External {
        /// The identifier under which the external blob store knows the
        /// binary.
        blob_identifier: String,
    },
}

#[derive(Clone, Copy, Debug)]
enum BinaryStreamSource {
    Direct {
        record_identifier: RecordIdentifier,
        content_offset: usize,
    },
    Blocks {
        list_identifier: RecordIdentifier,
        block_count: u64,
    },
}

/// A bounded-memory reader over an inline binary value.
///
/// The stream borrows its [`SegmentProvider`] and keeps no segment view or
/// content buffer alive between reads. Small and medium values are copied
/// directly from their value record. For a long value, each read resolves
/// only the list entry for the current 4 KiB block; the complete block list
/// and binary are never materialized by the stream itself.
///
/// This type implements [`io::Read`]. Call [`Self::read_chunk`] instead when
/// the caller needs `froe`'s precise [`Error`] variants: `io::Read` preserves
/// non-I/O errors as the inner value of an [`io::Error`], where they remain
/// available through [`io::Error::get_ref`] and downcasting. The stream is
/// `Send` whenever its concrete provider type is `Sync`, and its lifetime
/// prevents it from outliving that provider.
pub struct BinaryStream<
    'provider,
    Provider: SegmentProvider + ?Sized = dyn SegmentProvider + 'provider,
> {
    provider: &'provider Provider,
    source: BinaryStreamSource,
    length: u64,
    position: u64,
    resolved_block: Option<(u64, RecordIdentifier)>,
}

impl<Provider: SegmentProvider + ?Sized> BinaryStream<'_, Provider> {
    fn resolve_block_identifier(
        &mut self,
        list_identifier: RecordIdentifier,
        block_count: u64,
        block_index: u64,
    ) -> Result<RecordIdentifier> {
        if let Some((resolved_index, identifier)) = self.resolved_block
            && resolved_index == block_index
        {
            return Ok(identifier);
        }
        let identifier =
            uncounted_list_entry(self.provider, list_identifier, block_count, block_index)?;
        self.resolved_block = Some((block_index, identifier));
        Ok(identifier)
    }

    fn current_block_identifier(&mut self) -> Result<Option<RecordIdentifier>> {
        if self.position == self.length {
            return Ok(None);
        }
        match self.source {
            BinaryStreamSource::Direct { .. } => Ok(None),
            BinaryStreamSource::Blocks {
                list_identifier,
                block_count,
            } => self
                .resolve_block_identifier(list_identifier, block_count, self.position / BLOCK_SIZE)
                .map(Some),
        }
    }

    /// Returns the total binary length declared by the value record.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.length
    }

    /// Returns whether the binary is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Returns the number of bytes already read from the stream.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    /// Reads content into `buffer`, preserving `froe`'s typed errors.
    ///
    /// A long-value read stops at the current 4 KiB block boundary even when
    /// `buffer` has more room. Repeated calls continue with the next block,
    /// as permitted by [`io::Read`]. An empty buffer or an exhausted stream
    /// returns zero.
    pub fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize> {
        if buffer.is_empty() || self.position == self.length {
            return Ok(0);
        }

        let buffer_capacity = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        let remaining = self.length - self.position;
        let maximum_read = remaining.min(buffer_capacity);

        let read_length = match self.source {
            BinaryStreamSource::Direct {
                record_identifier,
                content_offset,
            } => {
                let read_length =
                    usize::try_from(maximum_read).map_err(|_| Error::InvalidFormat {
                        details: format!(
                            "binary read length does not fit this platform in record \
                             {record_identifier}"
                        ),
                    })?;
                let position =
                    usize::try_from(self.position).map_err(|_| Error::InvalidFormat {
                        details: format!(
                            "binary position does not fit this platform in record \
                             {record_identifier}"
                        ),
                    })?;
                let offset =
                    content_offset
                        .checked_add(position)
                        .ok_or_else(|| Error::InvalidFormat {
                            details: format!(
                                "binary content offset overflows in record {record_identifier}"
                            ),
                        })?;
                let view = self.provider.segment(record_identifier.segment)?;
                buffer[..read_length].copy_from_slice(view.read_bytes(
                    record_identifier.record_number,
                    offset,
                    read_length,
                )?);
                read_length
            }
            BinaryStreamSource::Blocks {
                list_identifier,
                block_count,
            } => {
                let block_index = self.position / BLOCK_SIZE;
                let block_offset = self.position % BLOCK_SIZE;
                let block_remaining = BLOCK_SIZE - block_offset;
                let read_length =
                    usize::try_from(maximum_read.min(block_remaining)).map_err(|_| {
                        Error::InvalidFormat {
                            details: format!(
                                "binary block read length does not fit this platform for list \
                             {list_identifier}"
                            ),
                        }
                    })?;
                let block_identifier =
                    self.resolve_block_identifier(list_identifier, block_count, block_index)?;
                let view = self.provider.segment(block_identifier.segment)?;
                buffer[..read_length].copy_from_slice(view.read_bytes(
                    block_identifier.record_number,
                    usize::try_from(block_offset).map_err(|_| Error::InvalidFormat {
                        details: format!(
                            "binary block offset does not fit this platform in record \
                             {block_identifier}"
                        ),
                    })?,
                    read_length,
                )?);
                read_length
            }
        };
        let read_length_u64 = u64::try_from(read_length).map_err(|_| Error::InvalidFormat {
            details: "binary read length does not fit the 64-bit value format".to_owned(),
        })?;
        self.position += read_length_u64;
        Ok(read_length)
    }
}

impl<Provider: SegmentProvider + ?Sized> io::Read for BinaryStream<'_, Provider> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_chunk(buffer).map_err(|error| match error {
            Error::InputOutput(source) => source,
            other => io::Error::other(other),
        })
    }
}

/// Reads the length of a small, medium, or long value record. Fails on the
/// external binary markers: their length is not stored in the segment.
pub fn read_value_length(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
) -> Result<u64> {
    let view = provider.segment(identifier.segment)?;
    let head = view.read_u8(identifier.record_number, 0)?;
    if head & 0x80 == 0 {
        Ok(u64::from(head))
    } else if head & 0x40 == 0 {
        let stored = view.read_u16(identifier.record_number, 0)?;
        Ok(u64::from(stored & 0x3FFF) + SMALL_VALUE_LIMIT)
    } else if head & 0x20 == 0 {
        let stored = view.read_u64(identifier.record_number, 0)?;
        Ok((stored & 0x1FFF_FFFF_FFFF_FFFF) + MEDIUM_VALUE_LIMIT)
    } else {
        Err(Error::InvalidFormat {
            details: format!(
                "value record {identifier} starts with external binary marker {head:#04x}; \
                 its length is not stored in the segment"
            ),
        })
    }
}

/// Returns the stored UTF-8 byte length of a string record without reading
/// or materializing its content.
pub(crate) fn read_string_stored_length(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
) -> Result<u64> {
    let view = provider.segment(identifier.segment)?;
    let head = view.read_u8(identifier.record_number, 0)?;
    if head & 0x80 == 0 {
        return Ok(u64::from(head));
    }
    if head & 0x40 == 0 {
        return Ok(
            u64::from(view.read_u16(identifier.record_number, 0)? & 0x3fff) + SMALL_VALUE_LIMIT,
        );
    }
    if head & 0xe0 != 0xc0 {
        return Err(Error::InvalidFormat {
            details: format!(
                "record {identifier} starts with binary marker {head:#04x} and is not a string"
            ),
        });
    }
    let stored = view.read_u64(identifier.record_number, 0)?;
    let length = (stored & 0x3fff_ffff_ffff_ffff) + MEDIUM_VALUE_LIMIT;
    if length >= i32::MAX as u64 {
        return Err(Error::InvalidFormat {
            details: format!("string of {length} bytes in record {identifier} is too long"),
        });
    }
    Ok(length)
}

/// Reads one string only after reserving its declared bytes from a cumulative
/// caller-owned materialization budget.
pub(crate) fn read_string_with_stored_byte_budget(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
    maximum_stored_bytes: u64,
    consumed_stored_bytes: &mut u64,
) -> Result<String> {
    let length = read_string_stored_length(provider, identifier)?;
    let attempted_stored_bytes = consumed_stored_bytes.saturating_add(length);
    if attempted_stored_bytes > maximum_stored_bytes {
        return Err(Error::StringMaterializationBudgetExceeded {
            maximum_stored_bytes,
            attempted_stored_bytes,
            value_identifier: identifier,
        });
    }
    *consumed_stored_bytes = attempted_stored_bytes;
    read_string(provider, identifier)
}

/// Reads a string value record and decodes it as UTF-8.
///
/// Invalid UTF-8 sequences become replacement characters, matching the
/// lenient decoding of the Java implementation.
pub fn read_string(provider: &dyn SegmentProvider, identifier: RecordIdentifier) -> Result<String> {
    let view = provider.segment(identifier.segment)?;
    let head = view.read_u8(identifier.record_number, 0)?;
    if head & 0x80 == 0 {
        // Small: the length byte, then the bytes.
        let length = usize::from(head);
        let bytes = view.read_bytes(identifier.record_number, 1, length)?;
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    if head & 0x40 == 0 {
        // Medium: a two-byte length, then the bytes.
        let stored = view.read_u16(identifier.record_number, 0)?;
        let length = usize::from(stored & 0x3FFF) + SMALL_VALUE_LIMIT as usize;
        let bytes = view.read_bytes(identifier.record_number, 2, length)?;
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    if head & 0x20 != 0 {
        return Err(Error::InvalidFormat {
            details: format!(
                "record {identifier} starts with binary marker {head:#04x} and is not a string"
            ),
        });
    }
    // Long: an eight-byte length, then the record identifier of the block
    // list. The Java string reader masks 62 bits here (the blob reader
    // masks 61); both agree for every length a writer can produce.
    let stored = view.read_u64(identifier.record_number, 0)?;
    let length = (stored & 0x3FFF_FFFF_FFFF_FFFF) + MEDIUM_VALUE_LIMIT;
    if length >= i32::MAX as u64 {
        return Err(Error::InvalidFormat {
            details: format!("string of {length} bytes in record {identifier} is too long"),
        });
    }
    let list_identifier = view.read_record_identifier(identifier.record_number, 8, 0)?;
    let bytes = read_block_list(provider, list_identifier, length)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Concatenates the blocks of a long value: `length.div_ceil(4096)` block
/// records of 4096 bytes each, the last one possibly shorter.
fn read_block_list(
    provider: &dyn SegmentProvider,
    list_identifier: RecordIdentifier,
    length: u64,
) -> Result<Vec<u8>> {
    let block_count = length.div_ceil(BLOCK_SIZE);
    let block_identifiers = uncounted_list_entries(provider, list_identifier, block_count)?;
    // Start with a bounded capacity: a corrupt length must not force a
    // huge allocation before the block reads fail.
    let mut content = Vec::with_capacity((length as usize).min(1 << 20));
    let mut remaining = length;
    for block_identifier in block_identifiers {
        let block_length = remaining.min(BLOCK_SIZE) as usize;
        let view = provider.segment(block_identifier.segment)?;
        content.extend_from_slice(view.read_bytes(
            block_identifier.record_number,
            0,
            block_length,
        )?);
        remaining -= block_length as u64;
    }
    Ok(content)
}

/// Classifies a binary value record: inline (with its length) or external
/// (with its blob identifier).
pub fn read_binary_value(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
) -> Result<BinaryValue> {
    let view = provider.segment(identifier.segment)?;
    let head = view.read_u8(identifier.record_number, 0)?;
    if head & 0x80 == 0 || head & 0x40 == 0 || head & 0x20 == 0 {
        // Small, medium, or long inline value.
        return Ok(BinaryValue::Inline {
            length: read_value_length(provider, identifier)?,
            record_identifier: identifier,
        });
    }
    if head & 0x10 == 0 {
        // `1110xxxx`: the identifier length in twelve bits, then the
        // identifier bytes.
        let stored = view.read_u16(identifier.record_number, 0)?;
        let length = usize::from(stored & 0x0FFF);
        let bytes = view.read_bytes(identifier.record_number, 2, length)?;
        return Ok(BinaryValue::External {
            blob_identifier: String::from_utf8_lossy(bytes).into_owned(),
        });
    }
    if head & 0x08 == 0 {
        // `11110xxx`: the record identifier of a string record holding the
        // blob identifier, deliberately unaligned at offset 1.
        let string_identifier = view.read_record_identifier(identifier.record_number, 1, 0)?;
        return Ok(BinaryValue::External {
            blob_identifier: read_string(provider, string_identifier)?,
        });
    }
    Err(Error::InvalidFormat {
        details: format!("unexpected value record marker {head:#04x} in record {identifier}"),
    })
}

/// Opens a bounded-memory stream over an inline binary value.
///
/// The returned stream borrows `provider` and implements [`io::Read`]. Its
/// own memory use is constant regardless of the binary's declared length;
/// caller-provided read buffers determine the amount copied at a time.
/// Requesting a short-identifier external binary fails with
/// [`Error::ExternalBinaryContentUnavailable`]. A long-identifier external
/// binary fails with [`Error::ExternalBinaryContentUnavailableByRecord`]
/// without following or materializing its identifier string record. This
/// keeps opening a stream bounded even when that string record is hostile or
/// unavailable.
pub fn read_binary_stream<Provider: SegmentProvider + ?Sized>(
    provider: &Provider,
    identifier: RecordIdentifier,
) -> Result<BinaryStream<'_, Provider>> {
    let view = provider.segment(identifier.segment)?;
    let head = view.read_u8(identifier.record_number, 0)?;
    let (length, source) = if head & 0x80 == 0 {
        (
            u64::from(head),
            BinaryStreamSource::Direct {
                record_identifier: identifier,
                content_offset: 1,
            },
        )
    } else if head & 0x40 == 0 {
        let stored = view.read_u16(identifier.record_number, 0)?;
        (
            u64::from(stored & 0x3FFF) + SMALL_VALUE_LIMIT,
            BinaryStreamSource::Direct {
                record_identifier: identifier,
                content_offset: 2,
            },
        )
    } else if head & 0x20 == 0 {
        let stored = view.read_u64(identifier.record_number, 0)?;
        let length = (stored & 0x1FFF_FFFF_FFFF_FFFF) + MEDIUM_VALUE_LIMIT;
        let block_count = length.div_ceil(BLOCK_SIZE);
        if block_count > MAXIMUM_LIST_SIZE {
            return Err(Error::InvalidFormat {
                details: format!(
                    "binary of {length} bytes in record {identifier} needs {block_count} blocks, \
                     exceeding the list maximum of {MAXIMUM_LIST_SIZE}"
                ),
            });
        }
        let list_identifier = view.read_record_identifier(identifier.record_number, 8, 0)?;
        (
            length,
            BinaryStreamSource::Blocks {
                list_identifier,
                block_count,
            },
        )
    } else if head & 0x10 == 0 {
        let stored = view.read_u16(identifier.record_number, 0)?;
        let length = usize::from(stored & 0x0FFF);
        let blob_identifier =
            String::from_utf8_lossy(view.read_bytes(identifier.record_number, 2, length)?)
                .into_owned();
        return Err(Error::ExternalBinaryContentUnavailable { blob_identifier });
    } else if head & 0x08 == 0 {
        let string_identifier = view.read_record_identifier(identifier.record_number, 1, 0)?;
        return Err(Error::ExternalBinaryContentUnavailableByRecord {
            value_identifier: identifier,
            blob_identifier_record: string_identifier,
        });
    } else {
        return Err(Error::InvalidFormat {
            details: format!("unexpected value record marker {head:#04x} in record {identifier}"),
        });
    };

    Ok(BinaryStream {
        provider,
        source,
        length,
        position: 0,
        resolved_block: None,
    })
}

/// Reads the full content of an inline binary. Requesting the content of
/// an external binary fails with
/// [`Error::ExternalBinaryContentUnavailable`].
///
/// This compatibility helper materializes the complete value. Prefer
/// [`read_binary_stream`] when binary size is not already known to be small.
/// Unlike that bounded stream opener, this legacy helper resolves a long
/// external identifier so existing callers retain the original error payload.
pub fn read_binary_content(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
) -> Result<Vec<u8>> {
    let mut stream = match read_binary_stream(provider, identifier) {
        Ok(stream) => stream,
        Err(Error::ExternalBinaryContentUnavailableByRecord { .. }) => {
            let BinaryValue::External { blob_identifier } =
                read_binary_value(provider, identifier)?
            else {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "binary value {identifier} changed classification while resolving its \
                         external identifier"
                    ),
                });
            };
            return Err(Error::ExternalBinaryContentUnavailable { blob_identifier });
        }
        Err(error) => return Err(error),
    };
    let mut content = Vec::with_capacity(
        usize::try_from(stream.len())
            .unwrap_or(usize::MAX)
            .min(1 << 20),
    );
    let mut buffer = [0u8; 8192];
    loop {
        let read_length = stream.read_chunk(&mut buffer)?;
        if read_length == 0 {
            return Ok(content);
        }
        content.extend_from_slice(&buffer[..read_length]);
    }
}

/// Resolves and reads every block of an inline binary without keeping the
/// content in memory — for consistency gates, which must survive
/// multi-gigabyte binaries that could never be materialized whole (Oak's
/// checker streams them in 8 KiB chunks for the same reason). External
/// binaries have no local content; long external identifiers are still
/// streamed and structurally validated so consistency checks retain their
/// historical coverage without materializing a hostile identifier.
pub fn verify_binary_content(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
) -> Result<()> {
    let mut stream = match read_binary_stream(provider, identifier) {
        Ok(stream) => stream,
        Err(Error::ExternalBinaryContentUnavailable { .. }) => return Ok(()),
        Err(Error::ExternalBinaryContentUnavailableByRecord {
            blob_identifier_record,
            ..
        }) => return verify_string_content(provider, blob_identifier_record),
        Err(error) => return Err(error),
    };
    let mut buffer = [0u8; 8192];
    loop {
        if stream.read_chunk(&mut buffer)? == 0 {
            return Ok(());
        }
    }
}

fn verify_string_content(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
) -> Result<()> {
    let length = read_string_stored_length(provider, identifier)?;
    let view = provider.segment(identifier.segment)?;
    let head = view.read_u8(identifier.record_number, 0)?;
    if head & 0x80 == 0 {
        view.read_bytes(identifier.record_number, 1, length as usize)?;
        return Ok(());
    }
    if head & 0x40 == 0 {
        view.read_bytes(identifier.record_number, 2, length as usize)?;
        return Ok(());
    }

    let list_identifier = view.read_record_identifier(identifier.record_number, 8, 0)?;
    let block_count = length.div_ceil(BLOCK_SIZE);
    let mut remaining = length;
    for block_index in 0..block_count {
        let block_identifier =
            uncounted_list_entry(provider, list_identifier, block_count, block_index)?;
        let block_length = remaining.min(BLOCK_SIZE) as usize;
        provider.segment(block_identifier.segment)?.read_bytes(
            block_identifier.record_number,
            0,
            block_length,
        )?;
        remaining -= block_length as u64;
    }
    Ok(())
}

/// Compares two inline binaries by content without materializing either,
/// assuming their lengths are already known to be equal. Long values
/// compare block by block: equal lengths mean both block lists chunk at
/// the same 4096-byte boundaries.
pub fn inline_binary_contents_equal(
    provider: &dyn SegmentProvider,
    first: RecordIdentifier,
    second: RecordIdentifier,
    _length: u64,
) -> Result<bool> {
    if first == second {
        return Ok(true);
    }
    let mut first_stream = read_binary_stream(provider, first)?;
    let mut second_stream = read_binary_stream(provider, second)?;

    let mut first_buffer = [0u8; 8192];
    let mut second_buffer = [0u8; 8192];
    loop {
        let first_block = first_stream.current_block_identifier()?;
        let second_block = second_stream.current_block_identifier()?;
        if first_block.is_some() && first_block == second_block {
            // Segment records are immutable. Preserve the existing
            // same-block fast path while resolving identifiers lazily: this
            // avoids both reads and treats two compacted values sharing a
            // block as equal even when that shared segment is unavailable.
            let first_chunk = (first_stream.length - first_stream.position)
                .min(BLOCK_SIZE - first_stream.position % BLOCK_SIZE);
            let second_chunk = (second_stream.length - second_stream.position)
                .min(BLOCK_SIZE - second_stream.position % BLOCK_SIZE);
            let skipped = first_chunk.min(second_chunk);
            first_stream.position += skipped;
            second_stream.position += skipped;
            continue;
        }
        let first_length = first_stream.read_chunk(&mut first_buffer)?;
        let second_length = second_stream.read_chunk(&mut second_buffer)?;
        if first_length != second_length
            || first_buffer[..first_length] != second_buffer[..second_length]
        {
            return Ok(false);
        }
        if first_length == 0 {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Read;
    use std::sync::Arc;

    use super::{
        BLOCK_SIZE, BinaryStream, BinaryValue, MEDIUM_VALUE_LIMIT, SMALL_VALUE_LIMIT,
        inline_binary_contents_equal, read_binary_content, read_binary_stream, read_binary_value,
        read_string, read_value_length, verify_binary_content,
    };
    use crate::content::list::uncounted_list_entry;
    use crate::content::provider::{SegmentProvider, tests::MemorySegmentProvider};
    use crate::content::template::{Template, read_template};
    use crate::error::{Error, Result};
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::parsed_segment::{
        MAXIMUM_SEGMENT_SIZE,
        tests::{bulk_segment_identifier, data_segment_identifier, synthetic_data_segment},
    };
    use crate::segment::record::RecordIdentifier;
    use crate::segment::view::SegmentView;

    fn small_string_record(text: &str) -> Vec<u8> {
        let mut bytes = vec![text.len() as u8];
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    fn direct_binary_record(content: &[u8]) -> Vec<u8> {
        let length = content.len() as u64;
        let mut record = if length < SMALL_VALUE_LIMIT {
            vec![length as u8]
        } else {
            assert!(length < MEDIUM_VALUE_LIMIT);
            ((0x8000u16) | (length as u16 - SMALL_VALUE_LIMIT as u16))
                .to_be_bytes()
                .to_vec()
        };
        record.extend_from_slice(content);
        record
    }

    fn local_record_identifier(record_number: u32) -> Vec<u8> {
        let mut bytes = vec![0, 0];
        bytes.extend_from_slice(&record_number.to_be_bytes());
        bytes
    }

    fn referenced_record_identifier(reference: u16, record_number: u32) -> Vec<u8> {
        let mut bytes = reference.to_be_bytes().to_vec();
        bytes.extend_from_slice(&record_number.to_be_bytes());
        bytes
    }

    fn repeated_local_identifiers(record_number: u32, count: usize) -> Vec<u8> {
        let identifier = local_record_identifier(record_number);
        let mut bytes = Vec::with_capacity(identifier.len() * count);
        for _ in 0..count {
            bytes.extend_from_slice(&identifier);
        }
        bytes
    }

    fn long_binary_record(length: u64, list_record_number: u32) -> Vec<u8> {
        assert!(length >= MEDIUM_VALUE_LIMIT);
        let mut record = ((length - MEDIUM_VALUE_LIMIT) | (0x3 << 62))
            .to_be_bytes()
            .to_vec();
        record.extend_from_slice(&local_record_identifier(list_record_number));
        record
    }

    struct CountingProvider<'provider> {
        inner: &'provider MemorySegmentProvider,
        segment_reads: Cell<usize>,
    }

    impl<'provider> CountingProvider<'provider> {
        fn new(inner: &'provider MemorySegmentProvider) -> Self {
            Self {
                inner,
                segment_reads: Cell::new(0),
            }
        }

        fn segment_reads(&self) -> usize {
            self.segment_reads.get()
        }

        fn reset_segment_reads(&self) {
            self.segment_reads.set(0);
        }
    }

    impl SegmentProvider for CountingProvider<'_> {
        fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
            self.segment_reads.set(self.segment_reads.get() + 1);
            self.inner.segment(identifier)
        }

        fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
            read_string(self, identifier).map(Arc::from)
        }

        fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
            read_template(self, identifier).map(Arc::new)
        }
    }

    #[test]
    fn reads_small_strings() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(0, 4, small_string_record("jcr:content"))]),
        );
        let identifier = RecordIdentifier::new(segment, 0);
        assert_eq!(
            read_value_length(&provider, identifier).expect("length"),
            11
        );
        assert_eq!(
            read_string(&provider, identifier).expect("string"),
            "jcr:content"
        );
    }

    #[test]
    fn reads_empty_and_boundary_small_strings() {
        let segment = data_segment_identifier(1);
        let longest_small = "x".repeat(127);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (0, 4, small_string_record("")),
                    (1, 4, small_string_record(&longest_small)),
                ],
            ),
        );
        assert_eq!(
            read_string(&provider, RecordIdentifier::new(segment, 0)).expect("empty"),
            ""
        );
        assert_eq!(
            read_string(&provider, RecordIdentifier::new(segment, 1)).expect("boundary"),
            longest_small
        );
    }

    #[test]
    fn reads_medium_strings() {
        let segment = data_segment_identifier(1);
        let text = "y".repeat(128);
        let mut record = ((0x8000u16) | (text.len() as u16 - 128))
            .to_be_bytes()
            .to_vec();
        record.extend_from_slice(text.as_bytes());
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &[(0, 4, record)]));
        let identifier = RecordIdentifier::new(segment, 0);
        assert_eq!(
            read_value_length(&provider, identifier).expect("length"),
            128
        );
        assert_eq!(read_string(&provider, identifier).expect("string"), text);
    }

    #[test]
    fn reads_long_strings_from_block_lists() {
        let segment = data_segment_identifier(1);
        let text = "z".repeat(20_000);
        // Five blocks: records 1-5, each 4096 bytes except the last.
        let mut records: Vec<(u32, u8, Vec<u8>)> = Vec::new();
        for (block_index, chunk) in text.as_bytes().chunks(4096).enumerate() {
            records.push((1 + block_index as u32, 5, chunk.to_vec()));
        }
        // Record 10: the bucket listing the five blocks.
        let mut bucket = Vec::new();
        for block_record in 1..=5u32 {
            bucket.extend_from_slice(&[0, 0]);
            bucket.extend_from_slice(&block_record.to_be_bytes());
        }
        records.push((10, 2, bucket));
        // Record 11: the long value head: length and list identifier.
        let mut value = ((text.len() as u64 - 16512) | (0x3 << 62))
            .to_be_bytes()
            .to_vec();
        value.extend_from_slice(&[0, 0]);
        value.extend_from_slice(&10u32.to_be_bytes());
        records.push((11, 4, value));

        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));
        let identifier = RecordIdentifier::new(segment, 11);
        assert_eq!(
            read_value_length(&provider, identifier).expect("length"),
            20_000
        );
        assert_eq!(read_string(&provider, identifier).expect("string"), text);
        assert_eq!(
            read_binary_content(&provider, identifier).expect("content"),
            text.as_bytes()
        );
    }

    #[test]
    fn classifies_external_binaries() {
        let segment = data_segment_identifier(1);
        let blob_identifier = "datastore-reference-0001";
        let mut short_external = ((0xE000u16) | blob_identifier.len() as u16)
            .to_be_bytes()
            .to_vec();
        short_external.extend_from_slice(blob_identifier.as_bytes());

        // Long external: marker byte 0xF0 then the identifier of a string
        // record (record 1) holding the blob identifier.
        let mut long_external = vec![0xF0u8];
        long_external.extend_from_slice(&[0, 0]);
        long_external.extend_from_slice(&1u32.to_be_bytes());
        let truncated_external = vec![0xE0, 20, b'x'];

        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (0, 8, short_external),
                    (1, 4, small_string_record(blob_identifier)),
                    (2, 8, long_external),
                    (3, 8, truncated_external),
                ],
            ),
        );

        for record_number in [0u32, 2] {
            let value = read_binary_value(&provider, RecordIdentifier::new(segment, record_number))
                .expect("binary value");
            assert_eq!(
                value,
                BinaryValue::External {
                    blob_identifier: blob_identifier.to_owned()
                },
                "record {record_number}"
            );
        }

        match read_binary_content(&provider, RecordIdentifier::new(segment, 0)) {
            Err(Error::ExternalBinaryContentUnavailable {
                blob_identifier: reported,
            }) => {
                assert_eq!(reported, blob_identifier);
            }
            other => panic!("expected external binary error, got {other:?}"),
        }
        match read_binary_content(&provider, RecordIdentifier::new(segment, 2)) {
            Err(Error::ExternalBinaryContentUnavailable {
                blob_identifier: reported,
            }) => assert_eq!(reported, blob_identifier),
            other => panic!("expected legacy long-external error, got {other:?}"),
        }
        match read_binary_stream(&provider, RecordIdentifier::new(segment, 0)) {
            Err(Error::ExternalBinaryContentUnavailable {
                blob_identifier: reported,
            }) => assert_eq!(reported, blob_identifier),
            _ => panic!("expected short external binary stream error"),
        }
        match read_binary_stream(&provider, RecordIdentifier::new(segment, 2)) {
            Err(Error::ExternalBinaryContentUnavailableByRecord {
                value_identifier,
                blob_identifier_record,
            }) => {
                assert_eq!(value_identifier, RecordIdentifier::new(segment, 2));
                assert_eq!(blob_identifier_record, RecordIdentifier::new(segment, 1));
            }
            _ => panic!("expected bounded long external binary stream error"),
        }
        for record_number in [0u32, 2] {
            verify_binary_content(&provider, RecordIdentifier::new(segment, record_number))
                .expect("external binaries have no local content to verify");
        }
        assert!(matches!(
            verify_binary_content(&provider, RecordIdentifier::new(segment, 3)),
            Err(Error::InvalidFormat { .. })
        ));
    }

    #[test]
    fn long_external_stream_does_not_follow_the_identifier_record() {
        let value_segment = data_segment_identifier(11);
        let identifier_segment = data_segment_identifier(12);
        let mut long_external = vec![0xF0u8];
        long_external.extend_from_slice(&referenced_record_identifier(1, 7));

        let mut inner = MemorySegmentProvider::default();
        inner.insert(
            value_segment,
            synthetic_data_segment(&[identifier_segment], &[(3, 8, long_external)]),
        );
        inner.insert(
            identifier_segment,
            synthetic_data_segment(
                &[],
                &[(7, 4, small_string_record("identifier-must-not-be-read"))],
            ),
        );
        let provider = CountingProvider::new(&inner);
        let value_identifier = RecordIdentifier::new(value_segment, 3);

        match read_binary_stream(&provider, value_identifier) {
            Err(Error::ExternalBinaryContentUnavailableByRecord {
                value_identifier: reported_value,
                blob_identifier_record,
            }) => {
                assert_eq!(reported_value, value_identifier);
                assert_eq!(
                    blob_identifier_record,
                    RecordIdentifier::new(identifier_segment, 7)
                );
            }
            _ => panic!("expected bounded long-external error"),
        }
        assert_eq!(
            provider.segment_reads(),
            1,
            "opening the stream reads only the value record's segment"
        );
        provider.reset_segment_reads();
        verify_binary_content(&provider, value_identifier)
            .expect("consistency verification validates the identifier record");
        assert!(
            provider.segment_reads() > 1,
            "verification follows the identifier while the stream opener stays bounded"
        );
    }

    #[test]
    fn long_external_verification_reports_missing_or_non_string_identifiers() {
        let value_segment = data_segment_identifier(13);
        let identifier_segment = data_segment_identifier(14);
        let mut long_external = vec![0xF0u8];
        long_external.extend_from_slice(&referenced_record_identifier(1, 7));
        let value_identifier = RecordIdentifier::new(value_segment, 3);

        let mut missing = MemorySegmentProvider::default();
        missing.insert(
            value_segment,
            synthetic_data_segment(&[identifier_segment], &[(3, 8, long_external.clone())]),
        );
        assert!(matches!(
            verify_binary_content(&missing, value_identifier),
            Err(Error::SegmentNotFound { segment_identifier })
                if segment_identifier == identifier_segment
        ));

        let mut nested = MemorySegmentProvider::default();
        nested.insert(
            value_segment,
            synthetic_data_segment(&[identifier_segment], &[(3, 8, long_external)]),
        );
        nested.insert(
            identifier_segment,
            synthetic_data_segment(&[], &[(7, 8, vec![0xe0, 0])]),
        );
        assert!(matches!(
            verify_binary_content(&nested, value_identifier),
            Err(Error::InvalidFormat { .. })
        ));
    }

    #[test]
    fn reads_inline_binaries() {
        let segment = data_segment_identifier(1);
        let content = vec![0x00u8, 0xFF, 0x7F, 0x80];
        let record = direct_binary_record(&content);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &[(0, 4, record)]));
        let identifier = RecordIdentifier::new(segment, 0);
        let value = read_binary_value(&provider, identifier).expect("binary value");
        assert_eq!(
            value,
            BinaryValue::Inline {
                length: 4,
                record_identifier: identifier
            }
        );
        assert_eq!(
            read_binary_content(&provider, identifier).expect("content"),
            content
        );
    }

    #[test]
    fn streams_small_and_medium_binaries_through_io_read() {
        let segment = data_segment_identifier(1);
        let empty = Vec::new();
        let boundary_small: Vec<u8> = (0..127).map(|index| index as u8).collect();
        let first_medium: Vec<u8> = (0..128).map(|index| index as u8).collect();
        let boundary_medium: Vec<u8> = (0..16_511).map(|index| (index % 251) as u8).collect();
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (0, 4, direct_binary_record(&empty)),
                    (1, 4, direct_binary_record(&boundary_small)),
                    (2, 4, direct_binary_record(&first_medium)),
                    (3, 4, direct_binary_record(&boundary_medium)),
                ],
            ),
        );

        for (record_number, expected) in [
            (0, empty.as_slice()),
            (1, boundary_small.as_slice()),
            (2, first_medium.as_slice()),
            (3, boundary_medium.as_slice()),
        ] {
            let mut stream =
                read_binary_stream(&provider, RecordIdentifier::new(segment, record_number))
                    .expect("stream");
            assert_eq!(stream.len(), expected.len() as u64);
            assert_eq!(stream.is_empty(), expected.is_empty());
            assert_eq!(stream.position(), 0);

            let mut actual = Vec::new();
            let mut buffer = [0u8; 37];
            loop {
                let read_length = stream.read(&mut buffer).expect("read");
                if read_length == 0 {
                    break;
                }
                actual.extend_from_slice(&buffer[..read_length]);
            }
            assert_eq!(actual, expected);
            assert_eq!(stream.position(), expected.len() as u64);
            assert_eq!(
                read_binary_content(&provider, RecordIdentifier::new(segment, record_number))
                    .expect("compatibility helper"),
                expected
            );
        }
    }

    #[test]
    fn streams_long_binary_from_partial_bulk_segment_across_block_boundaries() {
        let data_segment = data_segment_identifier(1);
        let bulk_segment = bulk_segment_identifier(2);
        let content: Vec<u8> = (0..20_000).map(|index| (index % 251) as u8).collect();
        let first_virtual_offset = (MAXIMUM_SEGMENT_SIZE - content.len()) as u32;

        let mut block_list = Vec::new();
        for block_offset in (0..content.len()).step_by(BLOCK_SIZE as usize) {
            block_list.extend_from_slice(&referenced_record_identifier(
                1,
                first_virtual_offset + block_offset as u32,
            ));
        }
        let value_record = long_binary_record(content.len() as u64, 10);

        let mut provider = MemorySegmentProvider::default();
        provider.insert(bulk_segment, content.clone());
        provider.insert(
            data_segment,
            synthetic_data_segment(
                &[bulk_segment],
                &[(10, 2, block_list), (11, 4, value_record)],
            ),
        );

        let identifier = RecordIdentifier::new(data_segment, 11);
        let mut stream = read_binary_stream(&provider, identifier).expect("stream");
        let mut first = [0u8; 17];
        assert_eq!(stream.read(&mut first).expect("first bytes"), first.len());
        assert_eq!(&first, &content[..17]);

        let mut through_boundary = [0u8; 5000];
        assert_eq!(
            stream.read(&mut through_boundary).expect("rest of block"),
            BLOCK_SIZE as usize - first.len(),
            "a read stops at the current block boundary"
        );
        assert_eq!(stream.position(), BLOCK_SIZE);

        let mut actual = content[..BLOCK_SIZE as usize].to_vec();
        stream.read_to_end(&mut actual).expect("remaining blocks");
        assert_eq!(actual, content);
        assert_eq!(
            read_binary_content(&provider, identifier).expect("compatibility helper"),
            content
        );
    }

    #[test]
    fn large_declared_binary_resolves_only_the_current_list_branch() {
        let segment = data_segment_identifier(1);
        let block_count = 256u64;
        let length = block_count * BLOCK_SIZE;

        // A 256-entry list has a top bucket (record 11), whose first child
        // is a 255-entry bucket (record 10). All identifiers deliberately
        // reuse one valid block; a one-byte read must still resolve only the
        // current branch instead of materializing the 256 identifiers.
        let mut child_bucket = Vec::new();
        for _ in 0..255 {
            child_bucket.extend_from_slice(&local_record_identifier(1));
        }
        let mut top_bucket = local_record_identifier(10);
        top_bucket.extend_from_slice(&local_record_identifier(2));
        let mut repeated_block = vec![0u8; BLOCK_SIZE as usize];
        repeated_block[0] = 0xAB;
        repeated_block[1] = 0xCD;
        let records = [
            (1, 5, repeated_block),
            (2, 5, vec![0xEF; BLOCK_SIZE as usize]),
            (10, 2, child_bucket),
            (11, 2, top_bucket),
            (12, 4, long_binary_record(length, 11)),
        ];
        let mut inner = MemorySegmentProvider::default();
        inner.insert(segment, synthetic_data_segment(&[], &records));
        let provider = CountingProvider::new(&inner);

        let mut stream =
            read_binary_stream(&provider, RecordIdentifier::new(segment, 12)).expect("stream");
        assert_eq!(stream.len(), length);
        assert_eq!(
            provider.segment_reads(),
            1,
            "opening reads only the value record"
        );

        provider.reset_segment_reads();
        let mut byte = [0u8; 1];
        assert_eq!(stream.read(&mut byte).expect("first byte"), 1);
        assert_eq!(byte, [0xAB]);
        assert_eq!(
            provider.segment_reads(),
            3,
            "one top bucket, one child bucket, and the current block"
        );

        assert_eq!(stream.read(&mut byte).expect("second byte"), 1);
        assert_eq!(byte, [0xCD]);
        assert_eq!(
            provider.segment_reads(),
            4,
            "the one-entry block cache avoids traversing the list again"
        );

        // Finish through the 255-way bucket transition without collecting
        // the one-megabyte value. Blocks 0..=254 reuse record 1; block 255
        // is record 2, so observing its distinct last byte proves the top
        // bucket's pass-through child was selected.
        let mut last_byte = None;
        let mut buffer = [0u8; 8192];
        loop {
            let read_length = stream.read(&mut buffer).expect("remaining binary");
            if read_length == 0 {
                break;
            }
            last_byte = Some(buffer[read_length - 1]);
        }
        assert_eq!(stream.position(), length);
        assert_eq!(last_byte, Some(0xEF));
        let block_count = usize::try_from(block_count).expect("fixture block count fits usize");
        assert_eq!(
            provider.segment_reads(),
            4 + 1 + (block_count - 2) * 3 + 2,
            "blocks 1 through 254 resolve two buckets and one block; the pass-through final \
             child resolves one bucket and one block"
        );
    }

    #[test]
    fn canonical_list_resolver_accepts_concrete_and_erased_providers_at_boundaries() {
        let segment = data_segment_identifier(1);

        // 65,026 is the first size requiring a 65,025-entry top-level
        // bucket. Entry 65,024 takes all three levels; entry 65,025 is the
        // one-element pass-through child of the root.
        let mut first_three_level_root = local_record_identifier(101);
        first_three_level_root.extend_from_slice(&local_record_identifier(2));

        // The exact maximum 255^3 list exercises three full 255-way levels.
        // Reusing child buckets is legal and keeps this boundary fixture
        // sparse without weakening the list arithmetic under test.
        let maximum_size = crate::content::list::MAXIMUM_LIST_SIZE;
        let records = [
            (1, 5, vec![0x11]),
            (2, 5, vec![0x22]),
            (3, 5, vec![0x33]),
            (100, 2, first_three_level_root),
            (101, 2, repeated_local_identifiers(102, 255)),
            (102, 2, repeated_local_identifiers(1, 255)),
            (200, 2, repeated_local_identifiers(201, 255)),
            (201, 2, repeated_local_identifiers(202, 255)),
            (202, 2, repeated_local_identifiers(3, 255)),
        ];
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));

        for (list_record, size, index, expected_record) in [
            (100, 65_026, 65_024, 1),
            (100, 65_026, 65_025, 2),
            (200, maximum_size, 0, 3),
            (200, maximum_size, maximum_size - 1, 3),
        ] {
            let list_identifier = RecordIdentifier::new(segment, list_record);
            let expected = RecordIdentifier::new(segment, expected_record);
            assert_eq!(
                uncounted_list_entry(&provider, list_identifier, size, index)
                    .expect("concrete provider"),
                expected
            );
            let erased: &dyn SegmentProvider = &provider;
            assert_eq!(
                uncounted_list_entry(erased, list_identifier, size, index)
                    .expect("erased provider"),
                expected,
                "generic and erased traversal differ at size {size}, index {index}"
            );
        }
    }

    #[test]
    fn binary_verification_streams_a_large_list_and_reports_a_truncated_second_block() {
        let segment = data_segment_identifier(1);
        let block_count = 65_026u64;
        let length = block_count * BLOCK_SIZE;

        // The first 65,025 entries use three bucket levels. Entry zero is a
        // complete block and every later entry in its leaf is the truncated
        // final record. A streaming verifier reaches that failure after two
        // blocks; eager list materialization would resolve all 65,026 block
        // identifiers first.
        let mut root = local_record_identifier(101);
        root.extend_from_slice(&local_record_identifier(900));
        let mut leaf = local_record_identifier(800);
        leaf.extend_from_slice(&repeated_local_identifiers(900, 254));
        let records = [
            (10, 4, long_binary_record(length, 100)),
            (100, 2, root),
            (101, 2, repeated_local_identifiers(102, 255)),
            (102, 2, leaf),
            (800, 5, vec![0x11; BLOCK_SIZE as usize]),
            (900, 5, vec![0x22; BLOCK_SIZE as usize - 4]),
        ];
        let mut inner = MemorySegmentProvider::default();
        inner.insert(segment, synthetic_data_segment(&[], &records));
        let provider = CountingProvider::new(&inner);

        let error = verify_binary_content(&provider, RecordIdentifier::new(segment, 10))
            .expect_err("truncated second block");
        assert!(matches!(error, Error::InvalidFormat { .. }));
        assert_eq!(
            provider.segment_reads(),
            9,
            "one value head plus three list levels and one block per streamed chunk"
        );
    }

    #[test]
    fn binary_comparison_streams_large_lists_across_a_boundary_before_truncation() {
        let segment = data_segment_identifier(1);
        let block_count = 65_026u64;
        let length = block_count * BLOCK_SIZE;

        let mut first_root = local_record_identifier(101);
        first_root.extend_from_slice(&local_record_identifier(900));
        let mut first_leaf = local_record_identifier(800);
        first_leaf.extend_from_slice(&repeated_local_identifiers(801, 254));
        let mut second_root = local_record_identifier(201);
        second_root.extend_from_slice(&local_record_identifier(900));
        let mut second_leaf = local_record_identifier(802);
        second_leaf.extend_from_slice(&repeated_local_identifiers(900, 254));
        let records = [
            (10, 4, long_binary_record(length, 100)),
            (11, 4, long_binary_record(length, 200)),
            (100, 2, first_root),
            (101, 2, repeated_local_identifiers(102, 255)),
            (102, 2, first_leaf),
            (200, 2, second_root),
            (201, 2, repeated_local_identifiers(202, 255)),
            (202, 2, second_leaf),
            (800, 5, vec![0x33; BLOCK_SIZE as usize]),
            (801, 5, vec![0x44; BLOCK_SIZE as usize]),
            (802, 5, vec![0x33; BLOCK_SIZE as usize]),
            (900, 5, vec![0x44; BLOCK_SIZE as usize - 4]),
        ];
        let mut inner = MemorySegmentProvider::default();
        inner.insert(segment, synthetic_data_segment(&[], &records));
        let provider = CountingProvider::new(&inner);

        let error = inline_binary_contents_equal(
            &provider,
            RecordIdentifier::new(segment, 10),
            RecordIdentifier::new(segment, 11),
            length,
        )
        .expect_err("second stream has a truncated second block");
        assert!(matches!(error, Error::InvalidFormat { .. }));
        assert_eq!(
            provider.segment_reads(),
            18,
            "two value heads plus two three-level list traversals and block reads per chunk"
        );
    }

    #[test]
    fn binary_comparison_preserves_the_shared_block_identifier_fast_path() {
        let segment = data_segment_identifier(1);
        let length = MEDIUM_VALUE_LIMIT;
        let shared_missing_blocks = repeated_local_identifiers(999, 5);
        let records = [
            (10, 4, long_binary_record(length, 20)),
            (11, 4, long_binary_record(length, 21)),
            (20, 2, shared_missing_blocks.clone()),
            (21, 2, shared_missing_blocks),
        ];
        let mut inner = MemorySegmentProvider::default();
        inner.insert(segment, synthetic_data_segment(&[], &records));
        let provider = CountingProvider::new(&inner);

        assert!(
            inline_binary_contents_equal(
                &provider,
                RecordIdentifier::new(segment, 10),
                RecordIdentifier::new(segment, 11),
                length,
            )
            .expect("immutable shared block identifiers are content-equal")
        );
        assert_eq!(
            provider.segment_reads(),
            12,
            "the two heads and two lazy list lookups per block are read, but shared blocks are not"
        );
    }

    #[test]
    fn stream_preserves_corrupt_and_missing_record_errors() {
        let segment = data_segment_identifier(1);
        let truncated_direct = direct_binary_record(&[1, 2, 3, 4]);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(0, 4, truncated_direct[..3].to_vec())]),
        );
        let mut stream =
            read_binary_stream(&provider, RecordIdentifier::new(segment, 0)).expect("stream");
        let mut content = [0u8; 4];
        let error = stream
            .read_chunk(&mut content)
            .expect_err("truncated value");
        assert!(matches!(error, Error::InvalidFormat { .. }));
        assert_eq!(stream.position(), 0, "failed reads do not advance");
        assert!(matches!(
            read_binary_content(&provider, RecordIdentifier::new(segment, 0)),
            Err(Error::InvalidFormat { .. })
        ));

        let mut stream =
            read_binary_stream(&provider, RecordIdentifier::new(segment, 0)).expect("stream");
        let error = stream.read(&mut content).expect_err("io error");
        assert!(matches!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<Error>()),
            Some(Error::InvalidFormat { .. })
        ));
    }

    #[test]
    fn stream_reports_missing_long_block_after_completed_prefix() {
        let segment = data_segment_identifier(1);
        let length = MEDIUM_VALUE_LIMIT;
        let mut block_list = Vec::new();
        for record_number in [1u32, 2, 3, 4, 99] {
            block_list.extend_from_slice(&local_record_identifier(record_number));
        }
        let records = [
            (1, 5, vec![1; BLOCK_SIZE as usize]),
            (2, 5, vec![2; BLOCK_SIZE as usize]),
            (3, 5, vec![3; BLOCK_SIZE as usize]),
            (4, 5, vec![4; BLOCK_SIZE as usize]),
            (10, 2, block_list),
            (11, 4, long_binary_record(length, 10)),
        ];
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));
        let mut stream =
            read_binary_stream(&provider, RecordIdentifier::new(segment, 11)).expect("stream");
        let mut block = [0u8; BLOCK_SIZE as usize];
        for expected in 1..=4 {
            assert_eq!(
                stream.read_chunk(&mut block).expect("present block"),
                block.len()
            );
            assert!(block.iter().all(|&byte| byte == expected));
        }
        let error = stream
            .read_chunk(&mut block)
            .expect_err("missing fifth block");
        assert!(matches!(error, Error::InvalidFormat { .. }));
        assert_eq!(stream.position(), 4 * BLOCK_SIZE);
    }

    #[test]
    fn stream_rejects_a_truncated_block_list_bucket() {
        let segment = data_segment_identifier(1);
        let length = MEDIUM_VALUE_LIMIT;
        let mut truncated_block_list = Vec::new();
        for record_number in 1..=4u32 {
            truncated_block_list.extend_from_slice(&local_record_identifier(record_number));
        }
        // Record 20 is last in physical record order, so resolving its
        // absent fifth identifier cannot accidentally consume a following
        // record's bytes.
        let records = [
            (1, 5, vec![1; BLOCK_SIZE as usize]),
            (2, 5, vec![2; BLOCK_SIZE as usize]),
            (3, 5, vec![3; BLOCK_SIZE as usize]),
            (4, 5, vec![4; BLOCK_SIZE as usize]),
            (10, 4, long_binary_record(length, 20)),
            (20, 2, truncated_block_list),
        ];
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));
        let mut stream =
            read_binary_stream(&provider, RecordIdentifier::new(segment, 10)).expect("stream");
        let mut block = [0u8; BLOCK_SIZE as usize];
        for _ in 0..4 {
            assert_eq!(
                stream.read_chunk(&mut block).expect("present block"),
                block.len()
            );
        }
        match stream.read_chunk(&mut block) {
            Err(Error::InvalidFormat { details }) => {
                assert!(details.contains("record 20"), "unexpected error: {details}");
            }
            _ => panic!("expected truncated list bucket error"),
        }
        assert_eq!(stream.position(), 4 * BLOCK_SIZE);
    }

    #[test]
    fn stream_preserves_missing_segment_identity() {
        let data_segment = data_segment_identifier(1);
        let missing_bulk_segment = bulk_segment_identifier(2);
        let length = MEDIUM_VALUE_LIMIT;
        let missing_block =
            referenced_record_identifier(1, (MAXIMUM_SEGMENT_SIZE - BLOCK_SIZE as usize) as u32);
        let mut block_list = Vec::new();
        for _ in 0..length.div_ceil(BLOCK_SIZE) {
            block_list.extend_from_slice(&missing_block);
        }
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            data_segment,
            synthetic_data_segment(
                &[missing_bulk_segment],
                &[(10, 2, block_list), (11, 4, long_binary_record(length, 10))],
            ),
        );

        let mut stream =
            read_binary_stream(&provider, RecordIdentifier::new(data_segment, 11)).expect("stream");
        let mut byte = [0u8; 1];
        match stream.read_chunk(&mut byte) {
            Err(Error::SegmentNotFound { segment_identifier }) => {
                assert_eq!(segment_identifier, missing_bulk_segment);
            }
            _ => panic!("expected missing bulk segment error"),
        }
        assert_eq!(stream.position(), 0);
    }

    #[test]
    fn stream_rejects_truncated_heads_and_oversized_block_lists() {
        let segment = data_segment_identifier(1);
        let truncated = ((MEDIUM_VALUE_LIMIT - MEDIUM_VALUE_LIMIT) | (0x3 << 62))
            .to_be_bytes()
            .to_vec();
        let oversized_length = (crate::content::list::MAXIMUM_LIST_SIZE + 1) * BLOCK_SIZE;
        let oversized = long_binary_record(oversized_length, 99);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(0, 4, truncated), (1, 4, oversized)]),
        );

        assert!(matches!(
            read_binary_stream(&provider, RecordIdentifier::new(segment, 0)),
            Err(Error::InvalidFormat { .. })
        ));
        match read_binary_stream(&provider, RecordIdentifier::new(segment, 1)) {
            Err(Error::InvalidFormat { details }) => {
                assert!(details.contains("exceeding the list maximum"));
            }
            _ => panic!("expected oversized list error"),
        }
    }

    #[test]
    fn binary_stream_is_send_when_its_provider_is_sync() {
        fn assert_send<Type: Send>() {}
        assert_send::<BinaryStream<'static, MemorySegmentProvider>>();
    }

    #[test]
    fn rejects_invalid_markers() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(0, 4, vec![0xF8, 0, 0, 0])]),
        );
        let identifier = RecordIdentifier::new(segment, 0);
        assert!(read_binary_value(&provider, identifier).is_err());
        assert!(read_binary_stream(&provider, identifier).is_err());
        assert!(read_string(&provider, identifier).is_err());
        assert!(read_value_length(&provider, identifier).is_err());
    }
}
