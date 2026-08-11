//! Node records: the content tree itself.
//!
//! A node record is a short sequence of record identifiers:
//!
//! | slot | content                                                    |
//! |------|------------------------------------------------------------|
//! | 0    | stable identifier (see [`NodeState::stable_identifier`])   |
//! | 1    | template record                                            |
//! | 2    | child map (many children) *or* child node (one child)      |
//! | 2/3  | property value list (only when the template has properties)|
//!
//! The property value list slot is 2 when the node has no children,
//! 3 otherwise. Entry *i* of that list is the value record of the
//! property at template index *i*; multi-valued properties point at a
//! counted list of value records instead of a single value.

use std::sync::Arc;

use crate::content::list::{read_counted_list, uncounted_list_entries, uncounted_list_entry};
use crate::content::map::{
    map_entries, map_entries_with_limits, map_entry, map_size, map_size_with_maximum_work,
};
use crate::content::property::{PropertyType, PropertyValue, read_property_value};
use crate::content::provider::SegmentProvider;
use crate::content::template::{ChildNodeArity, Template};
use crate::content::value::read_string_with_stored_byte_budget;
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;

/// One property of a node: its name, type, and decoded values.
#[derive(Clone, PartialEq, Debug)]
pub struct PropertyState {
    /// The property name.
    pub name: String,
    /// The property's type.
    pub property_type: PropertyType,
    /// The decoded values.
    pub values: PropertyValues,
}

/// The values of a property: one for single-valued properties, any number
/// for multi-valued ones.
#[derive(Clone, PartialEq, Debug)]
pub enum PropertyValues {
    /// The value of a single-valued property.
    Single(PropertyValue),
    /// The values of a multi-valued property (possibly empty).
    Multiple(Vec<PropertyValue>),
}

/// A node of the content tree, addressed by its node record.
#[derive(Clone, Copy)]
pub struct NodeState<'provider> {
    provider: &'provider dyn SegmentProvider,
    record_identifier: RecordIdentifier,
}

type ChildNodeEntry<'provider> = (String, NodeState<'provider>);

impl<'provider> NodeState<'provider> {
    /// Creates a node state for the node record at `record_identifier`.
    #[must_use]
    pub fn new(
        provider: &'provider dyn SegmentProvider,
        record_identifier: RecordIdentifier,
    ) -> Self {
        Self {
            provider,
            record_identifier,
        }
    }

    /// The node record this state reads from.
    #[must_use]
    pub fn record_identifier(&self) -> RecordIdentifier {
        self.record_identifier
    }

    /// The node's template.
    pub fn template(&self) -> Result<Arc<Template>> {
        self.provider.template(self.template_identifier()?)
    }

    /// The node's stable identifier: `<segment UUID>:<record number>`.
    ///
    /// The stable identifier survives compaction: slot 0 either marks the
    /// node itself (a self-reference) or points at a 20-byte block holding
    /// the identifier of the original node record before compaction
    /// rewrote it.
    pub fn stable_identifier(&self) -> Result<String> {
        let view = self.provider.segment(self.record_identifier.segment)?;
        let slot = view.read_record_identifier(self.record_identifier.record_number, 0, 0)?;
        if slot == self.record_identifier {
            // The record number renders as a signed Java int.
            return Ok(format!("{}:{}", slot.segment, slot.record_number as i32));
        }
        let target = self.provider.segment(slot.segment)?;
        let bytes = target.read_bytes(slot.record_number, 0, 20)?;
        let most_significant_bits = u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        let least_significant_bits = u64::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let record_number = i32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let segment = crate::segment::identifier::SegmentIdentifier::new(
            most_significant_bits,
            least_significant_bits,
        );
        Ok(format!("{segment}:{record_number}"))
    }

    /// The node's 20-byte stable identifier: the serialized (`msb`, `lsb`,
    /// record number) of the record its identity descends from. When slot 0
    /// is a self reference, this is the node's own record identifier
    /// serialized. Rewriters propagate these bytes so a node keeps its
    /// identity across compaction.
    pub fn stable_identifier_bytes(&self) -> Result<[u8; 20]> {
        let view = self.provider.segment(self.record_identifier.segment)?;
        let slot = view.read_record_identifier(self.record_identifier.record_number, 0, 0)?;
        if slot == self.record_identifier {
            let mut bytes = [0u8; 20];
            bytes[0..8].copy_from_slice(&slot.segment.most_significant_bits.to_be_bytes());
            bytes[8..16].copy_from_slice(&slot.segment.least_significant_bits.to_be_bytes());
            bytes[16..20].copy_from_slice(&slot.record_number.to_be_bytes());
            return Ok(bytes);
        }
        let target = self.provider.segment(slot.segment)?;
        let stored = target.read_bytes(slot.record_number, 0, 20)?;
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(stored);
        Ok(bytes)
    }

