//! Template records: the shared structure of similar nodes.
//!
//! A template captures everything about a node that rarely changes when
//! its values change: the primary type, the mixin types, the property
//! names and types, and whether the node has zero, one, or many child
//! nodes (with the child's name in the single-child case). Nodes reference
//! their template, so the many nodes sharing a structure store it once.
//!
//! `jcr:primaryType` and `jcr:mixinTypes` live in the template head — they
//! are *not* part of the property list, and node records store no values
//! for them.

use crate::content::list::uncounted_list_entries;
use crate::content::property::PropertyType;
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;

/// How many child nodes a template's nodes have.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChildNodeArity {
    /// No child nodes.
    Zero,
    /// Exactly one child node, whose name the template stores.
    One {
        /// The single child's name.
        child_name: String,
    },
    /// More than one child node; the node record points to a child map.
    Many,
}

/// One property slot of a template.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PropertyTemplate {
    /// The property name.
    pub name: String,
    /// The property's type.
    pub property_type: PropertyType,
    /// Whether the property is multi-valued.
    pub is_multiple: bool,
}

/// A parsed template record.
#[derive(Clone, PartialEq, Debug)]
pub struct Template {
    /// The node's `jcr:primaryType` value, when present.
    pub primary_type: Option<String>,
    /// The node's `jcr:mixinTypes` values; empty when absent.
    pub mixin_types: Vec<String>,
    /// The child node arity.
    pub child_arity: ChildNodeArity,
    /// The property slots, in on-disk order. The position of a slot is the
    /// index of its value in the node's property value list.
    pub properties: Vec<PropertyTemplate>,
}

impl Template {
    /// Finds a property slot by name, returning its index and template.
    #[must_use]
    pub fn property_by_name(&self, name: &str) -> Option<(usize, &PropertyTemplate)> {
        self.properties
            .iter()
            .enumerate()
            .find(|(_, property)| property.name == name)
    }
}

