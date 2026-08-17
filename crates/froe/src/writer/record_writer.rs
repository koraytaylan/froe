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

/// The block size long values are split into.
const BLOCK_SIZE: usize = 4096;

/// Byte budgets for the writer's record-reuse caches.
///
/// Oak sizes the equivalents by entry count — 15000 strings, 3000 templates
/// (`WriterCacheManager`). Bytes rather than entries here, matching the rest
/// of froe's caches, and generous because the whole point is to hold the
/// repeated vocabulary of a tree rather than a sample of it.
const VALUE_DEDUP_BUDGET_BYTES: usize = 32 * 1024 * 1024;
const TEMPLATE_DEDUP_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// The largest value worth remembering for reuse. Repetition lives in short
/// values; a long one neither repeats nor earns its place in the budget.
const MAXIMUM_DEDUPLICATED_VALUE_BYTES: usize = 1024;

/// Lengths below this use the one-byte small value encoding.
const SMALL_VALUE_LIMIT: usize = 128;

/// Lengths below this use the two-byte medium value encoding.
/// The length at or above which a binary is stored as a block list rather
/// than materialized. Read by the reclamation prediction, which must apply the
/// same threshold `copy_binary_value` does or its answer is wrong.
pub(crate) const MEDIUM_VALUE_LIMIT: usize = (1 << 14) + 128;

/// Maximum identifiers per list bucket.
const LIST_BUCKET_CAPACITY: usize = 255;

/// Maximum entries in a map leaf below the deepest trie level.
const MAP_LEAF_CAPACITY: usize = 32;

/// The deepest map trie level; records at this level are always leaves.
const MAP_MAXIMUM_LEVEL: u32 = 7;

/// External blob identifiers below this length are stored inline.
const BLOB_IDENTIFIER_SMALL_LIMIT: usize = 4096;

/// Receives finished segments in write order.
pub trait SegmentSink {
    /// Persists one finished segment. Segments arrive in reference order:
    /// a segment is written only after every segment it references.
    fn write_segment(&mut self, segment: BuiltSegment) -> Result<()>;
}

/// A property of a node to be written.
pub struct PropertyToWrite {
    /// The property name.
    pub name: String,
    /// The property type.
    pub property_type: PropertyType,
    /// The value record identifiers: one for a single-valued property,
    /// written as a counted list for a multi-valued one.
    pub values: PropertyValuesToWrite,
}

/// The written shape of a property's values.
pub enum PropertyValuesToWrite {
    /// A single value record.
    Single(RecordIdentifier),
    /// Multi-valued: any number of value records.
    Multiple(Vec<RecordIdentifier>),
    /// A slot preserved from an existing node: for single values the
    /// value record, for multi values the existing counted list record.
    PreservedSlot {
        /// The record the node's property value list will point at.
        value_slot: RecordIdentifier,
        /// Whether the preserved property is multi-valued (drives the
        /// template's type byte sign).
        is_multiple: bool,
    },
}

/// The child node shape of a node to be written.
pub enum ChildNodesToWrite {
    /// No children.
    Zero,
    /// One child with the given name and node record.
    One {
        /// The child's name.
        name: String,
        /// The child's node record.
        node: RecordIdentifier,
    },
    /// Many children as `(name, node record)` pairs.
    Many(Vec<(String, RecordIdentifier)>),
    /// Many children through an existing, unchanged map record.
    ManyExistingMap(RecordIdentifier),
}

/// Writes records into segments, rolling over as segments fill.
pub struct RecordWriter<Sink: SegmentSink> {
    sink: Sink,
    generation: GarbageCollectionGeneration,
    writer_identifier: String,
    segment_sequence: u32,
    current: SegmentBufferBuilder,
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
    value_cache: BoundedCache<Vec<u8>, RecordIdentifier>,
    /// Template records already written by this writer, keyed by shape.
    template_cache: BoundedCache<TemplateKey, RecordIdentifier>,
}

/// The identity of a template: everything that decides its serialized form.
///
/// Two nodes share a template when their primary type, mixins, child-node
/// arity and property slots agree; the property *values* differ per node and
/// are not part of it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct TemplateKey {
    primary_type: Option<String>,
    mixin_types: Vec<String>,
    child_arity: u8,
    single_child_name: Option<String>,
    properties: Vec<(String, u8)>,
}

