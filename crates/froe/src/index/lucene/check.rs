//! The Lucene consistency check froe can run offline: Oak's **level 1**.
//!
//! `docs/analysis/index-lucene-storage.md` §5 records both of Oak's levels.
//! Level 1 (`IndexConsistencyChecker.Level.BLOBS_ONLY`) asks one question of
//! a definition — can every blob it references be read to its declared
//! length? — and nothing about the Lucene 4.7.2 bytes inside those blobs.
//! Level 2 runs Lucene's own `CheckIndex`, needs the file format, and is
//! plan 0008's; the interop suite gets that verdict from the judge in the
//! meantime.
//!
//! Two details of Oak's implementation are easy to get wrong and are what
//! the check is worth:
//!
//! * **The type is read converting**, not strictly:
//!   `TYPE_LUCENE.equals(type.getValue(Type.STRING))`. A definition whose
//!   `type` is stored as a `NAME` therefore still checks, where the model's
//!   strict read would call it typeless.
//! * **The walk covers hidden children and hidden properties.** Oak reaches
//!   the subtree through `ImmutableTree`, which overrides
//!   `isHidden` to return `false` precisely so nothing is filtered. Every
//!   byte a Lucene index holds lives under `:data` or `:suggest-data` or a
//!   mount-decorated spelling of one, so a walk that skipped hidden children
//!   would check exactly nothing and report a clean index.

use std::io::Read;

use crate::content::node::NodeState;
use crate::content::property::{PropertyType, PropertyValue};
use crate::content::value::{BinaryValue, read_binary_stream};
use crate::content::{PropertyValues, SegmentProvider};
use crate::index::{IndexResult, converting_strings};

/// How many bytes a blob is streamed through at a time. A Lucene compound
/// file runs to megabytes and is never held whole.
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

/// What the level-1 pass found.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LuceneBlobReport {
    /// Whether the definition's `type` read as `lucene` at all. Oak sets
    /// `typeMismatch` and calls the index unclean in this case rather than
    /// skipping it, so a definition pointed at the wrong checker is a
    /// finding rather than a silence.
    pub type_mismatch: bool,
    /// Blobs that could not be read at all, by the path of the property
    /// holding them.
    pub missing_blobs: Vec<BlobFault>,
    /// Blobs whose streamed length disagreed with the length they declare.
    pub invalid_blobs: Vec<BlobFault>,
    /// How many binary values were streamed to their end successfully.
    pub blobs_checked: u64,
    /// How many bytes were read doing it.
    pub bytes_read: u64,
}

impl LuceneBlobReport {
    /// Whether the index passed the level-1 check.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        !self.type_mismatch && self.missing_blobs.is_empty() && self.invalid_blobs.is_empty()
    }
}

/// One blob that could not be read, or could not be read to its length.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct BlobFault {
    /// The path of the node holding the property, relative to the
    /// definition.
    pub node_path: String,
    /// The property name.
    pub property_name: String,
    /// The index of the value inside a multi-valued property; `0` for a
    /// single one.
    pub value_index: usize,
    /// What the blob declares its length to be.
    pub declared_length: u64,
    /// What reading it actually yielded, or `None` when it could not be read
    /// at all.
    pub streamed_length: Option<u64>,
    /// The reason it could not be read, when it could not.
    pub reason: Option<String>,
}

/// Runs Oak's level-1 check over one definition subtree.
///
/// `definition_node` is the definition itself; the walk descends into every
/// child, hidden ones included, exactly as Oak's does.
pub fn check_blobs(
    provider: &dyn SegmentProvider,
    definition_node: &NodeState<'_>,
) -> IndexResult<LuceneBlobReport> {
    let mut report = LuceneBlobReport::default();

    // Oak's own guard, and its converting read: a definition whose `type` is
    // stored as a NAME still passes it.
    let declared_type = definition_node.property("type")?;
    let reads_as_lucene = converting_strings(declared_type.as_ref())
        .first()
        .is_some_and(|text| text == "lucene");
    if !reads_as_lucene {
        report.type_mismatch = true;
        return Ok(report);
    }

    let mut buffer = vec![0u8; STREAM_BUFFER_BYTES];
    let mut stack = vec![(String::new(), *definition_node)];
    while let Some((node_path, node)) = stack.pop() {
        for property in node.properties()? {
            if property.property_type != PropertyType::Binary {
                continue;
            }
            let values = match &property.values {
                PropertyValues::Single(value) => std::slice::from_ref(value),
                PropertyValues::Multiple(values) => values.as_slice(),
            };
            for (value_index, value) in values.iter().enumerate() {
                check_one_blob(
                    provider,
                    &mut buffer,
                    &node_path,
                    &property.name,
                    value_index,
                    value,
                    &mut report,
                );
            }
        }
        // Hidden children included: see the module doc. Reversed, so the
        // stack yields them in stored order and a report reads in the order
        // the store holds.
        for (name, child) in node.child_node_entries()?.into_iter().rev() {
            stack.push((format!("{node_path}/{name}"), child));
        }
    }
    Ok(report)
}

fn check_one_blob(
    provider: &dyn SegmentProvider,
    buffer: &mut [u8],
    node_path: &str,
    property_name: &str,
    value_index: usize,
    value: &PropertyValue,
    report: &mut LuceneBlobReport,
) {
    let fault = |declared_length, streamed_length, reason| BlobFault {
        node_path: node_path.to_owned(),
        property_name: property_name.to_owned(),
        value_index,
        declared_length,
        streamed_length,
        reason,
    };
    let (declared_length, record_identifier) = match value {
        PropertyValue::Binary(BinaryValue::Inline {
            length,
            record_identifier,
        }) => (*length, *record_identifier),
        PropertyValue::Binary(BinaryValue::External { blob_identifier }) => {
            // Oak asks its blob store; froe has none, so the honest answer
            // is that the blob is unreadable rather than that it is fine.
            report.missing_blobs.push(fault(
                0,
                None,
                Some(format!(
                    "the value names the external blob {blob_identifier:?}, which lives \
                     outside the segment store"
                )),
            ));
            return;
        }
        _ => return,
    };

    let mut stream = match read_binary_stream(provider, record_identifier) {
        Ok(stream) => stream,
        Err(error) => {
            report
                .missing_blobs
                .push(fault(declared_length, None, Some(error.to_string())));
            return;
        }
    };
    let mut streamed_length = 0u64;
    loop {
        match stream.read(buffer) {
            Ok(0) => break,
            Ok(count) => streamed_length += count as u64,
            Err(error) => {
                report.invalid_blobs.push(fault(
                    declared_length,
                    Some(streamed_length),
                    Some(error.to_string()),
                ));
                return;
            }
        }
    }
    report.bytes_read += streamed_length;
    if streamed_length == declared_length {
        report.blobs_checked += 1;
    } else {
        report
            .invalid_blobs
            .push(fault(declared_length, Some(streamed_length), None));
    }
}
