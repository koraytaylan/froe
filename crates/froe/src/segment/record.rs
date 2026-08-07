//! Record identifiers and record types.
//!
//! Everything inside a data segment is stored as a *record*: a contiguous,
//! four-byte-aligned run of bytes. Records reference each other through
//! record identifiers, forming a graph whose roots are the node states
//! reachable from the journal.

use std::fmt;

use crate::segment::identifier::SegmentIdentifier;

/// The type tag of a record, stored as one byte in the segment header's
/// record reference table. The numeric values are the ordinals of the Java
/// `RecordType` enumeration.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum RecordType {
    /// A leaf of a map, storing the map entries directly.
    MapLeaf = 0,
    /// A branch of a map, dispatching to sub-maps by key hash.
    MapBranch = 1,
    /// A bucket of up to 255 record identifiers inside a list.
    ListBucket = 2,
    /// A list of record identifiers: a size plus an optional bucket tree.
    List = 3,
    /// A value: a length-prefixed string or binary, inline or block-backed.
    Value = 4,
    /// A raw run of bytes, the building block of large values.
    Block = 5,
    /// The structural description shared by similar nodes: primary type,
    /// mixin types, property names and types, and child node arity.
    Template = 6,
    /// A node state: stable identifier, template, children, and properties.
    Node = 7,
    /// The identifier of a binary stored outside the segment store.
    ExternalBlobIdentifier = 8,
}

impl RecordType {
    /// Decodes the record type byte used in segment headers.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::MapLeaf),
            1 => Some(Self::MapBranch),
            2 => Some(Self::ListBucket),
            3 => Some(Self::List),
            4 => Some(Self::Value),
            5 => Some(Self::Block),
            6 => Some(Self::Template),
            7 => Some(Self::Node),
            8 => Some(Self::ExternalBlobIdentifier),
            _ => None,
        }
    }
}

/// A reference to one record: the segment holding it plus the record's
/// logical number within that segment.
///
/// The record number is *not* an offset: it is resolved to a byte position
/// through the record reference table in the header of the owning segment.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordIdentifier {
    /// The segment the record lives in.
    pub segment: SegmentIdentifier,
    /// The logical record number within that segment.
    pub record_number: u32,
}

impl RecordIdentifier {
    /// Creates a record identifier.
    #[must_use]
    pub const fn new(segment: SegmentIdentifier, record_number: u32) -> Self {
        Self {
            segment,
            record_number,
        }
    }
}

impl fmt::Display for RecordIdentifier {
    /// Formats the identifier the way current Oak versions do:
    /// the segment UUID, a dot, and the record number as eight
    /// hexadecimal digits, for example
    /// `f81378fb-92b1-4b52-a5c8-e0a67152ed2c.000221a8`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{:08x}", self.segment, self.record_number)
    }
}

impl fmt::Debug for RecordIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "RecordIdentifier({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::{RecordIdentifier, RecordType};
    use crate::segment::identifier::SegmentIdentifier;

    #[test]
    fn record_type_bytes_round_trip() {
        for byte in 0..=8u8 {
            let record_type = RecordType::from_byte(byte).expect("valid record type byte");
            assert_eq!(record_type as u8, byte);
        }
        assert_eq!(RecordType::from_byte(9), None);
        assert_eq!(RecordType::from_byte(255), None);
    }

    #[test]
    fn record_identifier_formats_like_oak() {
        let identifier = RecordIdentifier::new(
            SegmentIdentifier::new(0xF813_78FB_92B1_4B52, 0xA5C8_E0A6_7152_ED2C),
            0x0002_21A8,
        );
        assert_eq!(
            identifier.to_string(),
            "f81378fb-92b1-4b52-a5c8-e0a67152ed2c.000221a8"
        );
    }
}
