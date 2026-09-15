//! Writing a node and the template it shares: the property order Oak
//! sorts by, and the stable identifier a rewrite preserves.

use super::{
    Error, PropertyType, RecordIdentifier, RecordType, RecordWriter, Result, SegmentSink,
    compare_utf16_strings,
};

pub(crate) const TEMPLATE_DEDUP_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// A property of a node to be written.
#[derive(Clone)]
pub struct PropertyToWrite {
    /// The property name.
    pub name: String,
    /// The property type.
    pub property_type: PropertyType,
    /// The value record identifiers: one for a single-valued property,
    /// written as a counted list for a multi-valued one.
    pub values: PropertyValuesToWrite,
}

/// The written shape of a property's values.
///
/// `Clone` because a rewrite that replaces some of a node's properties
/// carries the replacements alongside the preserved slots and writes them
/// together; every variant is a record identifier or a list of them, so a
/// clone copies identifiers rather than content.
#[derive(Clone)]
pub enum PropertyValuesToWrite {
    /// A single value record.
    Single(RecordIdentifier),
    /// Multi-valued: any number of value records.
    Multiple(Vec<RecordIdentifier>),
    /// A slot preserved from an existing node: for single values the
    /// value record, for multi values the existing counted list record.
    PreservedSlot {
        /// The record the node's property value list will point at.
        value_slot: RecordIdentifier,
        /// Whether the preserved property is multi-valued (drives the
        /// template's type byte sign).
        is_multiple: bool,
    },
}

/// The child node shape of a node to be written.
pub enum ChildNodesToWrite {
    /// No children.
    Zero,
    /// One child with the given name and node record.
    One {
        /// The child's name.
        name: String,
        /// The child's node record.
        node: RecordIdentifier,
    },
    /// Many children as `(name, node record)` pairs.
    Many(Vec<(String, RecordIdentifier)>),
    /// Many children through an existing, unchanged map record.
    ManyExistingMap(RecordIdentifier),
}

/// The identity of a template: everything that decides its serialized form.
///
/// Two nodes share a template when their primary type, mixins, child-node
/// arity and property slots agree; the property *values* differ per node and
/// are not part of it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct TemplateKey {
    pub(crate) primary_type: Option<String>,
    pub(crate) mixin_types: Vec<String>,
    pub(crate) child_arity: u8,
    pub(crate) single_child_name: Option<String>,
    pub(crate) properties: Vec<(String, u8)>,
}

impl TemplateKey {
    pub(in crate::writer) fn of(
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
    ) -> Self {
        // Only the arity and, for the single-child case, the child's name
        // are serialized into a template; which node it points at is part of
        // the node record, not the shape.
        let (child_arity, single_child_name) = match child_nodes {
            ChildNodesToWrite::Zero => (0u8, None),
            ChildNodesToWrite::One { name, .. } => (1u8, Some(name.clone())),
            ChildNodesToWrite::Many(_) | ChildNodesToWrite::ManyExistingMap(_) => (2u8, None),
        };
        Self {
            primary_type: primary_type.map(str::to_owned),
            mixin_types: mixin_types.to_vec(),
            child_arity,
            single_child_name,
            properties: properties
                .iter()
                .map(|property| {
                    (
                        property.name.clone(),
                        property_slot_tag(property.property_type, &property.values),
                    )
                })
                .collect(),
        }
    }
}

