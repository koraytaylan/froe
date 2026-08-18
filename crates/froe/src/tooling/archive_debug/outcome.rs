//! What a run reports: the typed outcomes a missing or superseded archive
//! produces, and the rows and references a successful one does.

use super::{
    ArchivePropertyDisplay, Error, PropertyType, RecordIdentifier, fmt, oak_property_type_name,
};

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
    pub(crate) fn retained_text_bytes(&self) -> usize {
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

    pub(crate) fn oak_rendered_utf16_sort_key(&self) -> Vec<u16> {
        // DebugTars inserts the complete rendered line into a Java TreeSet.
        // Names alone are not a sufficient key: adversarial property names
        // can share the node/template punctuation prefixes, and Java orders
        // the resulting UTF-16 code units rather than Rust scalar values.
        self.oak_rendered_line().encode_utf16().collect()
    }

    pub(crate) fn oak_rendered_line_byte_len(&self) -> usize {
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

    pub(crate) fn oak_rendered_line(&self) -> String {
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

#[cfg(test)]
mod tests {
    use super::ArchivePathReference;
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::record::RecordIdentifier;
    use crate::tooling::archive_debug::PropertyType;
    use crate::tooling::archive_debug::display::ArchivePropertyDisplay;

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
}
