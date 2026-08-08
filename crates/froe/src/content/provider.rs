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
}
