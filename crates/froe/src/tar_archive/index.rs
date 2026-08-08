//! The segment index: the `.idx` trailer entry of a segment archive.
//!
//! The index is the last entry of every complete archive, placed so that a
//! reader can find it at a fixed distance from the end of the file: the two
//! terminating zero blocks occupy the final 1024 bytes, and the index payload
//! ends immediately before them with a 16-byte footer whose last four bytes
//! are a magic number. The payload is front-padded with zeros to a 512-byte
//! multiple, so the footer always ends exactly on a block boundary.
//!
//! Two index versions exist. Version 1 (Oak 1.6) stores 28-byte entries;
//! version 2 (Oak 1.8 and later) appends the full garbage collection
//! generation and a compacted flag for 33-byte entries. Entries are sorted
//! by segment UUID compared as *signed* 64-bit halves — a detail that must
//! be preserved for binary search to work.

use crate::checksum::crc32;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;

/// Magic number terminating a version 1 index (`\n0K\n` big-endian).
const INDEX_VERSION_1_MAGIC: u32 = 0x0A30_4B0A;

/// Magic number terminating a version 2 index (`\n1K\n` big-endian).
const INDEX_VERSION_2_MAGIC: u32 = 0x0A31_4B0A;

/// Serialized size of a version 1 index entry.
const INDEX_VERSION_1_ENTRY_SIZE: usize = 28;

/// Serialized size of a version 2 index entry.
const INDEX_VERSION_2_ENTRY_SIZE: usize = 33;

/// Size of the footer shared by every trailer structure:
/// checksum, entry count, total size, and magic number, four bytes each.
const FOOTER_SIZE: usize = 16;

/// The two zero blocks terminating a tar file.
const TERMINATING_ZERO_BLOCKS: usize = 1024;

/// One segment recorded in the index of an archive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentIndexEntry {
    /// The segment's UUID.
    pub segment_identifier: SegmentIdentifier,
    /// File offset of the first byte of segment data (past the tar entry
    /// header), always a multiple of 512.
    pub position: u32,
    /// Exact size of the segment data in bytes.
    pub size: u32,
    /// Garbage collection generation of the segment.
    pub generation: i32,
    /// Full garbage collection generation. In a version 1 index this
    /// repeats [`Self::generation`].
    pub full_generation: i32,
    /// Whether the segment was produced by a compaction. Always `true` in a
    /// version 1 index.
    pub is_compacted: bool,
}

/// The parsed index of one archive: entries sorted by segment UUID.
#[derive(Clone, Debug)]
pub struct SegmentIndex {
    /// The index format version, 1 or 2.
    pub version: u8,
    entries: Vec<SegmentIndexEntry>,
}

impl SegmentIndex {
    /// All entries in their on-disk order (sorted by UUID, compared as
    /// signed 64-bit halves).
    #[must_use]
    pub fn entries(&self) -> &[SegmentIndexEntry] {
        &self.entries
    }

    /// Looks up a segment by identifier via binary search.
    #[must_use]
    pub fn find_entry(&self, segment_identifier: SegmentIdentifier) -> Option<&SegmentIndexEntry> {
        self.entries
            .binary_search_by_key(&signed_order_key(segment_identifier), |entry| {
                signed_order_key(entry.segment_identifier)
            })
            .ok()
            .map(|position| &self.entries[position])
    }
}

/// The UUID comparison key used for index ordering: Java compares the two
/// 64-bit halves as *signed* longs, so identifiers with the top bit set sort
/// before those without.
fn signed_order_key(identifier: SegmentIdentifier) -> (i64, i64) {
    (
        identifier.most_significant_bits as i64,
        identifier.least_significant_bits as i64,
    )
}

