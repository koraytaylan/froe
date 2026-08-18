//! Writing records: values, lists, maps, templates, and nodes.
//!
//! The record writer serializes content into segments, rolling over to a
//! fresh segment whenever the current one fills up. Finished segments go
//! to a [`SegmentSink`] — the writable store in production, an in-memory
//! collector in tests.
//!
//! Layout choices mirror Oak's writer:
//!
//! * long values are split into 4 KiB blocks — full 256 KiB runs become
//!   *bulk segments*, the remainder becomes block records in data
//!   segments — indexed by a list record;
//! * lists chunk into buckets of at most 255 identifiers, recursively;
//! * maps become a leaf for up to 32 entries (or at trie level 7) and a
//!   branch of hash-selected buckets otherwise, with entries sorted by
//!   unsigned scrambled hash and ties broken in Java's UTF-16 string
//!   order;
//! * every map, template, and node layout is byte-identical to what the
//!   reader in [`crate::content`] parses, which the round-trip tests
//!   assert.

use crate::cache::BoundedCache;
use crate::content::property::PropertyType;
use crate::error::{Error, Result};
use crate::hashing::{compare_utf16_strings, map_entry_hash};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::MAXIMUM_SEGMENT_SIZE;
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::writer::identifier_generator::{
    new_bulk_segment_identifier, new_data_segment_identifier,
};
use crate::writer::segment_builder::{
    BuiltSegment, GarbageCollectionGeneration, SegmentBufferBuilder,
};

mod collections;
mod nodes;
#[cfg(test)]
mod test_support;
mod values;

pub use nodes::*;
pub(crate) use values::*;

/// Receives finished segments in write order.
pub trait SegmentSink {
    /// Persists one finished segment. Segments arrive in reference order:
    /// a segment is written only after every segment it references.
    fn write_segment(&mut self, segment: BuiltSegment) -> Result<()>;
}

/// Writes records into segments, rolling over as segments fill.
pub struct RecordWriter<Sink: SegmentSink> {
    pub(crate) sink: Sink,
    pub(crate) generation: GarbageCollectionGeneration,
    pub(crate) writer_identifier: String,
    pub(crate) segment_sequence: u32,
    pub(crate) current: SegmentBufferBuilder,
    /// Value records already written by this writer, keyed by their bytes.
    ///
    /// Oak's `SegmentWriter` carries the same dedup (`WriterCacheManager`,
    /// 15000 strings / 3000 templates by default) and a port without it
    /// writes a fresh record for every repetition. On a real tree that is
    /// enormous amplification: a primary type takes a handful of distinct
    /// values across millions of nodes, and each one was becoming its own
    /// record, its own bytes, and eventually its own segment.
    ///
    /// Reuse is a plain cross-segment reference — the same thing a node
    /// makes to a child in an earlier segment — and the buffer accounts for
    /// the added reference before it commits, rolling the segment when the
    /// table is full. A miss simply writes the record again, so any budget
    /// including zero stays correct.
    pub(crate) value_cache: BoundedCache<Vec<u8>, RecordIdentifier>,
    /// Template records already written by this writer, keyed by shape.
    pub(crate) template_cache: BoundedCache<TemplateKey, RecordIdentifier>,
}

/// Whether a binary copy may leave bulk-segment blocks where they lie.
///
/// Oak shares bulk blocks by reference during compaction because bulk
/// segments are reclaimed by reachability rather than by the generation
/// predicate, so a reference from the new generation is exactly what keeps
/// one alive. That is a property of one store, not of copying in general.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BulkBlockSharing {
    /// Source and target are the same store, so a block already in a bulk
    /// segment is referenced where it lies. This is compaction.
    WithinOneStore,
    /// Source and target are different stores, so every block is copied.
    /// A reference into the source's bulk segments would not resolve in
    /// the target. This is backup and restore.
    AcrossStores,
}

impl<Sink: SegmentSink> RecordWriter<Sink> {
    /// Creates a writer stamping `generation` on every produced segment.
    #[must_use]
    pub fn new(sink: Sink, generation: GarbageCollectionGeneration) -> Self {
        Self::with_writer_identifier(sink, generation, "froe")
    }

    /// Creates a writer with an explicit writer identifier, recorded in
    /// each segment's info string.
    #[must_use]
    pub fn with_writer_identifier(
        sink: Sink,
        generation: GarbageCollectionGeneration,
        writer_identifier: &str,
    ) -> Self {
        let mut writer = Self {
            sink,
            generation,
            writer_identifier: writer_identifier.to_owned(),
            segment_sequence: 0,
            current: SegmentBufferBuilder::new(new_data_segment_identifier(), generation),
            value_cache: BoundedCache::new(VALUE_DEDUP_BUDGET_BYTES),
            template_cache: BoundedCache::new(TEMPLATE_DEDUP_BUDGET_BYTES),
        };
        writer.write_segment_info_record();
        writer
    }

    /// The sink, for inspection after writing.
    #[must_use]
    pub fn sink(&self) -> &Sink {
        &self.sink
    }

    /// Consumes the writer, flushing the current segment when it holds
    /// content beyond the segment-info record, and returns the sink.
    pub fn finish(mut self) -> Result<Sink> {
        self.flush_current_segment()?;
        Ok(self.sink)
    }

    /// Flushes the segment under construction to the sink when it holds
    /// content records — a segment with only the info record is not
    /// written.
    pub fn flush_current_segment(&mut self) -> Result<()> {
        if self.current.record_count() <= 1 {
            return Ok(());
        }
        let mut fresh = SegmentBufferBuilder::new(new_data_segment_identifier(), self.generation);
        std::mem::swap(&mut fresh, &mut self.current);
        self.write_segment_info_record();
        self.sink.write_segment(fresh.finish())
    }

