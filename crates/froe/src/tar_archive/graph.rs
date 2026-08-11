//! The segment graph: the `.gph` trailer entry of a segment archive.
//!
//! The graph records, for every data segment in the archive, which segments
//! its records reference. Garbage collection and diagnostics use it; plain
//! content reads never need it, so a missing or corrupt graph is tolerated
//! (the Java implementation recomputes it from the segments in that case).
//!
//! On disk the graph sits immediately before the index entry and shares the
//! trailer footer shape: 16 bytes of checksum, entry count, total size, and
//! magic number, with the magic in the last four bytes.

use std::collections::HashMap;

use crate::checksum::crc32;
use crate::segment::identifier::SegmentIdentifier;
use crate::tar_archive::index::{SegmentIndex, index_entry_disk_size, read_u32, read_u64};

/// Magic number terminating a graph structure (`\n0G\n` big-endian).
const GRAPH_MAGIC: u32 = 0x0A30_470A;

/// Size of the shared trailer footer.
const FOOTER_SIZE: usize = 16;

/// The adjacency data of one archive: source segment to referenced segments.
#[derive(Clone, Debug, Default)]
pub struct SegmentGraph {
    /// For each data segment (in on-disk order): the segments it references.
    pub adjacency: Vec<(SegmentIdentifier, Vec<SegmentIdentifier>)>,
}

/// Result of parsing a graph under a caller-provided work limit.
pub(crate) enum BoundedSegmentGraph {
    /// A valid graph and the graph-data bytes inspected to parse it.
    Available {
        graph: SegmentGraph,
        work_units: u64,
    },
    /// No structurally valid graph was present. `work_units` accounts for
    /// graph-data bytes checksummed or parsed before discovering corruption.
    Unavailable { work_units: u64 },
    /// The declared graph data alone exceeds the caller's remaining work.
    WorkBudgetExceeded { attempted_work_units: u64 },
    /// The graph declares more rows or edges than the caller permits.
    GraphBudgetExceeded {
        attempted_rows: usize,
        attempted_edges: usize,
    },
}

impl SegmentGraph {
    /// The total unpadded size of the graph structure on disk, including its
    /// footer. Needed to locate the binary references entry, which sits
    /// immediately before the graph entry.
    #[must_use]
    pub fn disk_structure_size(&self) -> usize {
        FOOTER_SIZE
            + self
                .adjacency
                .iter()
                .map(|(_, references)| 20 + 16 * references.len())
                .sum::<usize>()
    }

    /// The adjacency data as a map for random access.
    #[must_use]
    pub fn as_map(&self) -> HashMap<SegmentIdentifier, &[SegmentIdentifier]> {
        self.adjacency
            .iter()
            .map(|(source, references)| (*source, references.as_slice()))
            .collect()
    }
}

/// Parses the graph of a complete archive, or returns `None` when the graph
/// is missing or fails validation — mirroring the Java reader, which treats
/// every graph problem as "no graph available".
#[must_use]
pub fn parse_segment_graph(archive_bytes: &[u8], index: &SegmentIndex) -> Option<SegmentGraph> {
    match parse_segment_graph_with_maximum_work(archive_bytes, index, u64::MAX) {
        BoundedSegmentGraph::Available { graph, .. } => Some(graph),
        BoundedSegmentGraph::Unavailable { .. }
        | BoundedSegmentGraph::WorkBudgetExceeded { .. }
        | BoundedSegmentGraph::GraphBudgetExceeded { .. } => None,
    }
}

/// Parses like [`parse_segment_graph`], but refuses before checksum work or
/// graph-vector allocation when the declared graph data exceeds `maximum_work_units`.
pub(crate) fn parse_segment_graph_with_maximum_work(
    archive_bytes: &[u8],
    index: &SegmentIndex,
    maximum_work_units: u64,
) -> BoundedSegmentGraph {
    parse_segment_graph_with_limits(
        archive_bytes,
        index,
        maximum_work_units,
        usize::MAX,
        usize::MAX,
    )
}

