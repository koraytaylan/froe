//! Writing a value record: the size classes, the block list a long value
//! is assembled from, and the external blob identifier.

use super::{
    BuiltSegment, BulkBlockSharing, Error, GarbageCollectionGeneration, MAXIMUM_SEGMENT_SIZE,
    RecordIdentifier, RecordType, RecordWriter, Result, SegmentSink, new_bulk_segment_identifier,
};

/// The block size long values are split into.
pub(crate) const BLOCK_SIZE: usize = 4096;

/// Byte budgets for the writer's record-reuse caches.
///
/// Oak sizes the equivalents by entry count — 15000 strings, 3000 templates
/// (`WriterCacheManager`). Bytes rather than entries here, matching the rest
/// of froe's caches, and generous because the whole point is to hold the
/// repeated vocabulary of a tree rather than a sample of it.
pub(crate) const VALUE_DEDUP_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// The largest value worth remembering for reuse. Repetition lives in short
/// values; a long one neither repeats nor earns its place in the budget.
pub(crate) const MAXIMUM_DEDUPLICATED_VALUE_BYTES: usize = 1024;

/// Lengths below this use the one-byte small value encoding.
pub(crate) const SMALL_VALUE_LIMIT: usize = 128;

/// Lengths below this use the two-byte medium value encoding.
/// The length at or above which a binary is stored as a block list rather
/// than materialized. Read by the reclamation prediction, which must apply the
/// same threshold `copy_binary_value` does or its answer is wrong.
pub(crate) const MEDIUM_VALUE_LIMIT: usize = (1 << 14) + 128;

/// External blob identifiers below this length are stored inline.
pub(crate) const BLOB_IDENTIFIER_SMALL_LIMIT: usize = 4096;

impl<Sink: SegmentSink> RecordWriter<Sink> {
    /// Writes a string value record and returns its identifier, reusing an
    /// identical record this writer already produced.
    pub fn write_string(&mut self, text: &str) -> Result<RecordIdentifier> {
        self.write_deduplicated_value(text.as_bytes())
    }

    /// Writes a value record, returning an existing identical one instead
    /// when this writer has already written it.
    ///
    /// Only short values are worth remembering: the cache exists to collapse
    /// the endlessly repeated ones — primary types, property names, small
    /// flags — and a large value neither repeats nor fits a budget usefully.
    pub(super) fn write_deduplicated_value(&mut self, bytes: &[u8]) -> Result<RecordIdentifier> {
        if bytes.len() > MAXIMUM_DEDUPLICATED_VALUE_BYTES {
            return self.write_value_bytes(bytes);
        }
        if let Some(existing) = self.value_cache.get(&bytes.to_vec()) {
            return Ok(existing);
        }
        let written = self.write_value_bytes(bytes)?;
        self.value_cache.insert(bytes.to_vec(), written);
        Ok(written)
    }

    /// Writes an inline binary value record and returns its identifier.
    pub fn write_binary_content(&mut self, content: &[u8]) -> Result<RecordIdentifier> {
        self.write_value_bytes(content)
    }

