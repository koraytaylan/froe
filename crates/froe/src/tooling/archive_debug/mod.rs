//! Attribution of current-head content paths to one TAR archive.
//!
//! This is the read-only core of Oak's `debug PATH file.tar` diagnostic.
//! It walks the super-root so paths include both live content under `/root`
//! and checkpoint metadata, then attributes node, template, stored property,
//! and long-binary block records to an active archive. Missing and
//! superseded archives are typed outcomes rather than inferred from
//! diagnostic text. Like Oak's `TarFiles.getGraph`, the graph result has
//! one row for every segment in the requested archive: a valid stored graph
//! is trusted and totalized, while a missing or corrupt stored graph is
//! reconstructed from each data segment's reference table.
//!
//! One requested archive costs `O(nodes + stored properties + binary blocks +
//! archive segments + graph edges)` time: like Oak, the tool walks the head
//! once, inspects every block pointer of a long binary, and reads the archive
//! graph (or reconstructs it from data-segment reference tables). Auxiliary
//! memory is bounded by explicit graph row/edge, pending traversal, per-node
//! child/name, and returned-reference limits; block lists and multi-valued
//! binary lists are visited entry by entry, not materialized. A total
//! logical-work budget is charged before record/list/block/graph scans and
//! path-copy work. Child counts, concrete map entries, map-record visits,
//! template-name/list lookups, and stored name lengths are checked before the
//! corresponding expansion.
//! Returned path references have both a count and retained-text budget; a
//! candidate is individually preflighted and inserted into a per-node
//! TreeSet-equivalent, so duplicate rendered lines do not accumulate. A
//! rejected candidate can already hold work/name-bounded text but is never
//! retained in the report.
//!
//! The CLI shape is deliberately narrower and safer than oak-run's overloaded
//! command: the argument is one canonical archive file name in the store,
//! never an arbitrary or suffix-matched path. Valid properties use Oak's
//! value rendering and UTF-16 ordering. STRING values stream into Oak's
//! 60-UTF-16-unit preview; other values render fully or fail the retained-text
//! budget. A scalar binary whose size cannot be read renders `{-1 bytes}`;
//! this covers an unavailable blob store and a corrupt value marker without
//! resolving a long external identifier. Graph row/target order is
//! deterministic instead of Java `HashMap`/`HashSet` order, and a
//! structurally invalid data segment encountered during graph reconstruction
//! gets an explicit unavailable row so other archive rows remain useful.

use crate::content::template::Template;
use crate::segment::view::SegmentView;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use crate::content::list::{read_counted_list, uncounted_list_entry};
use crate::content::node::NodeState;
use crate::content::property::PropertyType;
use crate::content::provider::SegmentProvider;
use crate::content::template::{
    ChildNodeArity, PropertyTemplate, read_template_with_limits, template_name_lookup_work,
};
use crate::content::traversal::DepthFirstTraversal;
use crate::content::value::{BLOCK_SIZE, MEDIUM_VALUE_LIMIT};
use crate::error::Error;
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::tar_archive::graph::{BoundedSegmentGraph, SegmentGraph};

mod display;
mod graph;
mod options;
mod outcome;
#[cfg(test)]
mod test_support;

pub use display::*;
pub use graph::*;
pub use options::*;
pub use outcome::*;

/// Structured result of attributing paths to one requested TAR file.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ArchiveDebugReport {
    /// The validated archive file name supplied by the caller.
    pub archive_file_name: String,
    /// File state relative to the repository's active archive set.
    pub state: ArchiveDebugState,
    /// File size when the file exists.
    pub file_size: Option<u64>,
    /// Current-head node/template/property references, in depth-first path
    /// order and deterministic order within each node.
    pub references: Vec<ArchivePathReference>,
    /// Totalized archive graph. This is `Some` for every active archive;
    /// missing and inactive requests have no graph.
    pub graph: Option<ArchiveDebugGraph>,
    /// Deterministic amount of production traversal work performed.
    pub work: ArchiveDebugWork,
}

/// Attributes current-head content paths to one TAR archive.
///
/// `archive_file_name` is deliberately a canonical segment archive name,
/// not a path. Restricting it to the repository directory avoids Oak's
/// legacy suffix matching ambiguity and keeps the diagnostic's scope clear.
/// The function opens no file for write and never takes `repo.lock`.
pub fn debug_archive(
    repository: &Repository,
    archive_file_name: &str,
) -> ArchiveDebugResult<ArchiveDebugReport> {
    debug_archive_with_options(
        repository,
        archive_file_name,
        ArchiveDebugOptions::default(),
    )
}

