//! Attribution of current-head content paths to one TAR archive.
//!
//! This is the read-only core of Oak's `debug PATH file.tar` diagnostic.
//! It walks the super-root so paths include both live content under `/root`
//! and checkpoint metadata, then attributes node, template, stored property,
//! and long-binary bulk-block records to an active archive. Missing and
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
//! memory is `O(archive segments + graph edges + one node's property slots +
//! returned references)`; block lists and multi-valued binary lists are
//! visited entry by entry, not materialized.
//! Returned path references have both a count and retained-text budget, and
//! exceeding either returns a typed error before the result can grow without
//! bound.
//!
//! The CLI shape is deliberately narrower and safer than oak-run's overloaded
//! command: the argument is one canonical archive file name in the store,
//! never an arbitrary or suffix-matched path. Valid properties use Oak's
//! value summaries and UTF-16 ordering. Exceptionally large non-binary values
//! are summarized instead of materialized, an external binary whose blob
//! store is unavailable reports no size, graph row/target order is
//! deterministic instead of Java `HashMap`/`HashSet` order, and a
//! structurally invalid data segment encountered during graph reconstruction
//! gets an explicit unavailable row so other archive rows remain useful.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::content::list::{read_counted_list, uncounted_list_entries, uncounted_list_entry};
use crate::content::node::NodeState;
use crate::content::property::{PropertyType, read_property_value};
use crate::content::provider::SegmentProvider;
use crate::content::template::{ChildNodeArity, PropertyTemplate};
use crate::content::traversal::DepthFirstTraversal;
use crate::content::value::{
    BLOCK_SIZE, BinaryValue, MEDIUM_VALUE_LIMIT, read_binary_value, read_value_length,
};
use crate::error::Error;
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::tar_archive::graph::SegmentGraph;

/// Oak renders complete non-binary arrays. Cap diagnostic presentation so a
/// corrupt property cannot allocate or print millions of decoded values;
/// attribution still examines every binary block when that is the question.
const MAXIMUM_PRESENTED_ARRAY_VALUES: u64 = 1_024;

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

/// Result-retention limits for [`debug_archive_with_options`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArchiveDebugOptions {
    /// Maximum node, template, and property attribution rows retained.
    pub maximum_path_references: usize,
    /// Maximum UTF-8 bytes cloned into retained paths, names, and values.
    pub maximum_reference_text_bytes: usize,
}

impl Default for ArchiveDebugOptions {
    fn default() -> Self {
        Self {
            maximum_path_references: DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES,
            maximum_reference_text_bytes: DEFAULT_MAXIMUM_ARCHIVE_REFERENCE_TEXT_BYTES,
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
        }
    }
}

impl std::error::Error for ArchiveDebugError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Repository(source) => Some(source),
            Self::ResultBudgetExceeded { .. } => None,
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
            source @ ArchiveDebugError::ResultBudgetExceeded { .. } => Error::InvalidFormat {
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
pub enum ArchiveGraphOrigin {
    /// A structurally valid `.gph` trailer was present and trusted.
    Stored,
    /// The `.gph` trailer was missing or invalid, so data-segment reference
    /// tables were read directly.
    Reconstructed,
}

/// Reference-set availability for one segment in the diagnostic graph.
#[derive(Clone, PartialEq, Eq, Debug)]
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
pub struct ArchiveGraphRow {
    /// Source segment from the requested archive.
    pub segment_identifier: SegmentIdentifier,
    /// Outgoing graph edges, or a local reconstruction failure.
    pub references: ArchiveGraphReferences,
}

/// Oak `TarFiles.getGraph`-style graph for one active archive.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ArchiveDebugGraph {
    /// Whether edges came from the stored trailer or segment bytes.
    pub origin: ArchiveGraphOrigin,
    /// Exactly one row per archive segment, in archive index/scan order.
    pub rows: Vec<ArchiveGraphRow>,
}

/// Oak-style presentation data for one stored property's value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ArchivePropertyDisplay {
    /// The first STRING value (also for a non-empty STRINGS property). The
    /// CLI applies Oak's Java escaping and default 60-character truncation
    /// when rendering it.
    String(String),
    /// An empty STRINGS property. Oak's special STRING rendering prints this
    /// as an unquoted empty value after `name = `.
    EmptyStrings,
    /// The portion following `name = ` in
    /// `AbstractPropertyState.toString`: a scalar, a Java-list-like array,
    /// or binary size/count summary.
    Other(String),
}

