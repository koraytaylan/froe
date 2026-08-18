//! Walking a long binary's blocks as an `io::Read`, so a value larger
//! than memory is consumed a block at a time.

use super::{
    BLOCK_SIZE, Error, MAXIMUM_LIST_SIZE, MEDIUM_VALUE_LIMIT, RecordIdentifier, Result,
    SMALL_VALUE_LIMIT, SegmentProvider, io, uncounted_list_entry_with_provider,
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum BinaryStreamSource {
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
    pub(crate) provider: &'provider Provider,
    pub(crate) source: BinaryStreamSource,
    pub(crate) length: u64,
    pub(crate) position: u64,
    pub(crate) resolved_block: Option<(u64, RecordIdentifier)>,
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
        let identifier = uncounted_list_entry_with_provider(
            self.provider,
            list_identifier,
            block_count,
            block_index,
        )?;
        self.resolved_block = Some((block_index, identifier));
        Ok(identifier)
    }

    pub(in crate::content) fn current_block_identifier(
        &mut self,
    ) -> Result<Option<RecordIdentifier>> {
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

#[cfg(test)]
mod tests {
    use super::{BinaryStream, read_binary_stream};
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::content::value::binary::{read_binary_content, verify_binary_content};
    use crate::content::value::test_support::{
        CountingProvider, direct_binary_record, local_record_identifier, long_binary_record,
        referenced_record_identifier, small_string_record,
    };
    use crate::content::value::{BLOCK_SIZE, MEDIUM_VALUE_LIMIT};
    use crate::error::Error;
    use crate::segment::parsed_segment::{
        MAXIMUM_SEGMENT_SIZE,
        tests::{bulk_segment_identifier, data_segment_identifier, synthetic_data_segment},
    };
    use crate::segment::record::RecordIdentifier;
    use std::io::Read;

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
}
