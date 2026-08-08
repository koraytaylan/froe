//! Building one segment buffer.
//!
//! Records accumulate from the end of a virtual 256 KiB buffer toward the
//! header, exactly like Oak's segment buffer writer: record offsets are
//! positions within that virtual buffer, which keeps them valid after the
//! final trim. When the segment is finished, the header, the referenced
//! segment table, and the record reference table are assembled in front of
//! the record data and the whole segment is trimmed to a 16-byte-aligned
//! length.
//!
//! The builder enforces the format limits the reader validates: at most
//! 65533 referenced segments, record numbers assigned sequentially (so
//! the table is sorted), offsets four-byte aligned, and a total size of
//! at most 256 KiB. When a record does not fit, [`SegmentBufferFull`] is
//! returned and the caller flushes the segment and retries in a fresh
//! one.

use std::collections::HashMap;

use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::MAXIMUM_SEGMENT_SIZE;
use crate::segment::record::RecordType;

/// The fixed data segment header size.
const HEADER_SIZE: usize = 32;

/// Bytes per referenced segment table entry.
const SEGMENT_REFERENCE_SIZE: usize = 16;

/// Bytes per record reference table entry.
const RECORD_REFERENCE_SIZE: usize = 9;

/// The most referenced segments the *reader* accepts (`count + 1 < 0xFFFF`).
const MAXIMUM_SEGMENT_REFERENCES: usize = 0xFFFD;

/// Returned by [`SegmentBufferBuilder::allocate`] when the record cannot
/// fit into the segment under construction; the caller must finish this
/// segment and allocate in a fresh one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentBufferFull;

/// The garbage collection generation stamped on written segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GarbageCollectionGeneration {
    /// The generation, incremented by every compaction.
    pub generation: i32,
    /// The full generation, incremented only by full compactions.
    pub full_generation: i32,
    /// Whether the segment was produced by a compactor.
    pub is_compacted: bool,
}

/// A finished segment ready to be persisted.
pub struct BuiltSegment {
    /// The segment's identifier.
    pub identifier: SegmentIdentifier,
    /// The serialized segment.
    pub bytes: Vec<u8>,
    /// The generation stamped into the header.
    pub generation: GarbageCollectionGeneration,
    /// The segments this segment references.
    pub referenced_segments: Vec<SegmentIdentifier>,
    /// The external binary identifiers recorded in this segment, for the
    /// archive's binary references catalog.
    pub binary_reference_identifiers: Vec<String>,
}

/// Builds one data segment.
pub struct SegmentBufferBuilder {
    identifier: SegmentIdentifier,
    generation: GarbageCollectionGeneration,
    /// The full virtual buffer; record data grows down from the end.
    buffer: Vec<u8>,
    /// The lowest buffer position occupied by record data.
    data_start: usize,
    referenced_segments: Vec<SegmentIdentifier>,
    reference_lookup: HashMap<SegmentIdentifier, u16>,
    /// `(record number, type, virtual offset)` in allocation order;
    /// record numbers are sequential, so this is sorted.
    record_table: Vec<(u32, RecordType, u32)>,
    record_positions: HashMap<u32, (usize, usize)>,
    binary_reference_identifiers: Vec<String>,
}

impl SegmentBufferBuilder {
    /// Starts a fresh segment with the given identifier and generation.
    #[must_use]
    pub fn new(identifier: SegmentIdentifier, generation: GarbageCollectionGeneration) -> Self {
        Self {
            identifier,
            generation,
            buffer: vec![0u8; MAXIMUM_SEGMENT_SIZE],
            data_start: MAXIMUM_SEGMENT_SIZE,
            referenced_segments: Vec::new(),
            reference_lookup: HashMap::new(),
            record_table: Vec::new(),
            record_positions: HashMap::new(),
            binary_reference_identifiers: Vec::new(),
        }
    }

    /// Registers an external binary identifier stored in this segment for
    /// the archive's binary references catalog.
    pub fn register_binary_reference(&mut self, blob_identifier: String) {
        self.binary_reference_identifiers.push(blob_identifier);
    }

    /// The identifier of the segment under construction.
    #[must_use]
    pub fn identifier(&self) -> SegmentIdentifier {
        self.identifier
    }

