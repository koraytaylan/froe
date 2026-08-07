//! Segment identifiers.
//!
//! Every segment is identified by a 128-bit UUID, stored as two 64-bit
//! halves. The UUIDs are syntactically valid RFC 4122 version 4 identifiers,
//! but the segment store reserves the four most significant bits of the least
//! significant half to distinguish the two kinds of segments:
//!
//! * `xxxxxxxx-xxxx-4xxx-Axxx-xxxxxxxxxxxx` — a *data* segment, which has a
//!   header and contains structured records (nodes, templates, maps, …);
//! * `xxxxxxxx-xxxx-4xxx-Bxxx-xxxxxxxxxxxx` — a *bulk* segment, which is a
//!   raw sequence of 4 KiB block records holding binary or long string data.

use std::fmt;
use std::str::FromStr;

use crate::error::Error;

/// The 128-bit UUID identifying one segment.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegmentIdentifier {
    /// The first eight bytes of the UUID in big-endian order.
    pub most_significant_bits: u64,
    /// The last eight bytes of the UUID in big-endian order.
    pub least_significant_bits: u64,
}

/// The kind of content a segment holds, encoded in its identifier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegmentKind {
    /// Structured records with a segment header.
    Data,
    /// Raw block records without any header.
    Bulk,
}

impl SegmentIdentifier {
    /// Creates an identifier from the two halves of a UUID.
    #[must_use]
    pub const fn new(most_significant_bits: u64, least_significant_bits: u64) -> Self {
        Self {
            most_significant_bits,
            least_significant_bits,
        }
    }

    /// Returns `true` when this identifies a data segment
    /// (the four most significant bits of the lower half are `0xA`).
    #[must_use]
    pub const fn is_data_segment(self) -> bool {
        self.least_significant_bits >> 60 == 0xA
    }

    /// Returns `true` when this identifies a bulk segment
    /// (the four most significant bits of the lower half are `0xB`).
    #[must_use]
    pub const fn is_bulk_segment(self) -> bool {
        self.least_significant_bits >> 60 == 0xB
    }

    /// Classifies the segment by its identifier, or `None` when the
    /// identifier carries neither the data nor the bulk marker.
    #[must_use]
    pub const fn kind(self) -> Option<SegmentKind> {
        match self.least_significant_bits >> 60 {
            0xA => Some(SegmentKind::Data),
            0xB => Some(SegmentKind::Bulk),
            _ => None,
        }
    }
}

impl fmt::Display for SegmentIdentifier {
    /// Formats the identifier in canonical UUID form,
    /// for example `f81378fb-92b1-4b52-a5c8-e0a67152ed2c`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let high = self.most_significant_bits;
        let low = self.least_significant_bits;
        write!(
            formatter,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            high >> 32,
            (high >> 16) & 0xFFFF,
            high & 0xFFFF,
            low >> 48,
            low & 0xFFFF_FFFF_FFFF,
        )
    }
}

impl fmt::Debug for SegmentIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "SegmentIdentifier({self})")
    }
}

impl FromStr for SegmentIdentifier {
    type Err = Error;

    /// Parses the canonical hyphenated UUID form.
    ///
    /// Only lowercase hexadecimal digits are accepted, mirroring the Java
    /// patterns that recognize segment UUIDs in tar entry names and record
    /// identifier strings.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || Error::InvalidFormat {
            details: format!("not a valid segment identifier UUID: {text:?}"),
        };
        let bytes = text.as_bytes();
        if bytes.len() != 36
            || bytes[8] != b'-'
            || bytes[13] != b'-'
            || bytes[18] != b'-'
            || bytes[23] != b'-'
        {
            return Err(invalid());
        }
        let hexadecimal = |range: std::ops::Range<usize>| {
            if text.as_bytes()[range.clone()]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            {
                u64::from_str_radix(&text[range], 16).map_err(|_| invalid())
            } else {
                Err(invalid())
            }
        };
        let most_significant_bits =
            hexadecimal(0..8)? << 32 | hexadecimal(9..13)? << 16 | hexadecimal(14..18)?;
        let least_significant_bits = hexadecimal(19..23)? << 48 | hexadecimal(24..36)?;
        Ok(Self {
            most_significant_bits,
            least_significant_bits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{SegmentIdentifier, SegmentKind};

    #[test]
    fn formats_as_canonical_uuid() {
        let identifier = SegmentIdentifier::new(0xF813_78FB_92B1_4B52, 0xA5C8_E0A6_7152_ED2C);
        assert_eq!(
            identifier.to_string(),
            "f81378fb-92b1-4b52-a5c8-e0a67152ed2c"
        );
    }

    #[test]
    fn parses_canonical_uuid() {
        let identifier: SegmentIdentifier = "f81378fb-92b1-4b52-a5c8-e0a67152ed2c"
            .parse()
            .expect("valid UUID");
        assert_eq!(identifier.most_significant_bits, 0xF813_78FB_92B1_4B52);
        assert_eq!(identifier.least_significant_bits, 0xA5C8_E0A6_7152_ED2C);
    }

    #[test]
    fn round_trips_through_display_and_parse() {
        let identifier = SegmentIdentifier::new(0x0123_4567_89AB_CDEF, 0xB000_0000_0000_0001);
        let round_tripped: SegmentIdentifier = identifier.to_string().parse().expect("round trip");
        assert_eq!(identifier, round_tripped);
    }

    #[test]
    fn rejects_malformed_text() {
        assert!("not-a-uuid".parse::<SegmentIdentifier>().is_err());
        assert!(
            "f81378fb92b14b52a5c8e0a67152ed2c"
                .parse::<SegmentIdentifier>()
                .is_err()
        );
        assert!(
            "f81378fb-92b1-4b52-a5c8-e0a67152ed2g"
                .parse::<SegmentIdentifier>()
                .is_err(),
            "trailing non-hexadecimal character must be rejected"
        );
        assert!(
            "F81378FB-92B1-4B52-A5C8-E0A67152ED2C"
                .parse::<SegmentIdentifier>()
                .is_err(),
            "uppercase digits must be rejected like in the Java patterns"
        );
        assert!(
            "+81378fb-92b1-4b52-a5c8-e0a67152ed2c"
                .parse::<SegmentIdentifier>()
                .is_err(),
            "sign characters must be rejected"
        );
    }

    #[test]
    fn classifies_data_and_bulk_segments() {
        let data = SegmentIdentifier::new(0, 0xA000_0000_0000_0000);
        let bulk = SegmentIdentifier::new(0, 0xB000_0000_0000_0000);
        let neither = SegmentIdentifier::new(0, 0x1000_0000_0000_0000);
        assert!(data.is_data_segment() && !data.is_bulk_segment());
        assert!(bulk.is_bulk_segment() && !bulk.is_data_segment());
        assert_eq!(data.kind(), Some(SegmentKind::Data));
        assert_eq!(bulk.kind(), Some(SegmentKind::Bulk));
        assert_eq!(neither.kind(), None);
    }
}