/// Restates a traversal refusal in the vocabulary of `froe debug`.
///
/// A per-node bound the caller set is reported as that bound; anything else
/// the traversal refused was charged against the shared work budget, so it
/// is reported as work.
pub(crate) fn translate_scheduling_error(
    error: Error,
    options: ArchiveDebugOptions,
    work_budget: &WorkBudget,
) -> ArchiveDebugError {
    match error {
        Error::TraversalSchedulingBudgetExceeded {
            attempted_scheduled_children,
            ..
        } if attempted_scheduled_children > options.maximum_scheduled_children_per_node => {
            ArchiveDebugError::NodeChildBudgetExceeded {
                maximum_scheduled_children_per_node: options.maximum_scheduled_children_per_node,
                attempted_scheduled_children,
            }
        }
        Error::TraversalSchedulingBudgetExceeded {
            attempted_scheduled_children,
            ..
        } => work_budget.exceeded_by(attempted_scheduled_children),
        Error::TraversalChildNameBudgetExceeded {
            attempted_stored_child_name_bytes,
            ..
        } if attempted_stored_child_name_bytes > options.maximum_name_bytes_per_node => {
            ArchiveDebugError::NodeNameBudgetExceeded {
                maximum_name_bytes_per_node: options.maximum_name_bytes_per_node,
                attempted_name_bytes: attempted_stored_child_name_bytes,
            }
        }
        Error::TraversalChildNameBudgetExceeded {
            attempted_stored_child_name_bytes,
            scheduled_children,
            ..
        } => work_budget
            .exceeded_by(scheduled_children.saturating_add(attempted_stored_child_name_bytes)),
        Error::TraversalSchedulingWorkBudgetExceeded {
            attempted_scheduling_work,
            ..
        } => work_budget.exceeded_by(attempted_scheduling_work),
        Error::TraversalPendingBudgetExceeded {
            attempted_pending_nodes,
            ..
        } => ArchiveDebugError::PendingNodeBudgetExceeded {
            maximum_pending_nodes: options.maximum_pending_nodes,
            attempted_pending_nodes,
        },
        other => ArchiveDebugError::Repository(other),
    }
}

/// Attributes current-head content paths with explicit work, traversal,
/// graph, and result-retention limits.
///
/// Traversal work may exceed the number of returned rows because every node,
/// property slot, and long-binary block must be inspected to decide whether
/// it belongs to the requested archive. Both that logical work and retained
/// result memory are bounded by `options`.
pub fn debug_archive_with_options(
    repository: &Repository,
    archive_file_name: &str,
    options: ArchiveDebugOptions,
) -> ArchiveDebugResult<ArchiveDebugReport> {
    if ArchiveFileName::parse(archive_file_name).is_none() {
        return Err(Error::InvalidFormat {
            details: format!(
                "debug archive name {archive_file_name:?} is not a canonical data*.tar file name"
            ),
        }
        .into());
    }

    let requested_path = repository.directory().join(archive_file_name);
    let (requested_path_exists, discovered_file_size) = match std::fs::metadata(&requested_path) {
        Ok(metadata) => (true, metadata.is_file().then_some(metadata.len())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, None),
        Err(error) => return Err(Error::from(error).into()),
    };
    let Some(archive) = repository
        .archives()
        .iter()
        .find(|archive| archive.file_name() == archive_file_name)
    else {
        return Ok(ArchiveDebugReport {
            archive_file_name: archive_file_name.to_owned(),
            state: if requested_path_exists {
                ArchiveDebugState::Inactive
            } else {
                ArchiveDebugState::Missing
            },
            file_size: discovered_file_size,
            references: Vec::new(),
            graph: None,
            work: ArchiveDebugWork::default(),
        });
    };

    let mut references = Vec::new();
    let mut work = ArchiveDebugWork::default();
    let mut work_budget = WorkBudget::new(options.maximum_work_units);
    let mut result_budget = ResultBudget::new(options);
    let mut traversal = DepthFirstTraversal::new(repository.head(), "/", None);
    loop {
        work_budget.charge_one()?;
        let remaining_work = work_budget.remaining();
        let traversal_step = traversal
            .next_node_with_scheduling_limits(
                options.maximum_scheduled_children_per_node,
                options.maximum_name_bytes_per_node,
                remaining_work,
                options.maximum_pending_nodes,
            )
            .map_err(|error| translate_scheduling_error(error, options, &work_budget))?;
        let Some(visited) = traversal_step else {
            break;
        };
        work_budget.charge_amount(
            visited
                .scheduled_children
                .saturating_add(visited.scheduled_child_name_bytes)
                .saturating_add(visited.scheduled_child_map_records),
        )?;
        work.visited_nodes += 1;
        let oak_path_bytes = visited
            .visited
            .path
            .len()
            .saturating_add(usize::from(visited.visited.path != "/"));
        work_budget.charge_many(oak_path_bytes)?;
        let path = oak_node_path(visited.visited.path);
        let node_references = references_for_node(
            repository,
            visited.visited.node,
            &path,
            archive,
            &mut work,
            &mut work_budget,
            &mut result_budget,
            options.maximum_name_bytes_per_node,
            visited.scheduled_child_name_bytes,
        )?;
        references.extend(node_references);
    }
    let graph = diagnostic_archive_graph(archive, &mut work_budget, options)?;
    work.consumed_work_units = work_budget.consumed;
    work.retained_path_references = result_budget.retained_path_references as u64;
    work.retained_reference_text_bytes = result_budget.retained_reference_text_bytes as u64;

    Ok(ArchiveDebugReport {
        archive_file_name: archive_file_name.to_owned(),
        state: ArchiveDebugState::Active,
        file_size: Some(archive.file_size()),
        references,
        graph: Some(graph),
        work,
    })
}

