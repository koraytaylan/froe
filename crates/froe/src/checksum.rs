//! CRC32 checksum as used by the segment-tar format.
//!
//! The tar index, graph, and binary references entries each carry a CRC32
//! checksum (the standard IEEE polynomial, identical to `java.util.zip.CRC32`)
//! over their payload bytes, and every segment's tar entry name carries the
//! CRC32 of the segment payload.
//!
//! Archive certification — which compaction and cleanup run before they
//! mutate anything — verifies the name CRC of every indexed entry, so this
//! function is called over every byte of every source archive. That makes it
//! the throughput floor of those commands, which is why the implementation is
//! slice-by-16 rather than the byte-at-a-time loop the trailer checks alone
//! would have justified. Measured on a 256 MiB buffer: 618 MB/s
//! byte-at-a-time against 3350 MB/s here.

/// The number of bytes consumed per iteration of the main loop. One lookup
/// table is needed per byte of the stride.
const STRIDE: usize = 16;

/// Lookup tables for the reflected IEEE CRC32 polynomial `0xEDB88320`.
///
/// Table 0 is the ordinary byte-at-a-time table. Each later table folds one
/// additional zero byte through the polynomial, so table `lane` maps a byte
/// to its contribution when it is followed by `lane` more bytes. Combining
/// all sixteen therefore consumes a whole stride at once.
///
/// A `static` rather than a `const`: at 16 KiB, a `const` would risk being
/// materialized separately at each use site.
static CRC32_TABLES: [[u32; 256]; STRIDE] = generate_crc32_tables();

const fn generate_crc32_tables() -> [[u32; 256]; STRIDE] {
    let mut tables = [[0u32; 256]; STRIDE];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                (value >> 1) ^ 0xEDB8_8320
            } else {
                value >> 1
            };
            bit += 1;
        }
        tables[0][index] = value;
        index += 1;
    }
    let mut lane = 1;
    while lane < STRIDE {
        let mut index = 0;
        while index < 256 {
            let previous = tables[lane - 1][index];
            tables[lane][index] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            index += 1;
        }
        lane += 1;
    }
    tables
}

/// Computes the IEEE CRC32 checksum of `bytes`, matching `java.util.zip.CRC32`.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut running = Crc32::new();
    running.update(bytes);
    running.finish()
}

/// A CRC32 computed over a value that arrives in pieces.
///
/// The checksum is a streaming operation, so folding a chunk at a time
/// yields exactly what [`crc32`] returns for the concatenation — which is
/// what lets a digest checksum a multi-megabyte binary through a fixed
/// buffer instead of materializing it. Both paths run the same striding
/// loop, so neither can drift from the other and the one-shot form keeps
/// the throughput that archive certification depends on.
#[derive(Clone, Copy, Debug)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// Starts a checksum over an empty byte sequence.
    #[must_use]
    pub const fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    /// Folds the next chunk of the value into the checksum.
    pub fn update(&mut self, bytes: &[u8]) {
        self.state = fold(self.state, bytes);
    }

    /// The checksum of everything folded in so far.
    #[must_use]
    pub const fn finish(self) -> u32 {
        !self.state
    }
}

/// Folds `bytes` into a running checksum state, sixteen bytes at a time.
///
/// The stride is an optimization, not part of the definition: a chunk whose
/// length is not a multiple of the stride finishes byte-at-a-time, and
/// resuming with the next chunk is still the same polynomial. That is what
/// makes [`Crc32`] agree with [`crc32`] at any chunking.
fn fold(mut checksum: u32, bytes: &[u8]) -> u32 {
    let mut blocks = bytes.chunks_exact(STRIDE);
    for block in &mut blocks {
        // The first four bytes are folded into the running checksum before
        // the lookup; the remaining twelve index their tables directly.
        checksum ^= u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        checksum = CRC32_TABLES[15][(checksum & 0xFF) as usize]
            ^ CRC32_TABLES[14][((checksum >> 8) & 0xFF) as usize]
            ^ CRC32_TABLES[13][((checksum >> 16) & 0xFF) as usize]
            ^ CRC32_TABLES[12][(checksum >> 24) as usize]
            ^ CRC32_TABLES[11][block[4] as usize]
            ^ CRC32_TABLES[10][block[5] as usize]
            ^ CRC32_TABLES[9][block[6] as usize]
            ^ CRC32_TABLES[8][block[7] as usize]
            ^ CRC32_TABLES[7][block[8] as usize]
            ^ CRC32_TABLES[6][block[9] as usize]
            ^ CRC32_TABLES[5][block[10] as usize]
            ^ CRC32_TABLES[4][block[11] as usize]
            ^ CRC32_TABLES[3][block[12] as usize]
            ^ CRC32_TABLES[2][block[13] as usize]
            ^ CRC32_TABLES[1][block[14] as usize]
            ^ CRC32_TABLES[0][block[15] as usize];
    }
    for &byte in blocks.remainder() {
        let table_index = ((checksum ^ u32::from(byte)) & 0xFF) as usize;
        checksum = (checksum >> 8) ^ CRC32_TABLES[0][table_index];
    }
    checksum
}