    /// Writes the segment-info string as record 0 of the current builder:
    /// `{"wid":"<id>","sno":<sequence>,"t":<milliseconds>}`. Diagnostic
    /// only, but Oak guarantees every data segment's first record is a
    /// string, and tooling relies on it.
    pub(crate) fn write_segment_info_record(&mut self) {
        let milliseconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let info = format!(
            "{{\"wid\":\"{}\",\"sno\":{},\"t\":{milliseconds}}}",
            self.writer_identifier, self.segment_sequence
        );
        self.segment_sequence = self.segment_sequence.wrapping_add(1);
        let bytes = info.as_bytes();
        // Short identifiers keep the info string under the 128-byte small
        // limit; a pathologically long identifier falls back to the medium
        // encoding so record 0 is always a valid string.
        if bytes.len() < SMALL_VALUE_LIMIT {
            let record = self
                .current
                .allocate(RecordType::Value, 1 + bytes.len(), &[])
                .expect("the segment-info record always fits an empty segment");
            let target = self.current.record_bytes_mut(record);
            target[0] = bytes.len() as u8;
            target[1..=bytes.len()].copy_from_slice(bytes);
        } else {
            let truncated = &bytes[..bytes.len().min(MEDIUM_VALUE_LIMIT - 1)];
            let record = self
                .current
                .allocate(RecordType::Value, 2 + truncated.len(), &[])
                .expect("the segment-info record always fits an empty segment");
            let stored = (truncated.len() - SMALL_VALUE_LIMIT) as u16 | 0x8000;
            let target = self.current.record_bytes_mut(record);
            target[0..2].copy_from_slice(&stored.to_be_bytes());
            target[2..2 + truncated.len()].copy_from_slice(truncated);
        }
    }

    /// Allocates a record, rolling to a fresh segment when full.
    pub(crate) fn allocate(
        &mut self,
        record_type: RecordType,
        size: usize,
        referenced: &[RecordIdentifier],
    ) -> Result<u32> {
        let referenced_segments: Vec<SegmentIdentifier> = referenced
            .iter()
            .map(|identifier| identifier.segment)
            .collect();
        if let Ok(record_number) = self
            .current
            .allocate(record_type, size, &referenced_segments)
        {
            return Ok(record_number);
        }
        self.flush_current_segment()?;
        self.current
            .allocate(record_type, size, &referenced_segments)
            .map_err(|_| Error::InvalidFormat {
                details: format!("record of {size} bytes cannot fit even an empty segment"),
            })
    }

    /// Serializes `identifier` into six bytes of the current segment's
    /// record `record_number` at `offset`.
    pub(crate) fn write_identifier_at(
        &mut self,
        record_number: u32,
        offset: usize,
        identifier: RecordIdentifier,
    ) {
        let reference = self.current.reference_for(identifier.segment);
        let bytes = self.current.record_bytes_mut(record_number);
        let target: &mut [u8; 6] = (&mut bytes[offset..offset + 6])
            .try_into()
            .expect("the slice is exactly six bytes");
        SegmentBufferBuilder::write_record_identifier_bytes(
            reference,
            identifier.record_number,
            target,
        );
    }

    /// The identifier of a record in the segment under construction.
    pub(crate) fn identifier_of(&self, record_number: u32) -> RecordIdentifier {
        RecordIdentifier::new(self.current.identifier(), record_number)
    }
}

#[cfg(test)]
mod tests {
    use super::RecordWriter;
    use crate::content::value::read_string;
    use crate::segment::record::RecordIdentifier;
    use crate::writer::record_writer::nodes::ChildNodesToWrite;
    use crate::writer::record_writer::test_support::{MemoryStore, new_writer};

    #[test]
    fn a_repeated_string_or_template_reuses_the_record_it_already_wrote() {
        // Oak's writer dedups both (WriterCacheManager, 15000 strings /
        // 3000 templates). Without it every node wrote its own copy of a
        // shape a whole tree shares, which is the write path's largest
        // source of amplification. A miss is still correct — it writes the
        // record again — so this pins the reuse rather than any byte layout.
        let mut writer = new_writer();

        let first = writer.write_string("nt:unstructured").expect("first");
        let second = writer.write_string("nt:unstructured").expect("second");
        assert_eq!(first, second, "an identical value reuses its record");

        let distinct = writer.write_string("cq:Page").expect("distinct");
        assert_ne!(first, distinct, "a different value gets its own record");

        let shape = |writer: &mut RecordWriter<MemoryStore>| {
            writer
                .write_template(
                    Some("nt:unstructured"),
                    &["mix:versionable".to_owned()],
                    &ChildNodesToWrite::Zero,
                    &[],
                )
                .expect("template")
        };
        let first_template = shape(&mut writer);
        let second_template = shape(&mut writer);
        assert_eq!(
            first_template, second_template,
            "an identical shape reuses its template record"
        );

        let other_template = writer
            .write_template(Some("cq:Page"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("other template");
        assert_ne!(
            first_template, other_template,
            "a different shape gets its own template record"
        );
    }

    #[test]
    fn writing_rolls_over_across_segments() {
        let mut writer = new_writer();
        // Each medium string consumes ~16 KiB, so this forces rollover.
        let identifiers: Vec<(String, RecordIdentifier)> = (0..40)
            .map(|index| {
                let text = format!("{index:04}").repeat(4000);
                let identifier = writer.write_string(&text).expect("write");
                (text, identifier)
            })
            .collect();
        let store = writer.finish().expect("finish");
        assert!(
            store.write_order.len() > 1,
            "forty 16 KiB strings cannot fit one segment"
        );
        for (text, identifier) in identifiers {
            assert_eq!(read_string(&store, identifier).expect("read"), text);
        }
    }
}
