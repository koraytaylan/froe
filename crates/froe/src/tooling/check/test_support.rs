//! The stores and providers these checks run against, including ones that
//! hide a record or count how often it is read.

use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::segment::view::SegmentView;
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::store_writer::WritableRepository;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct TestDirectory {
    pub(crate) path: std::path::PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-check-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A provider that makes selected, otherwise-valid segments disappear.
/// Its string/template readers deliberately route back through `self`,
/// so their record accesses cannot bypass the hiding behavior by
/// delegating directly to the wrapped repository.
pub(crate) struct HidingProvider<'store> {
    pub(crate) store: &'store WritableRepository,
    pub(crate) exact: Option<SegmentIdentifier>,
    pub(crate) bulk: bool,
}

impl SegmentProvider for HidingProvider<'_> {
    fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        if self.exact == Some(identifier) || self.bulk && identifier.is_bulk_segment() {
            return Err(Error::SegmentNotFound {
                segment_identifier: identifier,
            });
        }
        self.store.segment(identifier)
    }

    fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, identifier).map(Arc::from)
    }

    fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, identifier).map(Arc::new)
    }
}

/// Counts every segment resolution and can make one segment unavailable.
/// String/template reads route through `self`, so all record access is
/// observable by the counter.
pub(crate) struct CountingProvider<'provider> {
    pub(crate) inner: &'provider dyn SegmentProvider,
    pub(crate) hidden: Option<SegmentIdentifier>,
    pub(crate) reads: RefCell<HashMap<SegmentIdentifier, usize>>,
}

impl<'provider> CountingProvider<'provider> {
    pub(crate) fn new(inner: &'provider dyn SegmentProvider) -> Self {
        Self {
            inner,
            hidden: None,
            reads: RefCell::new(HashMap::new()),
        }
    }

    pub(crate) fn hiding(inner: &'provider dyn SegmentProvider, hidden: SegmentIdentifier) -> Self {
        Self {
            inner,
            hidden: Some(hidden),
            reads: RefCell::new(HashMap::new()),
        }
    }

    pub(crate) fn reads_of(&self, segment: SegmentIdentifier) -> usize {
        self.reads.borrow().get(&segment).copied().unwrap_or(0)
    }
}

impl SegmentProvider for CountingProvider<'_> {
    fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        *self.reads.borrow_mut().entry(identifier).or_default() += 1;
        if self.hidden == Some(identifier) {
            return Err(Error::SegmentNotFound {
                segment_identifier: identifier,
            });
        }
        self.inner.segment(identifier)
    }

    fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, identifier).map(Arc::from)
    }

    fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, identifier).map(Arc::new)
    }
}

pub(crate) fn write_content_revision(directory: &std::path::Path, title: &str) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let value = writer.write_string(title).expect("value");
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "title".to_owned(),
                property_type: crate::content::property::PropertyType::String,
                values: PropertyValuesToWrite::Single(value),
            }],
        )
        .expect("content");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "content".to_owned(),
                node: content,
            },
            &[],
        )
        .expect("root");
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &[],
        )
        .expect("super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
}
