//! The value records these tests read: built by hand, so a decoder is
//! checked against bytes it did not produce.

use super::{MEDIUM_VALUE_LIMIT, SMALL_VALUE_LIMIT, read_string};
use crate::content::provider::{SegmentProvider, tests::MemorySegmentProvider};
use crate::content::template::{Template, read_template};
use crate::error::Result;
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::segment::view::SegmentView;
use std::cell::Cell;
use std::sync::Arc;

pub(crate) fn small_string_record(text: &str) -> Vec<u8> {
    let mut bytes = vec![text.len() as u8];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

pub(crate) fn direct_binary_record(content: &[u8]) -> Vec<u8> {
    let length = content.len() as u64;
    let mut record = if length < SMALL_VALUE_LIMIT {
        vec![length as u8]
    } else {
        assert!(length < MEDIUM_VALUE_LIMIT);
        ((0x8000u16) | (length as u16 - SMALL_VALUE_LIMIT as u16))
            .to_be_bytes()
            .to_vec()
    };
    record.extend_from_slice(content);
    record
}

pub(crate) fn local_record_identifier(record_number: u32) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&record_number.to_be_bytes());
    bytes
}

pub(crate) fn referenced_record_identifier(reference: u16, record_number: u32) -> Vec<u8> {
    let mut bytes = reference.to_be_bytes().to_vec();
    bytes.extend_from_slice(&record_number.to_be_bytes());
    bytes
}

pub(crate) fn repeated_local_identifiers(record_number: u32, count: usize) -> Vec<u8> {
    let identifier = local_record_identifier(record_number);
    let mut bytes = Vec::with_capacity(identifier.len() * count);
    for _ in 0..count {
        bytes.extend_from_slice(&identifier);
    }
    bytes
}

pub(crate) fn long_binary_record(length: u64, list_record_number: u32) -> Vec<u8> {
    assert!(length >= MEDIUM_VALUE_LIMIT);
    let mut record = ((length - MEDIUM_VALUE_LIMIT) | (0x3 << 62))
        .to_be_bytes()
        .to_vec();
    record.extend_from_slice(&local_record_identifier(list_record_number));
    record
}

pub(crate) struct CountingProvider<'provider> {
    inner: &'provider MemorySegmentProvider,
    segment_reads: Cell<usize>,
}

impl<'provider> CountingProvider<'provider> {
    pub(crate) fn new(inner: &'provider MemorySegmentProvider) -> Self {
        Self {
            inner,
            segment_reads: Cell::new(0),
        }
    }

    pub(crate) fn segment_reads(&self) -> usize {
        self.segment_reads.get()
    }

    pub(crate) fn reset_segment_reads(&self) {
        self.segment_reads.set(0);
    }
}

impl SegmentProvider for CountingProvider<'_> {
    fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        self.segment_reads.set(self.segment_reads.get() + 1);
        self.inner.segment(identifier)
    }

    fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, identifier).map(Arc::from)
    }

    fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, identifier).map(Arc::new)
    }
}
