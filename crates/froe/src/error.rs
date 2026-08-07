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
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::InputOutput(source) => Some(source),
            Error::InvalidFormat { .. }
            | Error::SegmentNotFound { .. }
            | Error::ExternalBinaryContentUnavailable { .. } => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::InputOutput(source)
    }
}
