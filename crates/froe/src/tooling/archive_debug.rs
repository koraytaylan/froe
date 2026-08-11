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
//! path-copy work. Child counts, concrete map entries, map-record visits, and
//! stored name lengths are checked before the corresponding expansion.
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
//! budget. An external scalar binary whose blob store is unavailable renders
//! `{-1 bytes}` without resolving a long identifier. Graph row/target order is
//! deterministic instead of Java `HashMap`/`HashSet` order, and a
//! structurally invalid data segment encountered during graph reconstruction
//! gets an explicit unavailable row so other archive rows remain useful.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use crate::content::list::{read_counted_list, uncounted_list_entry};
use crate::content::node::NodeState;
use crate::content::property::PropertyType;
use crate::content::provider::SegmentProvider;
use crate::content::template::{ChildNodeArity, PropertyTemplate, read_template_with_limits};
use crate::content::traversal::DepthFirstTraversal;
use crate::content::value::{BLOCK_SIZE, MEDIUM_VALUE_LIMIT};
use crate::error::Error;
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::tar_archive::graph::{BoundedSegmentGraph, SegmentGraph};

/// Default maximum number of path-attribution rows retained in one report.
///
/// Two hundred and fifty thousand rows leave room for a large production
/// tree while keeping the enum/vector portion of one diagnostic result in
/// the tens of MiB instead of allowing a hostile billion-node tree to grow
/// until the process aborts.
pub const DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES: usize = 250_000;

/// Default maximum UTF-8 bytes cloned into path-attribution rows.
///
/// This counts paths, property names, and rendered values. Fixed-size record
/// identifiers and enum discriminants are separately bounded by
/// [`DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES`].
pub const DEFAULT_MAXIMUM_ARCHIVE_REFERENCE_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum number of logical record-graph operations in one archive
/// diagnostic.
///
/// A unit is charged for each traversal step, node/template/property lookup,
/// list element, long-value block identifier, stored name byte, graph-data
/// byte, and graph row/edge totalization operation. Allocation-heavy
/// operations preflight their complete charge before materializing. The limit
/// therefore remains deterministic across repository cache state and bounds
/// compact hostile records that repeatedly point at the same list.
pub const DEFAULT_MAXIMUM_ARCHIVE_WORK_UNITS: u64 = 100_000_000;

/// Default maximum child entries one traversal step may materialize.
pub const DEFAULT_MAXIMUM_ARCHIVE_SCHEDULED_CHILDREN_PER_NODE: u64 = 250_000;

/// Default cumulative stored bytes of names materialized while expanding and
/// interpreting one node.
pub const DEFAULT_MAXIMUM_ARCHIVE_NAME_BYTES_PER_NODE: u64 = 16 * 1024 * 1024;

/// Default maximum number of child visits retained on the traversal stack.
pub const DEFAULT_MAXIMUM_ARCHIVE_PENDING_NODES: u64 = 250_000;

/// Default maximum number of rows retained in an archive graph.
pub const DEFAULT_MAXIMUM_ARCHIVE_GRAPH_ROWS: usize = 250_000;

/// Default maximum number of edges parsed and retained in an archive graph.
pub const DEFAULT_MAXIMUM_ARCHIVE_GRAPH_EDGES: usize = 1_000_000;

/// Resource limits for [`debug_archive_with_options`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct ArchiveDebugOptions {
    /// Maximum node, template, and property attribution rows retained.
    pub maximum_path_references: usize,
    /// Maximum UTF-8 bytes cloned into retained paths, names, and values.
    pub maximum_reference_text_bytes: usize,
    /// Maximum logical record-graph operations performed by the diagnostic.
    pub maximum_work_units: u64,
    /// Maximum child entries materialized while expanding one node.
    pub maximum_scheduled_children_per_node: u64,
    /// Maximum cumulative stored bytes of child and template names
    /// materialized while processing one node.
    pub maximum_name_bytes_per_node: u64,
    /// Maximum child visits retained on the traversal stack at one time.
    pub maximum_pending_nodes: u64,
    /// Maximum rows parsed or retained in the archive graph.
    pub maximum_graph_rows: usize,
    /// Maximum edges parsed or retained in the archive graph.
    pub maximum_graph_edges: usize,
}

impl Default for ArchiveDebugOptions {
    fn default() -> Self {
        Self {
            maximum_path_references: DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES,
            maximum_reference_text_bytes: DEFAULT_MAXIMUM_ARCHIVE_REFERENCE_TEXT_BYTES,
            maximum_work_units: DEFAULT_MAXIMUM_ARCHIVE_WORK_UNITS,
            maximum_scheduled_children_per_node:
                DEFAULT_MAXIMUM_ARCHIVE_SCHEDULED_CHILDREN_PER_NODE,
            maximum_name_bytes_per_node: DEFAULT_MAXIMUM_ARCHIVE_NAME_BYTES_PER_NODE,
            maximum_pending_nodes: DEFAULT_MAXIMUM_ARCHIVE_PENDING_NODES,
            maximum_graph_rows: DEFAULT_MAXIMUM_ARCHIVE_GRAPH_ROWS,
            maximum_graph_edges: DEFAULT_MAXIMUM_ARCHIVE_GRAPH_EDGES,
        }
    }
}