    /// The number of child nodes.
    pub fn child_node_count(&self) -> Result<u64> {
        match self.template()?.child_arity {
            ChildNodeArity::Zero => Ok(0),
            ChildNodeArity::One { .. } => Ok(1),
            ChildNodeArity::Many => map_size(self.provider, self.child_map_identifier()?),
        }
    }

    /// Reads the child count without materializing template names, bounding
    /// map records followed through a many-child diff chain.
    pub(crate) fn child_node_count_with_maximum_work(
        &self,
        maximum_work_units: u64,
    ) -> Result<(u64, u64)> {
        let template_identifier = self.template_identifier()?;
        let view = self.provider.segment(template_identifier.segment)?;
        let head = view.read_u32(template_identifier.record_number, 0)?;
        if head & (1 << 28) != 0 {
            map_size_with_maximum_work(
                self.provider,
                self.child_map_identifier()?,
                maximum_work_units,
            )
        } else if head & (1 << 29) != 0 {
            Ok((0, 0))
        } else {
            Ok((1, 0))
        }
    }

    /// Looks up a child node by name.
    pub fn child_node(&self, name: &str) -> Result<Option<NodeState<'provider>>> {
        match &self.template()?.child_arity {
            ChildNodeArity::Zero => Ok(None),
            ChildNodeArity::One { child_name } => {
                if child_name == name {
                    let child_identifier = self.child_map_identifier()?;
                    Ok(Some(NodeState::new(self.provider, child_identifier)))
                } else {
                    Ok(None)
                }
            }
            ChildNodeArity::Many => {
                let map_identifier = self.child_map_identifier()?;
                Ok(map_entry(self.provider, map_identifier, name)?
                    .map(|child_identifier| NodeState::new(self.provider, child_identifier)))
            }
        }
    }

    /// All child nodes with their names, in storage order.
    pub fn child_node_entries(&self) -> Result<Vec<(String, NodeState<'provider>)>> {
        match &self.template()?.child_arity {
            ChildNodeArity::Zero => Ok(Vec::new()),
            ChildNodeArity::One { child_name } => {
                let child_identifier = self.child_map_identifier()?;
                Ok(vec![(
                    child_name.clone(),
                    NodeState::new(self.provider, child_identifier),
                )])
            }
            ChildNodeArity::Many => {
                let map_identifier = self.child_map_identifier()?;
                Ok(map_entries(self.provider, map_identifier)?
                    .into_iter()
                    .map(|entry| (entry.name, NodeState::new(self.provider, entry.value)))
                    .collect())
            }
        }
    }

    /// Reads child entries after bounding concrete entries, cumulative stored
    /// child-name bytes, and map enumeration work.
    pub(crate) fn child_node_entries_with_limits(
        &self,
        maximum_entries: u64,
        maximum_stored_name_bytes: u64,
        maximum_work_units: u64,
    ) -> Result<(Vec<ChildNodeEntry<'provider>>, u64, u64)> {
        let template_identifier = self.template_identifier()?;
        let view = self.provider.segment(template_identifier.segment)?;
        let head = view.read_u32(template_identifier.record_number, 0)?;
        if head & (1 << 28) != 0 {
            let map_identifier = self.child_map_identifier()?;
            let (entries, stored_name_bytes, visited_map_records) = map_entries_with_limits(
                self.provider,
                map_identifier,
                maximum_entries,
                maximum_stored_name_bytes,
                maximum_work_units,
            )?;
            return Ok((
                entries
                    .into_iter()
                    .map(|entry| (entry.name, NodeState::new(self.provider, entry.value)))
                    .collect(),
                stored_name_bytes,
                visited_map_records,
            ));
        }
        if head & (1 << 29) != 0 {
            return Ok((Vec::new(), 0, 0));
        }

        let has_primary_type = head & (1 << 31) != 0;
        let has_mixin_types = head & (1 << 30) != 0;
        let mixin_count = (head >> 18) & 0x3ff;
        let cursor = 4usize
            + usize::from(has_primary_type) * 6
            + usize::from(has_mixin_types) * mixin_count as usize * 6;
        let child_name_identifier =
            view.read_record_identifier(template_identifier.record_number, cursor, 0)?;
        let mut stored_name_bytes = 0;
        let child_name = read_string_with_stored_byte_budget(
            self.provider,
            child_name_identifier,
            maximum_stored_name_bytes,
            &mut stored_name_bytes,
        )?;
        let child_identifier = self.child_map_identifier()?;
        Ok((
            vec![(child_name, NodeState::new(self.provider, child_identifier))],
            stored_name_bytes,
            0,
        ))
    }

    /// All properties, in template order, with `jcr:primaryType` and
    /// `jcr:mixinTypes` synthesized from the template head first — the
    /// same view Oak's node state API presents.
    pub fn properties(&self) -> Result<Vec<PropertyState>> {
        let template = self.template()?;
        let mut properties = Vec::with_capacity(2 + template.properties.len());
        if let Some(primary_type) = &template.primary_type {
            properties.push(PropertyState {
                name: "jcr:primaryType".to_owned(),
                property_type: PropertyType::Name,
                values: PropertyValues::Single(PropertyValue::Name(primary_type.clone())),
            });
        }
        if !template.mixin_types.is_empty() {
            properties.push(PropertyState {
                name: "jcr:mixinTypes".to_owned(),
                property_type: PropertyType::Name,
                values: PropertyValues::Multiple(
                    template
                        .mixin_types
                        .iter()
                        .cloned()
                        .map(PropertyValue::Name)
                        .collect(),
                ),
            });
        }
        for property_index in 0..template.properties.len() {
            properties.push(self.property_at(&template, property_index)?);
        }
        Ok(properties)
    }

    /// The node's *stored* properties, in template order, **without** the
    /// synthesized `jcr:primaryType` and `jcr:mixinTypes`. Rewriting a node
    /// must use this together with the template's primary type and mixin
    /// types, never [`Self::properties`] filtered by name — a property
    /// literally named `jcr:primaryType` of a non-name type is a real
    /// stored property and would otherwise be lost.
    pub fn stored_properties(&self) -> Result<Vec<PropertyState>> {
        let template = self.template()?;
        let mut properties = Vec::with_capacity(template.properties.len());
        for property_index in 0..template.properties.len() {
            properties.push(self.property_at(&template, property_index)?);
        }
        Ok(properties)
    }

    /// Looks up a property by name, including the synthesized
    /// `jcr:primaryType` and `jcr:mixinTypes`.
    ///
    /// When the template head carries no synthetic entry for those names,
    /// the lookup falls through to the ordinary property list: a property
    /// literally named `jcr:primaryType` whose type is not a single NAME
    /// is stored as an ordinary property, and Java returns it.
    pub fn property(&self, name: &str) -> Result<Option<PropertyState>> {
        let template = self.template()?;
        if name == "jcr:primaryType"
            && let Some(primary_type) = &template.primary_type
        {
            return Ok(Some(PropertyState {
                name: "jcr:primaryType".to_owned(),
                property_type: PropertyType::Name,
                values: PropertyValues::Single(PropertyValue::Name(primary_type.clone())),
            }));
        }
        if name == "jcr:mixinTypes" && !template.mixin_types.is_empty() {
            return Ok(Some(PropertyState {
                name: "jcr:mixinTypes".to_owned(),
                property_type: PropertyType::Name,
                values: PropertyValues::Multiple(
                    template
                        .mixin_types
                        .iter()
                        .cloned()
                        .map(PropertyValue::Name)
                        .collect(),
                ),
            }));
        }
        match template.property_by_name(name) {
            None => Ok(None),
            Some((property_index, _)) => Ok(Some(self.property_at(&template, property_index)?)),
        }
    }

    /// Reads the property at `property_index` of the template's property
    /// list.
    fn property_at(&self, template: &Template, property_index: usize) -> Result<PropertyState> {
        let property_template = &template.properties[property_index];
        let list_identifier = self.property_list_identifier(template)?;
        let value_identifier = uncounted_list_entry(
            self.provider,
            list_identifier,
            template.properties.len() as u64,
            property_index as u64,
        )?;
        let values = if property_template.is_multiple {
            let counted = read_counted_list(self.provider, value_identifier)?;
            let mut values = Vec::with_capacity(counted.size as usize);
            if let Some(body) = counted.body {
                for element in uncounted_list_entries(self.provider, body, u64::from(counted.size))?
                {
                    values.push(read_property_value(
                        self.provider,
                        element,
                        property_template.property_type,
                    )?);
                }
            }
            PropertyValues::Multiple(values)
        } else {
            PropertyValues::Single(read_property_value(
                self.provider,
                value_identifier,
                property_template.property_type,
            )?)
        };
        Ok(PropertyState {
            name: property_template.name.clone(),
            property_type: property_template.property_type,
            values,
        })
    }

    /// The record identifier in slot 2: the child map for many children,
    /// the single child's node record for one child.
    fn child_map_identifier(&self) -> Result<RecordIdentifier> {
        let view = self.provider.segment(self.record_identifier.segment)?;
        view.read_record_identifier(self.record_identifier.record_number, 0, 2)
    }

    fn template_identifier(&self) -> Result<RecordIdentifier> {
        let view = self.provider.segment(self.record_identifier.segment)?;
        view.read_record_identifier(self.record_identifier.record_number, 0, 1)
    }

    /// The record identifier of the property value list: slot 2 without
    /// children, slot 3 with children.
    fn property_list_identifier(&self, template: &Template) -> Result<RecordIdentifier> {
        if template.properties.is_empty() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "node {} has no properties and therefore no property value list",
                    self.record_identifier
                ),
            });
        }
        let slot = if template.child_arity == ChildNodeArity::Zero {
            2
        } else {
            3
        };
        let view = self.provider.segment(self.record_identifier.segment)?;
        view.read_record_identifier(self.record_identifier.record_number, 0, slot)
    }
}