/// The per-slot type byte a template records: the property type, negated
/// when the slot is multi-valued, exactly as the serialized form encodes it.
///
/// This is the single source of truth for that byte. Both
/// [`TemplateKey::of`], which decides whether two nodes may share a
/// template record, and `write_template_record`, which serializes the
/// byte, go through it — because if they disagree, two nodes whose slots
/// differ *only* in arity hash to the same key, the second silently
/// inherits the first one's template, and its values are then decoded at
/// the wrong arity: a single value record read as a counted list, or a
/// counted list read as one value. Nothing rejects the result. The store
/// parses, Oak boots, and the property is quietly wrong.
///
/// The match is exhaustive rather than a `matches!` against one variant so
/// that a new [`PropertyValuesToWrite`] variant is a compile error here
/// instead of defaulting to single-valued.
pub(crate) fn property_slot_tag(property_type: PropertyType, values: &PropertyValuesToWrite) -> u8 {
    let multiple = match values {
        PropertyValuesToWrite::Single(_) => false,
        PropertyValuesToWrite::Multiple(_) => true,
        PropertyValuesToWrite::PreservedSlot { is_multiple, .. } => *is_multiple,
    };
    let tag = property_type as i8;
    (if multiple { -tag } else { tag }) as u8
}

/// Whether a 20-byte stable identifier names exactly `record`.
pub(crate) fn stable_identifier_names(
    stable_identifier: Option<[u8; 20]>,
    record: RecordIdentifier,
) -> bool {
    let Some(bytes) = stable_identifier else {
        return false;
    };
    let most = u64::from_be_bytes(bytes[0..8].try_into().expect("8 bytes"));
    let least = u64::from_be_bytes(bytes[8..16].try_into().expect("8 bytes"));
    let number = u32::from_be_bytes(bytes[16..20].try_into().expect("4 bytes"));
    most == record.segment.most_significant_bits
        && least == record.segment.least_significant_bits
        && number == record.record_number
}

/// Sorts properties into the on-disk template order: by Java string hash
/// of the name, then by name in UTF-16 order, then by type tag — the
/// order `Template`'s constructor establishes in Java.
pub fn sort_properties_for_template(properties: &mut [PropertyToWrite]) {
    properties.sort_by(template_order);
}

/// The comparator [`sort_properties_for_template`] sorts by, and the one
/// [`RecordWriter::write_node`] checks against before it sorts a copy.
fn template_order(first: &PropertyToWrite, second: &PropertyToWrite) -> std::cmp::Ordering {
    crate::hashing::utf16_string_hash(&first.name)
        .cmp(&crate::hashing::utf16_string_hash(&second.name))
        .then_with(|| compare_utf16_strings(&first.name, &second.name))
        .then_with(|| (first.property_type as u8).cmp(&(second.property_type as u8)))
}

/// Whether a property list is already in that order.
///
/// Checked rather than assumed, and checked rather than always sorting a
/// copy: compaction writes every node in the store through this path and
/// already hands it a sorted list, so the common case must not allocate.
fn in_template_order(properties: &[PropertyToWrite]) -> bool {
    properties
        .windows(2)
        .all(|pair| template_order(&pair[0], &pair[1]) != std::cmp::Ordering::Greater)
}