/// Failure from archive path attribution.
#[derive(Debug)]
#[non_exhaustive]
pub enum ArchiveDebugError {
    /// Reading or interpreting repository content failed.
    Repository(Error),
    /// Retaining another result row would exceed a configured count or text
    /// budget.
    ResultBudgetExceeded {
        /// Configured maximum result rows.
        maximum_path_references: usize,
        /// Configured maximum cloned UTF-8 bytes.
        maximum_reference_text_bytes: usize,
        /// Row count that the rejected insertion would have reached.
        attempted_path_references: usize,
        /// Cloned text bytes that the rejected insertion would have reached.
        attempted_reference_text_bytes: usize,
    },
    /// Performing the next logical record-graph operation would exceed the
    /// configured total-work budget.
    WorkBudgetExceeded {
        /// Configured maximum logical work units.
        maximum_work_units: u64,
        /// Work total the rejected operation would have reached.
        attempted_work_units: u64,
    },
    /// One node declares more children than the configured per-node
    /// materialization cap.
    NodeChildBudgetExceeded {
        /// Configured maximum child entries materialized for one node.
        maximum_scheduled_children_per_node: u64,
        /// Child count read before allocating the entry vector.
        attempted_scheduled_children: u64,
    },
    /// One node's child and template names exceed the configured
    /// materialization cap.
    NodeNameBudgetExceeded {
        /// Configured maximum cumulative stored name bytes for one node.
        maximum_name_bytes_per_node: u64,
        /// Cumulative stored bytes including the rejected name.
        attempted_name_bytes: u64,
    },
    /// Expanding one node would retain more pending child visits than the
    /// configured traversal-stack cap.
    PendingNodeBudgetExceeded {
        /// Configured maximum pending child visits.
        maximum_pending_nodes: u64,
        /// Pending visits after the rejected expansion.
        attempted_pending_nodes: u64,
    },
    /// Parsing or totalizing the archive graph would exceed its configured
    /// row or edge cap.
    GraphBudgetExceeded {
        /// Configured maximum graph rows.
        maximum_graph_rows: usize,
        /// Configured maximum graph edges.
        maximum_graph_edges: usize,
        /// Rows after the rejected operation.
        attempted_graph_rows: usize,
        /// Edges after the rejected operation.
        attempted_graph_edges: usize,
    },
}

impl fmt::Display for ArchiveDebugError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Repository(source) => source.fmt(formatter),
            Self::ResultBudgetExceeded {
                maximum_path_references,
                maximum_reference_text_bytes,
                attempted_path_references,
                attempted_reference_text_bytes,
            } => write!(
                formatter,
                "archive attribution result would retain {attempted_path_references} rows and \
                 {attempted_reference_text_bytes} text bytes, exceeding limits of \
                {maximum_path_references} rows and {maximum_reference_text_bytes} text bytes"
            ),
            Self::WorkBudgetExceeded {
                maximum_work_units,
                attempted_work_units,
            } => write!(
                formatter,
                "archive attribution would perform {attempted_work_units} logical work units, \
                 exceeding the limit of {maximum_work_units}"
            ),
            Self::NodeChildBudgetExceeded {
                maximum_scheduled_children_per_node,
                attempted_scheduled_children,
            } => write!(
                formatter,
                "archive attribution node declares {attempted_scheduled_children} children, \
                 exceeding the per-node limit of {maximum_scheduled_children_per_node}"
            ),
            Self::NodeNameBudgetExceeded {
                maximum_name_bytes_per_node,
                attempted_name_bytes,
            } => write!(
                formatter,
                "archive attribution node would materialize {attempted_name_bytes} stored name \
                 bytes, exceeding the per-node limit of {maximum_name_bytes_per_node}"
            ),
            Self::PendingNodeBudgetExceeded {
                maximum_pending_nodes,
                attempted_pending_nodes,
            } => write!(
                formatter,
                "archive attribution would retain {attempted_pending_nodes} pending node visits, \
                 exceeding the limit of {maximum_pending_nodes}"
            ),
            Self::GraphBudgetExceeded {
                maximum_graph_rows,
                maximum_graph_edges,
                attempted_graph_rows,
                attempted_graph_edges,
            } => write!(
                formatter,
                "archive graph would process {attempted_graph_rows} rows and \
                 {attempted_graph_edges} edges, exceeding limits of {maximum_graph_rows} rows \
                 and {maximum_graph_edges} edges"
            ),
        }
    }
}

impl std::error::Error for ArchiveDebugError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Repository(source) => Some(source),
            Self::ResultBudgetExceeded { .. }
            | Self::WorkBudgetExceeded { .. }
            | Self::NodeChildBudgetExceeded { .. }
            | Self::NodeNameBudgetExceeded { .. }
            | Self::PendingNodeBudgetExceeded { .. }
            | Self::GraphBudgetExceeded { .. } => None,
        }
    }
}

impl From<Error> for ArchiveDebugError {
    fn from(source: Error) -> Self {
        Self::Repository(source)
    }
}

impl From<ArchiveDebugError> for Error {
    fn from(source: ArchiveDebugError) -> Self {
        match source {
            ArchiveDebugError::Repository(source) => source,
            source @ (ArchiveDebugError::ResultBudgetExceeded { .. }
            | ArchiveDebugError::WorkBudgetExceeded { .. }
            | ArchiveDebugError::NodeChildBudgetExceeded { .. }
            | ArchiveDebugError::NodeNameBudgetExceeded { .. }
            | ArchiveDebugError::PendingNodeBudgetExceeded { .. }
            | ArchiveDebugError::GraphBudgetExceeded { .. }) => Error::InvalidFormat {
                details: source.to_string(),
            },
        }
    }
}

/// Result type returned by archive attribution.
pub type ArchiveDebugResult<Value> = std::result::Result<Value, ArchiveDebugError>;

/// Whether the requested file participates in the repository's active
/// read-only archive set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ArchiveDebugState {
    /// No file with the requested archive name exists in the store.
    Missing,
    /// The file exists but was superseded by another generation or could
    /// not be opened as the active reader for its archive number.
    Inactive,
    /// The file is one of the archives the repository currently reads.
    Active,
}

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

