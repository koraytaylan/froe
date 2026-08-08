//! Generation of fresh segment identifiers.
//!
//! Oak assigns every new segment a random UUID that is syntactically a
//! version 4 RFC 4122 identifier, with the variant nibble replaced by the
//! segment kind marker: `0xA` for data segments, `0xB` for bulk segments.
//! Uniqueness matters — a collision with any existing segment in the
//! repository would be catastrophic — so the generator seeds itself from
//! the operating system's entropy source and mixes a fresh stream per
//! process.

use std::sync::Mutex;
use std::sync::OnceLock;

use crate::segment::identifier::SegmentIdentifier;

/// The process-wide generator state: a splitmix64 stream seeded from
/// operating system entropy.
static GENERATOR_STATE: OnceLock<Mutex<u64>> = OnceLock::new();

/// Produces the next raw 64-bit value of the generator stream.
fn next_random() -> u64 {
    let state = GENERATOR_STATE.get_or_init(|| Mutex::new(initial_seed()));
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // splitmix64: a well-distributed 64-bit permutation with a Weyl
    // sequence increment; passes BigCrush and cannot get stuck.
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = *state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^ (mixed >> 31)
}

/// Seeds the generator from `/dev/urandom` where available, falling back
/// to a mixture of the clock, the process identifier, and an address —
/// still unique in practice, though with less entropy.
fn initial_seed() -> u64 {
    if let Ok(bytes) = read_entropy_bytes() {
        return u64::from_ne_bytes(bytes);
    }
    let nanoseconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    let process = u64::from(std::process::id());
    let stack_address = std::ptr::from_ref(&nanoseconds) as u64;
    nanoseconds ^ process.rotate_left(32) ^ stack_address.rotate_left(17)
}

/// Reads eight bytes from the operating system entropy source.
fn read_entropy_bytes() -> std::io::Result<[u8; 8]> {
    use std::io::Read;
    let mut bytes = [0u8; 8];
    let mut source = std::fs::File::open("/dev/urandom")?;
    source.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Generates a fresh *data* segment identifier:
/// `xxxxxxxx-xxxx-4xxx-Axxx-xxxxxxxxxxxx`.
#[must_use]
pub fn new_data_segment_identifier() -> SegmentIdentifier {
    new_segment_identifier(0xA)
}

/// Generates a fresh *bulk* segment identifier:
/// `xxxxxxxx-xxxx-4xxx-Bxxx-xxxxxxxxxxxx`.
#[must_use]
pub fn new_bulk_segment_identifier() -> SegmentIdentifier {
    new_segment_identifier(0xB)
}

fn new_segment_identifier(kind_nibble: u64) -> SegmentIdentifier {
    // Force the version nibble of the upper half to 4 and the variant
    // nibble of the lower half to the segment kind, like Oak's
    // SegmentIdFactory.
    let most_significant_bits = (next_random() & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
    let least_significant_bits = (next_random() & 0x0FFF_FFFF_FFFF_FFFF) | (kind_nibble << 60);
    SegmentIdentifier::new(most_significant_bits, least_significant_bits)
}

#[cfg(test)]
mod tests {
    use super::{new_bulk_segment_identifier, new_data_segment_identifier};

    #[test]
    fn generated_identifiers_carry_the_correct_markers() {
        for _ in 0..64 {
            let data = new_data_segment_identifier();
            assert!(data.is_data_segment());
            assert_eq!(
                (data.most_significant_bits >> 12) & 0xF,
                4,
                "version nibble must be 4"
            );
            let bulk = new_bulk_segment_identifier();
            assert!(bulk.is_bulk_segment());
        }
    }

    #[test]
    fn generated_identifiers_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1024 {
            assert!(
                seen.insert(new_data_segment_identifier()),
                "collision in 1024 draws"
            );
        }
    }
}
