//! Segments held in memory, written and read back without a store.
//!
//! The one thing a caller cannot do with a `RecordWriter` alone is *read*
//! what it wrote: a `NodeState` needs a `SegmentProvider`, and the store's
//! own provider only serves what a session has flushed. So a caller that
//! has to turn a tree it holds in memory into node states — the Lucene
//! import's definitions file, which must be compared against the store's
//! own definition *before the first record is appended to the store* —
//! writes it here instead.
//!
//! This is deliberately not a store: nothing is sealed, nothing is
//! journalled, and the segments live exactly as long as the value does.

use std::collections::HashMap;
use std::sync::Arc;

use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;
use crate::segment::view::SegmentView;
use crate::writer::record_writer::SegmentSink;
use crate::writer::segment_builder::BuiltSegment;

/// Collects written segments and serves them back as a provider, so
/// what was written is read through the production reader.
#[derive(Default)]
pub(crate) struct MemorySegments {
    pub(crate) segments: HashMap<SegmentIdentifier, (Arc<ParsedSegment>, Vec<u8>)>,
    pub(crate) write_order: Vec<SegmentIdentifier>,
}

impl SegmentSink for MemorySegments {
    fn write_segment(&mut self, segment: BuiltSegment) -> Result<()> {
        let parsed = Arc::new(ParsedSegment::parse(segment.identifier, &segment.bytes)?);
        self.segments
            .insert(segment.identifier, (parsed, segment.bytes));
        self.write_order.push(segment.identifier);
        Ok(())
    }
}

impl SegmentProvider for MemorySegments {
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