    /// Copies an inline binary value from `source` into a fresh value
    /// record without holding the whole binary in memory: short and medium
    /// binaries are materialized (they are small by definition), while a
    /// long binary is streamed 4 KiB block at a time into new block
    /// records and indexed by a fresh list. This lets compaction and
    /// backup re-copy multi-gigabyte inline binaries in bounded memory.
    ///
    /// `sharing` decides what happens to a block that already lives in a
    /// bulk segment, and getting it wrong is silent: the copy succeeds, the
    /// store opens, the content tree serves, and only reading the binary
    /// back reveals that its blocks are not there. It has no default for
    /// that reason — every caller states which store its target is.
    pub fn copy_binary_value(
        &mut self,
        source: &dyn crate::content::provider::SegmentProvider,
        source_value: RecordIdentifier,
        sharing: BulkBlockSharing,
    ) -> Result<RecordIdentifier> {
        let length = crate::content::value::read_value_length(source, source_value)?;
        if length < MEDIUM_VALUE_LIMIT as u64 {
            // Small or medium: materializing at most ~16 KiB is fine.
            let content = crate::content::value::read_binary_content(source, source_value)?;
            return self.write_binary_content(&content);
        }

        // Long: stream the source's blocks into fresh block records.
        let block_count = length.div_ceil(BLOCK_SIZE as u64);
        let source_view = source.segment(source_value.segment)?;
        let list_identifier =
            source_view.read_record_identifier(source_value.record_number, 8, 0)?;
        let source_blocks =
            crate::content::list::uncounted_list_entries(source, list_identifier, block_count)?;

        let mut block_identifiers = Vec::with_capacity(source_blocks.len());
        let mut remaining = length;
        for source_block in source_blocks {
            let block_length = remaining.min(BLOCK_SIZE as u64) as usize;
            remaining -= block_length as u64;
            // Value sharing, as Oak does it: a block already living in a
            // bulk segment is referenced where it lies instead of being
            // copied. Bulk segments are reclaimed by reachability rather
            // than by the generation predicate, so a reference from the new
            // generation is exactly what keeps one alive — which is why the
            // rule is the segment kind and not the generation.
            //
            // A block in a *data* segment must still be copied. Those are
            // reclaimed by generation no matter who points at them, so
            // sharing one across a compaction would leave the new head
            // referencing bytes the same run then deletes. froe's own writer
            // puts blocks in data segments, so this is the path a
            // froe-authored store takes; an Oak-authored one shares.
            //
            // All of that reasoning assumes one store. Copying *between*
            // stores — backup and restore — has no such option: a reference
            // to a bulk segment that lives only in the source resolves to
            // nothing in the target, and the resulting backup boots, serves
            // its content tree and passes a consistency check that does not
            // read binaries, while having silently left the binaries behind.
            if source_block.segment.is_bulk_segment() && sharing == BulkBlockSharing::WithinOneStore
            {
                block_identifiers.push(source_block);
                continue;
            }
            let block_view = source.segment(source_block.segment)?;
            let block_bytes = block_view.read_bytes(source_block.record_number, 0, block_length)?;
            let record = self.allocate(RecordType::Block, block_length, &[])?;
            self.current.record_bytes_mut(record)[..block_length].copy_from_slice(block_bytes);
            block_identifiers.push(self.identifier_of(record));
        }

        let body =
            self.write_list_body(&block_identifiers)?
                .ok_or_else(|| Error::InvalidFormat {
                    details: "a long binary always has at least one block".to_owned(),
                })?;
        let record = self.allocate(RecordType::Value, 8 + 6, &[body])?;
        let stored = (length - MEDIUM_VALUE_LIMIT as u64) | (0b11 << 62);
        self.current.record_bytes_mut(record)[0..8].copy_from_slice(&stored.to_be_bytes());
        self.write_identifier_at(record, 8, body);
        Ok(self.identifier_of(record))
    }

    /// Writes a value record: inline below 16512 bytes, block-backed
    /// above.
    pub(super) fn write_value_bytes(&mut self, content: &[u8]) -> Result<RecordIdentifier> {
        if content.len() < SMALL_VALUE_LIMIT {
            let record = self.allocate(RecordType::Value, 1 + content.len(), &[])?;
            let bytes = self.current.record_bytes_mut(record);
            bytes[0] = content.len() as u8;
            bytes[1..=content.len()].copy_from_slice(content);
            return Ok(self.identifier_of(record));
        }
        if content.len() < MEDIUM_VALUE_LIMIT {
            let record = self.allocate(RecordType::Value, 2 + content.len(), &[])?;
            let stored = (content.len() - SMALL_VALUE_LIMIT) as u16 | 0x8000;
            let bytes = self.current.record_bytes_mut(record);
            bytes[0..2].copy_from_slice(&stored.to_be_bytes());
            bytes[2..2 + content.len()].copy_from_slice(content);
            return Ok(self.identifier_of(record));
        }

        // Long value: 4 KiB blocks — full 256 KiB runs as bulk segments,
        // the remainder as block records — indexed by a list record.
        let block_identifiers = self.write_blocks(content)?;
        let list_body =
            self.write_list_body(&block_identifiers)?
                .ok_or_else(|| Error::InvalidFormat {
                    details: "a long value always has at least one block".to_owned(),
                })?;
        let record = self.allocate(RecordType::Value, 8 + 6, &[list_body])?;
        let stored = (content.len() - MEDIUM_VALUE_LIMIT) as u64 | (0b11 << 62);
        let header = stored.to_be_bytes();
        self.current.record_bytes_mut(record)[0..8].copy_from_slice(&header);
        self.write_identifier_at(record, 8, list_body);
        Ok(self.identifier_of(record))
    }

    /// Splits `content` into 4 KiB blocks: full 256 KiB runs become bulk
    /// segments (whose record numbers are the virtual block offsets), the
    /// remainder block records in data segments.
    pub(super) fn write_blocks(&mut self, content: &[u8]) -> Result<Vec<RecordIdentifier>> {
        let mut block_identifiers = Vec::with_capacity(content.len().div_ceil(BLOCK_SIZE));
        let mut remaining = content;
        while remaining.len() >= MAXIMUM_SEGMENT_SIZE {
            let bulk_identifier = new_bulk_segment_identifier();
            let (bulk_content, rest) = remaining.split_at(MAXIMUM_SEGMENT_SIZE);
            // Bulk segments are indexed with the null generation, like
            // Oak's writer.
            self.sink.write_segment(BuiltSegment {
                identifier: bulk_identifier,
                bytes: bulk_content.to_vec(),
                generation: GarbageCollectionGeneration {
                    generation: 0,
                    full_generation: 0,
                    is_compacted: false,
                },
                referenced_segments: Vec::new(),
                binary_reference_identifiers: Vec::new(),
            })?;
            for block_offset in (0..MAXIMUM_SEGMENT_SIZE).step_by(BLOCK_SIZE) {
                block_identifiers.push(RecordIdentifier::new(bulk_identifier, block_offset as u32));
            }
            remaining = rest;
        }
        for chunk in remaining.chunks(BLOCK_SIZE) {
            let record = self.allocate(RecordType::Block, chunk.len(), &[])?;
            self.current.record_bytes_mut(record)[..chunk.len()].copy_from_slice(chunk);
            block_identifiers.push(self.identifier_of(record));
        }
        Ok(block_identifiers)
    }