/// Parses a graph while bounding checksum/parse work and all row/edge
/// vectors before allocating them.
pub(crate) fn parse_segment_graph_with_limits(
    archive_bytes: &[u8],
    index: &SegmentIndex,
    maximum_work_units: u64,
    maximum_rows: usize,
    maximum_edges: usize,
) -> BoundedSegmentGraph {
    let anchor = archive_bytes
        .len()
        .checked_sub(1024 + index_entry_disk_size(index));
    let Some(anchor) = anchor else {
        return BoundedSegmentGraph::Unavailable { work_units: 0 };
    };
    if anchor < FOOTER_SIZE {
        return BoundedSegmentGraph::Unavailable { work_units: 0 };
    }
    let footer = &archive_bytes[anchor - FOOTER_SIZE..anchor];
    let stored_checksum = read_u32(footer, 0);
    let entry_count = read_u32(footer, 4) as i32;
    let declared_size = read_u32(footer, 8) as i32;
    let magic = read_u32(footer, 12);

    if magic != GRAPH_MAGIC || entry_count < 0 {
        return BoundedSegmentGraph::Unavailable { work_units: 0 };
    }
    // Deliberate quirk kept from SegmentGraph.load: the minimum-size bound
    // uses a constant left over from an older graph format.
    if i64::from(declared_size) < 4 + i64::from(entry_count) * 34 {
        return BoundedSegmentGraph::Unavailable { work_units: 0 };
    }
    // The quirk bound alone would accept a declared size smaller than the
    // footer (for a zero entry count); reject it instead of underflowing.
    if declared_size < FOOTER_SIZE as i32 {
        return BoundedSegmentGraph::Unavailable { work_units: 0 };
    }
    let declared_size = declared_size as usize;
    let Some(buffer_start) = anchor.checked_sub(declared_size) else {
        return BoundedSegmentGraph::Unavailable { work_units: 0 };
    };
    let buffer = &archive_bytes[buffer_start..anchor];
    let data = &buffer[..declared_size - FOOTER_SIZE];
    let work_units = u64::try_from(data.len()).unwrap_or(u64::MAX);
    if work_units > maximum_work_units {
        return BoundedSegmentGraph::WorkBudgetExceeded {
            attempted_work_units: work_units,
        };
    }
    if crc32(data) != stored_checksum {
        return BoundedSegmentGraph::Unavailable { work_units };
    }

    let entry_count = entry_count as usize;
    if entry_count > maximum_rows {
        return BoundedSegmentGraph::GraphBudgetExceeded {
            attempted_rows: entry_count,
            attempted_edges: 0,
        };
    }

    let mut position = 0usize;
    let mut total_edges = 0usize;
    let mut adjacency = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        if position + 20 > data.len() {
            return BoundedSegmentGraph::Unavailable { work_units };
        }
        let source = SegmentIdentifier::new(read_u64(data, position), read_u64(data, position + 8));
        let reference_count = read_u32(data, position + 16) as i32;
        position += 20;
        if reference_count < 0 {
            return BoundedSegmentGraph::Unavailable { work_units };
        }
        let reference_count = reference_count as usize;
        // Checked: on 32-bit targets a huge reference count could
        // otherwise wrap this bound and pass.
        let references_end = reference_count
            .checked_mul(16)
            .and_then(|references_size| position.checked_add(references_size));
        if references_end.is_none_or(|end| end > data.len()) {
            return BoundedSegmentGraph::Unavailable { work_units };
        }
        total_edges = total_edges.saturating_add(reference_count);
        if total_edges > maximum_edges {
            return BoundedSegmentGraph::GraphBudgetExceeded {
                attempted_rows: entry_count,
                attempted_edges: total_edges,
            };
        }
        let mut references = Vec::with_capacity(reference_count);
        for _ in 0..reference_count {
            references.push(SegmentIdentifier::new(
                read_u64(data, position),
                read_u64(data, position + 8),
            ));
            position += 16;
        }
        adjacency.push((source, references));
    }
    BoundedSegmentGraph::Available {
        graph: SegmentGraph { adjacency },
        work_units,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedSegmentGraph, SegmentGraph, parse_segment_graph,
        parse_segment_graph_with_maximum_work,
    };
    use crate::checksum::crc32;
    use crate::segment::identifier::SegmentIdentifier;
    use crate::tar_archive::index::parse_segment_index;

    /// Builds an archive holding one segment data block, a graph, an index,
    /// and the terminating zero blocks.
    fn synthetic_archive(adjacency: &[(SegmentIdentifier, Vec<SegmentIdentifier>)]) -> Vec<u8> {
        // Graph payload.
        let mut graph_data = Vec::new();
        for (source, references) in adjacency {
            graph_data.extend_from_slice(&source.most_significant_bits.to_be_bytes());
            graph_data.extend_from_slice(&source.least_significant_bits.to_be_bytes());
            graph_data.extend_from_slice(&(references.len() as u32).to_be_bytes());
            for reference in references {
                graph_data.extend_from_slice(&reference.most_significant_bits.to_be_bytes());
                graph_data.extend_from_slice(&reference.least_significant_bits.to_be_bytes());
            }
        }
        let graph_size = graph_data.len() + 16;
        let mut graph_entry = Vec::new();
        // Padding before the data, so the structure ends on a block boundary.
        graph_entry.extend(std::iter::repeat_n(
            0u8,
            graph_size.div_ceil(512) * 512 - graph_size,
        ));
        graph_entry.extend_from_slice(&graph_data);
        graph_entry.extend_from_slice(&crc32(&graph_data).to_be_bytes());
        graph_entry.extend_from_slice(&(adjacency.len() as u32).to_be_bytes());
        graph_entry.extend_from_slice(&(graph_size as u32).to_be_bytes());
        graph_entry.extend_from_slice(&0x0A30_470Au32.to_be_bytes());

        // Index payload for a single fake segment entry.
        let mut index_entries = Vec::new();
        index_entries.extend_from_slice(&1u64.to_be_bytes());
        index_entries.extend_from_slice(&0xA000_0000_0000_0001u64.to_be_bytes());
        index_entries.extend_from_slice(&0u32.to_be_bytes());
        index_entries.extend_from_slice(&512u32.to_be_bytes());
        index_entries.extend_from_slice(&0u32.to_be_bytes());
        index_entries.extend_from_slice(&0u32.to_be_bytes());
        index_entries.push(1);
        let index_data_size = index_entries.len() + 16;
        let index_padded = index_data_size.div_ceil(512) * 512;

        let mut archive = vec![0u8; 512];
        archive.extend_from_slice(&graph_entry);
        // The index entry's tar header block precedes its payload; the
        // graph anchor computation accounts for it.
        archive.extend_from_slice(&[0u8; 512]);
        archive.extend(std::iter::repeat_n(0u8, index_padded - index_data_size));
        archive.extend_from_slice(&index_entries);
        archive.extend_from_slice(&crc32(&index_entries).to_be_bytes());
        archive.extend_from_slice(&1u32.to_be_bytes());
        archive.extend_from_slice(&(index_padded as u32).to_be_bytes());
        archive.extend_from_slice(&0x0A31_4B0Au32.to_be_bytes());
        archive.extend_from_slice(&[0u8; 1024]);
        assert_eq!(archive.len() % 512, 0);
        archive
    }

    #[test]
    fn parses_adjacency_lists() {
        let source = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let first = SegmentIdentifier::new(2, 0xA000_0000_0000_0002);
        let second = SegmentIdentifier::new(3, 0xB000_0000_0000_0003);
        let archive = synthetic_archive(&[(source, vec![first, second])]);
        let index = parse_segment_index(&archive).expect("valid index");
        let graph = parse_segment_graph(&archive, &index).expect("valid graph");
        assert_eq!(graph.adjacency.len(), 1);
        assert_eq!(graph.adjacency[0].0, source);
        assert_eq!(graph.adjacency[0].1, vec![first, second]);
        assert_eq!(graph.as_map()[&source].len(), 2);
    }

    #[test]
    fn bounded_parser_refuses_before_graph_allocation_and_checksum_work() {
        let source = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let first = SegmentIdentifier::new(2, 0xA000_0000_0000_0002);
        let second = SegmentIdentifier::new(3, 0xB000_0000_0000_0003);
        let archive = synthetic_archive(&[(source, vec![first, second])]);
        let index = parse_segment_index(&archive).expect("valid index");

        assert!(matches!(
            parse_segment_graph_with_maximum_work(&archive, &index, 51),
            BoundedSegmentGraph::WorkBudgetExceeded {
                attempted_work_units: 52,
            }
        ));
        assert!(matches!(
            parse_segment_graph_with_maximum_work(&archive, &index, 52),
            BoundedSegmentGraph::Available { work_units: 52, .. }
        ));
    }

    #[test]
    fn empty_graph_is_valid() {
        let archive = synthetic_archive(&[]);
        let index = parse_segment_index(&archive).expect("valid index");
        let graph = parse_segment_graph(&archive, &index).expect("valid graph");
        assert!(graph.adjacency.is_empty());
    }

    #[test]
    fn corrupt_graph_yields_none() {
        let source = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let mut archive = synthetic_archive(&[(source, vec![])]);
        let index = parse_segment_index(&archive).expect("valid index");
        // Flip a byte inside the graph data area.
        let length = archive.len();
        archive[length - 1024 - 1024 - 20] ^= 0x01;
        assert!(parse_segment_graph(&archive, &index).is_none());
    }

    #[test]
    fn disk_structure_size_matches_serialized_size() {
        let source = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let reference = SegmentIdentifier::new(2, 0xA000_0000_0000_0002);
        let graph = SegmentGraph {
            adjacency: vec![(source, vec![reference])],
        };
        assert_eq!(graph.disk_structure_size(), 16 + 20 + 16);
    }
}
