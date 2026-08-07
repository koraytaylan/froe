//! The binary references catalog: the `.brf` trailer entry of an archive.
//!
//! The catalog lists, per garbage collection generation and per segment,
//! the identifiers of binaries stored in an *external* blob store that the
//! segment references. Blob garbage collection uses it; content reads never
//! do, so a missing or corrupt catalog is tolerated.
//!
//! The catalog sits immediately before the graph entry and shares the
//! trailer footer shape (checksum, count, size, magic — magic last).

use crate::checksum::crc32;
use crate::segment::identifier::SegmentIdentifier;
use crate::tar_archive::index::{read_u32, read_u64};

/// Magic number of the version 1 catalog (`\n0B\n`), written by Oak 1.6.
const BINARY_REFERENCES_VERSION_1_MAGIC: u32 = 0x0A30_420A;

/// Magic number of the version 2 catalog (`\n1B\n`), written since Oak 1.8.
const BINARY_REFERENCES_VERSION_2_MAGIC: u32 = 0x0A31_420A;

/// Size of the shared trailer footer.
const FOOTER_SIZE: usize = 16;

/// The external binaries referenced by the segments of one garbage
/// collection generation.
#[derive(Clone, Debug)]
pub struct GenerationBinaryReferences {
    /// The garbage collection generation the segments belong to.
    pub generation: i32,
    /// The full garbage collection generation. In a version 1 catalog this
    /// repeats [`Self::generation`].
    pub full_generation: i32,
    /// Whether the segments were produced by a compaction. Always `true` in
    /// a version 1 catalog.
    pub is_compacted: bool,
    /// Per referencing segment: the identifiers of the referenced binaries.
    pub segments: Vec<(SegmentIdentifier, Vec<String>)>,
}

/// The parsed binary references catalog of one archive.
#[derive(Clone, Debug, Default)]
pub struct BinaryReferences {
    /// One entry per generation, in on-disk order.
    pub generations: Vec<GenerationBinaryReferences>,
}

/// Parses the binary references catalog, or returns `None` when it is
/// missing or fails validation.
///
/// `anchor` must be the file position where the catalog structure ends:
/// the start of the graph entry's padded payload region. The caller derives
/// it from the on-disk sizes of the index and graph entries.
#[must_use]
pub fn parse_binary_references(archive_bytes: &[u8], anchor: usize) -> Option<BinaryReferences> {
    if anchor < FOOTER_SIZE || anchor > archive_bytes.len() {
        return None;
    }
    let footer = &archive_bytes[anchor - FOOTER_SIZE..anchor];
    let stored_checksum = read_u32(footer, 0);
    let generation_count = read_u32(footer, 4) as i32;
    let declared_size = read_u32(footer, 8) as i32;
    let magic = read_u32(footer, 12);

    let version = match magic {
        BINARY_REFERENCES_VERSION_1_MAGIC => 1u8,
        BINARY_REFERENCES_VERSION_2_MAGIC => 2u8,
        _ => return None,
    };
    if generation_count < 0 {
        return None;
    }
    // Deliberate quirk kept from BinaryReferencesIndexLoader: the minimum
    // size bound uses 22 bytes per generation in both versions.
    if i64::from(declared_size) < i64::from(generation_count) * 22 + FOOTER_SIZE as i64 {
        return None;
    }
    let declared_size = declared_size as usize;
    let buffer_start = anchor.checked_sub(declared_size)?;
    let buffer = &archive_bytes[buffer_start..anchor];
    let data = &buffer[..declared_size - FOOTER_SIZE];
    if crc32(data) != stored_checksum {
        return None;
    }

    let mut position = 0usize;
    // Capacities are bounded by what the data area could physically hold,
    // so a corrupt count cannot force a huge allocation.
    let mut generations = Vec::with_capacity((generation_count as usize).min(data.len() / 8 + 1));
    for _ in 0..generation_count {
        let (generation, full_generation, is_compacted) = if version == 2 {
            if position + 9 > data.len() {
                return None;
            }
            let generation = read_u32(data, position) as i32;
            let full_generation = read_u32(data, position + 4) as i32;
            let is_compacted = data[position + 8] != 0;
            position += 9;
            (generation, full_generation, is_compacted)
        } else {
            if position + 4 > data.len() {
                return None;
            }
            let generation = read_u32(data, position) as i32;
            position += 4;
            (generation, generation, true)
        };

        if position + 4 > data.len() {
            return None;
        }
        let segment_count = read_u32(data, position) as i32;
        position += 4;
        if segment_count < 0 {
            return None;
        }
        let mut segments = Vec::with_capacity((segment_count as usize).min(data.len() / 20 + 1));
        for _ in 0..segment_count {
            if position + 20 > data.len() {
                return None;
            }
            let segment_identifier =
                SegmentIdentifier::new(read_u64(data, position), read_u64(data, position + 8));
            let reference_count = read_u32(data, position + 16) as i32;
            position += 20;
            if reference_count < 0 {
                return None;
            }
            let mut references =
                Vec::with_capacity((reference_count as usize).min(data.len() / 4 + 1));
            for _ in 0..reference_count {
                if position + 4 > data.len() {
                    return None;
                }
                let length = read_u32(data, position) as i32;
                position += 4;
                if length < 0 || position + length as usize > data.len() {
                    return None;
                }
                // Java decodes reference strings leniently; invalid
                // UTF-8 becomes replacement characters rather than
                // discarding the catalog.
                let reference =
                    String::from_utf8_lossy(&data[position..position + length as usize]);
                references.push(reference.into_owned());
                position += length as usize;
            }
            segments.push((segment_identifier, references));
        }
        generations.push(GenerationBinaryReferences {
            generation,
            full_generation,
            is_compacted,
            segments,
        });
    }
    Some(BinaryReferences { generations })
}