/// Parses and validates the index of a complete archive file.
///
/// `archive_bytes` must be the entire file. The validation mirrors the Java
/// `IndexLoader` exactly, including its quirks: the minimum-size check uses
/// the version 1 entry size even for version 2 indexes, and entry ordering
/// is checked with signed comparison. Any failure means the archive has no
/// usable index and the caller must fall back to the recovery scan.
#[allow(
    clippy::too_many_lines,
    reason = "the validation sequence mirrors the Java loader step for step and reads best linearly"
)]
pub fn parse_segment_index(archive_bytes: &[u8]) -> Result<SegmentIndex> {
    let invalid = |details: String| Error::InvalidFormat { details };
    let length = archive_bytes.len();
    if !length.is_multiple_of(512) {
        return Err(invalid(format!(
            "archive length {length} is not a multiple of 512"
        )));
    }
    if length < 6 * 512 {
        return Err(invalid(format!(
            "archive of {length} bytes is too short to hold an index"
        )));
    }
    if length > i32::MAX as usize {
        return Err(invalid(format!(
            "archive of {length} bytes exceeds the 2 GiB addressing limit"
        )));
    }

    let anchor = length - TERMINATING_ZERO_BLOCKS;
    let footer = &archive_bytes[anchor - FOOTER_SIZE..anchor];
    let stored_checksum = read_u32(footer, 0);
    let entry_count = read_u32(footer, 4) as i32;
    let declared_size = read_u32(footer, 8) as i32;
    let magic = read_u32(footer, 12);

    let (version, entry_size) = match magic {
        INDEX_VERSION_1_MAGIC => (1u8, INDEX_VERSION_1_ENTRY_SIZE),
        INDEX_VERSION_2_MAGIC => (2u8, INDEX_VERSION_2_ENTRY_SIZE),
        other => {
            return Err(invalid(format!(
                "unrecognized index magic number {other:#010x}"
            )));
        }
    };
    if entry_count < 1 {
        return Err(invalid(format!("invalid index entry count {entry_count}")));
    }
    let entry_count = entry_count as usize;
    // Deliberate quirk kept from IndexLoaderV2: the minimum size is computed
    // with the version 1 entry size for both versions. The arithmetic is
    // 64-bit so a huge file-supplied count cannot wrap it on 32-bit
    // targets.
    let minimum_size = entry_count as u64 * INDEX_VERSION_1_ENTRY_SIZE as u64 + FOOTER_SIZE as u64;
    if declared_size < 0 || (declared_size as u64) < minimum_size {
        return Err(invalid(format!("invalid index size {declared_size}")));
    }
    if declared_size % 512 != 0 {
        return Err(invalid(format!(
            "index size {declared_size} is not aligned to 512 bytes"
        )));
    }

    // Checked: on 32-bit targets a huge entry count could otherwise wrap
    // past the bounds check below.
    let entries_size = entry_count.checked_mul(entry_size).ok_or_else(|| {
        invalid(format!(
            "index with {entry_count} entries does not fit the archive"
        ))
    })?;
    let entries_end = anchor - FOOTER_SIZE;
    let entries_start = entries_end.checked_sub(entries_size).ok_or_else(|| {
        invalid(format!(
            "index with {entry_count} entries does not fit the archive"
        ))
    })?;
    let entries_bytes = &archive_bytes[entries_start..entries_end];
    if crc32(entries_bytes) != stored_checksum {
        return Err(invalid("index checksum mismatch".to_owned()));
    }

    let mut entries = Vec::with_capacity(entry_count);
    let mut previous_key: Option<(i64, i64)> = None;
    for entry_index in 0..entry_count {
        let entry_bytes = &entries_bytes[entry_index * entry_size..(entry_index + 1) * entry_size];
        let most_significant_bits = read_u64(entry_bytes, 0);
        let least_significant_bits = read_u64(entry_bytes, 8);
        let position = read_u32(entry_bytes, 16) as i32;
        let size = read_u32(entry_bytes, 20) as i32;
        let generation = read_u32(entry_bytes, 24) as i32;
        let (full_generation, is_compacted) = if version == 2 {
            (read_u32(entry_bytes, 28) as i32, entry_bytes[32] != 0)
        } else {
            (generation, true)
        };

        let key = (most_significant_bits as i64, least_significant_bits as i64);
        if let Some(previous) = previous_key {
            if previous > key {
                return Err(invalid(
                    "index entries are not sorted by segment identifier".to_owned(),
                ));
            }
            if previous == key {
                return Err(invalid("duplicate segment identifier in index".to_owned()));
            }
        }
        previous_key = Some(key);

        if position < 0 {
            return Err(invalid(format!("negative index entry position {position}")));
        }
        if position % 512 != 0 {
            return Err(invalid(format!(
                "index entry position {position} is not aligned to 512 bytes"
            )));
        }
        if size < 1 {
            return Err(invalid(format!("invalid index entry size {size}")));
        }

        entries.push(SegmentIndexEntry {
            segment_identifier: SegmentIdentifier::new(
                most_significant_bits,
                least_significant_bits,
            ),
            position: position as u32,
            size: size as u32,
            generation,
            full_generation,
            is_compacted,
        });
    }

    Ok(SegmentIndex { version, entries })
}