#[cfg(test)]
mod tests {
    use super::{Crc32, STRIDE, crc32};

    /// The textbook bit-at-a-time definition of the reflected IEEE CRC32,
    /// written from the polynomial alone. It shares no table, no stride, and
    /// no code path with [`crc32`], so agreement between the two is evidence
    /// about the sliced implementation rather than a restatement of it.
    fn crc32_bit_at_a_time(bytes: &[u8]) -> u32 {
        let mut checksum = 0xFFFF_FFFFu32;
        for &byte in bytes {
            checksum ^= u32::from(byte);
            for _ in 0..8 {
                checksum = if checksum & 1 == 1 {
                    (checksum >> 1) ^ 0xEDB8_8320
                } else {
                    checksum >> 1
                };
            }
        }
        !checksum
    }

    #[test]
    fn empty_input_produces_zero() {
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn known_test_vectors_match() {
        // The canonical CRC32 check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn single_byte_matches_reference_value() {
        // Value produced by java.util.zip.CRC32 for a single zero byte.
        assert_eq!(crc32(&[0]), 0xD202_EF8D);
    }

    #[test]
    fn chunked_updates_agree_with_the_one_shot_at_every_chunk_width() {
        // A digest folds a binary in through a fixed buffer, so the chunk
        // boundaries fall wherever the reader happens to stop — including
        // inside a stride. Every width up to past two strides is covered
        // here, and each result is checked against the independent
        // bit-at-a-time definition rather than against `crc32` alone, so a
        // shared mistake in the sliced tables could not hide.
        let bytes: Vec<u8> = (0..=200u16).map(|index| (index % 251) as u8).collect();
        let expected = crc32_bit_at_a_time(&bytes);
        assert_eq!(crc32(&bytes), expected);

        for chunk_width in 1..=(2 * STRIDE + 3) {
            let mut running = Crc32::new();
            for chunk in bytes.chunks(chunk_width) {
                running.update(chunk);
            }
            assert_eq!(
                running.finish(),
                expected,
                "folding in {chunk_width}-byte chunks must equal the one-shot checksum"
            );
        }

        // An empty update anywhere in the sequence must not disturb it.
        let mut running = Crc32::new();
        running.update(&[]);
        running.update(&bytes[..7]);
        running.update(&[]);
        running.update(&bytes[7..]);
        assert_eq!(running.finish(), expected);
        assert_eq!(Crc32::new().finish(), crc32(&[]));
    }

    #[test]
    fn every_length_matches_the_independent_bitwise_definition() {
        // Well past four strides, so every partition into whole blocks plus
        // a remainder of each possible width is covered.
        let data: Vec<u8> = (0..=255u8).cycle().take(STRIDE * 4 + 3).collect();
        for length in 0..data.len() {
            let sliced = crc32(&data[..length]);
            let bitwise = crc32_bit_at_a_time(&data[..length]);
            assert_eq!(
                sliced, bitwise,
                "slice-by-{STRIDE} and the bitwise definition disagree at length {length}"
            );
        }
    }

    #[test]
    fn high_bytes_and_repeated_patterns_match_the_bitwise_definition() {
        // Inputs whose bytes concentrate in the ranges a table indexing bug
        // would confuse: all zeros, all ones, and an alternating pattern.
        for pattern in [
            vec![0x00; 129],
            vec![0xFF; 129],
            vec![0xA5, 0x5A].into_iter().cycle().take(129).collect(),
        ] {
            assert_eq!(crc32(&pattern), crc32_bit_at_a_time(&pattern));
        }
    }
}
