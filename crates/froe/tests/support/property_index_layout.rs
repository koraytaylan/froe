//! An independent builder for property-index storage subtrees.
//!
//! The readers in `froe::index::property` are checked against `:index`
//! subtrees this module encodes, not against ones froe's own writer
//! produced. A self-consistent writer/reader pair would agree about a wrong
//! `match` property, a wrong `entry` arity or a wrong key name without
//! anything failing; this builder shares no encoding code with either, so a
//! disagreement shows up.
//!
//! It is deliberately naive: an in-memory tree of nodes, each with its own
//! template, serialized record by record through
//! [`super::SegmentBuilder`]. Nothing is shared, nothing is deduplicated,
//! and the child maps go through the independent map hash beside it. The
//! keys are spelled by the *caller*, so a test can store a key froe's own
//! encoder would never produce and watch the reader report it.
//!
//! Plan 0007's builder tests reuse the same helper against the writer, which
//! is why the entry description is a plain data type rather than something
//! shaped for the reader.

#![allow(
    dead_code,
    reason = "this module is compiled into every test binary that declares `support`, \
              and each one uses a different part of it"
)]

use std::collections::BTreeMap;

use super::{
    ArchiveBuilder, MapEntryFixture, SegmentBuilder, TYPE_LIST_BUCKET, TYPE_NODE, TYPE_TEMPLATE,
    TYPE_VALUE, build_child_map, data_segment_uuid, format_uuid, record_identifier_bytes,
    string_record, write_repository,
};

/// One property to encode on a synthetic node.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Property {
    /// A single `BOOLEAN`, which is what `match` is.
    Boolean(bool),
    /// A single `LONG`, which is what a `:count_*` counter is.
    Long(i64),
    /// A single `STRING`.
    Text(String),
    /// A single `NAME`, which is what `jcr:primaryType` is when it is a
    /// stored property rather than a template slot.
    Name(String),
    /// A multi-valued `STRING`, which is what `entry` is.
    Texts(Vec<String>),
    /// A multi-valued `NAME`, which is what `propertyNames` is.
    Names(Vec<String>),
}

impl Property {
    /// The signed type byte a template stores for this property: the JCR
    /// type tag, negated for a multi-valued property.
    fn type_byte(&self) -> i8 {
        match self {
            Property::Boolean(_) => 6,
            Property::Long(_) => 3,
            Property::Text(_) => 1,
            Property::Name(_) => 7,
            Property::Texts(_) => -1,
            Property::Names(_) => -7,
        }
    }

    /// The stored strings of the property's values, in order.
    fn value_texts(&self) -> Vec<String> {
        match self {
            Property::Boolean(value) => vec![value.to_string()],
            Property::Long(value) => vec![value.to_string()],
            Property::Text(value) | Property::Name(value) => vec![value.clone()],
            Property::Texts(values) | Property::Names(values) => values.clone(),
        }
    }

    fn is_multiple(&self) -> bool {
        matches!(self, Property::Texts(_) | Property::Names(_))
    }
}

/// A node of a synthetic tree: its properties and its children, by name.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Node {
    properties: BTreeMap<String, Property>,
    children: BTreeMap<String, Node>,
}

impl Node {
    /// An empty node.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a property, replacing any property of the same name.
    #[must_use]
    pub fn with(mut self, name: &str, property: Property) -> Self {
        self.properties.insert(name.to_owned(), property);
        self
    }

    /// Adds or replaces a child.
    #[must_use]
    pub fn with_child(mut self, name: &str, child: Node) -> Self {
        self.children.insert(name.to_owned(), child);
        self
    }

    /// Adds `match = true` and every intermediate node along `path`,
    /// creating them as bare nodes. `path` is relative and may be empty, in
    /// which case `match` lands on this node — the root-path case, which the
    /// mirror strategy reaches whenever the indexed path is `/`.
    #[must_use]
    pub fn with_match_at(mut self, path: &str) -> Self {
        let elements: Vec<&str> = path.split('/').filter(|name| !name.is_empty()).collect();
        if elements.is_empty() {
            return self.with("match", Property::Boolean(true));
        }
        let (first, rest) = elements.split_first().expect("a non-empty path");
        let child = self
            .children
            .remove(*first)
            .unwrap_or_default()
            .with_match_at(&rest.join("/"));
        self.children.insert((*first).to_owned(), child);
        self
    }

    fn child_names(&self) -> Vec<&String> {
        self.children.keys().collect()
    }
}

