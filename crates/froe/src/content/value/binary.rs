//! Reading a binary value: the inline forms, the external identifier, and
//! comparing two without materializing either.

use super::{
    BLOCK_SIZE, Error, RecordIdentifier, Result, SegmentProvider, read_binary_stream, read_string,
    read_value_length, verify_string_content,
};

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

/// Compares two inline binaries by content without materializing either,
/// assuming their lengths are already known to equal `expected_length`.
/// Except for the identical-record fast path, a disagreement between that
/// supplied length and either value record is reported as corrupt format.
/// Long values compare block by block: equal lengths mean both block lists
/// chunk at the same 4096-byte boundaries.
pub fn inline_binary_contents_equal(
    provider: &dyn SegmentProvider,
    first: RecordIdentifier,
    second: RecordIdentifier,
    expected_length: u64,
) -> Result<bool> {
    if first == second {
        return Ok(true);
    }
    let mut first_stream = read_binary_stream(provider, first)?;
    let mut second_stream = read_binary_stream(provider, second)?;
    if first_stream.len() != expected_length || second_stream.len() != expected_length {
        return Err(Error::InvalidFormat {
            details: format!(
                "inline binary comparison expected {expected_length} bytes, but records {first} \
                 and {second} declare {} and {} bytes",
                first_stream.len(),
                second_stream.len()
            ),
        });
    }

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
    use super::{
        BinaryValue, inline_binary_contents_equal, read_binary_content, read_binary_value,
        verify_binary_content,
    };
    use crate::content::list::uncounted_list_entry;
    use crate::content::provider::{SegmentProvider, tests::MemorySegmentProvider};
    use crate::content::value::stream::read_binary_stream;
    use crate::content::value::test_support::{
        CountingProvider, direct_binary_record, local_record_identifier, long_binary_record,
        referenced_record_identifier, repeated_local_identifiers, small_string_record,
    };
    use crate::content::value::{BLOCK_SIZE, MEDIUM_VALUE_LIMIT};
    use crate::error::Error;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;
    use std::io::Read;

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
    fn binary_comparison_checks_the_supplied_length_against_both_records() {
        let segment = data_segment_identifier(1);
        let records = [
            (10, 4, direct_binary_record(b"abc")),
            (11, 4, direct_binary_record(b"abc")),
        ];
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));
        let first = RecordIdentifier::new(segment, 10);
        let second = RecordIdentifier::new(segment, 11);

        let error = inline_binary_contents_equal(&provider, first, second, 2)
            .expect_err("the caller-provided length is part of the comparison contract");
        assert!(matches!(
            error,
            Error::InvalidFormat { details }
                if details.contains("expected 2 bytes")
                    && details.contains("declare 3 and 3 bytes")
        ));
        assert!(
            inline_binary_contents_equal(&provider, first, second, 3)
                .expect("matching declared and expected lengths")
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
}
