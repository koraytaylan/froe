//! Parsing of segment buffers: header, reference tables, and addressing.
//!
//! A *data* segment starts with a 32-byte header (magic `0aK`, format
//! version 12 or 13, garbage collection generations, table sizes) followed
//! by two tables: the referenced segment UUIDs and the record references
//! mapping logical record numbers to offsets. Record data fills the tail of
//! the buffer.
//!
//! Record offsets are positions within a *virtual* 256 KiB segment whose
//! end coincides with the end of the stored buffer; stored segments are
//! usually trimmed below 256 KiB, so a virtual offset converts to a buffer
//! position as `buffer_length - 262144 + offset`.
//!
//! A *bulk* segment has no header at all: its record numbers are the
//! virtual offsets themselves, and its content is raw block data.

use crate::error::{Error, Result};
use crate::segment::identifier::{SegmentIdentifier, SegmentKind};
use crate::segment::record::RecordType;
use crate::tar_archive::index::{read_u32, read_u64};

/// The virtual segment size all record offsets are relative to.
pub const MAXIMUM_SEGMENT_SIZE: usize = 1 << 18;

/// The fixed data segment header size.
const HEADER_SIZE: usize = 32;

/// Bytes per entry of the segment reference table.
const SEGMENT_REFERENCE_SIZE: usize = 16;

/// Bytes per entry of the record reference table.
const RECORD_REFERENCE_SIZE: usize = 9;

/// One entry of the record reference table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordTableEntry {
    /// The logical record number.
    pub record_number: u32,
    /// The raw record type byte. Java resolves it to a record type only
    /// when *iterating* records (throwing on unknown bytes there); plain
    /// record lookup by number never consults it, so an unknown byte must
    /// not fail the segment parse — that would lose content Java serves.
    pub type_byte: u8,
    /// The record's offset within the virtual 256 KiB segment.
    pub offset: u32,
}

impl RecordTableEntry {
    /// The record's type, when the type byte is a known ordinal.
    #[must_use]
    pub const fn record_type(&self) -> Option<RecordType> {
        RecordType::from_byte(self.type_byte)
    }
}

/// The parsed structure of one segment (metadata only — the segment bytes
/// are kept separately, typically as a slice of a memory-mapped archive).
#[derive(Debug)]
pub struct ParsedSegment {
    /// The segment's identifier.
    pub identifier: SegmentIdentifier,
    /// Data or bulk, as encoded in the identifier.
    pub kind: SegmentKind,
    /// The stored buffer length in bytes.
    pub size: usize,
    /// The segment format version: 12 or 13 for data segments, `None` for
    /// bulk segments (which have no header).
    pub version: Option<u8>,
    /// The garbage collection generation (0 for bulk segments).
    pub generation: i32,
    /// The full garbage collection generation. Version 12 segments repeat
    /// [`Self::generation`]; bulk segments report 0.
    pub full_generation: i32,
    /// Whether the segment was produced by a compaction. Version 12
    /// segments always report `true`; bulk segments report `false`.
    pub is_compacted: bool,
    /// The segments referenced by record identifiers in this segment.
    /// Serialized references are 1-based: reference `n` resolves to entry
    /// `n - 1`; reference 0 is the segment itself.
    pub referenced_segments: Vec<SegmentIdentifier>,
    /// The record reference table, sorted ascending by record number.
    record_table: Vec<RecordTableEntry>,
}

/// Validated fixed-header and table-layout metadata for a data segment.
/// Keeping this separate lets bounded diagnostics inspect the reference
/// count without allocating either parsed table.
#[derive(Clone, Copy, Debug)]
struct DataSegmentHeader {
    version: u8,
    generation: i32,
    full_generation: i32,
    is_compacted: bool,
    segment_reference_count: usize,
    record_count: usize,
    record_table_start: usize,
}

