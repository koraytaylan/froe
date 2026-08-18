//! End-to-end diagnostic tests over repositories written by the independent
//! test encoder. These fixtures prove both path attribution across archive
//! boundaries and the strict read-only contract.

#![allow(
    dead_code,
    reason = "the shared independent encoder exposes fixtures used by other integration tests"
)]
#![allow(
    unreachable_pub,
    reason = "test binaries have no external interface; pub only means module-visible"
)]

#[path = "../support/mod.rs"]
mod support;

use froe::PropertyType;
use froe::segment::{MAXIMUM_SEGMENT_SIZE, identifier::SegmentIdentifier};
use froe::store::Repository;
use froe::tooling::{
    ArchiveDebugError, ArchiveDebugOptions, ArchiveDebugState, ArchiveGraphOrigin,
    ArchiveGraphReferences, ArchivePathReference, ArchivePropertyDisplay, debug_archive,
    debug_archive_with_options, dump_segment,
};
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, sort_properties_for_template,
};
use froe::writer::store_writer::WritableRepository;
use support::filesystem_snapshot::directory_snapshot;
use support::{
    ArchiveBuilder, SegmentBuilder, TYPE_LIST, TYPE_LIST_BUCKET, TYPE_MAP_BRANCH, TYPE_MAP_LEAF,
    TYPE_NODE, TYPE_TEMPLATE, TYPE_VALUE, TestDirectory, data_segment_uuid, format_uuid,
    independent_map_entry_hash, record_identifier_bytes, string_record, write_repository,
};

mod attribution;
mod budgets;
mod fixtures;
mod graph;
mod rendering;

pub(crate) use fixtures::*;
