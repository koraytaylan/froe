//! The definitions file, turned into node states.
//!
//! [`drift::compare`](super::drift::compare) compares two `NodeState`s,
//! because the comparison it extends is Oak's own and Oak's own works over
//! node states. The file's side is a parsed JSON tree, so it has to become
//! records before it can be compared — and it must become them **without
//! touching the store**, since the drift refusal is a planning refusal and
//! a refused import leaves a store byte-identical to the one it found.
//!
//! So the tree is written into [`MemorySegments`], which is a sink and a
//! provider both. The records are thrown away with the value.

use crate::PropertyType;
use crate::content::node::NodeState;
use crate::error::Result;
use crate::index::definitions_json_reader::{ParsedNode, ParsedProperty};
use crate::segment::record::RecordIdentifier;
use crate::writer::memory_segments::MemorySegments;
use crate::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use crate::writer::segment_builder::GarbageCollectionGeneration;

/// A parsed definition, written into memory and readable as a node.
pub(crate) struct MaterializedDefinition {
    segments: MemorySegments,
    record: RecordIdentifier,
}

impl MaterializedDefinition {
    /// The definition as a node state over the in-memory segments.
    pub(crate) fn node(&self) -> NodeState<'_> {
        NodeState::new(&self.segments, self.record)
    }
}

/// Writes `parsed` into memory and returns it as a readable node.
pub(crate) fn materialize(parsed: &ParsedNode) -> Result<MaterializedDefinition> {
    // The generation is part of every record identifier but means nothing
    // here: these segments never reach a store, and nothing compares their
    // identifiers with the store's.
    let mut writer = RecordWriter::new(
        MemorySegments::default(),
        GarbageCollectionGeneration {
            generation: 1,
            full_generation: 1,
            is_compacted: false,
        },
    );
    let record = write_node(&mut writer, parsed)?;
    let segments = writer.finish()?;
    Ok(MaterializedDefinition { segments, record })
}

/// One parsed node and everything beneath it.
fn write_node<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    parsed: &ParsedNode,
) -> Result<RecordIdentifier> {
    let mut children = Vec::new();
    for (name, child) in &parsed.children {
        let record = write_node(writer, child)?;
        children.push((name.clone(), record));
    }

    let mut properties = Vec::new();
    for (name, property) in &parsed.properties {
        properties.push(write_property(writer, name, property)?);
    }

    let children = match children.len() {
        0 => ChildNodesToWrite::Zero,
        1 => {
            let (name, node) = children.remove(0);
            ChildNodesToWrite::One { name, node }
        }
        _ => ChildNodesToWrite::Many(children),
    };
    writer.write_node(None, &[], &children, &properties)
}

/// One parsed property, written with the type the file named.
///
/// Every value is a string record, which is how Oak stores every scalar
/// type but `BINARY`: the type lives in the template rather than in the
/// value. A `BINARY` the file carries is a `:blobId:`, so it is written as
/// the external identifier it names — the only encoding that reference
/// can have, and the only one the dumper ever prints. An *inline* binary in
/// the store would therefore read as drift against a file that names an
/// external one, which is a difference this comparison cannot resolve and
/// which no index definition in the wild carries.
fn write_property<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    name: &str,
    property: &ParsedProperty,
) -> Result<PropertyToWrite> {
    let mut written = Vec::new();
    for value in &property.values {
        written.push(if property.property_type == PropertyType::Binary {
            writer.write_external_binary_identifier(value)?
        } else {
            writer.write_string(value)?
        });
    }
    let values = if property.multiple {
        PropertyValuesToWrite::Multiple(written)
    } else {
        // A scalar with no value cannot be expressed, and the parser never
        // produces one: a non-array property carries exactly one value.
        PropertyValuesToWrite::Single(written.into_iter().next().ok_or_else(|| {
            crate::error::Error::InvalidFormat {
                details: format!("{name} is a scalar property with no value"),
            }
        })?)
    };
    Ok(PropertyToWrite {
        name: name.to_owned(),
        property_type: property.property_type,
        values,
    })
}