/// Deterministic work counters for the attribution scan.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ArchiveDebugWork {
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
    /// bulk blocks, lives in the archive.
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
        /// Number of long-binary bulk block records in this archive. Zero
        /// for non-binary values and binaries whose blocks live elsewhere.
        /// A count keeps the result bounded when one property spans millions
        /// of blocks; the containing archive is already identified by the
        /// report.
        binary_bulk_block_count: u64,
        /// Oak-style value presentation, bounded for hostile exceptionally
        /// large strings and non-binary arrays as documented by this module.
        display: ArchivePropertyDisplay,
    },
}

impl ArchivePathReference {
    /// The line prefix Oak inserts into a per-node `TreeSet`. Values cannot
    /// break a tie because a node cannot contain duplicate property names.
    fn oak_within_node_sort_key(&self) -> &str {
        match self {
            Self::Node { .. } => " ",
            Self::Template { .. } => "[",
            Self::Property { name, .. } => name,
        }
    }

    fn retained_text_bytes(&self) -> usize {
        match self {
            Self::Node { path, .. } | Self::Template { path, .. } => path.len(),
            Self::Property {
                path,
                name,
                display,
                ..
            } => {
                let display_bytes = match display {
                    ArchivePropertyDisplay::String(value)
                    | ArchivePropertyDisplay::Other(value) => value.len(),
                    ArchivePropertyDisplay::EmptyStrings => 0,
                };
                path.len()
                    .saturating_add(name.len())
                    .saturating_add(display_bytes)
            }
        }
    }

    fn oak_compare_within_node(&self, other: &Self) -> std::cmp::Ordering {
        // Java String ordering compares UTF-16 code units, not Unicode
        // scalar values or UTF-8 bytes. Every row for this sort has the same
        // path prefix, so comparing only the suffix avoids allocating a
        // duplicate full-path key for every row.
        self.oak_within_node_sort_key()
            .encode_utf16()
            .cmp(other.oak_within_node_sort_key().encode_utf16())
    }
}

/// Structured result of attributing paths to one requested TAR file.
#[derive(Clone, Debug)]
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

/// Attributes current-head content paths with explicit result-retention
/// limits.
///
/// Traversal work may exceed the number of returned rows because every node,
/// property slot, and long-binary block must be inspected to decide whether
/// it belongs to the requested archive. Only retained result memory is
/// bounded by `options`.
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

    let archive_segments: HashSet<SegmentIdentifier> = archive.segment_identifiers().collect();
    let mut references = Vec::new();
    let mut work = ArchiveDebugWork::default();
    let mut result_budget = ResultBudget::new(options);
    let mut traversal = DepthFirstTraversal::new(repository.head(), "/", None);
    while let Some(visited) = traversal.next_node()? {
        work.visited_nodes += 1;
        let mut node_references = references_for_node(
            repository,
            visited.node,
            &oak_node_path(visited.path),
            &archive_segments,
            &mut work,
            &mut result_budget,
        )?;
        node_references.sort_by(ArchivePathReference::oak_compare_within_node);
        references.extend(node_references);
    }
    work.retained_path_references = result_budget.retained_path_references as u64;
    work.retained_reference_text_bytes = result_budget.retained_reference_text_bytes as u64;

    Ok(ArchiveDebugReport {
        archive_file_name: archive_file_name.to_owned(),
        state: ArchiveDebugState::Active,
        file_size: Some(archive.file_size()),
        references,
        graph: Some(diagnostic_archive_graph(archive)),
        work,
    })
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
}

fn retain_reference(
    references: &mut Vec<ArchivePathReference>,
    reference: ArchivePathReference,
    result_budget: &mut ResultBudget,
) -> ArchiveDebugResult<()> {
    result_budget.retain(&reference)?;
    references.push(reference);
    Ok(())
}

fn diagnostic_archive_graph(archive: &crate::tar_archive::TarArchiveReader) -> ArchiveDebugGraph {
    if let Some(stored_graph) = archive.segment_graph() {
        return totalize_stored_graph(archive, &stored_graph);
    }

    let rows = archive
        .segment_identifiers()
        .map(|segment_identifier| {
            let references = if segment_identifier.is_data_segment() {
                archive.segment_data(segment_identifier).map_or_else(
                    || ArchiveGraphReferences::Unavailable {
                        details: "archive index does not resolve this segment's bytes".to_owned(),
                    },
                    |bytes| match ParsedSegment::parse(segment_identifier, bytes) {
                        Ok(segment) => ArchiveGraphReferences::Available(
                            sorted_unique_segment_identifiers(segment.referenced_segments),
                        ),
                        Err(error) => ArchiveGraphReferences::Unavailable {
                            details: error.to_string(),
                        },
                    },
                )
            } else {
                ArchiveGraphReferences::Available(Vec::new())
            };
            ArchiveGraphRow {
                segment_identifier,
                references,
            }
        })
        .collect();
    ArchiveDebugGraph {
        origin: ArchiveGraphOrigin::Reconstructed,
        rows,
    }
}

