//! An in-memory sink that is also a provider, so a written record can be
//! read back without a store.

use super::{RecordWriter, SegmentSink};
use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;
use crate::segment::view::SegmentView;
use crate::writer::segment_builder::{BuiltSegment, GarbageCollectionGeneration};
use std::collections::HashMap;
use std::sync::Arc;

/// Collects written segments and serves them back as a provider, so
/// every test reads its output through the production reader.
#[derive(Default)]
pub(crate) struct MemoryStore {
    pub(crate) segments: HashMap<SegmentIdentifier, (Arc<ParsedSegment>, Vec<u8>)>,
    pub(crate) write_order: Vec<SegmentIdentifier>,
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

pub(crate) fn test_generation() -> GarbageCollectionGeneration {
    GarbageCollectionGeneration {
        generation: 1,
        full_generation: 1,
        is_compacted: false,
    }
}

pub(crate) fn new_writer() -> RecordWriter<MemoryStore> {
    RecordWriter::new(MemoryStore::default(), test_generation())
}