/// The bytes the index occupies on disk as a complete tar entry: header
/// block plus the zero-padded payload. Needed to locate the graph entry,
/// which sits immediately before the index entry.
#[must_use]
pub fn index_entry_disk_size(index: &SegmentIndex) -> usize {
    let entry_size = if index.version == 2 {
        INDEX_VERSION_2_ENTRY_SIZE
    } else {
        INDEX_VERSION_1_ENTRY_SIZE
    };
    let payload = index.entries.len() * entry_size + FOOTER_SIZE;
    512 + payload.div_ceil(512) * 512
}

/// Reads a big-endian unsigned 32-bit integer.
pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

/// Reads a big-endian unsigned 64-bit integer.
pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
}

#[cfg(test)]
mod tests {
    use super::{SegmentIndex, index_entry_disk_size, parse_segment_index};
    use crate::checksum::crc32;
    use crate::segment::identifier::SegmentIdentifier;

    /// Builds a minimal archive: zero-filled body, index payload, footer,
    /// and the two terminating zero blocks.
    fn synthetic_archive(entries: &[(u64, u64, u32, u32, i32, i32, bool)], version: u8) -> Vec<u8> {
        let entry_size = if version == 2 { 33 } else { 28 };
        let mut serialized_entries = Vec::new();
        for &(most, least, position, size, generation, full_generation, compacted) in entries {
            serialized_entries.extend_from_slice(&most.to_be_bytes());
            serialized_entries.extend_from_slice(&least.to_be_bytes());
            serialized_entries.extend_from_slice(&position.to_be_bytes());
            serialized_entries.extend_from_slice(&size.to_be_bytes());
            serialized_entries.extend_from_slice(&generation.to_be_bytes());
            if version == 2 {
                serialized_entries.extend_from_slice(&full_generation.to_be_bytes());
                serialized_entries.push(u8::from(compacted));
            }
        }
        assert_eq!(serialized_entries.len(), entries.len() * entry_size);

        let data_size = serialized_entries.len() + 16;
        let padded_size = data_size.div_ceil(512) * 512;
        let magic: u32 = if version == 2 {
            0x0A31_4B0A
        } else {
            0x0A30_4B0A
        };

        // Body: three zero blocks standing in for segment entries (keeping
        // the file at the 3072-byte minimum), then the front-padded index
        // payload, then the footer and two zero blocks.
        let mut archive = vec![0u8; 3 * 512];
        archive.extend(std::iter::repeat_n(0u8, padded_size - data_size));
        archive.extend_from_slice(&serialized_entries);
        archive.extend_from_slice(&crc32(&serialized_entries).to_be_bytes());
        archive.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        archive.extend_from_slice(&(padded_size as u32).to_be_bytes());
        archive.extend_from_slice(&magic.to_be_bytes());
        archive.extend_from_slice(&[0u8; 1024]);
        assert_eq!(archive.len() % 512, 0);
        archive
    }

    fn parse(entries: &[(u64, u64, u32, u32, i32, i32, bool)], version: u8) -> SegmentIndex {
        parse_segment_index(&synthetic_archive(entries, version)).expect("valid index")
    }

    #[test]
    fn parses_version_2_entries() {
        let index = parse(
            &[
                (1, 0xA000_0000_0000_0001, 0, 512, 3, 2, true),
                (2, 0xA000_0000_0000_0002, 512, 100, 3, 2, false),
            ],
            2,
        );
        assert_eq!(index.version, 2);
        assert_eq!(index.entries().len(), 2);
        assert_eq!(index.entries()[0].position, 0);
        assert_eq!(index.entries()[1].size, 100);
        assert!(index.entries()[0].is_compacted);
        assert!(!index.entries()[1].is_compacted);
        assert_eq!(index.entries()[1].full_generation, 2);
    }

    #[test]
    fn version_1_entries_repeat_generation_and_are_compacted() {
        let index = parse(&[(5, 0xB000_0000_0000_0005, 512, 4096, 7, 0, false)], 1);
        assert_eq!(index.version, 1);
        let entry = &index.entries()[0];
        assert_eq!(entry.full_generation, 7);
        assert!(entry.is_compacted);
    }

