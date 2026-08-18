//! Reading a string value: small and medium forms inline, long ones
//! assembled from the block list their record points at.

use super::{
    BLOCK_SIZE, Error, MEDIUM_VALUE_LIMIT, RecordIdentifier, Result, SMALL_VALUE_LIMIT,
    SegmentProvider, read_block_list, uncounted_list_entry,
};

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

pub(crate) fn verify_string_content(
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

#[cfg(test)]
mod tests {
    use super::read_string;
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::content::value::binary::read_binary_content;
    use crate::content::value::read_value_length;
    use crate::content::value::test_support::small_string_record;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;

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
}
