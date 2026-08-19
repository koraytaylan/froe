//! The segment provider: how record readers reach segment content.
//!
//! Records reference each other freely across segments, so every reader of
//! record content needs to resolve *any* segment identifier to that
//! segment's parsed structure and bytes. Beyond raw segment access the
//! provider also serves strings and templates, because those two record
//! kinds are read over and over — every map lookup compares key strings,
//! and most nodes share a small set of templates — and a provider backed
//! by a repository caches them. The free functions
//! [`read_string`](crate::content::value::read_string) and
//! [`read_template`](crate::content::template::read_template) are the
//! uncached implementations providers can delegate to.

use std::sync::Arc;

use crate::content::template::Template;
use crate::error::Result;
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::segment::view::SegmentView;

/// Resolves segment identifiers to segment content.
pub trait SegmentProvider {
    /// Returns the given segment, or
    /// [`Error::SegmentNotFound`](crate::error::Error::SegmentNotFound)
    /// when no archive of the repository contains it.
    fn segment(&self, segment_identifier: SegmentIdentifier) -> Result<SegmentView<'_>>;

    /// Reads the string value record at `record_identifier`, possibly from
    /// a cache.
    fn string(&self, record_identifier: RecordIdentifier) -> Result<Arc<str>>;

    /// Reads the template record at `record_identifier`, possibly from a
    /// cache.
    fn template(&self, record_identifier: RecordIdentifier) -> Result<Arc<Template>>;
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::SegmentProvider;
    use crate::content::template::{Template, read_template};
    use crate::content::value::read_string;
    use crate::error::{Error, Result};
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::parsed_segment::ParsedSegment;
    use crate::segment::record::RecordIdentifier;
    use crate::segment::view::SegmentView;

    /// A segment provider over in-memory segment buffers, for tests.
    /// Strings and templates are read uncached.
    #[derive(Default)]
    pub(crate) struct MemorySegmentProvider {
        segments: HashMap<SegmentIdentifier, (Arc<ParsedSegment>, Vec<u8>)>,
    }

    impl MemorySegmentProvider {
        pub(crate) fn insert(&mut self, identifier: SegmentIdentifier, bytes: Vec<u8>) {
            let structure =
                Arc::new(ParsedSegment::parse(identifier, &bytes).expect("valid test segment"));
            self.segments.insert(identifier, (structure, bytes));
        }
    }

    impl SegmentProvider for MemorySegmentProvider {
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

    /// Observes whether ordinary readers use the provider's cache-aware
    /// string/template surface instead of bypassing it for raw parsing.
    pub(crate) struct CountingSegmentProvider<'provider> {
        inner: &'provider MemorySegmentProvider,
        string_reads: Cell<usize>,
        template_reads: Cell<usize>,
        reads_by_segment: RefCell<HashMap<SegmentIdentifier, usize>>,
    }

    impl<'provider> CountingSegmentProvider<'provider> {
        pub(crate) fn new(inner: &'provider MemorySegmentProvider) -> Self {
            Self {
                inner,
                string_reads: Cell::new(0),
                template_reads: Cell::new(0),
                reads_by_segment: RefCell::new(HashMap::new()),
            }
        }

        pub(crate) fn string_reads(&self) -> usize {
            self.string_reads.get()
        }

        pub(crate) fn template_reads(&self) -> usize {
            self.template_reads.get()
        }

        pub(crate) fn segment_reads_for(&self, segment_identifier: SegmentIdentifier) -> usize {
            self.reads_by_segment
                .borrow()
                .get(&segment_identifier)
                .copied()
                .unwrap_or(0)
        }
    }

    impl SegmentProvider for CountingSegmentProvider<'_> {
        fn segment(&self, segment_identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
            *self
                .reads_by_segment
                .borrow_mut()
                .entry(segment_identifier)
                .or_insert(0) += 1;
            self.inner.segment(segment_identifier)
        }

        fn string(&self, record_identifier: RecordIdentifier) -> Result<Arc<str>> {
            self.string_reads.set(self.string_reads.get() + 1);
            self.inner.string(record_identifier)
        }

        fn template(&self, record_identifier: RecordIdentifier) -> Result<Arc<Template>> {
            self.template_reads.set(self.template_reads.get() + 1);
            self.inner.template(record_identifier)
        }
    }
}
