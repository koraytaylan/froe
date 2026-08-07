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

use crate::content::list::uncounted_list_entries;
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

/// Reads the full content of an inline binary. Requesting the content of
/// an external binary fails with
/// [`Error::ExternalBinaryContentUnavailable`].
pub fn read_binary_content(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
) -> Result<Vec<u8>> {
    match read_binary_value(provider, identifier)? {
        BinaryValue::External { blob_identifier } => {
            Err(Error::ExternalBinaryContentUnavailable { blob_identifier })
        }
        BinaryValue::Inline {
            length,
            record_identifier,
        } => {
            let view = provider.segment(record_identifier.segment)?;
            if length < SMALL_VALUE_LIMIT {
                Ok(view
                    .read_bytes(record_identifier.record_number, 1, length as usize)?
                    .to_vec())
            } else if length < MEDIUM_VALUE_LIMIT {
                Ok(view
                    .read_bytes(record_identifier.record_number, 2, length as usize)?
                    .to_vec())
            } else {
                let list_identifier =
                    view.read_record_identifier(record_identifier.record_number, 8, 0)?;
                read_block_list(provider, list_identifier, length)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BinaryValue, read_binary_content, read_binary_value, read_string, read_value_length,
    };
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::error::Error;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;

    fn small_string_record(text: &str) -> Vec<u8> {
        let mut bytes = vec![text.len() as u8];
        bytes.extend_from_slice(text.as_bytes());
        bytes
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

        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (0, 8, short_external),
                    (1, 4, small_string_record(blob_identifier)),
                    (2, 8, long_external),
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
    }

    #[test]
    fn reads_inline_binaries() {
        let segment = data_segment_identifier(1);
        let content = vec![0x00u8, 0xFF, 0x7F, 0x80];
        let mut record = vec![content.len() as u8];
        record.extend_from_slice(&content);
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
    fn rejects_invalid_markers() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(0, 4, vec![0xF8, 0, 0, 0])]),
        );
        let identifier = RecordIdentifier::new(segment, 0);
        assert!(read_binary_value(&provider, identifier).is_err());
        assert!(read_string(&provider, identifier).is_err());
        assert!(read_value_length(&provider, identifier).is_err());
    }
}