/// Oak-style presentation data for one stored property's value.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ArchivePropertyDisplay {
    /// A bounded summary of the first STRING value (also for a non-empty
    /// STRINGS property).
    String {
        /// At most the first 60 Java UTF-16 code units. This can end with an
        /// unpaired high surrogate because Java truncates with `substring`.
        preview_utf16: Vec<u16>,
        /// Full value length in Java UTF-16 code units.
        utf16_length: u64,
    },
    /// An empty STRINGS property. Oak's special STRING rendering prints this
    /// as an unquoted empty value after `name = `.
    EmptyStrings,
    /// The portion following `name = ` in
    /// `AbstractPropertyState.toString`: a scalar, a Java-list-like array,
    /// or binary size/count summary.
    Other(String),
}

impl ArchivePropertyDisplay {
    /// Renders the value portion of Oak's `SegmentPropertyState` diagnostic
    /// line, before terminal sanitization by a presentation layer.
    #[must_use]
    pub fn oak_rendered_value(&self) -> String {
        match self {
            Self::String {
                preview_utf16,
                utf16_length,
            } => java_string_display(preview_utf16, *utf16_length),
            Self::EmptyStrings => String::new(),
            Self::Other(value) => value.clone(),
        }
    }

    fn oak_rendered_value_bytes(&self) -> usize {
        match self {
            Self::String {
                preview_utf16,
                utf16_length,
            } => java_string_display(preview_utf16, *utf16_length).len(),
            Self::EmptyStrings => 0,
            Self::Other(value) => value.len(),
        }
    }
}

/// Deterministic work counters for the attribution scan.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct ArchiveDebugWork {
    /// Logical work units consumed under
    /// [`ArchiveDebugOptions::maximum_work_units`].
    pub consumed_work_units: u64,
    /// Content paths visited from the super-root.
    pub visited_nodes: u64,
    /// Stored property slots inspected.
    pub inspected_properties: u64,
    /// Long-binary block identifiers inspected. This counts every block,
    /// whether or not it belongs to the requested archive.
    pub inspected_binary_blocks: u64,
    /// Path-attribution rows retained in the report.
    pub retained_path_references: u64,
    /// UTF-8 bytes cloned into retained paths, names, and rendered values.
    pub retained_reference_text_bytes: u64,
}

/// A current-head record whose content is attributable to an archive.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ArchivePathReference {
    /// The node record itself lives in the archive.
    Node {
        /// Oak-style path from the super-root, ending in `/`.
        path: String,
        /// The matching node record.
        record_identifier: RecordIdentifier,
    },
    /// The node's template record lives in the archive.
    Template {
        /// Oak-style path from the super-root, ending in `/`.
        path: String,
        /// The matching template record.
        record_identifier: RecordIdentifier,
    },
    /// A stored property's value/list record, or one or more long-binary
    /// block segments, lives in the archive.
    Property {
        /// Oak-style path from the super-root, ending in `/`.
        path: String,
        /// Property name.
        name: String,
        /// JCR property type from the template.
        property_type: PropertyType,
        /// Whether the property holds an array of values.
        is_multiple: bool,
        /// The single value record or multi-value counted-list record Oak
        /// uses as the `SegmentPropertyState` identity.
        record_identifier: RecordIdentifier,
        /// Whether that property identity record itself is in the archive.
        record_is_in_archive: bool,
        /// Oak-style value presentation, bounded for hostile exceptionally
        /// large strings and non-binary arrays as documented by this module.
        display: ArchivePropertyDisplay,
    },
}

impl ArchivePathReference {
    fn retained_text_bytes(&self) -> usize {
        match self {
            Self::Node { path, .. } | Self::Template { path, .. } => path.len(),
            Self::Property {
                path,
                name,
                display,
                ..
            } => {
                let display_bytes = display.oak_rendered_value_bytes();
                path.len()
                    .saturating_add(name.len())
                    .saturating_add(display_bytes)
            }
        }
    }

    fn oak_rendered_utf16_sort_key(&self) -> Vec<u16> {
        // DebugTars inserts the complete rendered line into a Java TreeSet.
        // Names alone are not a sufficient key: adversarial property names
        // can share the node/template punctuation prefixes, and Java orders
        // the resulting UTF-16 code units rather than Rust scalar values.
        self.oak_rendered_line().encode_utf16().collect()
    }

    fn oak_rendered_line_byte_len(&self) -> usize {
        // Preflight the UTF-16 sort-key allocation by its UTF-8 source size.
        // A rendered line cannot contain more UTF-16 units than UTF-8 bytes.
        match self {
            Self::Node {
                path,
                record_identifier,
            } => path
                .len()
                .saturating_add(" [SegmentNodeState@".len())
                .saturating_add(record_identifier.to_string().len())
                .saturating_add(1),
            Self::Template {
                path,
                record_identifier,
            } => path
                .len()
                .saturating_add("[Template@".len())
                .saturating_add(record_identifier.to_string().len())
                .saturating_add(1),
            Self::Property {
                path,
                name,
                property_type,
                is_multiple,
                record_identifier,
                display,
                ..
            } => path
                .len()
                .saturating_add(name.len())
                .saturating_add(" = ".len())
                .saturating_add(display.oak_rendered_value_bytes())
                .saturating_add(" [SegmentPropertyState<".len())
                .saturating_add(oak_property_type_name(*property_type, *is_multiple).len())
                .saturating_add(1)
                .saturating_add(record_identifier.to_string().len())
                .saturating_add(1),
        }
    }

    fn oak_rendered_line(&self) -> String {
        match self {
            Self::Node {
                path,
                record_identifier,
            } => format!("{path} [SegmentNodeState@{record_identifier}]"),
            Self::Template {
                path,
                record_identifier,
            } => format!("{path}[Template@{record_identifier}]"),
            Self::Property {
                path,
                name,
                property_type,
                is_multiple,
                record_identifier,
                display,
                ..
            } => {
                let display = display.oak_rendered_value();
                format!(
                    "{path}{name} = {display} [SegmentPropertyState<{}>@{record_identifier}]",
                    oak_property_type_name(*property_type, *is_multiple)
                )
            }
        }
    }
}