/// A mirror-index subtree: `:index/<key>/<path elements…>` with `match`.
///
/// The keys are spelled exactly as given, so a test may store one froe's own
/// encoder would never produce.
#[must_use]
pub fn mirror_storage(entries: &[(&str, &str)]) -> Node {
    let mut storage = Node::new();
    for (key, path) in entries {
        let key_node = storage
            .children
            .remove(*key)
            .unwrap_or_default()
            .with_match_at(path);
        storage.children.insert((*key).to_owned(), key_node);
    }
    storage
}

/// A unique-index subtree: `:index/<key>` with an `entry` array.
#[must_use]
pub fn unique_storage(entries: &[(&str, &[&str])]) -> Node {
    let mut storage = Node::new();
    for (key, paths) in entries {
        storage.children.insert(
            (*key).to_owned(),
            Node::new().with(
                "entry",
                Property::Texts(paths.iter().map(|path| (*path).to_owned()).collect()),
            ),
        );
    }
    storage
}

/// Writes a complete repository whose content root is `root`, through the
/// independent encoder.
///
/// The whole tree goes into one segment, which keeps the encoding simple and
/// is legal: nothing in the format requires a particular distribution of
/// records across segments.
pub fn write_repository_with_tree(directory: &std::path::Path, root: &Node) {
    let uuid = data_segment_uuid(0x51);
    let mut segment = SegmentBuilder::new(uuid);
    let mut next_record = 1u32;
    let mut allocate = move || {
        let record = next_record;
        next_record += 1;
        record
    };
    let root_record = encode_node(&mut segment, root, &mut allocate);
    let super_root_record = encode_super_root(&mut segment, root_record, &mut allocate);
    let bytes = segment.build();
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(uuid, bytes);
    write_repository(
        directory,
        &[("data00000a.tar".to_owned(), archive.build("data00000a.tar"))],
        &[format!(
            "{}:{super_root_record} root 1700000000000",
            format_uuid(uuid)
        )],
    );
}

/// The super-root: one child named `root`, pointing at the already-encoded
/// content root rather than at a second copy of it, which is what a real
/// store does and what makes the head and the content root share a tree.
fn encode_super_root(
    segment: &mut SegmentBuilder,
    root_record: u32,
    allocate: &mut impl FnMut() -> u32,
) -> u32 {
    let name_record = encode_string(segment, "root", allocate);
    let mut template = 0u32.to_be_bytes().to_vec();
    template.extend(record_identifier_bytes(0, name_record));
    let template_record = allocate();
    segment.add_record(template_record, TYPE_TEMPLATE, template);

    let record = allocate();
    let mut bytes = record_identifier_bytes(0, record);
    bytes.extend(record_identifier_bytes(0, template_record));
    bytes.extend(record_identifier_bytes(0, root_record));
    segment.add_record(record, TYPE_NODE, bytes);
    record
}

/// Encodes one node and its whole subtree, returning the node's record
/// number. Templates are per node rather than shared, which a real writer
/// would never do and which the format permits.
fn encode_node(
    segment: &mut SegmentBuilder,
    node: &Node,
    allocate: &mut impl FnMut() -> u32,
) -> u32 {
    let child_records: Vec<(String, u32)> = node
        .children
        .iter()
        .map(|(name, child)| (name.clone(), encode_node(segment, child, allocate)))
        .collect();

    let template_record = encode_template(segment, node, allocate);
    let property_list_record = encode_property_values(segment, node, allocate);
    let child_slot = encode_child_slot(segment, &child_records, allocate);

    let record = allocate();
    let mut bytes = record_identifier_bytes(0, record);
    bytes.extend(record_identifier_bytes(0, template_record));
    if let Some(child_slot) = child_slot {
        bytes.extend(record_identifier_bytes(0, child_slot));
    }
    if let Some(property_list_record) = property_list_record {
        bytes.extend(record_identifier_bytes(0, property_list_record));
    }
    segment.add_record(record, TYPE_NODE, bytes);
    record
}