    /// Writes an external binary identifier record.
    pub fn write_external_binary_identifier(
        &mut self,
        blob_identifier: &str,
    ) -> Result<RecordIdentifier> {
        let encoded = blob_identifier.as_bytes();
        if encoded.len() < BLOB_IDENTIFIER_SMALL_LIMIT {
            let record =
                self.allocate(RecordType::ExternalBlobIdentifier, 2 + encoded.len(), &[])?;
            // Registered after allocation: allocation may roll the segment,
            // and the reference belongs to the segment holding the record.
            self.current
                .register_binary_reference(blob_identifier.to_owned());
            let stored = 0xE000u16 | encoded.len() as u16;
            let bytes = self.current.record_bytes_mut(record);
            bytes[0..2].copy_from_slice(&stored.to_be_bytes());
            bytes[2..2 + encoded.len()].copy_from_slice(encoded);
            return Ok(self.identifier_of(record));
        }
        let string_identifier = self.write_string(blob_identifier)?;
        let record = self.allocate(
            RecordType::ExternalBlobIdentifier,
            1 + 6,
            &[string_identifier],
        )?;
        self.current
            .register_binary_reference(blob_identifier.to_owned());
        self.current.record_bytes_mut(record)[0] = 0xF0;
        self.write_identifier_at(record, 1, string_identifier);
        Ok(self.identifier_of(record))
    }
}

#[cfg(test)]
mod tests {
    use crate::content::value::{BinaryValue, read_binary_value, read_string};
    use crate::segment::record::RecordIdentifier;
    use crate::writer::record_writer::test_support::new_writer;

    #[test]
    fn strings_round_trip_at_every_size_class() {
        let mut writer = new_writer();
        let small = "hello".to_owned();
        let boundary_small = "x".repeat(127);
        let medium = "y".repeat(4000);
        let boundary_medium = "m".repeat(16511);
        let long = "z".repeat(20_000);
        let identifiers: Vec<(String, RecordIdentifier)> =
            [small, boundary_small, medium, boundary_medium, long]
                .into_iter()
                .map(|text| {
                    let identifier = writer.write_string(&text).expect("write");
                    (text, identifier)
                })
                .collect();
        let store = writer.finish().expect("finish");
        for (text, identifier) in identifiers {
            assert_eq!(read_string(&store, identifier).expect("read"), text);
        }
    }

    #[test]
    fn huge_values_use_bulk_segments() {
        let mut writer = new_writer();
        // 600 KiB: two full bulk segments plus trailing blocks.
        let content: Vec<u8> = (0..600 * 1024).map(|index| (index % 251) as u8).collect();
        let identifier = writer.write_binary_content(&content).expect("write");
        let store = writer.finish().expect("finish");

        let bulk_count = store
            .write_order
            .iter()
            .filter(|segment| segment.is_bulk_segment())
            .count();
        assert_eq!(bulk_count, 2, "two full 256 KiB runs become bulk segments");

        match read_binary_value(&store, identifier).expect("classify") {
            BinaryValue::Inline { length, .. } => assert_eq!(length, content.len() as u64),
            BinaryValue::External { .. } => panic!("inline binary expected"),
        }
        assert_eq!(
            crate::content::value::read_binary_content(&store, identifier).expect("content"),
            content
        );
    }

    #[test]
    fn external_binary_identifiers_round_trip() {
        let mut writer = new_writer();
        let short = writer
            .write_external_binary_identifier("datastore-0001")
            .expect("short");
        let long_identifier_text = "reference-".repeat(500);
        let long = writer
            .write_external_binary_identifier(&long_identifier_text)
            .expect("long");
        let store = writer.finish().expect("finish");

        assert_eq!(
            read_binary_value(&store, short).expect("read"),
            BinaryValue::External {
                blob_identifier: "datastore-0001".to_owned()
            }
        );
        assert_eq!(
            read_binary_value(&store, long).expect("read"),
            BinaryValue::External {
                blob_identifier: long_identifier_text
            }
        );
    }
}