impl ParsedSegment {
    /// Parses a segment buffer.
    ///
    /// Bulk segments (identifier kind `B`) are accepted as-is; data
    /// segments are validated: magic bytes, version 12 or 13, table sizes
    /// within bounds, and a record table sorted by record number.
    pub fn parse(identifier: SegmentIdentifier, bytes: &[u8]) -> Result<Self> {
        Self::validate_maximum_size(identifier, bytes)?;
        let kind = Self::validate_segment_kind(identifier)?;
        if kind == SegmentKind::Bulk {
            return Ok(Self {
                identifier,
                kind,
                size: bytes.len(),
                version: None,
                generation: 0,
                full_generation: 0,
                is_compacted: false,
                referenced_segments: Vec::new(),
                record_table: Vec::new(),
            });
        }
        Self::parse_data_segment(identifier, bytes)
    }

    fn validate_maximum_size(identifier: SegmentIdentifier, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAXIMUM_SEGMENT_SIZE {
            return Err(Error::InvalidFormat {
                details: format!(
                    "segment {identifier} has {} bytes, exceeding the {MAXIMUM_SEGMENT_SIZE}-byte format limit",
                    bytes.len()
                ),
            });
        }
        Ok(())
    }

    fn validate_segment_kind(identifier: SegmentIdentifier) -> Result<SegmentKind> {
        identifier.kind().ok_or_else(|| Error::InvalidFormat {
            details: format!("segment {identifier} is neither a data nor a bulk segment"),
        })
    }

    /// Validates a data segment's fixed header and table layout, returning
    /// its reference count without allocating or parsing either table.
    pub(crate) fn validated_data_segment_reference_count(
        identifier: SegmentIdentifier,
        bytes: &[u8],
    ) -> Result<usize> {
        Self::validate_maximum_size(identifier, bytes)?;
        if Self::validate_segment_kind(identifier)? != SegmentKind::Data {
            return Err(Error::InvalidFormat {
                details: format!("segment {identifier} is not a data segment"),
            });
        }
        Ok(Self::validate_data_segment_header(identifier, bytes)?.segment_reference_count)
    }

    /// Parses and validates a data segment's header and tables.
    fn parse_data_segment(identifier: SegmentIdentifier, bytes: &[u8]) -> Result<Self> {
        let header = Self::validate_data_segment_header(identifier, bytes)?;
        let mut referenced_segments = Vec::with_capacity(header.segment_reference_count);
        for reference_index in 0..header.segment_reference_count {
            let base = HEADER_SIZE + reference_index * SEGMENT_REFERENCE_SIZE;
            referenced_segments.push(SegmentIdentifier::new(
                read_u64(bytes, base),
                read_u64(bytes, base + 8),
            ));
        }

        let record_table = Self::parse_record_table(
            identifier,
            bytes,
            header.record_table_start,
            header.record_count,
        )?;

        Ok(Self {
            identifier,
            kind: SegmentKind::Data,
            size: bytes.len(),
            version: Some(header.version),
            generation: header.generation,
            full_generation: header.full_generation,
            is_compacted: header.is_compacted,
            referenced_segments,
            record_table,
        })
    }