impl TemplateKey {
    fn of(
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
    ) -> Self {
        // Only the arity and, for the single-child case, the child's name
        // are serialized into a template; which node it points at is part of
        // the node record, not the shape.
        let (child_arity, single_child_name) = match child_nodes {
            ChildNodesToWrite::Zero => (0u8, None),
            ChildNodesToWrite::One { name, .. } => (1u8, Some(name.clone())),
            ChildNodesToWrite::Many(_) | ChildNodesToWrite::ManyExistingMap(_) => (2u8, None),
        };
        Self {
            primary_type: primary_type.map(str::to_owned),
            mixin_types: mixin_types.to_vec(),
            child_arity,
            single_child_name,
            properties: properties
                .iter()
                .map(|property| {
                    (
                        property.name.clone(),
                        property_slot_tag(property.property_type, &property.values),
                    )
                })
                .collect(),
        }
    }
}

/// The per-slot type byte a template records: the property type, negated
/// when the slot is multi-valued, exactly as the serialized form encodes it.
///
/// This is the single source of truth for that byte. Both
/// [`TemplateKey::of`], which decides whether two nodes may share a
/// template record, and `write_template_record`, which serializes the
/// byte, go through it — because if they disagree, two nodes whose slots
/// differ *only* in arity hash to the same key, the second silently
/// inherits the first one's template, and its values are then decoded at
/// the wrong arity: a single value record read as a counted list, or a
/// counted list read as one value. Nothing rejects the result. The store
/// parses, Oak boots, and the property is quietly wrong.
///
/// The match is exhaustive rather than a `matches!` against one variant so
/// that a new [`PropertyValuesToWrite`] variant is a compile error here
/// instead of defaulting to single-valued.
fn property_slot_tag(property_type: PropertyType, values: &PropertyValuesToWrite) -> u8 {
    let multiple = match values {
        PropertyValuesToWrite::Single(_) => false,
        PropertyValuesToWrite::Multiple(_) => true,
        PropertyValuesToWrite::PreservedSlot { is_multiple, .. } => *is_multiple,
    };
    let tag = property_type as i8;
    (if multiple { -tag } else { tag }) as u8
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
    fn write_segment_info_record(&mut self) {
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
    fn allocate(
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
    fn write_identifier_at(
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
    fn identifier_of(&self, record_number: u32) -> RecordIdentifier {
        RecordIdentifier::new(self.current.identifier(), record_number)
    }

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
    fn write_deduplicated_value(&mut self, bytes: &[u8]) -> Result<RecordIdentifier> {
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
    pub fn copy_binary_value(
        &mut self,
        source: &dyn crate::content::provider::SegmentProvider,
        source_value: RecordIdentifier,
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
            if source_block.segment.is_bulk_segment() {
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
    fn write_value_bytes(&mut self, content: &[u8]) -> Result<RecordIdentifier> {
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
    fn write_blocks(&mut self, content: &[u8]) -> Result<Vec<RecordIdentifier>> {
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

    /// Writes the body of an uncounted list: `None` for an empty list,
    /// the single element itself for one entry, and a bucket tree above.
    pub fn write_list_body(
        &mut self,
        identifiers: &[RecordIdentifier],
    ) -> Result<Option<RecordIdentifier>> {
        if identifiers.is_empty() {
            return Ok(None);
        }
        let mut level: Vec<RecordIdentifier> = identifiers.to_vec();
        while level.len() > 1 {
            let mut next_level = Vec::with_capacity(level.len().div_ceil(LIST_BUCKET_CAPACITY));
            for chunk in level.chunks(LIST_BUCKET_CAPACITY) {
                if chunk.len() == 1 {
                    // Single-element chunks pass through unwrapped.
                    next_level.push(chunk[0]);
                    continue;
                }
                let record = self.allocate(RecordType::ListBucket, chunk.len() * 6, chunk)?;
                for (position, identifier) in chunk.iter().enumerate() {
                    self.write_identifier_at(record, position * 6, *identifier);
                }
                next_level.push(self.identifier_of(record));
            }
            level = next_level;
        }
        Ok(Some(level[0]))
    }

    /// Writes a counted list record: a size prefix plus the body pointer
    /// when non-empty.
    pub fn write_counted_list(
        &mut self,
        identifiers: &[RecordIdentifier],
    ) -> Result<RecordIdentifier> {
        let body = self.write_list_body(identifiers)?;
        match body {
            None => {
                let record = self.allocate(RecordType::List, 4, &[])?;
                self.current.record_bytes_mut(record)[0..4].copy_from_slice(&0u32.to_be_bytes());
                Ok(self.identifier_of(record))
            }
            Some(body) => {
                let record = self.allocate(RecordType::List, 4 + 6, &[body])?;
                let count = (identifiers.len() as u32).to_be_bytes();
                self.current.record_bytes_mut(record)[0..4].copy_from_slice(&count);
                self.write_identifier_at(record, 4, body);
                Ok(self.identifier_of(record))
            }
        }
    }

    /// Writes a child map: keys become string records, the structure a
    /// hash trie of leaf and branch records. Fails on duplicate names and
    /// on maps of `MapRecord.MAX_SIZE` entries or more — Java's writer
    /// enforces both (its `Map`-typed API makes duplicates impossible),
    /// and packing a larger size would silently corrupt the head's level
    /// bits.
    pub fn write_map(
        &mut self,
        entries: &[(String, RecordIdentifier)],
    ) -> Result<RecordIdentifier> {
        // Java: checkIndex(size, MapRecord.MAX_SIZE) with
        // MAX_SIZE = (1 << 29) - 1, so size == MAX_SIZE is already
        // rejected before any head word is packed.
        if entries.len() >= (1 << 29) - 1 {
            return Err(Error::InvalidFormat {
                details: format!("a child map of {} entries exceeds MAX_SIZE", entries.len()),
            });
        }
        let mut prepared: Vec<(u32, String, RecordIdentifier, RecordIdentifier)> =
            Vec::with_capacity(entries.len());
        let mut names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (name, value) in entries {
            if !names.insert(name.as_str()) {
                return Err(Error::InvalidFormat {
                    details: format!("duplicate child name {name:?} in a map"),
                });
            }
            let key_identifier = self.write_string(name)?;
            prepared.push((map_entry_hash(name), name.clone(), key_identifier, *value));
        }
        self.write_map_bucket(&mut prepared, 0)
    }

    /// Writes one map trie level: a leaf for small buckets or the deepest
    /// level, a branch of sub-buckets otherwise.
    fn write_map_bucket(
        &mut self,
        entries: &mut [(u32, String, RecordIdentifier, RecordIdentifier)],
        level: u32,
    ) -> Result<RecordIdentifier> {
        if entries.len() <= MAP_LEAF_CAPACITY || level == MAP_MAXIMUM_LEVEL {
            return self.write_map_leaf(entries, level);
        }
        // Partition by five hash bits at this level (Java's masked shift).
        let shift = (32i32 - (level as i32 + 1) * 5) & 31;
        let mut buckets: Vec<Vec<(u32, String, RecordIdentifier, RecordIdentifier)>> =
            vec![Vec::new(); 32];
        for entry in entries.iter() {
            let bucket_index = (((entry.0 as i32) >> shift) & 0x1F) as usize;
            buckets[bucket_index].push(entry.clone());
        }
        let mut bitmap = 0u32;
        let mut bucket_identifiers = Vec::new();
        for (bucket_index, mut bucket) in buckets.into_iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            bitmap |= 1 << bucket_index;
            bucket_identifiers.push(self.write_map_bucket(&mut bucket, level + 1)?);
        }
        let record = self.allocate(
            RecordType::MapBranch,
            4 + 4 + bucket_identifiers.len() * 6,
            &bucket_identifiers,
        )?;
        let head = (level << 29) | entries.len() as u32;
        {
            let bytes = self.current.record_bytes_mut(record);
            bytes[0..4].copy_from_slice(&head.to_be_bytes());
            bytes[4..8].copy_from_slice(&bitmap.to_be_bytes());
        }
        for (position, identifier) in bucket_identifiers.iter().enumerate() {
            self.write_identifier_at(record, 8 + position * 6, *identifier);
        }
        Ok(self.identifier_of(record))
    }

    /// Writes a map leaf: sorted hashes, then interleaved key and value
    /// identifiers.
    fn write_map_leaf(
        &mut self,
        entries: &mut [(u32, String, RecordIdentifier, RecordIdentifier)],
        level: u32,
    ) -> Result<RecordIdentifier> {
        entries.sort_by(|first, second| {
            first
                .0
                .cmp(&second.0)
                .then_with(|| compare_utf16_strings(&first.1, &second.1))
        });
        let all_identifiers: Vec<RecordIdentifier> = entries
            .iter()
            .flat_map(|entry| [entry.2, entry.3])
            .collect();
        let record = self.allocate(
            RecordType::MapLeaf,
            4 + entries.len() * 4 + entries.len() * 12,
            &all_identifiers,
        )?;
        let head = (level << 29) | entries.len() as u32;
        self.current.record_bytes_mut(record)[0..4].copy_from_slice(&head.to_be_bytes());
        for (position, entry) in entries.iter().enumerate() {
            let hash = entry.0.to_be_bytes();
            self.current.record_bytes_mut(record)[4 + position * 4..8 + position * 4]
                .copy_from_slice(&hash);
        }
        let identifiers_base = 4 + entries.len() * 4;
        for (position, entry) in entries.iter().enumerate() {
            self.write_identifier_at(record, identifiers_base + position * 12, entry.2);
            self.write_identifier_at(record, identifiers_base + position * 12 + 6, entry.3);
        }
        Ok(self.identifier_of(record))
    }

    /// Writes a template record. `properties` must already be in on-disk
    /// order (see [`sort_properties_for_template`]).
    #[allow(
        clippy::missing_panics_doc,
        reason = "record slice indexing is in-bounds by construction of the allocation"
    )]
    pub fn write_template(
        &mut self,
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
    ) -> Result<RecordIdentifier> {
        // A template is the *shape* of a node, and a tree has far fewer
        // shapes than nodes — Oak caps its own template cache at 3000 for
        // that reason. Without this every node wrote its own copy, which on
        // a large repository is the single largest source of write
        // amplification in the whole path.
        let key = TemplateKey::of(primary_type, mixin_types, child_nodes, properties);
        if let Some(existing) = self.template_cache.get(&key) {
            return Ok(existing);
        }
        let written =
            self.write_template_record(primary_type, mixin_types, child_nodes, properties)?;
        self.template_cache.insert(key, written);
        Ok(written)
    }

    fn write_template_record(
        &mut self,
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
    ) -> Result<RecordIdentifier> {
        if mixin_types.len() >= 1 << 10 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} mixin types exceed the template limit of 1023",
                    mixin_types.len()
                ),
            });
        }
        if properties.len() >= 1 << 18 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} properties exceed the template limit of 262143",
                    properties.len()
                ),
            });
        }
        let mut head = 0u32;
        let mut trailing_identifiers: Vec<RecordIdentifier> = Vec::new();

        let primary_type_identifier = match primary_type {
            Some(name) => {
                head |= 1 << 31;
                Some(self.write_string(name)?)
            }
            None => None,
        };
        let mut mixin_identifiers = Vec::with_capacity(mixin_types.len());
        if !mixin_types.is_empty() {
            head |= 1 << 30;
            head |= (mixin_types.len() as u32) << 18;
            for mixin in mixin_types {
                mixin_identifiers.push(self.write_string(mixin)?);
            }
        }
        let single_child_name_identifier = match child_nodes {
            ChildNodesToWrite::Zero => {
                head |= 1 << 29;
                None
            }
            ChildNodesToWrite::Many(_) | ChildNodesToWrite::ManyExistingMap(_) => {
                head |= 1 << 28;
                None
            }
            ChildNodesToWrite::One { name, .. } => Some(self.write_string(name)?),
        };
        head |= properties.len() as u32;

        let property_names_identifier = if properties.is_empty() {
            None
        } else {
            let mut name_identifiers = Vec::with_capacity(properties.len());
            for property in properties {
                name_identifiers.push(self.write_string(&property.name)?);
            }
            Some(
                self.write_list_body(&name_identifiers)?
                    .expect("non-empty list"),
            )
        };

        trailing_identifiers.extend(primary_type_identifier);
        trailing_identifiers.extend(mixin_identifiers.iter().copied());
        trailing_identifiers.extend(single_child_name_identifier);
        trailing_identifiers.extend(property_names_identifier);

        let size = 4 + trailing_identifiers.len() * 6 + properties.len();
        let record = self.allocate(RecordType::Template, size, &trailing_identifiers)?;
        self.current.record_bytes_mut(record)[0..4].copy_from_slice(&head.to_be_bytes());
        let mut cursor = 4;
        for identifier in &trailing_identifiers {
            self.write_identifier_at(record, cursor, *identifier);
            cursor += 6;
        }
        for property in properties {
            self.current.record_bytes_mut(record)[cursor] =
                property_slot_tag(property.property_type, &property.values);
            cursor += 1;
        }
        Ok(self.identifier_of(record))
    }

    /// Writes a node record with its template, child structure, and
    /// property values. Properties must be in template order.
    #[allow(
        clippy::missing_panics_doc,
        reason = "record slice indexing is in-bounds by construction of the allocation"
    )]
    pub fn write_node(
        &mut self,
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
    ) -> Result<RecordIdentifier> {
        self.write_node_with_stable_identifier(
            primary_type,
            mixin_types,
            child_nodes,
            properties,
            None,
        )
    }

    /// Writes a node, preserving an existing stable identifier when one is
    /// given. A stable identifier survives compaction: when it differs
    /// from the node's own record identifier, it is stored as a 20-byte
    /// block (`msb`, `lsb`, record number) and slot 0 points at it;
    /// otherwise slot 0 is a self reference. Preserving it lets Oak's
    /// stable-identifier fast path keep matching a node across rewrites.
    ///
    /// # Panics
    ///
    /// Panics only on internal allocation invariants, never on input.
    #[allow(
        clippy::missing_panics_doc,
        reason = "record slice indexing is in-bounds by construction of the allocation"
    )]
    pub fn write_node_with_stable_identifier(
        &mut self,
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
        stable_identifier: Option<[u8; 20]>,
    ) -> Result<RecordIdentifier> {
        let template_identifier =
            self.write_template(primary_type, mixin_types, child_nodes, properties)?;

        let child_identifier = match child_nodes {
            ChildNodesToWrite::Zero => None,
            ChildNodesToWrite::One { node, .. } => Some(*node),
            ChildNodesToWrite::Many(entries) => Some(self.write_map(entries)?),
            ChildNodesToWrite::ManyExistingMap(map) => Some(*map),
        };

        let property_list_identifier = if properties.is_empty() {
            None
        } else {
            let mut value_identifiers = Vec::with_capacity(properties.len());
            for property in properties {
                let identifier = match &property.values {
                    PropertyValuesToWrite::Single(value) => *value,
                    PropertyValuesToWrite::Multiple(values) => self.write_counted_list(values)?,
                    PropertyValuesToWrite::PreservedSlot { value_slot, .. } => *value_slot,
                };
                value_identifiers.push(identifier);
            }
            Some(
                self.write_list_body(&value_identifiers)?
                    .expect("non-empty list"),
            )
        };

        // A preserved stable identifier is stored as a 20-byte block that
        // slot 0 references, unless it happens to name the node itself.
        let stable_block = match stable_identifier {
            Some(bytes) => {
                let record = self.allocate(RecordType::Block, 20, &[])?;
                self.current.record_bytes_mut(record)[..20].copy_from_slice(&bytes);
                Some(self.identifier_of(record))
            }
            None => None,
        };

        let mut slots: Vec<RecordIdentifier> = vec![template_identifier];
        slots.extend(child_identifier);
        slots.extend(property_list_identifier);

        let mut referenced = slots.clone();
        referenced.extend(stable_block);
        let size = 6 + slots.len() * 6;
        let record = self.allocate(RecordType::Node, size, &referenced)?;
        let own_identifier = self.identifier_of(record);
        // Slot 0: the preserved stable-id block, or a self reference.
        let slot_zero = match stable_block {
            Some(block) => {
                // A stable identifier equal to the node's own record would
                // be redundant; the self-reference marker covers it.
                if stable_identifier_names(stable_identifier, own_identifier) {
                    own_identifier
                } else {
                    block
                }
            }
            None => own_identifier,
        };
        self.write_identifier_at(record, 0, slot_zero);
        for (position, identifier) in slots.iter().enumerate() {
            self.write_identifier_at(record, 6 + position * 6, *identifier);
        }
        Ok(own_identifier)
    }
}

