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
//! seed determines. Without that source there is no safe way to write:
//! the writable store refuses to open, and a failure after a successful
//! open fails the write with a panic rather than degrade — equivalent to
//! a crash, which the store's durability ordering already tolerates.

use std::io::Read;
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::error::{Error, Result};
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
}

impl EntropyBuffer {
    fn new() -> Self {
        Self {
            source: std::fs::File::open("/dev/urandom").ok(),
            buffer: [0u8; ENTROPY_CHUNK],
            position: ENTROPY_CHUNK,
        }
    }

    /// Fills `target` with entropy, refilling the buffer as needed. Fails
    /// when the operating system entropy source is unavailable.
    fn try_fill(&mut self, target: &mut [u8]) -> std::io::Result<()> {
        let mut written = 0;
        while written < target.len() {
            if self.position >= ENTROPY_CHUNK {
                self.try_refill()?;
            }
            let available = ENTROPY_CHUNK - self.position;
            let take = available.min(target.len() - written);
            target[written..written + take]
                .copy_from_slice(&self.buffer[self.position..self.position + take]);
            self.position += take;
            written += take;
        }
        Ok(())
    }

    /// Refills the buffer from the entropy source.
    fn try_refill(&mut self) -> std::io::Result<()> {
        let source = self.source.as_mut().ok_or_else(|| {
            std::io::Error::other("no operating system entropy source is available")
        })?;
        source.read_exact(&mut self.buffer)?;
        self.position = 0;
        Ok(())
    }
}

/// Runs `operation` on the process-wide entropy buffer.
fn with_entropy_buffer<T>(operation: impl FnOnce(&mut EntropyBuffer) -> T) -> T {
    let buffer = ENTROPY.get_or_init(|| Mutex::new(EntropyBuffer::new()));
    operation(
        &mut buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Confirms the operating system entropy source works, drawing a probe
/// from it. The writable store calls this at open and refuses to proceed
/// without it: segment identifiers from anything weaker could collide and
/// silently alias two different segments.
pub(crate) fn verify_entropy_source() -> Result<()> {
    let mut probe = [0u8; 16];
    with_entropy_buffer(|buffer| buffer.try_fill(&mut probe)).map_err(|source| {
        Error::InvalidFormat {
            details: format!(
                "cannot open a store for writing: the operating system entropy source \
                 needed for safe segment identifiers is unavailable ({source})"
            ),
        }
    })
}

/// Draws sixteen fresh entropy bytes as two 64-bit halves.
///
/// The entropy source was verified when the writable store opened; a
/// failure here means it broke mid-session, and failing the write with a
/// panic is the only response that cannot corrupt the repository (no
/// journal line ever references a segment that was not durably written).
fn draw_uuid_halves() -> (u64, u64) {
    let mut bytes = [0u8; 16];
    with_entropy_buffer(|buffer| buffer.try_fill(&mut bytes)).expect(
        "the operating system entropy source failed; refusing to generate segment \
         identifiers from anything weaker",
    );
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