fn oak_property_type_name(property_type: PropertyType, is_multiple: bool) -> String {
    let singular = property_type.jcr_name().to_ascii_uppercase();
    if !is_multiple {
        singular
    } else if property_type == PropertyType::Binary {
        "BINARIES".to_owned()
    } else {
        format!("{singular}S")
    }
}

fn java_string_display(preview_utf16: &[u16], utf16_length: u64) -> String {
    use std::fmt::Write as _;

    let mut display = String::from("\"");
    for &unit in preview_utf16 {
        match unit {
            0x08 => display.push_str("\\b"),
            0x09 => display.push_str("\\t"),
            0x0a => display.push_str("\\n"),
            0x0c => display.push_str("\\f"),
            0x0d => display.push_str("\\r"),
            0x22 => display.push_str("\\\""),
            0x5c => display.push_str("\\\\"),
            0x20..=0x7e => display.push(char::from_u32(u32::from(unit)).expect("ASCII unit")),
            _ => write!(display, "\\u{unit:04X}").expect("writing to a String cannot fail"),
        }
    }
    if utf16_length > preview_utf16.len() as u64 {
        write!(display, "... ({utf16_length} chars)").expect("writing to a String cannot fail");
    }
    display.push('"');
    display
}

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

/// Attributes current-head content paths with explicit work, traversal,
/// graph, and result-retention limits.
///
/// Traversal work may exceed the number of returned rows because every node,
/// property slot, and long-binary block must be inspected to decide whether
/// it belongs to the requested archive. Both that logical work and retained
/// result memory are bounded by `options`.
#[allow(
    clippy::too_many_lines,
    reason = "the linear scan keeps budget reservation, traversal, graph work, and final counters in one transaction"
)]
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
            .map_err(|error| match error {
                Error::TraversalSchedulingBudgetExceeded {
                    attempted_scheduled_children,
                    ..
                } if attempted_scheduled_children > options.maximum_scheduled_children_per_node => {
                    ArchiveDebugError::NodeChildBudgetExceeded {
                        maximum_scheduled_children_per_node: options
                            .maximum_scheduled_children_per_node,
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
                } => work_budget.exceeded_by(
                    scheduled_children.saturating_add(attempted_stored_child_name_bytes),
                ),
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
            })?;
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

struct WorkBudget {
    maximum: u64,
    consumed: u64,
}

impl WorkBudget {
    const fn new(maximum: u64) -> Self {
        Self {
            maximum,
            consumed: 0,
        }
    }

    fn charge_one(&mut self) -> ArchiveDebugResult<()> {
        self.charge_amount(1)
    }

    const fn remaining(&self) -> u64 {
        self.maximum.saturating_sub(self.consumed)
    }

    fn exceeded_by(&self, units: u64) -> ArchiveDebugError {
        ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: self.maximum,
            attempted_work_units: self.consumed.saturating_add(units),
        }
    }

    fn charge_amount(&mut self, units: u64) -> ArchiveDebugResult<()> {
        let attempted = self.consumed.saturating_add(units);
        if attempted > self.maximum {
            return Err(self.exceeded_by(units));
        }
        self.consumed = attempted;
        Ok(())
    }

    fn charge_many(&mut self, units: usize) -> ArchiveDebugResult<()> {
        self.charge_amount(u64::try_from(units).unwrap_or(u64::MAX))
    }
}

struct ResultBudget {
    options: ArchiveDebugOptions,
    retained_path_references: usize,
    retained_reference_text_bytes: usize,
}

impl ResultBudget {
    fn new(options: ArchiveDebugOptions) -> Self {
        Self {
            options,
            retained_path_references: 0,
            retained_reference_text_bytes: 0,
        }
    }

    fn retain(&mut self, reference: &ArchivePathReference) -> ArchiveDebugResult<()> {
        let attempted_path_references = self.retained_path_references.saturating_add(1);
        let attempted_reference_text_bytes = self
            .retained_reference_text_bytes
            .saturating_add(reference.retained_text_bytes());
        if attempted_path_references > self.options.maximum_path_references
            || attempted_reference_text_bytes > self.options.maximum_reference_text_bytes
        {
            return Err(ArchiveDebugError::ResultBudgetExceeded {
                maximum_path_references: self.options.maximum_path_references,
                maximum_reference_text_bytes: self.options.maximum_reference_text_bytes,
                attempted_path_references,
                attempted_reference_text_bytes,
            });
        }
        self.retained_path_references = attempted_path_references;
        self.retained_reference_text_bytes = attempted_reference_text_bytes;
        Ok(())
    }

    fn candidate_display_budget(
        &self,
        base_text_bytes: usize,
    ) -> ArchiveDebugResult<DisplayBudget> {
        let budget = DisplayBudget {
            // Uniqueness is known only after the complete Oak line is
            // rendered. Candidate construction is bounded by the configured
            // text cap; aggregate row/text reservation happens after dedup.
            maximum_path_references: self.options.maximum_path_references,
            maximum_reference_text_bytes: self.options.maximum_reference_text_bytes,
            attempted_path_references: 1,
            base_reference_text_bytes: base_text_bytes,
        };
        budget.check_display_bytes(0)?;
        Ok(budget)
    }
}

#[derive(Clone, Copy)]
struct DisplayBudget {
    maximum_path_references: usize,
    maximum_reference_text_bytes: usize,
    attempted_path_references: usize,
    base_reference_text_bytes: usize,
}

impl DisplayBudget {
    fn check_display_bytes(self, display_bytes: usize) -> ArchiveDebugResult<()> {
        let attempted_reference_text_bytes =
            self.base_reference_text_bytes.saturating_add(display_bytes);
        if self.attempted_path_references > self.maximum_path_references
            || attempted_reference_text_bytes > self.maximum_reference_text_bytes
        {
            return Err(ArchiveDebugError::ResultBudgetExceeded {
                maximum_path_references: self.maximum_path_references,
                maximum_reference_text_bytes: self.maximum_reference_text_bytes,
                attempted_path_references: self.attempted_path_references,
                attempted_reference_text_bytes,
            });
        }
        Ok(())
    }

