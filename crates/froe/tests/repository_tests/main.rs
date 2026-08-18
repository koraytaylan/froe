//! End-to-end tests: open synthetic repositories written by the
//! independent encoder in `support` and read them back through the public
//! API.

#![allow(
    unreachable_pub,
    reason = "test binaries have no external interface; pub only means module-visible"
)]

#[path = "../support/mod.rs"]
mod support;

use froe::content::{ChildNodeArity, PropertyType, PropertyValue, PropertyValues};
use froe::store::Repository;
use support::{
    ArchiveBuilder, MapEntryFixture, SegmentBuilder, TYPE_LIST_BUCKET, TYPE_NODE, TYPE_TEMPLATE,
    TYPE_VALUE, TestDirectory, build_child_map, data_segment_uuid, format_uuid,
    record_identifier_bytes, string_record, write_repository,
};

mod fixtures;
mod opening;
mod reading;

pub(crate) use fixtures::*;
