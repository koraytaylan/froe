//! The segment graph of one archive: a valid stored graph is trusted and
//! totalized, a missing or corrupt one is reconstructed from each data
//! segment's reference table.

use super::{
    ArchiveDebugError, ArchiveDebugOptions, ArchiveDebugResult, BoundedSegmentGraph, HashMap,
    HashSet, ParsedSegment, SegmentGraph, SegmentIdentifier, WorkBudget,
};

/// Where the diagnostic graph obtained its edges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ArchiveGraphOrigin {
    /// A structurally valid `.gph` trailer was present and trusted.
    Stored,
    /// The `.gph` trailer was missing or invalid, so data-segment reference
    /// tables were read directly.
    Reconstructed,
}

/// Reference-set availability for one segment in the diagnostic graph.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ArchiveGraphReferences {
    /// The segment's outgoing references, deduplicated and sorted by UUID.
    Available(Vec<SegmentIdentifier>),
    /// Reconstruction could not parse this data segment. Keeping the row is
    /// a safer diagnostic deviation from Oak, whose graph computation fails
    /// the whole request in this case.
    Unavailable {
        /// Terminal rendering must sanitize this diagnostic before printing.
        details: String,
    },
}

/// One totalized graph row for a segment in the requested archive.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct ArchiveGraphRow {
    /// Source segment from the requested archive.
    pub segment_identifier: SegmentIdentifier,
    /// Outgoing graph edges, or a local reconstruction failure.
    pub references: ArchiveGraphReferences,
}

/// Oak `TarFiles.getGraph`-style graph for one active archive.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct ArchiveDebugGraph {
    /// Whether edges came from the stored trailer or segment bytes.
    pub origin: ArchiveGraphOrigin,
    /// Exactly one row per archive segment, in archive index/scan order.
    pub rows: Vec<ArchiveGraphRow>,
}

pub(crate) fn diagnostic_archive_graph(
    archive: &crate::tar_archive::TarArchiveReader,
    work_budget: &mut WorkBudget,
    options: ArchiveDebugOptions,
) -> ArchiveDebugResult<ArchiveDebugGraph> {
    let segment_count = archive.segment_count();
    check_graph_budget(options, segment_count, 0)?;
    work_budget.charge_one()?;
    match archive.segment_graph_with_limits(
        work_budget.remaining(),
        options.maximum_graph_rows,
        options.maximum_graph_edges,
    ) {
        BoundedSegmentGraph::Available { graph, work_units } => {
            work_budget.charge_amount(work_units)?;
            return totalize_stored_graph(archive, &graph, work_budget, options);
        }
        BoundedSegmentGraph::Unavailable { work_units } => {
            work_budget.charge_amount(work_units)?;
        }
        BoundedSegmentGraph::WorkBudgetExceeded {
            attempted_work_units,
        } => return Err(work_budget.exceeded_by(attempted_work_units)),
        BoundedSegmentGraph::GraphBudgetExceeded {
            attempted_rows,
            attempted_edges,
        } => return Err(graph_budget_error(options, attempted_rows, attempted_edges)),
    }

    work_budget.charge_many(segment_count)?;
    let mut rows = Vec::with_capacity(segment_count);
    let mut graph_edges = 0usize;
    for segment_identifier in archive.segment_identifiers() {
        let references = if segment_identifier.is_data_segment() {
            match archive.segment_data(segment_identifier) {
                None => ArchiveGraphReferences::Unavailable {
                    details: "archive index does not resolve this segment's bytes".to_owned(),
                },
                Some(bytes) => {
                    // Parsing validates and materializes the record table, so
                    // charge the complete segment byte slice before entering
                    // the parser rather than only its declared references.
                    work_budget.charge_many(bytes.len())?;
                    match ParsedSegment::validated_data_segment_reference_count(
                        segment_identifier,
                        bytes,
                    ) {
                        Ok(reference_count) => {
                            graph_edges = graph_edges.saturating_add(reference_count);
                            check_graph_budget(options, segment_count, graph_edges)?;
                            work_budget.charge_many(reference_count)?;
                            match ParsedSegment::parse(segment_identifier, bytes) {
                                Ok(segment) => ArchiveGraphReferences::Available(
                                    sorted_unique_segment_identifiers(segment.referenced_segments),
                                ),
                                Err(error) => ArchiveGraphReferences::Unavailable {
                                    details: error.to_string(),
                                },
                            }
                        }
                        Err(error) => ArchiveGraphReferences::Unavailable {
                            details: error.to_string(),
                        },
                    }
                }
            }
        } else {
            ArchiveGraphReferences::Available(Vec::new())
        };
        rows.push(ArchiveGraphRow {
            segment_identifier,
            references,
        });
    }
    Ok(ArchiveDebugGraph {
        origin: ArchiveGraphOrigin::Reconstructed,
        rows,
    })
}

pub(crate) fn totalize_stored_graph(
    archive: &crate::tar_archive::TarArchiveReader,
    stored_graph: &SegmentGraph,
    work_budget: &mut WorkBudget,
    options: ArchiveDebugOptions,
) -> ArchiveDebugResult<ArchiveDebugGraph> {
    let mut references_by_source: HashMap<SegmentIdentifier, HashSet<SegmentIdentifier>> =
        HashMap::new();
    for (source, references) in &stored_graph.adjacency {
        work_budget.charge_one()?;
        work_budget.charge_many(references.len())?;
        // SegmentGraph.parse stores each row with Map.put, so a duplicate
        // source replaces the earlier row. Each row's vertices have set
        // semantics before TarFiles.getGraph exposes them.
        references_by_source.insert(*source, references.iter().copied().collect());
    }
    let segment_count = archive.segment_count();
    check_graph_budget(options, segment_count, 0)?;
    work_budget.charge_many(segment_count)?;
    let mut rows = Vec::with_capacity(segment_count);
    for segment_identifier in archive.segment_identifiers() {
        let references = references_by_source
            .remove(&segment_identifier)
            .map_or_else(Vec::new, sorted_unique_segment_identifiers);
        rows.push(ArchiveGraphRow {
            segment_identifier,
            references: ArchiveGraphReferences::Available(references),
        });
    }
    Ok(ArchiveDebugGraph {
        origin: ArchiveGraphOrigin::Stored,
        rows,
    })
}

pub(crate) fn check_graph_budget(
    options: ArchiveDebugOptions,
    attempted_rows: usize,
    attempted_edges: usize,
) -> ArchiveDebugResult<()> {
    if attempted_rows > options.maximum_graph_rows || attempted_edges > options.maximum_graph_edges
    {
        return Err(graph_budget_error(options, attempted_rows, attempted_edges));
    }
    Ok(())
}

pub(crate) fn graph_budget_error(
    options: ArchiveDebugOptions,
    attempted_rows: usize,
    attempted_edges: usize,
) -> ArchiveDebugError {
    ArchiveDebugError::GraphBudgetExceeded {
        maximum_graph_rows: options.maximum_graph_rows,
        maximum_graph_edges: options.maximum_graph_edges,
        attempted_graph_rows: attempted_rows,
        attempted_graph_edges: attempted_edges,
    }
}

pub(crate) fn sorted_unique_segment_identifiers(
    identifiers: impl IntoIterator<Item = SegmentIdentifier>,
) -> Vec<SegmentIdentifier> {
    let mut unique: HashSet<SegmentIdentifier> = identifiers.into_iter().collect();
    let mut identifiers: Vec<SegmentIdentifier> = unique.drain().collect();
    identifiers.sort_by_key(|identifier| {
        (
            identifier.most_significant_bits,
            identifier.least_significant_bits,
        )
    });
    identifiers
}