    fn builder(self) -> BoundedDisplay {
        BoundedDisplay {
            text: String::new(),
            budget: self,
        }
    }
}

struct BoundedDisplay {
    text: String,
    budget: DisplayBudget,
}

impl BoundedDisplay {
    fn push_str(&mut self, text: &str) -> ArchiveDebugResult<()> {
        let attempted = self.text.len().saturating_add(text.len());
        self.budget.check_display_bytes(attempted)?;
        self.text.push_str(text);
        Ok(())
    }

    fn push_char(&mut self, character: char) -> ArchiveDebugResult<()> {
        let attempted = self.text.len().saturating_add(character.len_utf8());
        self.budget.check_display_bytes(attempted)?;
        self.text.push(character);
        Ok(())
    }

    fn into_string(self) -> String {
        self.text
    }
}

fn collect_reference(
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

fn diagnostic_archive_graph(
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

fn totalize_stored_graph(
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

fn check_graph_budget(
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

fn graph_budget_error(
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

fn sorted_unique_segment_identifiers(
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

fn oak_node_path(path: &str) -> String {
    if path == "/" {
        "/".to_owned()
    } else {
        format!("{path}/")
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the per-node attribution transaction explicitly carries both resource ledgers and archive membership"
)]
fn references_for_node(
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
    let maximum_template_name_bytes = maximum_name_bytes_per_node
        .saturating_sub(scheduled_child_name_bytes)
        .min(work_budget.remaining());
    let (template, template_name_bytes) = read_template_with_limits(
        repository,
        template_identifier,
        work_budget.remaining(),
        maximum_template_name_bytes,
    )
    .map_err(|error| match error {
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
    })?;
    work_budget.charge_amount(template_name_bytes)?;
    // Oak uses a TreeSet of complete rendered lines per visited node. Using a
    // map here deduplicates as each candidate arrives, so duplicate hostile
    // template entries do not accumulate in a second pre-sort vector. Unique
    // entries reserve the aggregate result budget before insertion.
    let mut references = BTreeMap::new();

    if archive.contains_segment(node_identifier.segment) {
        collect_reference(
            &mut references,
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
            &mut references,
            ArchivePathReference::Template {
                path: path.to_owned(),
                record_identifier: template_identifier,
            },
            work_budget,
            result_budget,
        )?;
    }

    if template.properties.is_empty() {
        return Ok(references.into_values().collect());
    }
    let property_list_slot = if template.child_arity == ChildNodeArity::Zero {
        2
    } else {
        3
    };
    work_budget.charge_one()?;
    let property_list_identifier =
        node_view.read_record_identifier(node_identifier.record_number, 0, property_list_slot)?;

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

fn property_display(
    repository: &Repository,
    property: &PropertyTemplate,
    property_identifier: RecordIdentifier,
    work_budget: &mut WorkBudget,
    display_budget: DisplayBudget,
) -> ArchiveDebugResult<ArchivePropertyDisplay> {
    // DebugTars checks the JCR tag rather than scalar-vs-array identity, so
    // both STRING and STRINGS use its special first-value-only rendering.
    if property.property_type == PropertyType::String {
        let value_identifier = if property.is_multiple {
            work_budget.charge_one()?;
            let counted = read_counted_list(repository, property_identifier)?;
            let Some(body) = counted.body else {
                return Ok(ArchivePropertyDisplay::EmptyStrings);
            };
            work_budget.charge_one()?;
            uncounted_list_entry(repository, body, u64::from(counted.size), 0)?
        } else {
            property_identifier
        };
        let (preview_utf16, utf16_length) =
            streamed_string_summary(repository, value_identifier, work_budget)?;
        display_budget
            .check_display_bytes(java_string_display(&preview_utf16, utf16_length).len())?;
        return Ok(ArchivePropertyDisplay::String {
            preview_utf16,
            utf16_length,
        });
    }

    if property.is_multiple {
        work_budget.charge_one()?;
        let counted = read_counted_list(repository, property_identifier)?;
        if property.property_type == PropertyType::Binary {
            let text = format!("[{} binaries]", counted.size);
            display_budget.check_display_bytes(text.len())?;
            return Ok(ArchivePropertyDisplay::Other(text));
        }
        let size = u64::from(counted.size);
        let mut display = display_budget.builder();
        display.push_str("[")?;
        let Some(body) = counted.body else {
            display.push_str("]")?;
            return Ok(ArchivePropertyDisplay::Other(display.into_string()));
        };
        for value_index in 0..size {
            if value_index > 0 {
                display.push_str(", ")?;
            }
            work_budget.charge_one()?;
            let value_identifier = uncounted_list_entry(repository, body, size, value_index)?;
            append_scalar_display(
                repository,
                value_identifier,
                property.property_type,
                work_budget,
                &mut display,
            )?;
        }
        display.push_str("]")?;
        return Ok(ArchivePropertyDisplay::Other(display.into_string()));
    }

    if property.property_type == PropertyType::Binary {
        let text = binary_scalar_display(repository, property_identifier, work_budget)?;
        display_budget.check_display_bytes(text.len())?;
        return Ok(ArchivePropertyDisplay::Other(text));
    }

    let mut display = display_budget.builder();
    append_scalar_display(
        repository,
        property_identifier,
        property.property_type,
        work_budget,
        &mut display,
    )?;
    Ok(ArchivePropertyDisplay::Other(display.into_string()))
}

fn append_scalar_display(
    repository: &Repository,
    value_identifier: RecordIdentifier,
    property_type: PropertyType,
    work_budget: &mut WorkBudget,
    display: &mut BoundedDisplay,
) -> ArchiveDebugResult<()> {
    match property_type {
        PropertyType::Binary | PropertyType::String => Err(Error::InvalidFormat {
            details: format!(
                "property type {} cannot use ordinary scalar rendering at {value_identifier}",
                property_type.jcr_name()
            ),
        }
        .into()),
        PropertyType::Boolean => {
            let mut position = 0usize;
            let mut is_true = true;
            decode_value_utf8(repository, value_identifier, work_budget, |character| {
                const TRUE: [char; 4] = ['t', 'r', 'u', 'e'];
                is_true &= position < TRUE.len() && character.eq_ignore_ascii_case(&TRUE[position]);
                position = position.saturating_add(1);
                Ok(())
            })?;
            if position != 4 {
                is_true = false;
            }
            display.push_str(if is_true { "true" } else { "false" })
        }
        PropertyType::Long => {
            let start = display.text.len();
            decode_value_utf8(repository, value_identifier, work_budget, |character| {
                display.push_char(character)
            })?;
            let stored_length = display.text.len() - start;
            let parsed = display.text[start..].parse::<i64>().map_err(|_| {
                Error::InvalidFormat {
                    details: format!(
                        "stored long value at {value_identifier} has {stored_length} UTF-8 bytes \
                         and cannot be decoded"
                    ),
                }
            })?;
            display.text.truncate(start);
            display.push_str(&parsed.to_string())
        }
        PropertyType::Double => {
            let start = display.text.len();
            decode_value_utf8(repository, value_identifier, work_budget, |character| {
                display.push_char(character)
            })?;
            let stored_length = display.text.len() - start;
            display.text[start..]
                .parse::<f64>()
                .map_err(|_| Error::InvalidFormat {
                    details: format!(
                        "stored double value at {value_identifier} has {stored_length} UTF-8 \
                         bytes and cannot be decoded"
                    ),
                })?;
            // Oak stores Double.toString's canonical spelling. Keeping that
            // validated spelling preserves values such as Double.MIN_VALUE
            // (`4.9E-324`) that Rust's formatter spells differently.
            Ok(())
        }
        PropertyType::Date
        | PropertyType::Name
        | PropertyType::Path
        | PropertyType::Reference
        | PropertyType::WeakReference
        | PropertyType::Uri
        | PropertyType::Decimal => {
            decode_value_utf8(repository, value_identifier, work_budget, |character| {
                display.push_char(character)
            })
        }
    }
}

fn streamed_string_summary(
    provider: &dyn SegmentProvider,
    value_identifier: RecordIdentifier,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<(Vec<u16>, u64)> {
    let mut preview_utf16 = Vec::with_capacity(60);
    let mut utf16_length = 0u64;
    decode_value_utf8(provider, value_identifier, work_budget, |character| {
        let mut encoded = [0u16; 2];
        let units = character.encode_utf16(&mut encoded);
        utf16_length = utf16_length
            .checked_add(units.len() as u64)
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("UTF-16 length overflows for string value {value_identifier}"),
            })?;
        let remaining = 60usize.saturating_sub(preview_utf16.len());
        preview_utf16.extend_from_slice(&units[..units.len().min(remaining)]);
        Ok(())
    })?;
    Ok((preview_utf16, utf16_length))
}

fn decode_value_utf8(
    provider: &dyn SegmentProvider,
    value_identifier: RecordIdentifier,
    work_budget: &mut WorkBudget,
    mut consume: impl FnMut(char) -> ArchiveDebugResult<()>,
) -> ArchiveDebugResult<()> {
    use crate::content::value::read_binary_stream;

    // String records use a 62-bit long-length mask and Java's signed-int
    // length limit, while the generic binary stream uses a 61-bit mask.
    // Preflight the `110xxxxx` string form and reject binary markers before
    // opening that generic stream.
    work_budget.charge_one()?;
    let view = provider.segment(value_identifier.segment)?;
    let head = view.read_u8(value_identifier.record_number, 0)?;
    let is_long = head & 0xe0 == 0xc0;
    if is_long {
        let stored = view.read_u64(value_identifier.record_number, 0)?;
        let string_length = (stored & 0x3fff_ffff_ffff_ffff) + MEDIUM_VALUE_LIMIT;
        if string_length >= i32::MAX as u64 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "string of {string_length} bytes in record {value_identifier} is too long"
                ),
            }
            .into());
        }
    } else if head & 0x80 != 0 && head & 0x40 != 0 {
        return Err(Error::InvalidFormat {
            details: format!(
                "record {value_identifier} starts with binary marker {head:#04x} and is not a \
                 string"
            ),
        }
        .into());
    }

    work_budget.charge_one()?;
    let mut stream = read_binary_stream(provider, value_identifier)?;
    let mut buffer = [0u8; BLOCK_SIZE as usize];
    let mut pending = Vec::with_capacity(buffer.len() + 3);
    while stream.position() < stream.len() {
        let remaining = stream.len() - stream.position();
        let requested_bytes = if is_long {
            remaining
                .min(BLOCK_SIZE - stream.position() % BLOCK_SIZE)
                .min(buffer.len() as u64)
        } else {
            remaining.min(buffer.len() as u64)
        };
        let lookup_work = if is_long { 2 } else { 1 };
        work_budget.charge_amount(requested_bytes.saturating_add(lookup_work))?;
        let read_length = stream.read_chunk(&mut buffer)?;
        if read_length == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..read_length]);
        consume_utf8_prefix(&mut pending, false, &mut consume)?;
    }
    consume_utf8_prefix(&mut pending, true, &mut consume)
}

