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

use crate::content::list::{
    MAXIMUM_LIST_SIZE, uncounted_list_entries, uncounted_list_entry,
    uncounted_list_entry_with_provider,
};
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;

mod binary;
mod stream;
mod string;
#[cfg(test)]
mod test_support;

pub use binary::*;
pub use stream::*;
pub use string::*;

/// Lengths below this fit the one-byte small encoding.
pub const SMALL_VALUE_LIMIT: u64 = 128;

/// Lengths below this fit the two-byte medium encoding.
pub const MEDIUM_VALUE_LIMIT: u64 = (1 << 14) + 128;

/// The size of the block records a long value is split into.
pub const BLOCK_SIZE: u64 = 4096;

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

/// Concatenates the blocks of a long value: `length.div_ceil(4096)` block
/// records of 4096 bytes each, the last one possibly shorter.
pub(crate) fn read_block_list(
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

#[cfg(test)]
mod tests {
    use super::read_value_length;
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::content::value::binary::read_binary_value;
    use crate::content::value::stream::read_binary_stream;
    use crate::content::value::string::read_string;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;

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