pub(crate) fn oak_node_path(path: &str) -> String {
    if path == "/" {
        "/".to_owned()
    } else {
        format!("{path}/")
    }
}

/// Where a node's property-value list sits depends on its child arity: a
/// childless node has no child-map slot, so the list moves up one.
pub(crate) fn read_property_list_identifier(
    node_view: &SegmentView<'_>,
    node_identifier: RecordIdentifier,
    template: &Template,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<RecordIdentifier> {
    let property_list_slot = if template.child_arity == ChildNodeArity::Zero {
        2
    } else {
        3
    };
    work_budget.charge_one()?;
    Ok(node_view.read_record_identifier(node_identifier.record_number, 0, property_list_slot)?)
}

/// The two records a node always has: the node itself and its template.
#[derive(Clone, Copy)]
pub(crate) struct OwnRecords<'records> {
    pub(crate) path: &'records str,
    pub(crate) node_identifier: RecordIdentifier,
    pub(crate) template_identifier: RecordIdentifier,
}

/// Records whichever of the node's own two records live in this archive.
pub(crate) fn collect_own_records(
    references: &mut BTreeMap<Vec<u16>, ArchivePathReference>,
    records: OwnRecords<'_>,
    archive: &crate::tar_archive::TarArchiveReader,
    work_budget: &mut WorkBudget,
    result_budget: &mut ResultBudget,
) -> ArchiveDebugResult<()> {
    let OwnRecords {
        path,
        node_identifier,
        template_identifier,
    } = records;
    if archive.contains_segment(node_identifier.segment) {
        collect_reference(
            references,
            ArchivePathReference::Node {
                path: path.to_owned(),
                record_identifier: node_identifier,
            },
            work_budget,
            result_budget,
        )?;
    }
    if archive.contains_segment(template_identifier.segment) {
        collect_reference(
            references,
            ArchivePathReference::Template {
                path: path.to_owned(),
                record_identifier: template_identifier,
            },
            work_budget,
            result_budget,
        )?;
    }
    Ok(())
}