    /// Validates the fixed header and verifies that both declared tables fit
    /// in the stored buffer. This performs no table allocation or traversal.
    fn validate_data_segment_header(
        identifier: SegmentIdentifier,
        bytes: &[u8],
    ) -> Result<DataSegmentHeader> {
        let invalid = |details: String| Error::InvalidFormat { details };
        if bytes.len() < HEADER_SIZE {
            return Err(invalid(format!(
                "data segment {identifier} has only {} bytes, the header needs {HEADER_SIZE}",
                bytes.len()
            )));
        }
        if &bytes[0..3] != b"0aK" {
            return Err(invalid(format!(
                "data segment {identifier} does not start with the magic bytes \"0aK\""
            )));
        }
        let version = bytes[3];
        if version != 12 && version != 13 {
            return Err(invalid(format!(
                "data segment {identifier} has unsupported format version {version}"
            )));
        }
        let generation = read_u32(bytes, 10) as i32;
        let (full_generation, is_compacted) = if version == 13 {
            let raw = read_u32(bytes, 4) as i32;
            (raw & 0x7FFF_FFFF, raw < 0)
        } else {
            (generation, true)
        };

        let segment_reference_count = read_u32(bytes, 14) as i32;
        let record_count = read_u32(bytes, 18) as i32;
        // The count fields are signed in the format; validate before any
        // arithmetic so hostile values cannot overflow (the Java check is
        // `count + 1 < 0xffff`, hence the 65533 limit).
        if !(0..=0xFFFD).contains(&segment_reference_count) {
            return Err(invalid(format!(
                "data segment {identifier} declares {segment_reference_count} segment references, \
                 the limit is 65533"
            )));
        }
        if record_count < 0 {
            return Err(invalid(format!(
                "data segment {identifier} declares a negative record count"
            )));
        }
        let segment_reference_count = segment_reference_count as usize;
        let record_count = record_count as usize;

        let record_table_start = HEADER_SIZE + segment_reference_count * SEGMENT_REFERENCE_SIZE;
        // Checked arithmetic: on 32-bit targets a huge declared record count
        // could otherwise wrap the end position past the bounds check.
        if record_count
            .checked_mul(RECORD_REFERENCE_SIZE)
            .and_then(|table_size| record_table_start.checked_add(table_size))
            .is_none_or(|record_table_end| record_table_end > bytes.len())
        {
            return Err(invalid(format!(
                "data segment {identifier} of {} bytes cannot hold {segment_reference_count} \
                 segment references and {record_count} record references",
                bytes.len()
            )));
        }

        Ok(DataSegmentHeader {
            version,
            generation,
            full_generation,
            is_compacted,
            segment_reference_count,
            record_count,
            record_table_start,
        })
    }

    /// Parses the record reference table, validating each entry and the
    /// ascending record number order the format guarantees.
    fn parse_record_table(
        identifier: SegmentIdentifier,
        bytes: &[u8],
        table_start: usize,
        record_count: usize,
    ) -> Result<Vec<RecordTableEntry>> {
        let invalid = |details: String| Error::InvalidFormat { details };
        let mut record_table = Vec::with_capacity(record_count);
        let mut previous_record_number: Option<u32> = None;
        for record_index in 0..record_count {
            let base = table_start + record_index * RECORD_REFERENCE_SIZE;
            let record_number = read_u32(bytes, base) as i32;
            let type_byte = bytes[base + 4];
            let offset = read_u32(bytes, base + 5) as i32;
            if record_number < 0 {
                return Err(invalid(format!(
                    "data segment {identifier} contains negative record number {record_number}"
                )));
            }
            if offset < 0 || offset as usize >= MAXIMUM_SEGMENT_SIZE {
                return Err(invalid(format!(
                    "data segment {identifier} contains out-of-range record offset {offset}"
                )));
            }
            let record_number = record_number as u32;
            if let Some(previous) = previous_record_number
                && previous >= record_number
            {
                return Err(invalid(format!(
                    "record table of segment {identifier} is not sorted by record number"
                )));
            }
            previous_record_number = Some(record_number);
            record_table.push(RecordTableEntry {
                record_number,
                type_byte,
                offset: offset as u32,
            });
        }
        Ok(record_table)
    }

    /// The record reference table, sorted ascending by record number.
    /// Empty for bulk segments.
    #[must_use]
    pub fn record_table(&self) -> &[RecordTableEntry] {
        &self.record_table
    }

    /// Resolves a record number to its offset in the virtual 256 KiB
    /// segment. For bulk segments the record number *is* the virtual
    /// offset.
    #[must_use]
    pub fn record_offset(&self, record_number: u32) -> Option<u32> {
        match self.kind {
            SegmentKind::Bulk => Some(record_number),
            SegmentKind::Data => self
                .record_table
                .binary_search_by_key(&record_number, |entry| entry.record_number)
                .ok()
                .map(|position| self.record_table[position].offset),
        }
    }

    /// The type of a record, when the record table knows it and its type
    /// byte is a known ordinal.
    #[must_use]
    pub fn record_type(&self, record_number: u32) -> Option<RecordType> {
        self.record_table
            .binary_search_by_key(&record_number, |entry| entry.record_number)
            .ok()
            .and_then(|position| self.record_table[position].record_type())
    }

