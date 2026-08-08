//! CRC32 checksum as used by the segment-tar format.
//!
//! The tar index, graph, and binary references entries each carry a CRC32
//! checksum (the standard IEEE polynomial, identical to `java.util.zip.CRC32`)
//! over their payload bytes. The checksummed blocks are at most a few
//! megabytes and are verified once when an archive is opened, so a compact
//! table-driven implementation is sufficient.

/// Lookup table for the reflected IEEE CRC32 polynomial `0xEDB88320`.
const CRC32_TABLE: [u32; 256] = generate_crc32_table();

const fn generate_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
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
        table[index] = value;
        index += 1;
    }
    table
}

/// Computes the IEEE CRC32 checksum of `bytes`, matching `java.util.zip.CRC32`.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut checksum = 0xFFFF_FFFFu32;
    for &byte in bytes {
        let table_index = ((checksum ^ u32::from(byte)) & 0xFF) as usize;
        checksum = (checksum >> 8) ^ CRC32_TABLE[table_index];
    }
    !checksum
}

#[cfg(test)]
mod tests {
    use super::crc32;

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
}
