//! Segments: the unit of storage below the tar archive layer.
//!
//! A segment is an immutable chunk of up to 256 KiB identified by a UUID.
//! *Data* segments carry a header plus typed records; *bulk* segments are
//! raw block data for large binaries. This module provides the identifier
//! types and the segment parser.

pub mod identifier;
pub mod parsed_segment;
pub mod record;
pub mod view;

pub use identifier::{SegmentIdentifier, SegmentKind};
pub use parsed_segment::{MAXIMUM_SEGMENT_SIZE, ParsedSegment, RecordTableEntry};
pub use record::{RecordIdentifier, RecordType};
pub use view::SegmentView;