fn consume_utf8_prefix(
    pending: &mut Vec<u8>,
    end_of_value: bool,
    consume: &mut impl FnMut(char) -> ArchiveDebugResult<()>,
) -> ArchiveDebugResult<()> {
    let mut consumed = 0usize;
    while consumed < pending.len() {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(text) => {
                for character in text.chars() {
                    consume(character)?;
                }
                consumed = pending.len();
            }
            Err(error) => {
                let valid_end = consumed + error.valid_up_to();
                let valid = std::str::from_utf8(&pending[consumed..valid_end])
                    .expect("from_utf8 validated this prefix");
                for character in valid.chars() {
                    consume(character)?;
                }
                consumed = valid_end;
                if let Some(error_length) = error.error_len() {
                    consume('\u{fffd}')?;
                    consumed = consumed.saturating_add(error_length);
                } else if end_of_value {
                    consume('\u{fffd}')?;
                    consumed = pending.len();
                } else {
                    break;
                }
            }
        }
    }
    pending.drain(..consumed);
    Ok(())
}

fn binary_scalar_display(
    repository: &Repository,
    value_identifier: RecordIdentifier,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<String> {
    work_budget.charge_one()?;
    let view = repository.segment(value_identifier.segment)?;
    let head = view.read_u8(value_identifier.record_number, 0)?;
    let length = if head & 0x80 == 0 {
        Some(u64::from(head))
    } else if head & 0x40 == 0 {
        Some(u64::from(view.read_u16(value_identifier.record_number, 0)? & 0x3fff) + 128)
    } else if head & 0x20 == 0 {
        Some(
            (view.read_u64(value_identifier.record_number, 0)? & 0x1fff_ffff_ffff_ffff)
                + MEDIUM_VALUE_LIMIT,
        )
    } else if head & 0x10 == 0 || head & 0x08 == 0 {
        None
    } else {
        return Err(Error::InvalidFormat {
            details: format!(
                "unexpected value record marker {head:#04x} in record {value_identifier}"
            ),
        }
        .into());
    };
    Ok(length.map_or_else(
        || "{-1 bytes}".to_owned(),
        |length| format!("{{{length} bytes}}"),
    ))
}

fn has_matching_binary_block_segment(
    repository: &Repository,
    property_identifier: RecordIdentifier,
    is_multiple: bool,
    archive: &crate::tar_archive::TarArchiveReader,
    work: &mut ArchiveDebugWork,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<bool> {
    let mut matches_archive = false;
    if is_multiple {
        work_budget.charge_one()?;
        let counted = read_counted_list(repository, property_identifier)?;
        let Some(body) = counted.body else {
            return Ok(false);
        };
        let size = u64::from(counted.size);
        for value_index in 0..size {
            work_budget.charge_one()?;
            let value_identifier = uncounted_list_entry(repository, body, size, value_index)?;
            matches_archive |= long_binary_has_matching_block_segment(
                repository,
                value_identifier,
                property_identifier.segment,
                archive,
                work,
                work_budget,
            )?;
        }
    } else {
        matches_archive = long_binary_has_matching_block_segment(
            repository,
            property_identifier,
            property_identifier.segment,
            archive,
            work,
            work_budget,
        )?;
    }
    Ok(matches_archive)
}

fn long_binary_has_matching_block_segment(
    repository: &Repository,
    value_identifier: RecordIdentifier,
    property_segment: SegmentIdentifier,
    archive: &crate::tar_archive::TarArchiveReader,
    work: &mut ArchiveDebugWork,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<bool> {
    work_budget.charge_one()?;
    let view = repository.segment(value_identifier.segment)?;
    let head = view.read_u8(value_identifier.record_number, 0)?;
    // `110xxxxx` is the only encoding backed by block records. Small and
    // medium values live in the value record; `111xxxxx` are external blob
    // identifiers with no segment block records.
    if head & 0xe0 != 0xc0 {
        return Ok(false);
    }
    let length = (view.read_u64(value_identifier.record_number, 0)? & 0x1fff_ffff_ffff_ffff)
        + MEDIUM_VALUE_LIMIT;
    let block_count = length.div_ceil(BLOCK_SIZE);
    let list_identifier = view.read_record_identifier(value_identifier.record_number, 8, 0)?;
    let mut matches_archive = false;
    for block_index in 0..block_count {
        work.inspected_binary_blocks += 1;
        work_budget.charge_one()?;
        let block_identifier =
            uncounted_list_entry(repository, list_identifier, block_count, block_index)?;
        if block_identifier.segment != property_segment
            && archive.contains_segment(block_identifier.segment)
        {
            matches_archive = true;
        }
    }
    Ok(matches_archive)
}

#[cfg(test)]
mod tests {
    use super::{
        ArchiveDebugError, ArchivePathReference, ArchivePropertyDisplay, BLOCK_SIZE,
        MEDIUM_VALUE_LIMIT, PropertyType, WorkBudget, oak_node_path, streamed_string_summary,
    };
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;

    fn local_record_identifier(record_number: u32) -> Vec<u8> {
        let mut bytes = vec![0, 0];
        bytes.extend_from_slice(&record_number.to_be_bytes());
        bytes
    }

    fn medium_value(content: &[u8]) -> Vec<u8> {
        let mut value = (0x8000u16 | (content.len() as u16 - 128))
            .to_be_bytes()
            .to_vec();
        value.extend_from_slice(content);
        value
    }

    fn provider_with_long_value(content: &[u8]) -> (MemorySegmentProvider, RecordIdentifier) {
        let segment = data_segment_identifier(41);
        let mut records = Vec::new();
        let mut list = Vec::new();
        for (block_index, block) in content.chunks(BLOCK_SIZE as usize).enumerate() {
            let record_number = 1 + block_index as u32;
            records.push((record_number, 5, block.to_vec()));
            list.extend_from_slice(&local_record_identifier(record_number));
        }
        records.push((20, 2, list));
        let mut value = ((content.len() as u64 - MEDIUM_VALUE_LIMIT) | (0x3 << 62))
            .to_be_bytes()
            .to_vec();
        value.extend_from_slice(&local_record_identifier(20));
        records.push((21, 4, value));
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));
        (provider, RecordIdentifier::new(segment, 21))
    }

    #[test]
    fn oak_paths_end_in_one_separator() {
        assert_eq!(oak_node_path("/"), "/");
        assert_eq!(oak_node_path("/root"), "/root/");
    }

    #[test]
    fn per_node_order_uses_the_complete_rendered_java_string() {
        let record_identifier =
            RecordIdentifier::new(SegmentIdentifier::new(1, 0xa000_0000_0000_0001), 7);
        let property = |name: &str| ArchivePathReference::Property {
            path: "/".to_owned(),
            name: name.to_owned(),
            property_type: PropertyType::Long,
            is_multiple: false,
            record_identifier,
            record_is_in_archive: true,
            display: ArchivePropertyDisplay::Other("1".to_owned()),
        };
        let mut references = [
            property("\u{e000}"),
            ArchivePathReference::Template {
                path: "/".to_owned(),
                record_identifier,
            },
            property("["),
            property("\u{10000}"),
            ArchivePathReference::Node {
                path: "/".to_owned(),
                record_identifier,
            },
            property(" "),
        ];
        references.sort_by_cached_key(ArchivePathReference::oak_rendered_utf16_sort_key);

        let labels: Vec<&str> = references
            .iter()
            .map(|reference| match reference {
                ArchivePathReference::Node { .. } => "node",
                ArchivePathReference::Template { .. } => "template",
                ArchivePathReference::Property { name, .. } => name,
            })
            .collect();
        assert_eq!(
            labels,
            [" ", "node", "[", "template", "\u{10000}", "\u{e000}"]
        );
    }

    #[test]
    fn string_summary_streams_medium_and_long_boundaries_in_java_utf16_units() {
        let medium_segment = data_segment_identifier(40);
        let medium_text = vec![b'x'; 16_511];
        let mut medium_provider = MemorySegmentProvider::default();
        medium_provider.insert(
            medium_segment,
            synthetic_data_segment(&[], &[(1, 4, medium_value(&medium_text))]),
        );
        let mut work_budget = WorkBudget::new(u64::MAX);
        let (preview, length) = streamed_string_summary(
            &medium_provider,
            RecordIdentifier::new(medium_segment, 1),
            &mut work_budget,
        )
        .expect("16,511-byte medium string");
        assert_eq!(preview, vec![u16::from(b'x'); 60]);
        assert_eq!(length, 16_511);

        // The emoji straddles Java's 60-char preview boundary (only its
        // high surrogate is retained), while `é` straddles a 4 KiB block
        // boundary in UTF-8. The complete byte length is the first long
        // value boundary.
        let mut long_text = "a".repeat(59).into_bytes();
        long_text.extend_from_slice("\u{1f600}".as_bytes());
        long_text.extend(std::iter::repeat_n(b'x', 4_095 - long_text.len()));
        long_text.extend_from_slice("\u{e9}".as_bytes());
        long_text.extend(std::iter::repeat_n(b'x', 16_512 - long_text.len()));
        let (long_provider, long_identifier) = provider_with_long_value(&long_text);
        let mut work_budget = WorkBudget::new(u64::MAX);
        let (preview, length) =
            streamed_string_summary(&long_provider, long_identifier, &mut work_budget)
                .expect("16,512-byte long string");
        assert_eq!(&preview[..59], &vec![u16::from(b'a'); 59]);
        assert_eq!(
            preview[59], 0xd83d,
            "Java substring can split a surrogate pair"
        );
        assert_eq!(preview.len(), 60);
        assert_eq!(length, 16_509);
    }

    #[test]
    fn string_summary_preflights_payload_bytes_against_the_work_budget() {
        let content = vec![b'x'; 16_512];
        let (provider, identifier) = provider_with_long_value(&content);
        let mut work_budget = WorkBudget::new(100);

        assert!(matches!(
            streamed_string_summary(&provider, identifier, &mut work_budget),
            Err(ArchiveDebugError::WorkBudgetExceeded {
                maximum_work_units: 100,
                attempted_work_units: 4_100,
            })
        ));
        assert_eq!(
            work_budget.consumed, 2,
            "the first 4 KiB payload block is refused before it is read"
        );
    }

    #[test]
    fn string_summary_enforces_the_java_long_string_limit_before_streaming() {
        let segment = data_segment_identifier(42);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[(1, 4, 0xdfff_ffff_ffff_ffffu64.to_be_bytes().to_vec())],
            ),
        );
        let mut work_budget = WorkBudget::new(u64::MAX);

        match streamed_string_summary(
            &provider,
            RecordIdentifier::new(segment, 1),
            &mut work_budget,
        ) {
            Err(ArchiveDebugError::Repository(crate::Error::InvalidFormat { details })) => {
                assert!(details.contains("2305843009213710463 bytes"), "{details}");
                assert!(details.contains("is too long"), "{details}");
            }
            other => panic!("expected the Java string-length limit, got {other:?}"),
        }
        assert_eq!(work_budget.consumed, 1);
    }

    #[test]
    fn string_summary_rejects_external_binary_markers_canonically() {
        let segment = data_segment_identifier(43);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(1, 4, vec![0xe0, 0, 0, 0, 0, 0, 0, 0])]),
        );
        let mut work_budget = WorkBudget::new(u64::MAX);

        match streamed_string_summary(
            &provider,
            RecordIdentifier::new(segment, 1),
            &mut work_budget,
        ) {
            Err(ArchiveDebugError::Repository(crate::Error::InvalidFormat { details })) => {
                assert!(details.contains("binary marker 0xe0"), "{details}");
                assert!(details.contains("is not a string"), "{details}");
            }
            other => panic!("expected a string marker error, got {other:?}"),
        }
        assert_eq!(work_budget.consumed, 1);
    }
}
