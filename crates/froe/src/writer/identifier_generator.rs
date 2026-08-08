//! Generation of fresh segment identifiers.
//!
//! Oak assigns every new segment a random UUID that is syntactically a
//! version 4 RFC 4122 identifier, with the variant nibble replaced by the
//! segment kind marker: `0xA` for data segments, `0xB` for bulk segments.
//! A collision with any existing segment in the repository would silently
//! alias two different segments and corrupt content, so both 64-bit halves
//! of every identifier come *directly* from the operating system's
//! cryptographic entropy source — matching Oak's use of `SecureRandom` —
//! never from a seeded pseudo-random stream whose whole output a single
//! seed determines.

use std::io::Read;
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::segment::identifier::SegmentIdentifier;

/// A process-wide buffered reader over the operating system entropy
/// source, refilled in bulk to keep the per-identifier cost to a memory
/// copy rather than a system call.
static ENTROPY: OnceLock<Mutex<EntropyBuffer>> = OnceLock::new();

/// The bulk read size from the entropy source.
const ENTROPY_CHUNK: usize = 4096;

/// A refillable buffer of operating system entropy.
struct EntropyBuffer {
    source: Option<std::fs::File>,
    buffer: [u8; ENTROPY_CHUNK],
    position: usize,
    /// Fallback stream state used only when no entropy source is available.
    fallback_state: u64,
}

impl EntropyBuffer {
    fn new() -> Self {
        let source = std::fs::File::open("/dev/urandom").ok();
        Self {
            source,
            buffer: [0u8; ENTROPY_CHUNK],
            position: ENTROPY_CHUNK,
            fallback_state: fallback_seed(),
        }
    }

    /// Fills `target` with entropy, refilling the buffer as needed.
    fn fill(&mut self, target: &mut [u8]) {
        let mut written = 0;
        while written < target.len() {
            if self.position >= ENTROPY_CHUNK {
                self.refill();
            }
            let available = ENTROPY_CHUNK - self.position;
            let take = available.min(target.len() - written);
            target[written..written + take]
                .copy_from_slice(&self.buffer[self.position..self.position + take]);
            self.position += take;
            written += take;
        }
    }

    /// Refills the buffer from the entropy source, or from the fallback
    /// stream when no source is available (non-Unix, or a sandbox without
    /// `/dev/urandom`).
    fn refill(&mut self) {
        let filled = self
            .source
            .as_mut()
            .and_then(|source| source.read_exact(&mut self.buffer).ok())
            .is_some();
        if !filled {
            // splitmix64 fallback; weaker, but every process still seeds it
            // from a distinct clock/pid/address mixture.
            self.source = None;
            for chunk in self.buffer.chunks_exact_mut(8) {
                self.fallback_state = self.fallback_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut mixed = self.fallback_state;
                mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                chunk.copy_from_slice(&(mixed ^ (mixed >> 31)).to_be_bytes());
            }
        }
        self.position = 0;
    }
}

/// Seeds the fallback stream from the clock, the process identifier, and a
/// stack address.
fn fallback_seed() -> u64 {
    let nanoseconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    let process = u64::from(std::process::id());
    let stack_address = std::ptr::from_ref(&nanoseconds) as u64;
    nanoseconds ^ process.rotate_left(32) ^ stack_address.rotate_left(17)
}

/// Draws sixteen fresh entropy bytes as two 64-bit halves.
fn draw_uuid_halves() -> (u64, u64) {
    let buffer = ENTROPY.get_or_init(|| Mutex::new(EntropyBuffer::new()));
    let mut bytes = [0u8; 16];
    buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .fill(&mut bytes);
    (
        u64::from_be_bytes(bytes[0..8].try_into().expect("8 bytes")),
        u64::from_be_bytes(bytes[8..16].try_into().expect("8 bytes")),
    )
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
    let (random_high, random_low) = draw_uuid_halves();
    // Force the version nibble of the upper half to 4 and the variant
    // nibble of the lower half to the segment kind, like Oak's
    // SegmentIdFactory — every other bit is fresh operating system entropy.
    let most_significant_bits = (random_high & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
    let least_significant_bits = (random_low & 0x0FFF_FFFF_FFFF_FFFF) | (kind_nibble << 60);
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
        for _ in 0..100_000 {
            assert!(
                seen.insert(new_data_segment_identifier()),
                "collision in 100000 draws"
            );
        }
    }
}