/// The template head: bit 29 for no children, bit 28 for many, neither for
/// exactly one (whose name the template stores), and the property count in
/// the low eighteen bits. No primary type and no mixins, so bits 31 and 30
/// stay clear and `jcr:primaryType` is an ordinary stored property here.
fn encode_template(
    segment: &mut SegmentBuilder,
    node: &Node,
    allocate: &mut impl FnMut() -> u32,
) -> u32 {
    let names: Vec<&String> = node.properties.keys().collect();
    let mut head = names.len() as u32;
    match node.children.len() {
        0 => head |= 1 << 29,
        1 => {}
        _ => head |= 1 << 28,
    }
    let mut bytes = head.to_be_bytes().to_vec();
    if node.children.len() == 1 {
        let child_name = node.child_names()[0].clone();
        let name_record = encode_string(segment, &child_name, allocate);
        bytes.extend(record_identifier_bytes(0, name_record));
    }
    if !names.is_empty() {
        let name_records: Vec<u32> = names
            .iter()
            .map(|name| encode_string(segment, name, allocate))
            .collect();
        let list = encode_uncounted_list(segment, &name_records, allocate);
        bytes.extend(record_identifier_bytes(0, list));
        for name in &names {
            let property = &node.properties[*name];
            bytes.push(property.type_byte() as u8);
        }
    }
    let record = allocate();
    segment.add_record(record, TYPE_TEMPLATE, bytes);
    record
}

/// The property value list: one entry per template slot, in the same order.
/// A multi-valued property's entry is itself a list of value records.
fn encode_property_values(
    segment: &mut SegmentBuilder,
    node: &Node,
    allocate: &mut impl FnMut() -> u32,
) -> Option<u32> {
    if node.properties.is_empty() {
        return None;
    }
    let entries: Vec<u32> = node
        .properties
        .values()
        .map(|property| {
            if property.is_multiple() {
                encode_counted_list(segment, &property.value_texts(), allocate)
            } else {
                encode_string(segment, &property.value_texts()[0], allocate)
            }
        })
        .collect();
    Some(encode_uncounted_list(segment, &entries, allocate))
}

/// An uncounted list: **a one-element list is the element itself**, with no
/// bucket record at all, and a longer one is a bucket of identifiers. Getting
/// this wrong is invisible until a node has exactly one property, which is
/// why the helper exists rather than the bucket being inlined at each site.
fn encode_uncounted_list(
    segment: &mut SegmentBuilder,
    elements: &[u32],
    allocate: &mut impl FnMut() -> u32,
) -> u32 {
    assert!(
        !elements.is_empty(),
        "an uncounted list has no encoding for zero elements; its size lives in the parent"
    );
    if elements.len() == 1 {
        return elements[0];
    }
    assert!(
        elements.len() <= 255,
        "the fixture builder does not nest list buckets"
    );
    let mut bucket = Vec::new();
    for element in elements {
        bucket.extend(record_identifier_bytes(0, *element));
    }
    let record = allocate();
    segment.add_record(record, TYPE_LIST_BUCKET, bucket);
    record
}

/// Slot 2: the single child's node record, or the child map, or nothing.
fn encode_child_slot(
    segment: &mut SegmentBuilder,
    child_records: &[(String, u32)],
    allocate: &mut impl FnMut() -> u32,
) -> Option<u32> {
    match child_records.len() {
        0 => None,
        1 => Some(child_records[0].1),
        _ => {
            let entries: Vec<MapEntryFixture> = child_records
                .iter()
                .map(|(name, record)| {
                    let name_record = encode_string(segment, name, allocate);
                    (
                        name.clone(),
                        record_identifier_bytes(0, name_record),
                        record_identifier_bytes(0, *record),
                    )
                })
                .collect();
            Some(build_child_map(segment, allocate, &entries))
        }
    }
}

fn encode_string(
    segment: &mut SegmentBuilder,
    text: &str,
    allocate: &mut impl FnMut() -> u32,
) -> u32 {
    let record = allocate();
    segment.add_record(record, TYPE_VALUE, string_record(text));
    record
}

/// A counted list record: the entry count, then the bucket of value
/// identifiers. This is the shape a multi-valued property's value slot
/// points at.
fn encode_counted_list(
    segment: &mut SegmentBuilder,
    values: &[String],
    allocate: &mut impl FnMut() -> u32,
) -> u32 {
    let value_records: Vec<u32> = values
        .iter()
        .map(|value| encode_string(segment, value, allocate))
        .collect();
    let mut bytes = (values.len() as u32).to_be_bytes().to_vec();
    if !value_records.is_empty() {
        let body = encode_uncounted_list(segment, &value_records, allocate);
        bytes.extend(record_identifier_bytes(0, body));
    }
    let record = allocate();
    segment.add_record(record, super::TYPE_LIST, bytes);
    record
}