/// Whether a 20-byte stable identifier names exactly `record`.
fn stable_identifier_names(stable_identifier: Option<[u8; 20]>, record: RecordIdentifier) -> bool {
    let Some(bytes) = stable_identifier else {
        return false;
    };
    let most = u64::from_be_bytes(bytes[0..8].try_into().expect("8 bytes"));
    let least = u64::from_be_bytes(bytes[8..16].try_into().expect("8 bytes"));
    let number = u32::from_be_bytes(bytes[16..20].try_into().expect("4 bytes"));
    most == record.segment.most_significant_bits
        && least == record.segment.least_significant_bits
        && number == record.record_number
}

/// Sorts properties into the on-disk template order: by Java string hash
/// of the name, then by name in UTF-16 order, then by type tag — the
/// order `Template`'s constructor establishes in Java.
pub fn sort_properties_for_template(properties: &mut [PropertyToWrite]) {
    properties.sort_by(|first, second| {
        crate::hashing::utf16_string_hash(&first.name)
            .cmp(&crate::hashing::utf16_string_hash(&second.name))
            .then_with(|| compare_utf16_strings(&first.name, &second.name))
            .then_with(|| (first.property_type as u8).cmp(&(second.property_type as u8)))
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

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

    use super::{
        ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
        sort_properties_for_template,
    };
    use crate::content::list::read_counted_list;
    use crate::content::map::{map_entries, map_entry};
    use crate::content::node::{NodeState, PropertyValues};
    use crate::content::property::{PropertyType, PropertyValue};
    use crate::content::provider::SegmentProvider;
    use crate::content::template::{Template, read_template};
    use crate::content::value::{BinaryValue, read_binary_value, read_string};
    use crate::error::{Error, Result};
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::parsed_segment::ParsedSegment;
    use crate::segment::record::RecordIdentifier;
    use crate::segment::view::SegmentView;
    use crate::writer::segment_builder::{BuiltSegment, GarbageCollectionGeneration};

    /// Collects written segments and serves them back as a provider, so
    /// every test reads its output through the production reader.
    #[derive(Default)]
    struct MemoryStore {
        segments: HashMap<SegmentIdentifier, (Arc<ParsedSegment>, Vec<u8>)>,
        write_order: Vec<SegmentIdentifier>,
    }

    impl SegmentSink for MemoryStore {
        fn write_segment(&mut self, segment: BuiltSegment) -> Result<()> {
            let parsed = Arc::new(ParsedSegment::parse(segment.identifier, &segment.bytes)?);
            self.segments
                .insert(segment.identifier, (parsed, segment.bytes));
            self.write_order.push(segment.identifier);
            Ok(())
        }
    }

    impl SegmentProvider for MemoryStore {
        fn segment(&self, segment_identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
            let (structure, bytes) = self
                .segments
                .get(&segment_identifier)
                .ok_or(Error::SegmentNotFound { segment_identifier })?;
            Ok(SegmentView {
                structure: Arc::clone(structure),
                bytes: bytes.as_slice().into(),
            })
        }

        fn string(&self, record_identifier: RecordIdentifier) -> Result<Arc<str>> {
            read_string(self, record_identifier).map(Arc::from)
        }

        fn template(&self, record_identifier: RecordIdentifier) -> Result<Arc<Template>> {
            read_template(self, record_identifier).map(Arc::new)
        }
    }

    fn test_generation() -> GarbageCollectionGeneration {
        GarbageCollectionGeneration {
            generation: 1,
            full_generation: 1,
            is_compacted: false,
        }
    }

    fn new_writer() -> RecordWriter<MemoryStore> {
        RecordWriter::new(MemoryStore::default(), test_generation())
    }

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

    #[test]
    fn counted_lists_round_trip_across_bucket_boundaries() {
        let mut writer = new_writer();
        let elements: Vec<RecordIdentifier> = (0..600)
            .map(|index| {
                writer
                    .write_string(&format!("element-{index}"))
                    .expect("write")
            })
            .collect();
        let list = writer.write_counted_list(&elements).expect("write list");
        let empty = writer.write_counted_list(&[]).expect("write empty");
        let store = writer.finish().expect("finish");

        let counted = read_counted_list(&store, list).expect("read");
        assert_eq!(counted.size, 600);
        let body = counted.body.expect("non-empty body");
        let read_back =
            crate::content::list::uncounted_list_entries(&store, body, 600).expect("entries");
        assert_eq!(read_back, elements);

        assert_eq!(read_counted_list(&store, empty).expect("read").size, 0);
    }

    #[test]
    fn maps_round_trip_as_branches_and_leaves() {
        let mut writer = new_writer();
        let targets: Vec<(String, RecordIdentifier)> = (0..100)
            .map(|index| {
                let name = format!("child-{index:03}");
                let target = writer
                    .write_string(&format!("target-{index}"))
                    .expect("write");
                (name, target)
            })
            .collect();
        let map = writer.write_map(&targets).expect("write map");
        let store = writer.finish().expect("finish");

        assert_eq!(
            crate::content::map::map_size(&store, map).expect("size"),
            100
        );
        for (name, target) in &targets {
            assert_eq!(
                map_entry(&store, map, name).expect("lookup").as_ref(),
                Some(target),
                "{name}"
            );
        }
        assert_eq!(map_entry(&store, map, "absent").expect("lookup"), None);

        let mut enumerated: Vec<String> = map_entries(&store, map)
            .expect("entries")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        enumerated.sort();
        let mut expected: Vec<String> = targets.iter().map(|(name, _)| name.clone()).collect();
        expected.sort();
        assert_eq!(enumerated, expected);
    }

    #[test]
    fn nodes_round_trip_with_properties_and_children() {
        let mut writer = new_writer();

        let leaf = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("leaf");

        let title_value = writer.write_string("Hello").expect("value");
        let first_tag = writer.write_string("alpha").expect("value");
        let second_tag = writer.write_string("beta").expect("value");
        let count_value = writer.write_string("42").expect("value");
        let mut properties = vec![
            PropertyToWrite {
                name: "title".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Single(title_value),
            },
            PropertyToWrite {
                name: "tags".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Multiple(vec![first_tag, second_tag]),
            },
            PropertyToWrite {
                name: "count".to_owned(),
                property_type: PropertyType::Long,
                values: PropertyValuesToWrite::Single(count_value),
            },
        ];
        sort_properties_for_template(&mut properties);

        let parent = writer
            .write_node(
                Some("nt:unstructured"),
                &["mix:versionable".to_owned()],
                &ChildNodesToWrite::Many(vec![
                    ("first".to_owned(), leaf),
                    ("second".to_owned(), leaf),
                ]),
                &properties,
            )
            .expect("parent");

        let store = writer.finish().expect("finish");
        let node = NodeState::new(&store, parent);

        let template = node.template().expect("template");
        assert_eq!(template.primary_type.as_deref(), Some("nt:unstructured"));
        assert_eq!(template.mixin_types, vec!["mix:versionable"]);

        let title = node.property("title").expect("read").expect("present");
        assert_eq!(
            title.values,
            PropertyValues::Single(PropertyValue::String("Hello".to_owned()))
        );
        let count = node.property("count").expect("read").expect("present");
        assert_eq!(
            count.values,
            PropertyValues::Single(PropertyValue::Long(42))
        );
        let tags = node.property("tags").expect("read").expect("present");
        assert_eq!(
            tags.values,
            PropertyValues::Multiple(vec![
                PropertyValue::String("alpha".to_owned()),
                PropertyValue::String("beta".to_owned()),
            ])
        );

        assert_eq!(node.child_node_count().expect("count"), 2);
        let first = node.child_node("first").expect("lookup").expect("present");
        assert_eq!(first.record_identifier(), leaf);
        assert_eq!(
            node.stable_identifier().expect("stable"),
            format!("{}:{}", parent.segment, parent.record_number as i32)
        );
    }

    #[test]
    fn a_preserved_multi_valued_slot_never_shares_a_template_with_a_single_valued_one() {
        // The template cache keys on TemplateKey, and the per-slot type
        // byte carries the arity in its sign. When the key and the
        // serialized byte disagree about a preserved slot's arity, these
        // two nodes — identical in primary type, mixins, child arity and
        // property name and type, differing *only* in arity — collide.
        // The second then inherits the first's template record and its
        // values are decoded at the wrong arity: the counted list read as
        // one value, or the single value read as a counted list. Nothing
        // rejects that. It is the shape of damage a store still boots on.
        //
        // Both directions are exercised, because whichever node is written
        // first is the one that wins the cache and the other is the one
        // that gets corrupted.
        let mut writer = new_writer();

        let first_tag = writer.write_string("alpha").expect("value");
        let second_tag = writer.write_string("beta").expect("value");
        let preserved_list = writer
            .write_counted_list(&[first_tag, second_tag])
            .expect("counted list");
        let lone_value = writer.write_string("solo").expect("value");

        let multi_valued = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "tags".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::PreservedSlot {
                        value_slot: preserved_list,
                        is_multiple: true,
                    },
                }],
            )
            .expect("multi-valued node");

        let single_valued = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "tags".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::Single(lone_value),
                }],
            )
            .expect("single-valued node");

        let store = writer.finish().expect("finish");

        let multi_template = NodeState::new(&store, multi_valued)
            .template()
            .expect("multi template");
        let single_template = NodeState::new(&store, single_valued)
            .template()
            .expect("single template");
        assert!(
            multi_template.properties[0].is_multiple,
            "the preserved slot must serialize as multi-valued"
        );
        assert!(
            !single_template.properties[0].is_multiple,
            "the single-valued slot must not inherit the multi-valued template"
        );

        // The decoded values are what an Oak reader would actually see, so
        // assert those too: a template mix-up shows up here as an arity
        // flip, not as a parse failure.
        let multi_tags = NodeState::new(&store, multi_valued)
            .property("tags")
            .expect("read")
            .expect("present");
        assert_eq!(
            multi_tags.values,
            PropertyValues::Multiple(vec![
                PropertyValue::String("alpha".to_owned()),
                PropertyValue::String("beta".to_owned()),
            ])
        );
        let single_tags = NodeState::new(&store, single_valued)
            .property("tags")
            .expect("read")
            .expect("present");
        assert_eq!(
            single_tags.values,
            PropertyValues::Single(PropertyValue::String("solo".to_owned()))
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

    #[test]
    fn template_property_sort_orders_by_signed_hash_then_name_then_type() {
        use crate::content::property::PropertyType;
        use crate::segment::identifier::SegmentIdentifier;
        use crate::segment::record::RecordIdentifier;
        use crate::writer::record_writer::sort_properties_for_template;

        let property = |name: &str, property_type: PropertyType| PropertyToWrite {
            name: name.to_owned(),
            property_type,
            values: PropertyValuesToWrite::Single(RecordIdentifier::new(
                SegmentIdentifier::new(0, 0xA000_0000_0000_0001),
                0,
            )),
        };

        // Java hashes (signed): active = -1422950650, count = 94851343,
        // title = 110371416 — the negative hash must sort first, which an
        // unsigned comparison would get wrong. "Aa" and "BB" collide
        // (2112), so their tie breaks by name; two "count" entries tie on
        // hash and name, so their tie breaks by type tag (STRING=1 before
        // LONG=3).
        let mut properties = vec![
            property("title", PropertyType::String),
            property("BB", PropertyType::Long),
            property("count", PropertyType::Long),
            property("count", PropertyType::String),
            property("Aa", PropertyType::String),
            property("active", PropertyType::Boolean),
        ];
        sort_properties_for_template(&mut properties);
        let names_and_types: Vec<(&str, PropertyType)> = properties
            .iter()
            .map(|property| (property.name.as_str(), property.property_type))
            .collect();
        assert_eq!(
            names_and_types,
            [
                ("active", PropertyType::Boolean),
                ("Aa", PropertyType::String),
                ("BB", PropertyType::Long),
                ("count", PropertyType::String),
                ("count", PropertyType::Long),
                ("title", PropertyType::String),
            ]
        );
    }
}