/// Restates a template-read refusal in the vocabulary of `froe debug`.
///
/// A node's own name budget is reported against that budget, counting the
/// child names already scheduled for it; everything else was charged
/// against the shared work budget.
pub(crate) fn translate_template_error(
    error: Error,
    maximum_name_bytes_per_node: u64,
    scheduled_child_name_bytes: u64,
    work_budget: &WorkBudget,
) -> ArchiveDebugError {
    match error {
        Error::StringMaterializationBudgetExceeded {
            attempted_stored_bytes,
            ..
        } if scheduled_child_name_bytes.saturating_add(attempted_stored_bytes)
            > maximum_name_bytes_per_node =>
        {
            ArchiveDebugError::NodeNameBudgetExceeded {
                maximum_name_bytes_per_node,
                attempted_name_bytes: scheduled_child_name_bytes
                    .saturating_add(attempted_stored_bytes),
            }
        }
        Error::StringMaterializationBudgetExceeded {
            attempted_stored_bytes,
            ..
        } => work_budget.exceeded_by(attempted_stored_bytes),
        Error::TemplatePropertyBudgetExceeded {
            attempted_properties,
            ..
        } => work_budget.exceeded_by(attempted_properties),
        other => ArchiveDebugError::Repository(other),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the per-node attribution transaction explicitly carries both resource ledgers and archive membership"
)]
pub(crate) fn references_for_node(
    repository: &Repository,
    node: NodeState<'_>,
    path: &str,
    archive: &crate::tar_archive::TarArchiveReader,
    work: &mut ArchiveDebugWork,
    work_budget: &mut WorkBudget,
    result_budget: &mut ResultBudget,
    maximum_name_bytes_per_node: u64,
    scheduled_child_name_bytes: u64,
) -> ArchiveDebugResult<Vec<ArchivePathReference>> {
    let node_identifier = node.record_identifier();
    work_budget.charge_one()?;
    let node_view = repository.segment(node_identifier.segment)?;
    work_budget.charge_one()?;
    let template_identifier =
        node_view.read_record_identifier(node_identifier.record_number, 0, 1)?;
    work_budget.charge_one()?;
    let template_view = repository.segment(template_identifier.segment)?;
    let template_head = template_view.read_u32(template_identifier.record_number, 0)?;
    // Reserve every name-record resolution and property-name list entry
    // before parsing. Stored name bytes are separately preflighted by the
    // bounded parser and charged below.
    work_budget.charge_amount(template_name_lookup_work(template_head))?;
    let maximum_template_name_bytes = maximum_name_bytes_per_node
        .saturating_sub(scheduled_child_name_bytes)
        .min(work_budget.remaining());
    let (template, template_name_bytes) = read_template_with_limits(
        repository,
        template_identifier,
        work_budget.remaining(),
        maximum_template_name_bytes,
    )
    .map_err(|error| {
        translate_template_error(
            error,
            maximum_name_bytes_per_node,
            scheduled_child_name_bytes,
            work_budget,
        )
    })?;
    work_budget.charge_amount(template_name_bytes)?;
    // Oak uses a TreeSet of complete rendered lines per visited node. Using a
    // map here deduplicates as each candidate arrives, so duplicate hostile
    // template entries do not accumulate in a second pre-sort vector. Unique
    // entries reserve the aggregate result budget before insertion.
    let mut references = BTreeMap::new();

    collect_own_records(
        &mut references,
        OwnRecords {
            path,
            node_identifier,
            template_identifier,
        },
        archive,
        work_budget,
        result_budget,
    )?;

    if template.properties.is_empty() {
        return Ok(references.into_values().collect());
    }
    let property_list_identifier =
        read_property_list_identifier(&node_view, node_identifier, &template, work_budget)?;

    let property_count = template.properties.len() as u64;
    for (property_index, property) in template.properties.iter().enumerate() {
        work.inspected_properties += 1;
        work_budget.charge_one()?;
        let property_identifier = uncounted_list_entry(
            repository,
            property_list_identifier,
            property_count,
            property_index as u64,
        )?;
        let record_is_in_archive = archive.contains_segment(property_identifier.segment);
        let binary_block_segment_match = if property.property_type == PropertyType::Binary {
            has_matching_binary_block_segment(
                repository,
                property_identifier,
                property.is_multiple,
                archive,
                work,
                work_budget,
            )?
        } else {
            false
        };
        if record_is_in_archive || binary_block_segment_match {
            let display_budget = result_budget
                .candidate_display_budget(path.len().saturating_add(property.name.len()))?;
            let display = property_display(
                repository,
                property,
                property_identifier,
                work_budget,
                display_budget,
            )?;
            collect_reference(
                &mut references,
                ArchivePathReference::Property {
                    path: path.to_owned(),
                    name: property.name.clone(),
                    property_type: property.property_type,
                    is_multiple: property.is_multiple,
                    record_identifier: property_identifier,
                    record_is_in_archive,
                    display,
                },
                work_budget,
                result_budget,
            )?;
        }
    }
    Ok(references.into_values().collect())
}

pub(crate) fn collect_reference(
    references: &mut BTreeMap<Vec<u16>, ArchivePathReference>,
    reference: ArchivePathReference,
    work_budget: &mut WorkBudget,
    result_budget: &mut ResultBudget,
) -> ArchiveDebugResult<()> {
    // A candidate can be larger than the remaining aggregate allowance when
    // it duplicates a retained line, but it may never exceed the configured
    // single-candidate text bound before its rendered key is allocated.
    result_budget.candidate_display_budget(reference.retained_text_bytes())?;
    work_budget.charge_many(reference.oak_rendered_line_byte_len())?;
    let key = reference.oak_rendered_utf16_sort_key();
    if let std::collections::btree_map::Entry::Vacant(entry) = references.entry(key) {
        result_budget.retain(&reference)?;
        entry.insert(reference);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::oak_node_path;

    #[test]
    fn oak_paths_end_in_one_separator() {
        assert_eq!(oak_node_path("/"), "/");
        assert_eq!(oak_node_path("/root"), "/root/");
    }
}