    /// Converts a virtual offset to a position in the stored buffer:
    /// `size - 262144 + offset`.
    pub fn buffer_position(&self, virtual_offset: u32) -> Result<usize> {
        let position = self.size as i64 - MAXIMUM_SEGMENT_SIZE as i64 + i64::from(virtual_offset);
        if position < 0 || position >= self.size as i64 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "virtual offset {virtual_offset} is outside segment {} of {} bytes",
                    self.identifier, self.size
                ),
            });
        }
        Ok(position as usize)
    }

    /// Resolves the segment reference field of a serialized record
    /// identifier: 0 is this segment, `n > 0` is entry `n - 1` of the
    /// segment reference table.
    pub fn resolve_segment_reference(&self, reference: u16) -> Result<SegmentIdentifier> {
        if reference == 0 {
            return Ok(self.identifier);
        }
        self.referenced_segments.get(reference as usize - 1).copied().ok_or_else(|| {
            Error::InvalidFormat {
                details: format!(
                    "segment reference {reference} is out of bounds in segment {} with {} references",
                    self.identifier,
                    self.referenced_segments.len()
                ),
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{MAXIMUM_SEGMENT_SIZE, ParsedSegment};
    use crate::segment::identifier::{SegmentIdentifier, SegmentKind};
    use crate::segment::record::RecordType;

    pub(crate) fn data_segment_identifier(seed: u64) -> SegmentIdentifier {
        SegmentIdentifier::new(seed, 0xA000_0000_0000_0000 | seed)
    }

    pub(crate) fn bulk_segment_identifier(seed: u64) -> SegmentIdentifier {
        SegmentIdentifier::new(seed, 0xB000_0000_0000_0000 | seed)
    }

    /// Builds a version 13 data segment with the given referenced segments
    /// and records; each record is (number, type byte, record bytes). The
    /// records are laid out back to back at the end of a buffer just large
    /// enough to hold everything.
    pub(crate) fn synthetic_data_segment(
        referenced_segments: &[SegmentIdentifier],
        records: &[(u32, u8, Vec<u8>)],
    ) -> Vec<u8> {
        synthetic_data_segment_with_generations(referenced_segments, records, 13, 1, 1, true)
    }

    pub(crate) fn synthetic_data_segment_with_generations(
        referenced_segments: &[SegmentIdentifier],
        records: &[(u32, u8, Vec<u8>)],
        version: u8,
        generation: i32,
        full_generation: i32,
        is_compacted: bool,
    ) -> Vec<u8> {
        let table_end = 32 + referenced_segments.len() * 16 + records.len() * 9;
        let record_bytes: usize = records
            .iter()
            .map(|(_, _, bytes)| bytes.len().div_ceil(4) * 4)
            .sum();
        let size = (table_end + record_bytes).div_ceil(16) * 16;

        let mut buffer = vec![0u8; size];
        buffer[0..3].copy_from_slice(b"0aK");
        buffer[3] = version;
        if version == 13 {
            let raw =
                (full_generation & 0x7FFF_FFFF) as u32 | if is_compacted { 0x8000_0000 } else { 0 };
            buffer[4..8].copy_from_slice(&raw.to_be_bytes());
        }
        buffer[10..14].copy_from_slice(&generation.to_be_bytes());
        buffer[14..18].copy_from_slice(&(referenced_segments.len() as u32).to_be_bytes());
        buffer[18..22].copy_from_slice(&(records.len() as u32).to_be_bytes());
        for (reference_index, referenced) in referenced_segments.iter().enumerate() {
            let base = 32 + reference_index * 16;
            buffer[base..base + 8].copy_from_slice(&referenced.most_significant_bits.to_be_bytes());
            buffer[base + 8..base + 16]
                .copy_from_slice(&referenced.least_significant_bits.to_be_bytes());
        }

        // Lay the record data out from the end of the buffer backwards, in
        // reverse record order, each aligned to four bytes.
        let mut data_position = size;
        let mut offsets = vec![0u32; records.len()];
        for (record_index, (_, _, bytes)) in records.iter().enumerate().rev() {
            let aligned = bytes.len().div_ceil(4) * 4;
            data_position -= aligned;
            buffer[data_position..data_position + bytes.len()].copy_from_slice(bytes);
            offsets[record_index] = (MAXIMUM_SEGMENT_SIZE - (size - data_position)) as u32;
        }

        let record_table_start = 32 + referenced_segments.len() * 16;
        for (record_index, (record_number, type_byte, _)) in records.iter().enumerate() {
            let base = record_table_start + record_index * 9;
            buffer[base..base + 4].copy_from_slice(&record_number.to_be_bytes());
            buffer[base + 4] = *type_byte;
            buffer[base + 5..base + 9].copy_from_slice(&offsets[record_index].to_be_bytes());
        }
        buffer
    }

    #[test]
    fn parses_version_13_header() {
        let identifier = data_segment_identifier(1);
        let referenced = data_segment_identifier(2);
        let bytes = synthetic_data_segment_with_generations(
            &[referenced],
            &[(0, 4, vec![3, b'a', b'b', b'c'])],
            13,
            5,
            4,
            true,
        );
        let segment = ParsedSegment::parse(identifier, &bytes).expect("valid segment");
        assert_eq!(segment.kind, SegmentKind::Data);
        assert_eq!(segment.version, Some(13));
        assert_eq!(segment.generation, 5);
        assert_eq!(segment.full_generation, 4);
        assert!(segment.is_compacted);
        assert_eq!(segment.referenced_segments, vec![referenced]);
        assert_eq!(segment.record_table().len(), 1);
        assert_eq!(
            segment.record_table()[0].record_type(),
            Some(RecordType::Value)
        );
    }

    #[test]
    fn version_12_repeats_generation_and_is_compacted() {
        let identifier = data_segment_identifier(1);
        let bytes = synthetic_data_segment_with_generations(&[], &[], 12, 7, 0, false);
        let segment = ParsedSegment::parse(identifier, &bytes).expect("valid segment");
        assert_eq!(segment.version, Some(12));
        assert_eq!(segment.full_generation, 7);
        assert!(
            segment.is_compacted,
            "version 12 segments always report compacted"
        );
    }

    #[test]
    fn version_13_uncompacted_flag_round_trips() {
        let identifier = data_segment_identifier(1);
        let bytes = synthetic_data_segment_with_generations(&[], &[], 13, 3, 2, false);
        let segment = ParsedSegment::parse(identifier, &bytes).expect("valid segment");
        assert_eq!(segment.full_generation, 2);
        assert!(!segment.is_compacted);
    }

    #[test]
    fn validates_data_reference_count_without_materializing_tables() {
        let identifier = data_segment_identifier(1);
        let referenced = [data_segment_identifier(2), bulk_segment_identifier(3)];
        let bytes = synthetic_data_segment(&referenced, &[(0, 0xff, vec![0])]);
        assert_eq!(
            ParsedSegment::validated_data_segment_reference_count(identifier, &bytes)
                .expect("valid header and table layout"),
            2
        );

        let mut negative_record_count = bytes.clone();
        negative_record_count[18..22].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(
            ParsedSegment::validated_data_segment_reference_count(
                identifier,
                &negative_record_count
            )
            .is_err()
        );
        assert!(
            ParsedSegment::validated_data_segment_reference_count(
                bulk_segment_identifier(4),
                &bytes
            )
            .is_err()
        );
    }

    #[test]
    fn resolves_record_offsets_through_the_table() {
        let identifier = data_segment_identifier(1);
        let bytes = synthetic_data_segment(&[], &[(0, 4, vec![1, b'x']), (1, 4, vec![1, b'y'])]);
        let segment = ParsedSegment::parse(identifier, &bytes).expect("valid segment");
        let first_offset = segment.record_offset(0).expect("record 0 exists");
        let second_offset = segment.record_offset(1).expect("record 1 exists");
        assert!(
            first_offset < second_offset,
            "records are laid out in order"
        );
        assert_eq!(segment.record_offset(2), None);

        let position = segment.buffer_position(first_offset).expect("in range");
        assert_eq!(&bytes[position..position + 2], &[1, b'x']);
    }

    #[test]
    fn bulk_segments_use_identity_record_numbers() {
        let identifier = bulk_segment_identifier(9);
        let bytes = vec![0xABu8; 8192];
        let segment = ParsedSegment::parse(identifier, &bytes).expect("bulk segment");
        assert_eq!(segment.kind, SegmentKind::Bulk);
        assert_eq!(segment.version, None);
        // The record number is the virtual offset; for a segment of 8192
        // bytes, virtual offset 262144 - 8192 maps to buffer position 0.
        let virtual_offset = (super::MAXIMUM_SEGMENT_SIZE - 8192) as u32;
        assert_eq!(segment.record_offset(virtual_offset), Some(virtual_offset));
        assert_eq!(
            segment.buffer_position(virtual_offset).expect("in range"),
            0
        );
    }

    #[test]
    fn rejects_oversized_data_and_bulk_segments_before_parsing() {
        let bytes = vec![0u8; MAXIMUM_SEGMENT_SIZE + 1];
        for identifier in [data_segment_identifier(1), bulk_segment_identifier(2)] {
            let error = ParsedSegment::parse(identifier, &bytes).expect_err("oversized segment");
            assert_eq!(
                error.to_string(),
                format!(
                    "invalid segment-tar data: segment {identifier} has {} bytes, exceeding the {MAXIMUM_SEGMENT_SIZE}-byte format limit",
                    bytes.len()
                )
            );
        }
    }

    #[test]
    fn resolves_segment_references() {
        let identifier = data_segment_identifier(1);
        let referenced = data_segment_identifier(2);
        let bytes = synthetic_data_segment(&[referenced], &[]);
        let segment = ParsedSegment::parse(identifier, &bytes).expect("valid segment");
        assert_eq!(
            segment.resolve_segment_reference(0).expect("self"),
            identifier
        );
        assert_eq!(
            segment.resolve_segment_reference(1).expect("first"),
            referenced
        );
        assert!(segment.resolve_segment_reference(2).is_err());
    }

    #[test]
    fn rejects_corrupt_headers() {
        let identifier = data_segment_identifier(1);

        let mut wrong_magic = synthetic_data_segment(&[], &[]);
        wrong_magic[0] = b'X';
        assert!(ParsedSegment::parse(identifier, &wrong_magic).is_err());

        let mut wrong_version = synthetic_data_segment(&[], &[]);
        wrong_version[3] = 11;
        assert!(ParsedSegment::parse(identifier, &wrong_version).is_err());

        let mut oversized_tables = synthetic_data_segment(&[], &[]);
        oversized_tables[18..22].copy_from_slice(&10_000u32.to_be_bytes());
        assert!(ParsedSegment::parse(identifier, &oversized_tables).is_err());

        assert!(ParsedSegment::parse(identifier, &[0u8; 8]).is_err());
    }

    #[test]
    fn hostile_header_counts_are_rejected_without_panicking() {
        let identifier = data_segment_identifier(1);

        let mut hostile_references = synthetic_data_segment(&[], &[]);
        hostile_references[14..18].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
        assert!(ParsedSegment::parse(identifier, &hostile_references).is_err());

        let mut hostile_records = synthetic_data_segment(&[], &[]);
        hostile_records[18..22].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
        assert!(ParsedSegment::parse(identifier, &hostile_records).is_err());
    }

    #[test]
    fn rejects_unsorted_record_tables() {
        let identifier = data_segment_identifier(1);
        let mut bytes = synthetic_data_segment(&[], &[(0, 4, vec![0]), (1, 4, vec![0])]);
        // Swap the two record numbers so the table is descending.
        bytes[32..36].copy_from_slice(&1u32.to_be_bytes());
        bytes[41..45].copy_from_slice(&0u32.to_be_bytes());
        assert!(ParsedSegment::parse(identifier, &bytes).is_err());
    }

    #[test]
    fn buffer_position_rejects_out_of_range_offsets() {
        let identifier = data_segment_identifier(1);
        let bytes = synthetic_data_segment(&[], &[]);
        let segment = ParsedSegment::parse(identifier, &bytes).expect("valid segment");
        assert!(
            segment.buffer_position(0).is_err(),
            "offset before the stored buffer"
        );
    }
}