fn totalize_stored_graph(
    archive: &crate::tar_archive::TarArchiveReader,
    stored_graph: &SegmentGraph,
) -> ArchiveDebugGraph {
    let mut references_by_source: HashMap<SegmentIdentifier, HashSet<SegmentIdentifier>> =
        HashMap::new();
    for (source, references) in &stored_graph.adjacency {
        references_by_source
            .entry(*source)
            .or_default()
            .extend(references.iter().copied());
    }
    let rows = archive
        .segment_identifiers()
        .map(|segment_identifier| {
            let references = references_by_source
                .remove(&segment_identifier)
                .map_or_else(Vec::new, |references| {
                    sorted_unique_segment_identifiers(references)
                });
            ArchiveGraphRow {
                segment_identifier,
                references: ArchiveGraphReferences::Available(references),
            }
        })
        .collect();
    ArchiveDebugGraph {
        origin: ArchiveGraphOrigin::Stored,
        rows,
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

fn references_for_node(
    repository: &Repository,
    node: NodeState<'_>,
    path: &str,
    archive_segments: &HashSet<SegmentIdentifier>,
    work: &mut ArchiveDebugWork,
    result_budget: &mut ResultBudget,
) -> ArchiveDebugResult<Vec<ArchivePathReference>> {
    let node_identifier = node.record_identifier();
    let node_view = repository.segment(node_identifier.segment)?;
    let template_identifier =
        node_view.read_record_identifier(node_identifier.record_number, 0, 1)?;
    let template = repository.template(template_identifier)?;
    let mut references = Vec::new();

    if archive_segments.contains(&node_identifier.segment) {
        retain_reference(
            &mut references,
            ArchivePathReference::Node {
                path: path.to_owned(),
                record_identifier: node_identifier,
            },
            result_budget,
        )?;
    }
    if archive_segments.contains(&template_identifier.segment) {
        retain_reference(
            &mut references,
            ArchivePathReference::Template {
                path: path.to_owned(),
                record_identifier: template_identifier,
            },
            result_budget,
        )?;
    }

    if template.properties.is_empty() {
        return Ok(references);
    }
    let property_list_slot = if template.child_arity == ChildNodeArity::Zero {
        2
    } else {
        3
    };
    let property_list_identifier =
        node_view.read_record_identifier(node_identifier.record_number, 0, property_list_slot)?;
    let property_identifiers = uncounted_list_entries(
        repository,
        property_list_identifier,
        template.properties.len() as u64,
    )?;

    for (property, property_identifier) in template.properties.iter().zip(property_identifiers) {
        work.inspected_properties += 1;
        let record_is_in_archive = archive_segments.contains(&property_identifier.segment);
        let binary_bulk_block_count = if property.property_type == PropertyType::Binary {
            matching_binary_blocks(
                repository,
                property_identifier,
                property.is_multiple,
                archive_segments,
                work,
            )?
        } else {
            0
        };
        if record_is_in_archive || binary_bulk_block_count > 0 {
            let display = property_display(repository, property, property_identifier)?;
            retain_reference(
                &mut references,
                ArchivePathReference::Property {
                    path: path.to_owned(),
                    name: property.name.clone(),
                    property_type: property.property_type,
                    is_multiple: property.is_multiple,
                    record_identifier: property_identifier,
                    record_is_in_archive,
                    binary_bulk_block_count,
                    display,
                },
                result_budget,
            )?;
        }
    }
    Ok(references)
}

fn property_display(
    repository: &Repository,
    property: &PropertyTemplate,
    property_identifier: RecordIdentifier,
) -> crate::error::Result<ArchivePropertyDisplay> {
    // DebugTars checks the JCR tag rather than scalar-vs-array identity, so
    // both STRING and STRINGS use its special first-value-only rendering.
    if property.property_type == PropertyType::String {
        let value_identifier = if property.is_multiple {
            let counted = read_counted_list(repository, property_identifier)?;
            let Some(body) = counted.body else {
                return Ok(ArchivePropertyDisplay::EmptyStrings);
            };
            uncounted_list_entry(repository, body, u64::from(counted.size), 0)?
        } else {
            property_identifier
        };
        let length = read_value_length(repository, value_identifier)?;
        if length >= MEDIUM_VALUE_LIMIT {
            return Ok(ArchivePropertyDisplay::Other(format!(
                "{{value of {length} bytes omitted by bounded diagnostic}}"
            )));
        }
        let value = read_property_value(repository, value_identifier, PropertyType::String)?;
        return value
            .as_text()
            .map(ArchivePropertyDisplay::String)
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "string property {} at {property_identifier} has no text representation",
                    property.name
                ),
            });
    }

    if property.is_multiple {
        let counted = read_counted_list(repository, property_identifier)?;
        if property.property_type == PropertyType::Binary {
            return Ok(ArchivePropertyDisplay::Other(format!(
                "[{} binaries]",
                counted.size
            )));
        }
        let size = u64::from(counted.size);
        if size > MAXIMUM_PRESENTED_ARRAY_VALUES {
            return Ok(ArchivePropertyDisplay::Other(format!(
                "[{size} values; display omitted by bounded diagnostic]"
            )));
        }
        let Some(body) = counted.body else {
            return Ok(ArchivePropertyDisplay::Other("[]".to_owned()));
        };
        let mut display = String::from("[");
        for value_index in 0..size {
            if value_index > 0 {
                display.push_str(", ");
            }
            let value_identifier = uncounted_list_entry(repository, body, size, value_index)?;
            display.push_str(&bounded_scalar_display(
                repository,
                value_identifier,
                property.property_type,
            )?);
        }
        display.push(']');
        return Ok(ArchivePropertyDisplay::Other(display));
    }

    if property.property_type == PropertyType::Binary {
        return Ok(ArchivePropertyDisplay::Other(
            match read_binary_value(repository, property_identifier)? {
                BinaryValue::Inline { length, .. } => format!("{{{length} bytes}}"),
                BinaryValue::External { .. } => "{external binary; size unavailable}".to_owned(),
            },
        ));
    }

    let length = read_value_length(repository, property_identifier)?;
    if length >= MEDIUM_VALUE_LIMIT {
        return Ok(ArchivePropertyDisplay::Other(format!(
            "{{value of {length} bytes omitted by bounded diagnostic}}"
        )));
    }
    let value = read_property_value(repository, property_identifier, property.property_type)?;
    let text = value.as_text().ok_or_else(|| Error::InvalidFormat {
        details: format!(
            "non-binary property {} at {property_identifier} has no text representation",
            property.name
        ),
    })?;
    Ok(ArchivePropertyDisplay::Other(text))
}