    /// The number of records allocated so far.
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.record_table.len()
    }

    /// Whether a record of `size` bytes referencing `referenced` foreign
    /// segments would fit, assuming the worst case where every referenced
    /// segment is new to this segment's reference table.
    #[must_use]
    pub fn fits(&self, size: usize, referenced: &[SegmentIdentifier]) -> bool {
        let new_references = referenced
            .iter()
            .filter(|segment| {
                **segment != self.identifier && !self.reference_lookup.contains_key(*segment)
            })
            .count();
        if self.referenced_segments.len() + new_references > MAXIMUM_SEGMENT_REFERENCES {
            return false;
        }
        let aligned_size = size.div_ceil(4) * 4;
        let data_length = (MAXIMUM_SEGMENT_SIZE - self.data_start) + aligned_size;
        let tables_length = HEADER_SIZE
            + (self.referenced_segments.len() + new_references) * SEGMENT_REFERENCE_SIZE
            + (self.record_table.len() + 1) * RECORD_REFERENCE_SIZE;
        tables_length + data_length <= MAXIMUM_SEGMENT_SIZE
    }

    /// Allocates a record of `size` bytes, declaring the foreign segments
    /// its content may reference. Returns the record number, or
    /// [`SegmentBufferFull`] when the segment cannot hold it.
    pub fn allocate(
        &mut self,
        record_type: RecordType,
        size: usize,
        referenced: &[SegmentIdentifier],
    ) -> Result<u32, SegmentBufferFull> {
        if !self.fits(size, referenced) {
            return Err(SegmentBufferFull);
        }
        let aligned_size = size.div_ceil(4) * 4;
        self.data_start -= aligned_size;
        let record_number = self.record_table.len() as u32;
        self.record_table
            .push((record_number, record_type, self.data_start as u32));
        self.record_positions
            .insert(record_number, (self.data_start, size));
        Ok(record_number)
    }

    /// The writable bytes of an allocated record.
    ///
    /// # Panics
    ///
    /// Panics when `record_number` was not allocated by this builder —
    /// always a programming error, never reachable from input data.
    #[must_use]
    pub fn record_bytes_mut(&mut self, record_number: u32) -> &mut [u8] {
        let (position, size) = self.record_positions[&record_number];
        &mut self.buffer[position..position + size]
    }

    /// Resolves the reference value under which `segment` is addressed
    /// from this segment: 0 for the segment itself, otherwise the 1-based
    /// position in the reference table, adding the segment when new.
    ///
    /// # Panics
    ///
    /// Panics when the reference table overflows despite a prior
    /// [`Self::fits`] check — a programming error in the caller.
    pub fn reference_for(&mut self, segment: SegmentIdentifier) -> u16 {
        if segment == self.identifier {
            return 0;
        }
        if let Some(&reference) = self.reference_lookup.get(&segment) {
            return reference;
        }
        assert!(
            self.referenced_segments.len() < MAXIMUM_SEGMENT_REFERENCES,
            "segment reference table overflow; allocate() must be called with \
             every referenced segment declared"
        );
        self.referenced_segments.push(segment);
        let reference = self.referenced_segments.len() as u16;
        self.reference_lookup.insert(segment, reference);
        reference
    }

    /// Serializes a record identifier (reference and record number) into
    /// `target`, which must be exactly six bytes of a record's content.
    pub fn write_record_identifier_bytes(reference: u16, record_number: u32, target: &mut [u8]) {
        target[0..2].copy_from_slice(&reference.to_be_bytes());
        target[2..6].copy_from_slice(&record_number.to_be_bytes());
    }

    /// Finishes the segment: assembles header and tables in front of the
    /// record data and trims to a 16-byte-aligned total length.
    #[must_use]
    pub fn finish(self) -> BuiltSegment {
        let data_length = MAXIMUM_SEGMENT_SIZE - self.data_start;
        let tables_length = HEADER_SIZE
            + self.referenced_segments.len() * SEGMENT_REFERENCE_SIZE
            + self.record_table.len() * RECORD_REFERENCE_SIZE;
        let total_length = (tables_length + data_length).div_ceil(16) * 16;

        let mut bytes = vec![0u8; total_length];
        bytes[0..3].copy_from_slice(b"0aK");
        bytes[3] = 13;
        let full_generation_word = (self.generation.full_generation as u32 & 0x7FFF_FFFF)
            | if self.generation.is_compacted {
                0x8000_0000
            } else {
                0
            };
        bytes[4..8].copy_from_slice(&full_generation_word.to_be_bytes());
        bytes[10..14].copy_from_slice(&self.generation.generation.to_be_bytes());
        bytes[14..18].copy_from_slice(&(self.referenced_segments.len() as u32).to_be_bytes());
        bytes[18..22].copy_from_slice(&(self.record_table.len() as u32).to_be_bytes());

        for (reference_index, segment) in self.referenced_segments.iter().enumerate() {
            let base = HEADER_SIZE + reference_index * SEGMENT_REFERENCE_SIZE;
            bytes[base..base + 8].copy_from_slice(&segment.most_significant_bits.to_be_bytes());
            bytes[base + 8..base + 16]
                .copy_from_slice(&segment.least_significant_bits.to_be_bytes());
        }
        let table_base = HEADER_SIZE + self.referenced_segments.len() * SEGMENT_REFERENCE_SIZE;
        for (table_index, (record_number, record_type, offset)) in
            self.record_table.iter().enumerate()
        {
            let base = table_base + table_index * RECORD_REFERENCE_SIZE;
            bytes[base..base + 4].copy_from_slice(&record_number.to_be_bytes());
            bytes[base + 4] = *record_type as u8;
            bytes[base + 5..base + 9].copy_from_slice(&offset.to_be_bytes());
        }

        bytes[total_length - data_length..].copy_from_slice(&self.buffer[self.data_start..]);

        BuiltSegment {
            identifier: self.identifier,
            bytes,
            generation: self.generation,
            referenced_segments: self.referenced_segments,
            binary_reference_identifiers: self.binary_reference_identifiers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GarbageCollectionGeneration, SegmentBufferBuilder, SegmentBufferFull};

    use crate::segment::parsed_segment::ParsedSegment;
    use crate::segment::record::RecordType;
    use crate::writer::identifier_generator::new_data_segment_identifier;

    fn test_generation() -> GarbageCollectionGeneration {
        GarbageCollectionGeneration {
            generation: 3,
            full_generation: 2,
            is_compacted: true,
        }
    }

    #[test]
    fn built_segments_parse_and_read_back() {
        let identifier = new_data_segment_identifier();
        let foreign = new_data_segment_identifier();
        let mut builder = SegmentBufferBuilder::new(identifier, test_generation());

        // Record 0: a small string value.
        let text = b"\x05hello";
        let first = builder
            .allocate(RecordType::Value, text.len(), &[])
            .expect("fits");
        builder.record_bytes_mut(first).copy_from_slice(text);

        // Record 1: a record identifier pointing into the foreign segment.
        let second = builder
            .allocate(RecordType::List, 6, &[foreign])
            .expect("fits");
        let reference = builder.reference_for(foreign);
        let mut identifier_bytes = [0u8; 6];
        SegmentBufferBuilder::write_record_identifier_bytes(reference, 42, &mut identifier_bytes);
        builder
            .record_bytes_mut(second)
            .copy_from_slice(&identifier_bytes);

        let built = builder.finish();
        assert_eq!(built.bytes.len() % 16, 0);
        assert_eq!(built.referenced_segments, vec![foreign]);

        let parsed = ParsedSegment::parse(identifier, &built.bytes).expect("parses");
        assert_eq!(parsed.version, Some(13));
        assert_eq!(parsed.generation, 3);
        assert_eq!(parsed.full_generation, 2);
        assert!(parsed.is_compacted);
        assert_eq!(parsed.referenced_segments, vec![foreign]);
        assert_eq!(parsed.record_table().len(), 2);

        let first_offset = parsed.record_offset(0).expect("record 0");
        let position = parsed.buffer_position(first_offset).expect("in range");
        assert_eq!(&built.bytes[position..position + 6], text);

        assert_eq!(
            parsed.resolve_segment_reference(1).expect("reference"),
            foreign
        );
    }

    #[test]
    fn uncompacted_generations_round_trip() {
        let identifier = new_data_segment_identifier();
        let generation = GarbageCollectionGeneration {
            generation: 7,
            full_generation: 5,
            is_compacted: false,
        };
        let builder = SegmentBufferBuilder::new(identifier, generation);
        let built = builder.finish();
        let parsed = ParsedSegment::parse(identifier, &built.bytes).expect("parses");
        assert_eq!(parsed.generation, 7);
        assert_eq!(parsed.full_generation, 5);
        assert!(!parsed.is_compacted);
    }

    #[test]
    fn allocation_fails_when_the_segment_is_full() {
        let identifier = new_data_segment_identifier();
        let mut builder = SegmentBufferBuilder::new(identifier, test_generation());
        // Fill the segment with a record close to the maximum.
        builder
            .allocate(RecordType::Block, 262_000, &[])
            .expect("first large record fits");
        assert_eq!(
            builder.allocate(RecordType::Block, 4096, &[]),
            Err(SegmentBufferFull)
        );
        // A tiny record still fits in the remaining space.
        builder
            .allocate(RecordType::Block, 4, &[])
            .expect("small record fits");
    }

    #[test]
    fn duplicate_references_share_one_table_entry() {
        let identifier = new_data_segment_identifier();
        let foreign = new_data_segment_identifier();
        let mut builder = SegmentBufferBuilder::new(identifier, test_generation());
        builder
            .allocate(RecordType::List, 12, &[foreign, foreign])
            .expect("fits");
        assert_eq!(builder.reference_for(foreign), 1);
        assert_eq!(
            builder.reference_for(foreign),
            1,
            "second lookup reuses the entry"
        );
        assert_eq!(
            builder.reference_for(identifier),
            0,
            "self reference is zero"
        );
        let built = builder.finish();
        assert_eq!(built.referenced_segments.len(), 1);
    }

    #[test]
    fn record_alignment_is_four_bytes() {
        let identifier = new_data_segment_identifier();
        let mut builder = SegmentBufferBuilder::new(identifier, test_generation());
        let first = builder.allocate(RecordType::Value, 3, &[]).expect("fits");
        let second = builder.allocate(RecordType::Value, 5, &[]).expect("fits");
        let built = builder.finish();
        let parsed = ParsedSegment::parse(identifier, &built.bytes).expect("parses");
        let first_offset = parsed.record_offset(first).expect("first");
        let second_offset = parsed.record_offset(second).expect("second");
        assert_eq!(first_offset % 4, 0);
        assert_eq!(second_offset % 4, 0);
        assert_eq!(first_offset - second_offset, 8, "five bytes align to eight");
    }
}