impl<Sink: SegmentSink> RecordWriter<Sink> {
    /// Writes a template record. `properties` must already be in on-disk
    /// order (see [`sort_properties_for_template`]).
    #[allow(
        clippy::missing_panics_doc,
        reason = "record slice indexing is in-bounds by construction of the allocation"
    )]
    pub fn write_template(
        &mut self,
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
    ) -> Result<RecordIdentifier> {
        // A template is the *shape* of a node, and a tree has far fewer
        // shapes than nodes — Oak caps its own template cache at 3000 for
        // that reason. Without this every node wrote its own copy, which on
        // a large repository is the single largest source of write
        // amplification in the whole path.
        let key = TemplateKey::of(primary_type, mixin_types, child_nodes, properties);
        if let Some(existing) = self.template_cache.get(&key) {
            return Ok(existing);
        }
        let written =
            self.write_template_record(primary_type, mixin_types, child_nodes, properties)?;
        self.template_cache.insert(key, written);
        Ok(written)
    }

    pub(super) fn write_template_record(
        &mut self,
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
    ) -> Result<RecordIdentifier> {
        if mixin_types.len() >= 1 << 10 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} mixin types exceed the template limit of 1023",
                    mixin_types.len()
                ),
            });
        }
        if properties.len() >= 1 << 18 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} properties exceed the template limit of 262143",
                    properties.len()
                ),
            });
        }
        let mut head = 0u32;
        let mut trailing_identifiers: Vec<RecordIdentifier> = Vec::new();

        let primary_type_identifier = match primary_type {
            Some(name) => {
                head |= 1 << 31;
                Some(self.write_string(name)?)
            }
            None => None,
        };
        let mut mixin_identifiers = Vec::with_capacity(mixin_types.len());
        if !mixin_types.is_empty() {
            head |= 1 << 30;
            head |= (mixin_types.len() as u32) << 18;
            for mixin in mixin_types {
                mixin_identifiers.push(self.write_string(mixin)?);
            }
        }
        let single_child_name_identifier = match child_nodes {
            ChildNodesToWrite::Zero => {
                head |= 1 << 29;
                None
            }
            ChildNodesToWrite::Many(_) | ChildNodesToWrite::ManyExistingMap(_) => {
                head |= 1 << 28;
                None
            }
            ChildNodesToWrite::One { name, .. } => Some(self.write_string(name)?),
        };
        head |= properties.len() as u32;

        let property_names_identifier = if properties.is_empty() {
            None
        } else {
            let mut name_identifiers = Vec::with_capacity(properties.len());
            for property in properties {
                name_identifiers.push(self.write_string(&property.name)?);
            }
            Some(
                self.write_list_body(&name_identifiers)?
                    .expect("non-empty list"),
            )
        };

        trailing_identifiers.extend(primary_type_identifier);
        trailing_identifiers.extend(mixin_identifiers.iter().copied());
        trailing_identifiers.extend(single_child_name_identifier);
        trailing_identifiers.extend(property_names_identifier);

        let size = 4 + trailing_identifiers.len() * 6 + properties.len();
        let record = self.allocate(RecordType::Template, size, &trailing_identifiers)?;
        self.current.record_bytes_mut(record)[0..4].copy_from_slice(&head.to_be_bytes());
        let mut cursor = 4;
        for identifier in &trailing_identifiers {
            self.write_identifier_at(record, cursor, *identifier);
            cursor += 6;
        }
        for property in properties {
            self.current.record_bytes_mut(record)[cursor] =
                property_slot_tag(property.property_type, &property.values);
            cursor += 1;
        }
        Ok(self.identifier_of(record))
    }

    /// Writes a node record with its template, child structure, and
    /// property values. Properties must be in template order.
    #[allow(
        clippy::missing_panics_doc,
        reason = "record slice indexing is in-bounds by construction of the allocation"
    )]
    pub fn write_node(
        &mut self,
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
    ) -> Result<RecordIdentifier> {
        self.write_node_with_stable_identifier(
            primary_type,
            mixin_types,
            child_nodes,
            properties,
            None,
        )
    }

    /// Writes a node, preserving an existing stable identifier when one is
    /// given. A stable identifier survives compaction: when it differs
    /// from the node's own record identifier, it is stored as a 20-byte
    /// block (`msb`, `lsb`, record number) and slot 0 points at it;
    /// otherwise slot 0 is a self reference. Preserving it lets Oak's
    /// stable-identifier fast path keep matching a node across rewrites.
    ///
    /// # Panics
    ///
    /// Panics only on internal allocation invariants, never on input.
    #[allow(
        clippy::missing_panics_doc,
        reason = "record slice indexing is in-bounds by construction of the allocation"
    )]
    pub fn write_node_with_stable_identifier(
        &mut self,
        primary_type: Option<&str>,
        mixin_types: &[String],
        child_nodes: &ChildNodesToWrite,
        properties: &[PropertyToWrite],
        stable_identifier: Option<[u8; 20]>,
    ) -> Result<RecordIdentifier> {
        // **Sorted here, not by the caller.** Oak's own `getProperties`
        // pairs `template.getPropertyTemplates()[i]` — the array its
        // `Template` constructor sorted — with `properties.getEntry(i)`,
        // the value list's *i*-th slot. So the two agree only when the
        // on-disk name order already is that sort order, and a node
        // written with the names in any other order reads back with every
        // value against the wrong name: a `STRING` where Oak expects a
        // `LONG`, a single value read as a list of twenty million
        // elements. froe's own reader pairs on-disk position with on-disk
        // position and so cannot see it, which is why this is enforced at
        // the one place every node goes through rather than left to each
        // caller. The interop suite is what found it, in a definition
        // `rewrite_node_with_edits` had sorted by UTF-8 name bytes.
        let sorted;
        let properties = if in_template_order(properties) {
            properties
        } else {
            sorted = {
                let mut copy = properties.to_vec();
                sort_properties_for_template(&mut copy);
                copy
            };
            &sorted
        };
        let template_identifier =
            self.write_template(primary_type, mixin_types, child_nodes, properties)?;

        let child_identifier = match child_nodes {
            ChildNodesToWrite::Zero => None,
            ChildNodesToWrite::One { node, .. } => Some(*node),
            ChildNodesToWrite::Many(entries) => Some(self.write_map(entries)?),
            ChildNodesToWrite::ManyExistingMap(map) => Some(*map),
        };

        let property_list_identifier = if properties.is_empty() {
            None
        } else {
            let mut value_identifiers = Vec::with_capacity(properties.len());
            for property in properties {
                let identifier = match &property.values {
                    PropertyValuesToWrite::Single(value) => *value,
                    PropertyValuesToWrite::Multiple(values) => self.write_counted_list(values)?,
                    PropertyValuesToWrite::PreservedSlot { value_slot, .. } => *value_slot,
                };
                value_identifiers.push(identifier);
            }
            Some(
                self.write_list_body(&value_identifiers)?
                    .expect("non-empty list"),
            )
        };

        // A preserved stable identifier is stored as a 20-byte block that
        // slot 0 references, unless it happens to name the node itself.
        let stable_block = match stable_identifier {
            Some(bytes) => {
                let record = self.allocate(RecordType::Block, 20, &[])?;
                self.current.record_bytes_mut(record)[..20].copy_from_slice(&bytes);
                Some(self.identifier_of(record))
            }
            None => None,
        };

        let mut slots: Vec<RecordIdentifier> = vec![template_identifier];
        slots.extend(child_identifier);
        slots.extend(property_list_identifier);

        let mut referenced = slots.clone();
        referenced.extend(stable_block);
        let size = 6 + slots.len() * 6;
        let record = self.allocate(RecordType::Node, size, &referenced)?;
        let own_identifier = self.identifier_of(record);
        // Slot 0: the preserved stable-id block, or a self reference.
        let slot_zero = match stable_block {
            Some(block) => {
                // A stable identifier equal to the node's own record would
                // be redundant; the self-reference marker covers it.
                if stable_identifier_names(stable_identifier, own_identifier) {
                    own_identifier
                } else {
                    block
                }
            }
            None => own_identifier,
        };
        self.write_identifier_at(record, 0, slot_zero);
        for (position, identifier) in slots.iter().enumerate() {
            self.write_identifier_at(record, 6 + position * 6, *identifier);
        }
        Ok(own_identifier)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, sort_properties_for_template,
    };
    use crate::content::node::{NodeState, PropertyValues};
    use crate::content::property::{PropertyType, PropertyValue};
    use crate::writer::record_writer::test_support::new_writer;

    #[test]
    fn nodes_round_trip_with_properties_and_children() {
        let mut writer = new_writer();

        let leaf = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("leaf");

        let title_value = writer.write_string("Hello").expect("value");
        let first_tag = writer.write_string("alpha").expect("value");
        let second_tag = writer.write_string("beta").expect("value");
        let count_value = writer.write_string("42").expect("value");
        let mut properties = vec![
            PropertyToWrite {
                name: "title".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Single(title_value),
            },
            PropertyToWrite {
                name: "tags".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Multiple(vec![first_tag, second_tag]),
            },
            PropertyToWrite {
                name: "count".to_owned(),
                property_type: PropertyType::Long,
                values: PropertyValuesToWrite::Single(count_value),
            },
        ];
        sort_properties_for_template(&mut properties);

        let parent = writer
            .write_node(
                Some("nt:unstructured"),
                &["mix:versionable".to_owned()],
                &ChildNodesToWrite::Many(vec![
                    ("first".to_owned(), leaf),
                    ("second".to_owned(), leaf),
                ]),
                &properties,
            )
            .expect("parent");

        let store = writer.finish().expect("finish");
        let node = NodeState::new(&store, parent);

        let template = node.template().expect("template");
        assert_eq!(template.primary_type.as_deref(), Some("nt:unstructured"));
        assert_eq!(template.mixin_types, vec!["mix:versionable"]);

        let title = node.property("title").expect("read").expect("present");
        assert_eq!(
            title.values,
            PropertyValues::Single(PropertyValue::String("Hello".to_owned()))
        );
        let count = node.property("count").expect("read").expect("present");
        assert_eq!(
            count.values,
            PropertyValues::Single(PropertyValue::Long(42))
        );
        let tags = node.property("tags").expect("read").expect("present");
        assert_eq!(
            tags.values,
            PropertyValues::Multiple(vec![
                PropertyValue::String("alpha".to_owned()),
                PropertyValue::String("beta".to_owned()),
            ])
        );

        assert_eq!(node.child_node_count().expect("count"), 2);
        let first = node.child_node("first").expect("lookup").expect("present");
        assert_eq!(first.record_identifier(), leaf);
        assert_eq!(
            node.stable_identifier().expect("stable"),
            format!("{}:{}", parent.segment, parent.record_number as i32)
        );
    }

    #[test]
    fn a_preserved_multi_valued_slot_never_shares_a_template_with_a_single_valued_one() {
        // The template cache keys on TemplateKey, and the per-slot type
        // byte carries the arity in its sign. When the key and the
        // serialized byte disagree about a preserved slot's arity, these
        // two nodes — identical in primary type, mixins, child arity and
        // property name and type, differing *only* in arity — collide.
        // The second then inherits the first's template record and its
        // values are decoded at the wrong arity: the counted list read as
        // one value, or the single value read as a counted list. Nothing
        // rejects that. It is the shape of damage a store still boots on.
        //
        // Both directions are exercised, because whichever node is written
        // first is the one that wins the cache and the other is the one
        // that gets corrupted.
        let mut writer = new_writer();

        let first_tag = writer.write_string("alpha").expect("value");
        let second_tag = writer.write_string("beta").expect("value");
        let preserved_list = writer
            .write_counted_list(&[first_tag, second_tag])
            .expect("counted list");
        let lone_value = writer.write_string("solo").expect("value");

        let multi_valued = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "tags".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::PreservedSlot {
                        value_slot: preserved_list,
                        is_multiple: true,
                    },
                }],
            )
            .expect("multi-valued node");

        let single_valued = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "tags".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::Single(lone_value),
                }],
            )
            .expect("single-valued node");

        let store = writer.finish().expect("finish");

        let multi_template = NodeState::new(&store, multi_valued)
            .template()
            .expect("multi template");
        let single_template = NodeState::new(&store, single_valued)
            .template()
            .expect("single template");
        assert!(
            multi_template.properties[0].is_multiple,
            "the preserved slot must serialize as multi-valued"
        );
        assert!(
            !single_template.properties[0].is_multiple,
            "the single-valued slot must not inherit the multi-valued template"
        );

        // The decoded values are what an Oak reader would actually see, so
        // assert those too: a template mix-up shows up here as an arity
        // flip, not as a parse failure.
        let multi_tags = NodeState::new(&store, multi_valued)
            .property("tags")
            .expect("read")
            .expect("present");
        assert_eq!(
            multi_tags.values,
            PropertyValues::Multiple(vec![
                PropertyValue::String("alpha".to_owned()),
                PropertyValue::String("beta".to_owned()),
            ])
        );
        let single_tags = NodeState::new(&store, single_valued)
            .property("tags")
            .expect("read")
            .expect("present");
        assert_eq!(
            single_tags.values,
            PropertyValues::Single(PropertyValue::String("solo".to_owned()))
        );
    }

    /// A node written with its properties out of template order is stored
    /// **in** template order, names and values together.
    ///
    /// Oak's own `getProperties` pairs the *i*-th of the property
    /// templates its `Template` constructor sorted with the *i*-th value
    /// slot, so the two agree only when the on-disk name order already is
    /// that sort order. froe's own reader pairs on-disk position with
    /// on-disk position and cannot see a mismatch, so the assertion is on
    /// the **template record's name order** — which is what Oak sorts —
    /// beside the values read back by name.
    #[test]
    fn a_node_written_out_of_order_is_stored_in_template_order() {
        use crate::content::property::PropertyType;

        let mut writer = new_writer();
        // `title` hashes above `active` (negative) and above `count`, so
        // the UTF-8 name order this list is in — active, count, title — is
        // also the template order; `zz` and `Aa` are what make them differ.
        let mut properties = Vec::new();
        for (name, text) in [
            ("zz", "last by name"),
            ("Aa", "collides with BB"),
            ("active", "true"),
        ] {
            let identifier = writer.write_string(text).expect("write a value");
            properties.push(PropertyToWrite {
                name: name.to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Single(identifier),
            });
        }
        let node = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &properties,
            )
            .expect("write the node");
        let store = writer.finish().expect("finish");

        let state = NodeState::new(&store, node);
        let template = state.template().expect("read the template");
        // `active` hashes negative, `Aa` is 2112, `zz` is 3904.
        assert_eq!(
            template
                .properties
                .iter()
                .map(|property| property.name.as_str())
                .collect::<Vec<_>>(),
            ["active", "Aa", "zz"],
            "the stored name order is the order Oak's own Template sorts into"
        );
        // And each name still resolves to its own value, which is the
        // claim the order exists to protect.
        for (name, text) in [
            ("zz", "last by name"),
            ("Aa", "collides with BB"),
            ("active", "true"),
        ] {
            assert_eq!(
                state.property(name).expect("read").expect("present").values,
                PropertyValues::Single(PropertyValue::String(text.to_owned())),
                "{name} resolves to its own value"
            );
        }
    }

    #[test]
    fn template_property_sort_orders_by_signed_hash_then_name_then_type() {
        use crate::content::property::PropertyType;
        use crate::segment::identifier::SegmentIdentifier;
        use crate::segment::record::RecordIdentifier;
        use crate::writer::record_writer::sort_properties_for_template;

        let property = |name: &str, property_type: PropertyType| PropertyToWrite {
            name: name.to_owned(),
            property_type,
            values: PropertyValuesToWrite::Single(RecordIdentifier::new(
                SegmentIdentifier::new(0, 0xA000_0000_0000_0001),
                0,
            )),
        };

        // Java hashes (signed): active = -1422950650, count = 94851343,
        // title = 110371416 — the negative hash must sort first, which an
        // unsigned comparison would get wrong. "Aa" and "BB" collide
        // (2112), so their tie breaks by name; two "count" entries tie on
        // hash and name, so their tie breaks by type tag (STRING=1 before
        // LONG=3).
        let mut properties = vec![
            property("title", PropertyType::String),
            property("BB", PropertyType::Long),
            property("count", PropertyType::Long),
            property("count", PropertyType::String),
            property("Aa", PropertyType::String),
            property("active", PropertyType::Boolean),
        ];
        sort_properties_for_template(&mut properties);
        let names_and_types: Vec<(&str, PropertyType)> = properties
            .iter()
            .map(|property| (property.name.as_str(), property.property_type))
            .collect();
        assert_eq!(
            names_and_types,
            [
                ("active", PropertyType::Boolean),
                ("Aa", PropertyType::String),
                ("BB", PropertyType::Long),
                ("count", PropertyType::String),
                ("count", PropertyType::Long),
                ("title", PropertyType::String),
            ]
        );
    }
}