#[cfg(test)]
mod tests {
    use super::parse_binary_references;
    use crate::checksum::crc32;
    use crate::segment::identifier::SegmentIdentifier;

    /// One generation of a synthetic catalog:
    /// generation, full generation, compacted flag, and per-segment
    /// external binary identifiers.
    type GenerationFixture<'a> = (i32, i32, bool, Vec<(SegmentIdentifier, Vec<&'a str>)>);

    /// Serializes a version 2 catalog structure and returns it padded at the
    /// front to a block boundary, with the anchor at its end.
    fn synthetic_catalog(generations: &[GenerationFixture<'_>]) -> (Vec<u8>, usize) {
        let mut data = Vec::new();
        for (generation, full_generation, compacted, segments) in generations {
            data.extend_from_slice(&generation.to_be_bytes());
            data.extend_from_slice(&full_generation.to_be_bytes());
            data.push(u8::from(*compacted));
            data.extend_from_slice(&(segments.len() as u32).to_be_bytes());
            for (identifier, references) in segments {
                data.extend_from_slice(&identifier.most_significant_bits.to_be_bytes());
                data.extend_from_slice(&identifier.least_significant_bits.to_be_bytes());
                data.extend_from_slice(&(references.len() as u32).to_be_bytes());
                for reference in references {
                    data.extend_from_slice(&(reference.len() as u32).to_be_bytes());
                    data.extend_from_slice(reference.as_bytes());
                }
            }
        }
        let total_size = data.len() + 16;
        let padding = total_size.div_ceil(512) * 512 - total_size;
        let mut buffer = vec![0u8; padding];
        buffer.extend_from_slice(&data);
        buffer.extend_from_slice(&crc32(&data).to_be_bytes());
        buffer.extend_from_slice(&(generations.len() as u32).to_be_bytes());
        buffer.extend_from_slice(&(total_size as u32).to_be_bytes());
        buffer.extend_from_slice(&0x0A31_420Au32.to_be_bytes());
        let anchor = buffer.len();
        (buffer, anchor)
    }

    #[test]
    fn parses_version_2_catalog() {
        let segment = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let (buffer, anchor) =
            synthetic_catalog(&[(3, 2, true, vec![(segment, vec!["blob-one", "blob-two"])])]);
        let catalog = parse_binary_references(&buffer, anchor).expect("valid catalog");
        assert_eq!(catalog.generations.len(), 1);
        let generation = &catalog.generations[0];
        assert_eq!(generation.generation, 3);
        assert_eq!(generation.full_generation, 2);
        assert!(generation.is_compacted);
        assert_eq!(generation.segments.len(), 1);
        assert_eq!(generation.segments[0].0, segment);
        assert_eq!(generation.segments[0].1, vec!["blob-one", "blob-two"]);
    }

    #[test]
    fn empty_catalog_is_valid() {
        let (buffer, anchor) = synthetic_catalog(&[]);
        let catalog = parse_binary_references(&buffer, anchor).expect("valid catalog");
        assert!(catalog.generations.is_empty());
    }

    #[test]
    fn corrupt_catalog_yields_none() {
        let segment = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let (mut buffer, anchor) = synthetic_catalog(&[(0, 0, false, vec![(segment, vec!["x"])])]);
        let position = buffer.len() - 20;
        buffer[position] ^= 0x01;
        assert!(parse_binary_references(&buffer, anchor).is_none());
    }

    #[test]
    fn out_of_range_anchor_yields_none() {
        assert!(parse_binary_references(&[0u8; 32], 8).is_none());
        assert!(parse_binary_references(&[0u8; 32], 64).is_none());
    }
}
