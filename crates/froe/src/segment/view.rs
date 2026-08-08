//! The segment view: bounds-checked access to one segment's record data.
//!
//! [`SegmentView`] couples one segment's structure with its bytes and
//! offers the bounds-checked primitive reads every record decoder builds
//! on: fixed-width integers, byte runs, and serialized record identifiers.
//! Resolution *across* segments goes through
//! [`SegmentProvider`](crate::content::provider::SegmentProvider).

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;

/// The bytes backing a segment view: borrowed from a memory-mapped
/// archive on the read path, or shared ownership of a buffer for
/// segments written in the current session that have no mapping yet.
#[derive(Clone)]
pub enum SegmentBytes<'provider> {
    /// A slice of a memory-mapped archive.
    Borrowed(&'provider [u8]),
    /// A shared in-memory buffer.
    Shared(Arc<Vec<u8>>),
}

impl std::ops::Deref for SegmentBytes<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            SegmentBytes::Borrowed(bytes) => bytes,
            SegmentBytes::Shared(bytes) => bytes,
        }
    }
}

impl<'provider> From<&'provider [u8]> for SegmentBytes<'provider> {
    fn from(bytes: &'provider [u8]) -> Self {
        SegmentBytes::Borrowed(bytes)
    }
}

/// One segment's parsed structure together with its raw bytes.
#[derive(Clone)]
pub struct SegmentView<'provider> {
    /// The parsed header and tables.
    pub structure: Arc<ParsedSegment>,
    /// The segment's stored bytes.
    pub bytes: SegmentBytes<'provider>,
}

impl std::fmt::Debug for SegmentView<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "SegmentView({}, {} bytes)",
            self.structure.identifier,
            self.bytes.len()
        )
    }
}

impl SegmentView<'_> {
    /// Resolves a record number to a position in [`Self::bytes`].
    pub fn record_position(&self, record_number: u32) -> Result<usize> {
        let offset =
            self.structure
                .record_offset(record_number)
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "record {record_number} does not exist in segment {}",
                        self.structure.identifier
                    ),
                })?;
        self.structure.buffer_position(offset)
    }

    /// Reads `length` bytes starting `offset` bytes into the record.
    pub fn read_bytes(&self, record_number: u32, offset: usize, length: usize) -> Result<&[u8]> {
        let overrun = || Error::InvalidFormat {
            details: format!(
                "read of {length} bytes at offset {offset} of record {record_number} \
                 overruns segment {}",
                self.structure.identifier
            ),
        };
        let start = self
            .record_position(record_number)?
            .checked_add(offset)
            .ok_or_else(overrun)?;
        let end = start
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(overrun)?;
        Ok(&self.bytes[start..end])
    }

    /// Reads one byte of the record.
    pub fn read_u8(&self, record_number: u32, offset: usize) -> Result<u8> {
        Ok(self.read_bytes(record_number, offset, 1)?[0])
    }

    /// Reads a big-endian unsigned 16-bit integer.
    pub fn read_u16(&self, record_number: u32, offset: usize) -> Result<u16> {
        let bytes = self.read_bytes(record_number, offset, 2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Reads a big-endian unsigned 32-bit integer.
    pub fn read_u32(&self, record_number: u32, offset: usize) -> Result<u32> {
        let bytes = self.read_bytes(record_number, offset, 4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads a big-endian unsigned 64-bit integer.
    pub fn read_u64(&self, record_number: u32, offset: usize) -> Result<u64> {
        let bytes = self.read_bytes(record_number, offset, 8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Reads the serialized record identifier stored at
    /// `raw_offset + identifier_index * 6` bytes into the record: a 16-bit
    /// segment reference (0 means this segment, `n > 0` means entry `n - 1`
    /// of the segment reference table) followed by a 32-bit record number.
    pub fn read_record_identifier(
        &self,
        record_number: u32,
        raw_offset: usize,
        identifier_index: usize,
    ) -> Result<RecordIdentifier> {
        let offset = raw_offset + identifier_index * 6;
        let reference = self.read_u16(record_number, offset)?;
        let target_record_number = self.read_u32(record_number, offset + 2)?;
        let segment = self.structure.resolve_segment_reference(reference)?;
        Ok(RecordIdentifier::new(segment, target_record_number))
    }
}

#[cfg(test)]
mod tests {
    use crate::content::provider::SegmentProvider;
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::error::Error;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};

    #[test]
    fn reads_record_content_through_the_view() {
        let identifier = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            identifier,
            synthetic_data_segment(
                &[],
                &[(0, 4, vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08])],
            ),
        );
        let view = provider.segment(identifier).expect("segment exists");
        assert_eq!(view.read_u8(0, 0).expect("read"), 0x01);
        assert_eq!(view.read_u16(0, 0).expect("read"), 0x0102);
        assert_eq!(view.read_u32(0, 2).expect("read"), 0x0304_0506);
        assert_eq!(view.read_u64(0, 0).expect("read"), 0x0102_0304_0506_0708);
        assert_eq!(view.read_bytes(0, 6, 2).expect("read"), &[0x07, 0x08]);
    }

    #[test]
    fn resolves_record_identifiers_across_segments() {
        let identifier = data_segment_identifier(1);
        let referenced = data_segment_identifier(2);
        // Record 0 holds two serialized record identifiers:
        // (reference 0, record 7) and (reference 1, record 9).
        let record = vec![
            0x00, 0x00, 0x00, 0x00, 0x00, 0x07, // this segment, record 7
            0x00, 0x01, 0x00, 0x00, 0x00, 0x09, // referenced segment, record 9
        ];
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            identifier,
            synthetic_data_segment(&[referenced], &[(0, 4, record)]),
        );

        let view = provider.segment(identifier).expect("segment exists");
        let own = view.read_record_identifier(0, 0, 0).expect("read");
        assert_eq!(own.segment, identifier);
        assert_eq!(own.record_number, 7);
        let foreign = view.read_record_identifier(0, 0, 1).expect("read");
        assert_eq!(foreign.segment, referenced);
        assert_eq!(foreign.record_number, 9);
    }

    #[test]
    fn out_of_bounds_reads_are_errors() {
        let identifier = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            identifier,
            synthetic_data_segment(&[], &[(0, 4, vec![0x01])]),
        );
        let view = provider.segment(identifier).expect("segment exists");
        assert!(view.read_u64(0, usize::MAX - 4).is_err());
        assert!(view.read_bytes(0, 0, usize::MAX).is_err());
        assert!(view.read_u8(999, 0).is_err(), "unknown record number");
    }

    #[test]
    fn missing_segments_are_reported() {
        let provider = MemorySegmentProvider::default();
        let missing = data_segment_identifier(42);
        match provider.segment(missing) {
            Err(Error::SegmentNotFound { segment_identifier }) => {
                assert_eq!(segment_identifier, missing);
            }
            other => panic!("expected SegmentNotFound, got {other:?}"),
        }
    }
}
