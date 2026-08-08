//! Parsing of standard 512-byte tar entry headers.
//!
//! Oak stores segments inside ordinary tar archives, but the normal read
//! path never touches entry headers: segments are located through the index
//! at the end of the file. Headers are only parsed by the *recovery scan*
//! when an archive has no valid index — for example the archive a live
//! repository is currently writing, which gets its index only when closed.
//! The parsing here therefore mirrors the deliberately tolerant Java
//! recovery code: names decode lossily and sizes stop at the first
//! non-octal byte, because unparseable entries are skipped, not fatal.

/// The tar block size; headers occupy one block and entry contents are
/// zero-padded to a multiple of it.
pub const BLOCK_SIZE: u64 = 512;

/// The two fields of a tar entry header that the segment store uses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TarEntryHeader {
    /// The entry name, for example
    /// `f81378fb-92b1-4b52-a5c8-e0a67152ed2c.1f2e3d4c` for a segment or
    /// `data00000a.tar.idx` for the archive index.
    pub name: String,
    /// The size of the entry content in bytes, excluding header and
    /// padding. The Java scanner accumulates this in a wrapping 32-bit
    /// signed integer, so oversized octal fields wrap — possibly to a
    /// negative value — and the scan continues; reproducing that keeps
    /// the recovery scan finding every segment Java would find.
    pub size: i64,
}

impl TarEntryHeader {
    /// Parses one 512-byte header block.
    ///
    /// Returns `None` for a block shorter than [`BLOCK_SIZE`] bytes or of
    /// all zero bytes (tar uses two such blocks to terminate the
    /// archive). Malformed content never fails: the name decodes lossily
    /// and the size accumulates octal digits from the size field until
    /// the first non-octal byte, exactly like the Java recovery scanner —
    /// entries with nonsense names or sizes are later skipped by the
    /// caller's pattern matching and bounds checks.
    #[must_use]
    pub fn parse(block: &[u8]) -> Option<Self> {
        let block = block.get(..BLOCK_SIZE as usize)?;
        if block.iter().all(|&byte| byte == 0) {
            return None;
        }
        let name_field = &block[0..100];
        let name_length = name_field.iter().position(|&byte| byte == 0).unwrap_or(100);
        let name = String::from_utf8_lossy(&name_field[..name_length]).into_owned();

        let mut size: i32 = 0;
        for &byte in &block[124..136] {
            match byte {
                b'0'..=b'7' => {
                    size = size.wrapping_mul(8).wrapping_add(i32::from(byte - b'0'));
                }
                _ => break,
            }
        }
        Some(Self {
            name,
            size: i64::from(size),
        })
    }

    /// The number of bytes a well-formed entry occupies on disk including
    /// its header block and the zero padding after the content. Meaningful
    /// only for non-negative sizes; a wrapped negative size is clamped.
    #[must_use]
    pub fn occupied_bytes(&self) -> u64 {
        BLOCK_SIZE + (self.size.max(0) as u64).div_ceil(BLOCK_SIZE) * BLOCK_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_SIZE, TarEntryHeader};

    fn header_block(name: &str, size_field: &[u8]) -> Vec<u8> {
        let mut block = vec![0u8; BLOCK_SIZE as usize];
        block[..name.len()].copy_from_slice(name.as_bytes());
        block[124..124 + size_field.len()].copy_from_slice(size_field);
        block
    }

    #[test]
    fn parses_name_and_octal_size() {
        let block = header_block("data00000a.tar.idx", b"00000001750\0");
        let header = TarEntryHeader::parse(&block).expect("not a terminator block");
        assert_eq!(header.name, "data00000a.tar.idx");
        assert_eq!(header.size, 0o1750);
    }

    #[test]
    fn zero_block_yields_none() {
        assert_eq!(TarEntryHeader::parse(&vec![0u8; BLOCK_SIZE as usize]), None);
    }

    #[test]
    fn short_blocks_yield_none_instead_of_panicking() {
        assert_eq!(TarEntryHeader::parse(&[]), None);
        assert_eq!(TarEntryHeader::parse(&[0x41u8; 100]), None);
        assert_eq!(
            TarEntryHeader::parse(&vec![0u8; BLOCK_SIZE as usize - 1]),
            None
        );
    }

    #[test]
    fn size_parsing_stops_at_first_non_octal_byte() {
        let block = header_block("entry", b"0017x9\0");
        let header = TarEntryHeader::parse(&block).expect("header");
        assert_eq!(header.size, 0o17);

        let spaces = header_block("entry", b"   123\0");
        assert_eq!(TarEntryHeader::parse(&spaces).expect("header").size, 0);
    }

    #[test]
    fn oversized_octal_sizes_wrap_at_32_bits() {
        // 0o77777777777 = 0x1FFFFFFFFF wraps to -1 in a 32-bit integer.
        let wrapped = header_block("entry", b"77777777777\0");
        assert_eq!(TarEntryHeader::parse(&wrapped).expect("header").size, -1);

        // 0o40000000000 = 2^32 wraps to 0.
        let wrapped_to_zero = header_block("entry", b"40000000000\0");
        assert_eq!(
            TarEntryHeader::parse(&wrapped_to_zero)
                .expect("header")
                .size,
            0
        );
    }

    #[test]
    fn name_decodes_lossily() {
        let mut block = vec![0u8; BLOCK_SIZE as usize];
        block[0] = 0xFF;
        block[1] = b'x';
        let header = TarEntryHeader::parse(&block).expect("header");
        assert_eq!(header.name, "\u{FFFD}x");
    }

    #[test]
    fn occupied_bytes_includes_header_and_padding() {
        let header = TarEntryHeader {
            name: "x".to_owned(),
            size: 1,
        };
        assert_eq!(header.occupied_bytes(), 1024);
        let aligned = TarEntryHeader {
            name: "x".to_owned(),
            size: 512,
        };
        assert_eq!(aligned.occupied_bytes(), 1024);
        let empty = TarEntryHeader {
            name: "x".to_owned(),
            size: 0,
        };
        assert_eq!(empty.occupied_bytes(), 512);
        let wrapped = TarEntryHeader {
            name: "x".to_owned(),
            size: -1,
        };
        assert_eq!(wrapped.occupied_bytes(), 512);
    }
}
