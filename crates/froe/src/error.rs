//! Error and result types shared across the crate.

use std::fmt;

/// Convenient result alias used throughout the crate.
pub type Result<Value> = std::result::Result<Value, Error>;

/// The error type for every fallible operation in this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An underlying input/output operation failed.
    InputOutput(std::io::Error),
    /// A file or byte sequence did not match the segment-tar storage format.
    InvalidFormat {
        /// Human-readable description of what was expected and what was found.
        details: String,
    },
    /// A record referenced a segment that no archive of the repository
    /// contains. This mirrors Oak's `SegmentNotFoundException` and can be
    /// caused by garbage collection removing a segment while old record
    /// identifiers to it survive.
    SegmentNotFound {
        /// The identifier of the missing segment.
        segment_identifier: crate::segment::identifier::SegmentIdentifier,
    },
    /// The content of a binary stored in an external blob store was
    /// requested; the segment store only holds the binary's identifier.
    ExternalBinaryContentUnavailable {
        /// The identifier of the binary in the external blob store.
        blob_identifier: String,
    },
    /// The content of an external binary was requested, but its identifier
    /// is itself stored in another record. Bounded callers deliberately do
    /// not follow that record merely to construct an error message.
    ExternalBinaryContentUnavailableByRecord {
        /// The external binary value record that was requested.
        value_identifier: crate::segment::record::RecordIdentifier,
        /// The string record holding the external blob identifier.
        blob_identifier_record: crate::segment::record::RecordIdentifier,
    },
    /// Materializing another stored string would exceed a caller-provided
    /// cumulative byte limit.
    StringMaterializationBudgetExceeded {
        /// Maximum stored string bytes the caller permits.
        maximum_stored_bytes: u64,
        /// Cumulative stored bytes including the rejected string.
        attempted_stored_bytes: u64,
        /// String value that was rejected before its content was read.
        value_identifier: crate::segment::record::RecordIdentifier,
    },
    /// Parsing a template would materialize more property slots than a
    /// caller-provided limit.
    TemplatePropertyBudgetExceeded {
        /// Maximum property slots the caller permits.
        maximum_properties: u64,
        /// Property slots declared by the template.
        attempted_properties: u64,
    },
    /// Bounded map enumeration encountered more concrete entries than the
    /// caller permitted.
    MapEntryBudgetExceeded {
        /// Maximum concrete map entries permitted.
        maximum_entries: u64,
        /// Entry count including the rejected entry.
        attempted_entries: u64,
    },
    /// Bounded map enumeration would exceed its combined map-record and
    /// stored-name-byte work limit.
    MapTraversalWorkBudgetExceeded {
        /// Maximum combined map enumeration work.
        maximum_work_units: u64,
        /// Work including the rejected map record or stored name bytes.
        attempted_work_units: u64,
    },
    /// A content traversal refused to materialize one node's child list
    /// because its declared size exceeds the caller-provided scheduling
    /// budget.
    TraversalSchedulingBudgetExceeded {
        /// Maximum children the caller allowed this traversal step to
        /// schedule.
        maximum_scheduled_children: u64,
        /// Children the node declared before any child-list allocation.
        attempted_scheduled_children: u64,
    },
    /// A traversal refused to materialize a node's child names because their
    /// cumulative stored bytes exceed the caller-provided scheduling limit.
    TraversalChildNameBudgetExceeded {
        /// Maximum cumulative stored child-name bytes permitted.
        maximum_stored_child_name_bytes: u64,
        /// Cumulative stored bytes including the rejected name.
        attempted_stored_child_name_bytes: u64,
        /// Children whose scheduling work accompanies those name bytes.
        scheduled_children: u64,
    },
    /// A traversal's per-node scheduling expansion would exceed the caller's
    /// combined work allowance.
    TraversalSchedulingWorkBudgetExceeded {
        /// Maximum combined scheduling work permitted.
        maximum_scheduling_work: u64,
        /// Combined work including the rejected operation.
        attempted_scheduling_work: u64,
    },
    /// Scheduling another node would exceed the caller's total pending-node
    /// limit.
    TraversalPendingBudgetExceeded {
        /// Maximum pending node visits permitted.
        maximum_pending_nodes: u64,
        /// Pending visits after the rejected expansion.
        attempted_pending_nodes: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InputOutput(source) => write!(formatter, "input/output error: {source}"),
            Error::InvalidFormat { details } => {
                write!(formatter, "invalid segment-tar data: {details}")
            }
            Error::SegmentNotFound { segment_identifier } => {
                write!(
                    formatter,
                    "segment {segment_identifier} not found in any archive"
                )
            }
            Error::ExternalBinaryContentUnavailable { blob_identifier } => {
                write!(
                    formatter,
                    "binary {blob_identifier} is stored in an external blob store; \
                     only its identifier is available"
                )
            }
            Error::ExternalBinaryContentUnavailableByRecord {
                value_identifier,
                blob_identifier_record,
            } => write!(
                formatter,
                "binary value {value_identifier} is stored in an external blob store; its \
                identifier is held in record {blob_identifier_record} and was not read"
            ),
            Error::StringMaterializationBudgetExceeded {
                maximum_stored_bytes,
                attempted_stored_bytes,
                value_identifier,
            } => write!(
                formatter,
                "materializing string {value_identifier} would retain {attempted_stored_bytes} \
                 stored bytes, exceeding the limit of {maximum_stored_bytes}"
            ),
            Error::TemplatePropertyBudgetExceeded {
                maximum_properties,
                attempted_properties,
            } => write!(
                formatter,
                "template declares {attempted_properties} properties, exceeding the parsing \
                 limit of {maximum_properties}"
            ),
            Error::MapEntryBudgetExceeded {
                maximum_entries,
                attempted_entries,
            } => write!(
                formatter,
                "map enumeration would return {attempted_entries} entries, exceeding the limit \
                 of {maximum_entries}"
            ),
            Error::MapTraversalWorkBudgetExceeded {
                maximum_work_units,
                attempted_work_units,
            } => write!(
                formatter,
                "map enumeration would consume {attempted_work_units} work units, exceeding the \
                 limit of {maximum_work_units}"
            ),
            Error::TraversalSchedulingBudgetExceeded {
                maximum_scheduled_children,
                attempted_scheduled_children,
            } => write!(
                formatter,
                "content traversal would schedule {attempted_scheduled_children} children in one \
                 step, exceeding its budget of {maximum_scheduled_children}"
            ),
            Error::TraversalChildNameBudgetExceeded {
                maximum_stored_child_name_bytes,
                attempted_stored_child_name_bytes,
                ..
            } => write!(
                formatter,
                "content traversal would materialize {attempted_stored_child_name_bytes} stored \
                 child-name bytes in one step, exceeding its budget of \
                 {maximum_stored_child_name_bytes}"
            ),
            Error::TraversalSchedulingWorkBudgetExceeded {
                maximum_scheduling_work,
                attempted_scheduling_work,
            } => write!(
                formatter,
                "content traversal expansion would consume {attempted_scheduling_work} work \
                 units, exceeding its budget of {maximum_scheduling_work}"
            ),
            Error::TraversalPendingBudgetExceeded {
                maximum_pending_nodes,
                attempted_pending_nodes,
            } => write!(
                formatter,
                "content traversal would retain {attempted_pending_nodes} pending node visits, \
                 exceeding its budget of {maximum_pending_nodes}"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::InputOutput(source) => Some(source),
            Error::InvalidFormat { .. }
            | Error::SegmentNotFound { .. }
            | Error::ExternalBinaryContentUnavailable { .. }
            | Error::ExternalBinaryContentUnavailableByRecord { .. }
            | Error::StringMaterializationBudgetExceeded { .. }
            | Error::TemplatePropertyBudgetExceeded { .. }
            | Error::MapEntryBudgetExceeded { .. }
            | Error::MapTraversalWorkBudgetExceeded { .. }
            | Error::TraversalSchedulingBudgetExceeded { .. }
            | Error::TraversalChildNameBudgetExceeded { .. }
            | Error::TraversalSchedulingWorkBudgetExceeded { .. }
            | Error::TraversalPendingBudgetExceeded { .. } => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::InputOutput(source)
    }
}