fn bounded_scalar_display(
    repository: &Repository,
    value_identifier: RecordIdentifier,
    property_type: PropertyType,
) -> crate::error::Result<String> {
    let length = read_value_length(repository, value_identifier)?;
    if length >= MEDIUM_VALUE_LIMIT {
        return Ok(format!("{{value of {length} bytes omitted}}"));
    }
    read_property_value(repository, value_identifier, property_type)?
        .as_text()
        .ok_or_else(|| Error::InvalidFormat {
            details: format!(
                "non-binary array value at {value_identifier} has no text representation"
            ),
        })
}

fn matching_binary_blocks(
    repository: &Repository,
    property_identifier: RecordIdentifier,
    is_multiple: bool,
    archive_segments: &HashSet<SegmentIdentifier>,
    work: &mut ArchiveDebugWork,
) -> crate::error::Result<u64> {
    if is_multiple {
        let counted = read_counted_list(repository, property_identifier)?;
        let Some(body) = counted.body else {
            return Ok(0);
        };
        let size = u64::from(counted.size);
        let mut matching_count = 0u64;
        for value_index in 0..size {
            let value_identifier = uncounted_list_entry(repository, body, size, value_index)?;
            matching_count = matching_count
                .checked_add(count_matching_long_binary_blocks(
                    repository,
                    value_identifier,
                    archive_segments,
                    work,
                )?)
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "matching binary block count overflows for property {property_identifier}"
                    ),
                })?;
        }
        Ok(matching_count)
    } else {
        count_matching_long_binary_blocks(repository, property_identifier, archive_segments, work)
    }
}

fn count_matching_long_binary_blocks(
    repository: &Repository,
    value_identifier: RecordIdentifier,
    archive_segments: &HashSet<SegmentIdentifier>,
    work: &mut ArchiveDebugWork,
) -> crate::error::Result<u64> {
    let view = repository.segment(value_identifier.segment)?;
    let head = view.read_u8(value_identifier.record_number, 0)?;
    // `110xxxxx` is the only encoding backed by block records. Small and
    // medium values live in the value record; `111xxxxx` are external blob
    // identifiers with no segment bulk blocks.
    if head & 0xe0 != 0xc0 {
        return Ok(0);
    }
    let length = read_value_length(repository, value_identifier)?;
    let block_count = length.div_ceil(BLOCK_SIZE);
    let list_identifier = view.read_record_identifier(value_identifier.record_number, 8, 0)?;
    let mut matching_count = 0u64;
    for block_index in 0..block_count {
        work.inspected_binary_blocks += 1;
        let block_identifier =
            uncounted_list_entry(repository, list_identifier, block_count, block_index)?;
        if block_identifier.segment.is_bulk_segment()
            && archive_segments.contains(&block_identifier.segment)
        {
            matching_count += 1;
        }
    }
    Ok(matching_count)
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