impl std::fmt::Debug for NodeState<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "NodeState({})", self.record_identifier)
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeState, PropertyValues};
    use crate::content::property::{PropertyType, PropertyValue};
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::content::template::tests::{TemplateArity, template_record};
    use crate::content::value::BinaryValue;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;

    fn small_string_record(text: &str) -> Vec<u8> {
        let mut bytes = vec![text.len() as u8];
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    fn identifier_bytes(record_number: u32) -> [u8; 6] {
        let mut bytes = [0u8; 6];
        bytes[2..6].copy_from_slice(&record_number.to_be_bytes());
        bytes
    }

    /// A node record: stable identifier (self by default), template, and
    /// further slots.
    fn node_record(
        own_record_number: u32,
        template_record_number: u32,
        extra_slots: &[u32],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&identifier_bytes(own_record_number));
        bytes.extend_from_slice(&identifier_bytes(template_record_number));
        for slot in extra_slots {
            bytes.extend_from_slice(&identifier_bytes(*slot));
        }
        bytes
    }

    /// Builds a provider holding one segment with a two-property node:
    /// title (a string) and tags (a multi-valued string pair).
    fn provider_with_property_node() -> (MemorySegmentProvider, RecordIdentifier) {
        let segment = data_segment_identifier(1);
        // Property name strings.
        let mut records: Vec<(u32, u8, Vec<u8>)> = vec![
            (1, 4, small_string_record("title")),
            (2, 4, small_string_record("tags")),
            (3, 4, small_string_record("nt:unstructured")),
            // Values.
            (4, 4, small_string_record("Hello")),
            (5, 4, small_string_record("a")),
            (6, 4, small_string_record("b")),
        ];
        // Record 10: bucket of property names [1, 2].
        let mut name_bucket = Vec::new();
        name_bucket.extend_from_slice(&identifier_bytes(1));
        name_bucket.extend_from_slice(&identifier_bytes(2));
        records.push((10, 2, name_bucket));
        // Record 11: bucket of the two tag values [5, 6].
        let mut tags_bucket = Vec::new();
        tags_bucket.extend_from_slice(&identifier_bytes(5));
        tags_bucket.extend_from_slice(&identifier_bytes(6));
        records.push((11, 2, tags_bucket));
        // Record 12: counted list for tags: size 2, body = record 11.
        let mut counted = 2u32.to_be_bytes().to_vec();
        counted.extend_from_slice(&identifier_bytes(11));
        records.push((12, 3, counted));
        // Record 13: bucket of property values [4 (title), 12 (tags)].
        let mut value_bucket = Vec::new();
        value_bucket.extend_from_slice(&identifier_bytes(4));
        value_bucket.extend_from_slice(&identifier_bytes(12));
        records.push((13, 2, value_bucket));
        // Record 20: template, zero children, properties [STRING, -STRING].
        records.push((
            20,
            6,
            template_record(Some(3), &[], &TemplateArity::Zero, Some(10), &[1, -1]),
        ));
        // Record 30: the node.
        records.push((30, 7, node_record(30, 20, &[13])));

        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));
        (provider, RecordIdentifier::new(segment, 30))
    }

    #[test]
    fn reads_properties_with_synthesized_type_properties() {
        let (provider, node_identifier) = provider_with_property_node();
        let node = NodeState::new(&provider, node_identifier);

        let properties = node.properties().expect("properties");
        let names: Vec<&str> = properties
            .iter()
            .map(|property| property.name.as_str())
            .collect();
        assert_eq!(names, ["jcr:primaryType", "title", "tags"]);

        let primary_type = node
            .property("jcr:primaryType")
            .expect("read")
            .expect("present");
        assert_eq!(
            primary_type.values,
            PropertyValues::Single(PropertyValue::Name("nt:unstructured".to_owned()))
        );

        let title = node.property("title").expect("read").expect("present");
        assert_eq!(title.property_type, PropertyType::String);
        assert_eq!(
            title.values,
            PropertyValues::Single(PropertyValue::String("Hello".to_owned()))
        );

        let tags = node.property("tags").expect("read").expect("present");
        assert_eq!(
            tags.values,
            PropertyValues::Multiple(vec![
                PropertyValue::String("a".to_owned()),
                PropertyValue::String("b".to_owned()),
            ])
        );

        assert_eq!(node.property("missing").expect("read"), None);
        assert_eq!(node.property("jcr:mixinTypes").expect("read"), None);
        assert_eq!(node.child_node_count().expect("count"), 0);
        assert!(node.child_node_entries().expect("children").is_empty());
    }

    #[test]
    fn resolves_single_child_nodes() {
        let segment = data_segment_identifier(1);
        let mut records: Vec<(u32, u8, Vec<u8>)> = vec![
            (1, 4, small_string_record("jcr:content")),
            // Child template (zero children, no properties, no type).
            (
                20,
                6,
                template_record(None, &[], &TemplateArity::Zero, None, &[]),
            ),
            // Parent template: single child named jcr:content.
            (
                21,
                6,
                template_record(None, &[], &TemplateArity::One(1), None, &[]),
            ),
        ];
        // Record 30: child node; record 31: parent node.
        records.push((30, 7, node_record(30, 20, &[])));
        records.push((31, 7, node_record(31, 21, &[30])));
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));

        let parent = NodeState::new(&provider, RecordIdentifier::new(segment, 31));
        assert_eq!(parent.child_node_count().expect("count"), 1);
        let child = parent
            .child_node("jcr:content")
            .expect("read")
            .expect("present");
        assert_eq!(
            child.record_identifier(),
            RecordIdentifier::new(segment, 30)
        );
        assert!(parent.child_node("other").expect("read").is_none());

        let entries = parent.child_node_entries().expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "jcr:content");
    }

    #[test]
    fn stable_identifier_of_uncompacted_node_is_its_own_identifier() {
        let (provider, node_identifier) = provider_with_property_node();
        let node = NodeState::new(&provider, node_identifier);
        let stable = node.stable_identifier().expect("stable identifier");
        assert_eq!(
            stable,
            format!(
                "{}:{}",
                node_identifier.segment, node_identifier.record_number
            )
        );
    }

    #[test]
    fn reads_binary_properties() {
        let segment = data_segment_identifier(1);
        let content = vec![1u8, 2, 3];
        let mut binary_record = vec![content.len() as u8];
        binary_record.extend_from_slice(&content);
        let records: Vec<(u32, u8, Vec<u8>)> = vec![
            (1, 4, small_string_record("data")),
            (4, 4, binary_record),
            (
                20,
                6,
                template_record(None, &[], &TemplateArity::Zero, Some(1), &[2]),
            ),
            (30, 7, node_record(30, 20, &[4])),
        ];
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));

        let node = NodeState::new(&provider, RecordIdentifier::new(segment, 30));
        let data = node.property("data").expect("read").expect("present");
        assert_eq!(data.property_type, PropertyType::Binary);
        assert_eq!(
            data.values,
            PropertyValues::Single(PropertyValue::Binary(BinaryValue::Inline {
                length: 3,
                record_identifier: RecordIdentifier::new(segment, 4),
            }))
        );
    }
}
