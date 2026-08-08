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

use crate::content::property::PropertyType;
use crate::error::{Error, Result};
use crate::hashing::{java_compare_strings, map_entry_hash};
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

/// Lengths below this use the one-byte small value encoding.
const SMALL_VALUE_LIMIT: usize = 128;

/// Lengths below this use the two-byte medium value encoding.
const MEDIUM_VALUE_LIMIT: usize = (1 << 14) + 128;

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
    current: SegmentBufferBuilder,
}

impl<Sink: SegmentSink> RecordWriter<Sink> {
    /// Creates a writer stamping `generation` on every produced segment.
    #[must_use]
    pub fn new(sink: Sink, generation: GarbageCollectionGeneration) -> Self {
        Self {
            sink,
            generation,
            current: SegmentBufferBuilder::new(new_data_segment_identifier(), generation),
        }
    }

    /// The sink, for inspection after writing.
    #[must_use]
    pub fn sink(&self) -> &Sink {
        &self.sink
    }

    /// Consumes the writer, flushing the current segment when it holds
    /// any records, and returns the sink.
    pub fn finish(mut self) -> Result<Sink> {
        self.flush_current_segment()?;
        Ok(self.sink)
    }

    /// Flushes the segment under construction to the sink when it holds
    /// any records.
    pub fn flush_current_segment(&mut self) -> Result<()> {
        if self.current.record_count() == 0 {
            return Ok(());
        }
        let finished = std::mem::replace(
            &mut self.current,
            SegmentBufferBuilder::new(new_data_segment_identifier(), self.generation),
        );
        self.sink.write_segment(finished.finish())
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
        SegmentBufferBuilder::write_record_identifier_bytes(
            reference,
            identifier.record_number,
            &mut bytes[offset..offset + 6],
        );
    }

    /// The identifier of a record in the segment under construction.
    fn identifier_of(&self, record_number: u32) -> RecordIdentifier {
        RecordIdentifier::new(self.current.identifier(), record_number)
    }

    /// Writes a string value record and returns its identifier.
    pub fn write_string(&mut self, text: &str) -> Result<RecordIdentifier> {
        self.write_value_bytes(text.as_bytes())
    }

    /// Writes an inline binary value record and returns its identifier.
    pub fn write_binary_content(&mut self, content: &[u8]) -> Result<RecordIdentifier> {
        self.write_value_bytes(content)
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
    /// hash trie of leaf and branch records.
    pub fn write_map(
        &mut self,
        entries: &[(String, RecordIdentifier)],
    ) -> Result<RecordIdentifier> {
        let mut prepared: Vec<(u32, String, RecordIdentifier, RecordIdentifier)> =
            Vec::with_capacity(entries.len());
        for (name, value) in entries {
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
                .then_with(|| java_compare_strings(&first.1, &second.1))
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
            let tag = property.property_type as u8 as i8;
            let type_byte = match property.values {
                PropertyValuesToWrite::Single(_) => tag,
                PropertyValuesToWrite::Multiple(_) => -tag,
                PropertyValuesToWrite::PreservedSlot { is_multiple, .. } => {
                    if is_multiple {
                        -tag
                    } else {
                        tag
                    }
                }
            };
            self.current.record_bytes_mut(record)[cursor] = type_byte as u8;
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

        let mut slots: Vec<RecordIdentifier> = vec![template_identifier];
        slots.extend(child_identifier);
        slots.extend(property_list_identifier);

        let size = 6 + slots.len() * 6;
        let record = self.allocate(RecordType::Node, size, &slots)?;
        // Slot 0: the stable identifier as a self reference.
        let own_identifier = self.identifier_of(record);
        self.write_identifier_at(record, 0, own_identifier);
        for (position, identifier) in slots.iter().enumerate() {
            self.write_identifier_at(record, 6 + position * 6, *identifier);
        }
        Ok(own_identifier)
    }
}

/// Sorts properties into the on-disk template order: by Java string hash
/// of the name, then by name in UTF-16 order, then by type tag — the
/// order `Template`'s constructor establishes in Java.
pub fn sort_properties_for_template(properties: &mut [PropertyToWrite]) {
    properties.sort_by(|first, second| {
        crate::hashing::java_string_hash(&first.name)
            .cmp(&crate::hashing::java_string_hash(&second.name))
            .then_with(|| java_compare_strings(&first.name, &second.name))
            .then_with(|| (first.property_type as u8).cmp(&(second.property_type as u8)))
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

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
