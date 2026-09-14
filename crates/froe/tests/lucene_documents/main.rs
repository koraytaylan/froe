//! Oak's document maker: the exact set of fields one node yields.
//!
//! Each case publishes a small store, reads the definition back, and
//! compares the document's fields against a list written by hand from
//! `docs/analysis/lucene-oak-documents.md` §3 — name, kind, options,
//! stored flag, norms and value. The order is part of the claim: it fixes
//! positions, offsets and the stored-field sequence.
//!
//! Equality with Oak's own documents is task 1009's acceptance; what these
//! confirm is that every branch of the specification is reproduced.
//!
//! [`fixtures`] builds the stores, [`fields`] covers the field kinds and
//! the per-property pass, and [`branches`] the rest of §3 with §4's
//! aggregates, §5's facets and §6's binaries.

#![allow(
    unreachable_pub,
    reason = "test binaries have no external interface; pub only means module-visible"
)]

mod branches;
mod fields;
mod fixtures;

use froe::content::PropertyType;
use froe::index::IndexWarning;
use froe::index::lucene::codec::postings::IndexOptions;
use froe::index::lucene::documents::binaries::{BinaryTextFallback, BinaryTextPolicy};
use froe::index::lucene::documents::document_maker::{DocumentMaker, MadeDocument};
use froe::index::lucene::documents::name_pattern::ALL_PROPERTIES;
use froe::index::lucene::documents::rules::IndexingRules;
use froe::index::lucene::writer::{DocValue, Field, StoredValue};
use froe::segment::record::RecordIdentifier;
use froe::store::Repository;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use froe::writer::store_writer::WritableRepository;
use std::path::{Path, PathBuf};

use fixtures::*;