/// Reads the template record at `identifier`.
///
/// Layout: a 32-bit head (flag bits 31–28, ten bits of mixin count, and
/// eighteen bits of property count), then in order: the primary type name
/// identifier (bit 31), the mixin name identifiers (bit 30), the single
/// child name identifier (bits 29 and 28 both clear), the property name
/// list identifier and one signed type byte per property (property count
/// above zero). Negative type bytes mark multi-valued properties.
pub fn read_template(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
) -> Result<Template> {
    let view = provider.segment(identifier.segment)?;
    let record_number = identifier.record_number;
    let head = view.read_u32(record_number, 0)?;
    let has_primary_type = head & (1 << 31) != 0;
    let has_mixin_types = head & (1 << 30) != 0;
    let zero_child_nodes = head & (1 << 29) != 0;
    let many_child_nodes = head & (1 << 28) != 0;
    let mixin_count = ((head >> 18) & 0x3FF) as usize;
    let property_count = (head & 0x3FFFF) as usize;

    let mut cursor = 4usize;
    let read_identifier = |cursor: &mut usize| -> Result<RecordIdentifier> {
        let identifier = view.read_record_identifier(record_number, *cursor, 0)?;
        *cursor += 6;
        Ok(identifier)
    };

    let primary_type = if has_primary_type {
        let name_identifier = read_identifier(&mut cursor)?;
        Some(provider.string(name_identifier)?.as_ref().to_owned())
    } else {
        None
    };

    let mut mixin_types = Vec::new();
    if has_mixin_types {
        for _ in 0..mixin_count {
            let name_identifier = read_identifier(&mut cursor)?;
            mixin_types.push(provider.string(name_identifier)?.as_ref().to_owned());
        }
    }

    // The readers check the many-children bit first; well-formed data
    // never has both arity bits set.
    let child_arity = if many_child_nodes {
        ChildNodeArity::Many
    } else if zero_child_nodes {
        ChildNodeArity::Zero
    } else {
        let name_identifier = read_identifier(&mut cursor)?;
        ChildNodeArity::One {
            child_name: provider.string(name_identifier)?.as_ref().to_owned(),
        }
    };

    let mut properties = Vec::with_capacity(property_count);
    if property_count > 0 {
        let name_list_identifier = read_identifier(&mut cursor)?;
        let name_identifiers =
            uncounted_list_entries(provider, name_list_identifier, property_count as u64)?;
        for (property_index, name_identifier) in name_identifiers.into_iter().enumerate() {
            let type_byte = view.read_u8(record_number, cursor + property_index)? as i8;
            let tag = type_byte.unsigned_abs();
            let property_type =
                PropertyType::from_tag(tag).ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "template {identifier} declares invalid property type tag {type_byte}"
                    ),
                })?;
            properties.push(PropertyTemplate {
                name: provider.string(name_identifier)?.as_ref().to_owned(),
                property_type,
                is_multiple: type_byte < 0,
            });
        }
    }

    Ok(Template {
        primary_type,
        mixin_types,
        child_arity,
        properties,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{ChildNodeArity, read_template};
    use crate::content::property::PropertyType;
    use crate::content::provider::tests::MemorySegmentProvider;
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

    /// Builds a template record. Name arguments are record numbers of
    /// string records in the same segment.
    pub(crate) fn template_record(
        primary_type_record: Option<u32>,
        mixin_records: &[u32],
        arity: &TemplateArity,
        property_name_list_record: Option<u32>,
        property_types: &[i8],
    ) -> Vec<u8> {
        let mut head = 0u32;
        if primary_type_record.is_some() {
            head |= 1 << 31;
        }
        if !mixin_records.is_empty() {
            head |= 1 << 30;
            head |= (mixin_records.len() as u32) << 18;
        }
        match arity {
            TemplateArity::Zero => head |= 1 << 29,
            TemplateArity::Many => head |= 1 << 28,
            TemplateArity::One(_) => {}
        }
        head |= property_types.len() as u32;

        let mut bytes = head.to_be_bytes().to_vec();
        if let Some(record) = primary_type_record {
            bytes.extend_from_slice(&identifier_bytes(record));
        }
        for record in mixin_records {
            bytes.extend_from_slice(&identifier_bytes(*record));
        }
        if let TemplateArity::One(child_name_record) = arity {
            bytes.extend_from_slice(&identifier_bytes(*child_name_record));
        }
        if let Some(record) = property_name_list_record {
            bytes.extend_from_slice(&identifier_bytes(record));
        }
        for type_byte in property_types {
            bytes.push(*type_byte as u8);
        }
        bytes
    }

    pub(crate) enum TemplateArity {
        Zero,
        One(u32),
        #[allow(
            dead_code,
            reason = "exercised once node tests build many-children fixtures"
        )]
        Many,
    }

    #[test]
    fn reads_a_full_template() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        // Strings: 1 = primary type, 2 = mixin, 3-4 = property names.
        // Record 10: bucket of the two property name identifiers.
        // Record 20: the template.
        let mut property_name_bucket = Vec::new();
        property_name_bucket.extend_from_slice(&identifier_bytes(3));
        property_name_bucket.extend_from_slice(&identifier_bytes(4));
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record("nt:file")),
                    (2, 4, small_string_record("mix:versionable")),
                    (3, 4, small_string_record("jcr:created")),
                    (4, 4, small_string_record("jcr:predecessors")),
                    (10, 2, property_name_bucket),
                    (
                        20,
                        6,
                        template_record(
                            Some(1),
                            &[2],
                            &TemplateArity::Zero,
                            Some(10),
                            &[5, -9], // single DATE, multi-valued REFERENCE
                        ),
                    ),
                ],
            ),
        );

        let template =
            read_template(&provider, RecordIdentifier::new(segment, 20)).expect("template");
        assert_eq!(template.primary_type.as_deref(), Some("nt:file"));
        assert_eq!(template.mixin_types, vec!["mix:versionable"]);
        assert_eq!(template.child_arity, ChildNodeArity::Zero);
        assert_eq!(template.properties.len(), 2);
        assert_eq!(template.properties[0].name, "jcr:created");
        assert_eq!(template.properties[0].property_type, PropertyType::Date);
        assert!(!template.properties[0].is_multiple);
        assert_eq!(template.properties[1].name, "jcr:predecessors");
        assert_eq!(
            template.properties[1].property_type,
            PropertyType::Reference
        );
        assert!(template.properties[1].is_multiple);

        let (index, property) = template
            .property_by_name("jcr:predecessors")
            .expect("found");
        assert_eq!(index, 1);
        assert!(property.is_multiple);
        assert!(template.property_by_name("missing").is_none());
    }

    #[test]
    fn reads_single_child_templates() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record("jcr:content")),
                    (
                        20,
                        6,
                        template_record(None, &[], &TemplateArity::One(1), None, &[]),
                    ),
                ],
            ),
        );
        let template =
            read_template(&provider, RecordIdentifier::new(segment, 20)).expect("template");
        assert_eq!(template.primary_type, None);
        assert!(template.mixin_types.is_empty());
        assert_eq!(
            template.child_arity,
            ChildNodeArity::One {
                child_name: "jcr:content".to_owned()
            }
        );
        assert!(template.properties.is_empty());
    }

    #[test]
    fn rejects_invalid_property_type_tags() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record("broken")),
                    (
                        20,
                        6,
                        template_record(None, &[], &TemplateArity::Zero, Some(1), &[13]),
                    ),
                ],
            ),
        );
        assert!(read_template(&provider, RecordIdentifier::new(segment, 20)).is_err());
    }
}