    #[test]
    fn finds_entries_by_identifier() {
        let index = parse(
            &[
                (1, 0xA000_0000_0000_0001, 0, 512, 0, 0, true),
                (9, 0xA000_0000_0000_0009, 512, 512, 0, 0, true),
            ],
            2,
        );
        let found = index
            .find_entry(SegmentIdentifier::new(9, 0xA000_0000_0000_0009))
            .expect("entry exists");
        assert_eq!(found.position, 512);
        assert!(
            index
                .find_entry(SegmentIdentifier::new(4, 0xA000_0000_0000_0004))
                .is_none()
        );
    }

    #[test]
    fn signed_ordering_puts_high_bit_identifiers_first() {
        // 0xFFFF... as a signed long is -1 and sorts before 1.
        let index = parse(
            &[
                (
                    0xFFFF_FFFF_FFFF_FFFF,
                    0xA000_0000_0000_0001,
                    0,
                    512,
                    0,
                    0,
                    true,
                ),
                (1, 0xA000_0000_0000_0001, 512, 512, 0, 0, true),
            ],
            2,
        );
        assert!(
            index
                .find_entry(SegmentIdentifier::new(
                    0xFFFF_FFFF_FFFF_FFFF,
                    0xA000_0000_0000_0001
                ))
                .is_some()
        );
        assert!(
            index
                .find_entry(SegmentIdentifier::new(1, 0xA000_0000_0000_0001))
                .is_some()
        );
    }

    #[test]
    fn version_2_size_check_keeps_the_version_1_constant_quirk() {
        // Load-bearing quirk (tar-layer.md §4.4): the version 2 loader
        // validates the footer's size field against the version *1* entry
        // size (28), so a declared size in
        // [count*28 + 16, count*33 + 16) passes — Java accepts such a
        // file, and a "corrected" implementation using 33 would reject
        // it. With 32 entries the window is [912, 1072); 1024 is the one
        // 512-aligned value inside it.
        let entries: Vec<(u64, u64, u32, u32, i32, i32, bool)> = (1..=32u64)
            .map(|seed| (seed, 0xA000_0000_0000_0000 | seed, 0, 512, 0, 0, true))
            .collect();
        let mut archive = synthetic_archive(&entries, 2);
        let size_field_position = archive.len() - 1024 - 8;
        archive[size_field_position..size_field_position + 4]
            .copy_from_slice(&1024u32.to_be_bytes());
        let index = parse_segment_index(&archive).expect("the quirky size is accepted");
        assert_eq!(index.entries().len(), 32);
    }

    #[test]
    fn rejects_unsorted_duplicate_and_corrupt_indexes() {
        let unsorted = synthetic_archive(
            &[
                (2, 0xA000_0000_0000_0002, 0, 512, 0, 0, true),
                (1, 0xA000_0000_0000_0001, 512, 512, 0, 0, true),
            ],
            2,
        );
        assert!(parse_segment_index(&unsorted).is_err());

        let duplicated = synthetic_archive(
            &[
                (1, 0xA000_0000_0000_0001, 0, 512, 0, 0, true),
                (1, 0xA000_0000_0000_0001, 512, 512, 0, 0, true),
            ],
            2,
        );
        assert!(parse_segment_index(&duplicated).is_err());

        let mut bad_checksum = synthetic_archive(&[(1, 2, 0, 512, 0, 0, true)], 2);
        let length = bad_checksum.len();
        // Corrupt one entry byte without touching the stored checksum.
        bad_checksum[length - 1024 - 16 - 33] ^= 0xFF;
        assert!(parse_segment_index(&bad_checksum).is_err());
    }

    #[test]
    fn rejects_misaligned_and_undersized_entries() {
        let misaligned_position = synthetic_archive(&[(1, 2, 100, 512, 0, 0, true)], 2);
        assert!(parse_segment_index(&misaligned_position).is_err());

        let zero_size = synthetic_archive(&[(1, 2, 0, 0, 0, 0, true)], 2);
        assert!(parse_segment_index(&zero_size).is_err());
    }

    #[test]
    fn rejects_archives_without_index() {
        assert!(parse_segment_index(&vec![0u8; 512 * 6]).is_err());
        assert!(parse_segment_index(&[]).is_err());
        assert!(parse_segment_index(&[0u8; 100]).is_err());
    }

    #[test]
    fn computes_index_entry_disk_size() {
        let index = parse(&[(1, 0xA000_0000_0000_0001, 0, 512, 0, 0, true)], 2);
        // 33 bytes + 16-byte footer = 49 bytes, padded to 512, plus header.
        assert_eq!(index_entry_disk_size(&index), 1024);
    }
}
