//! Rewriting an index definition's bookkeeping on a store copy.
//!
//! The reindex oracle has to put a definition back into the state Oak found
//! it in, so froe rebuilds from the same starting point Oak did: `reindex`
//! flagged again, and `reindexCount` one below the value Oak left, so that
//! froe's own single increment lands back on exactly that value whatever
//! the attempt count was.
//!
//! Everything here goes through the public writer API — `write_node` for
//! the definition, `rewrite_node_with_child_edits` to splice it up the
//! spine — because a helper that reached into the encoder would be testing
//! froe against itself. Shared with plans 0008 and 0010, which need the
//! same reset for their own oracles.

use super::*;

/// One definition's bookkeeping, as it must be put back.
pub(crate) struct BookkeepingReset<'a> {
    /// The definition's name under `/oak:index`.
    pub(crate) name: &'a str,
    /// The `reindexCount` to store — Oak's value minus one.
    pub(crate) reindex_count: i64,
}

/// What an edit does to a definition beyond its `reindex` bookkeeping.
///
/// A struct rather than a run of booleans, because the callers combine
/// them: plan 0008's importer phase loses an index *and* flags it corrupt
/// to reach the remediation case the drift comparison has to accept.
#[derive(Clone, Copy, Default)]
pub(crate) struct DefinitionEdit {
    /// Remove every hidden child — the state an index has before anything
    /// indexed it, and the state a lost one leaves.
    pub(crate) drop_hidden: bool,
    /// Write `corrupt` as a `DATE`, exactly as Oak's async lane writes it
    /// when a cycle fails against an index, and as Oak's own cycle reads it
    /// converting before clearing it.
    pub(crate) flag_corrupt: bool,
    /// Remove `async`, which is what makes a definition synchronous.
    pub(crate) remove_async: bool,
}

/// Re-flags each named definition on `store` and sets its `reindexCount`.
///
/// Every other property and every hidden child is re-attached by identity:
/// the definition node is written afresh, but its children are the records
/// the store already holds, so the subtree below it is the same subtree.
pub(crate) fn reset_reindex_bookkeeping(store: &Path, definitions: &[BookkeepingReset<'_>]) {
    edit_definitions(store, definitions, DefinitionEdit::default());
}

/// The same, and additionally removes every hidden child of each named
/// definition — the state an index has before anything indexed it.
pub(crate) fn remove_hidden_children(store: &Path, definitions: &[BookkeepingReset<'_>]) {
    edit_definitions(
        store,
        definitions,
        DefinitionEdit {
            drop_hidden: true,
            ..DefinitionEdit::default()
        },
    );
}

/// The general form, for a caller that needs more than one of the edits.
pub(crate) fn edit_definitions(
    store: &Path,
    definitions: &[BookkeepingReset<'_>],
    edit: DefinitionEdit,
) {
    let writable = WritableRepository::open(store).expect("open the store for a definition edit");
    let generation = writable.writing_generation().expect("writing generation");
    let head = writable.head();
    let mut writer = writable.record_writer(generation);

    let repository = froe::Repository::open(store).expect("read the store");
    let mut index_edits = froe::writer::commit::ChildEdits::new();
    for definition in definitions {
        let path = format!("/oak:index/{}", definition.name);
        let node = repository
            .node_at_path(&path)
            .expect("resolve the definition")
            .unwrap_or_else(|| panic!("{path} is not in the store"));
        let record = rewrite_one_definition(&node, &mut writer, definition, edit);
        index_edits.insert(definition.name.to_owned(), Some(record));
    }

    let oak_index = repository
        .node_at_path("/oak:index")
        .expect("resolve /oak:index")
        .expect("/oak:index is in the store");
    let new_oak_index = rewrite_node_with_child_edits(
        &writable,
        &mut writer,
        Some(oak_index.record_identifier()),
        &index_edits,
    )
    .expect("rewrite /oak:index");

    let root = repository.content_root().expect("content root");
    let mut root_edits = froe::writer::commit::ChildEdits::new();
    root_edits.insert("oak:index".to_owned(), Some(new_oak_index));
    let new_root = rewrite_node_with_child_edits(
        &writable,
        &mut writer,
        Some(root.record_identifier()),
        &root_edits,
    )
    .expect("rewrite the root");

    let mut super_root_edits = froe::writer::commit::ChildEdits::new();
    super_root_edits.insert("root".to_owned(), Some(new_root));
    let new_head =
        rewrite_node_with_child_edits(&writable, &mut writer, Some(head), &super_root_edits)
            .expect("rewrite the super-root");

    drop(repository);
    writer.finish().expect("finish the definition edit");
    assert!(
        writable.compare_and_set_head(head, new_head),
        "nothing else is writing to this copy"
    );
    writable.close().expect("close the definition edit");
}

/// One definition node, rewritten with the bookkeeping Oak would have found.
fn rewrite_one_definition<Sink: SegmentSink>(
    node: &froe::content::node::NodeState<'_>,
    writer: &mut RecordWriter<Sink>,
    reset: &BookkeepingReset<'_>,
    edit: DefinitionEdit,
) -> RecordIdentifier {
    // The **stored** properties and the template's own primary type, never
    // `properties()` — which synthesizes `jcr:primaryType` from the
    // template. Writing that synthesized entry back as an ordinary
    // property would give the definition a shape no store Oak wrote has,
    // and the oracle would then be comparing froe's rebuild against a
    // fixture froe had already deformed.
    let template = node.template().expect("read the definition's template");
    let mut properties: Vec<PropertyToWrite> = Vec::new();
    for property in node
        .stored_properties()
        .expect("read the definition's properties")
    {
        if property.name == "reindex" || property.name == "reindexCount" {
            continue;
        }
        if edit.remove_async && property.name == "async" {
            continue;
        }
        properties.push(rewritten_property(&property, writer));
    }
    if edit.flag_corrupt {
        // A `DATE`, which is the type Oak's async lane writes and the type
        // its own cycle reads converting before clearing the flag.
        let flagged = writer
            .write_string("2026-09-14T12:00:00.000Z")
            .expect("write the corrupt flag");
        properties.push(PropertyToWrite {
            name: "corrupt".to_owned(),
            property_type: PropertyType::Date,
            values: PropertyValuesToWrite::Single(flagged),
        });
    }
    let flagged = writer.write_string("true").expect("write the flag");
    properties.push(PropertyToWrite {
        name: "reindex".to_owned(),
        property_type: PropertyType::Boolean,
        values: PropertyValuesToWrite::Single(flagged),
    });
    let count = writer
        .write_string(&reset.reindex_count.to_string())
        .expect("write the count");
    properties.push(PropertyToWrite {
        name: "reindexCount".to_owned(),
        property_type: PropertyType::Long,
        values: PropertyValuesToWrite::Single(count),
    });

    let children: Vec<(String, RecordIdentifier)> = node
        .child_node_entries()
        .expect("read the definition's children")
        .into_iter()
        .filter(|(name, _)| !(edit.drop_hidden && name.starts_with(':')))
        .map(|(name, child)| (name, child.record_identifier()))
        .collect();

    writer
        .write_node(
            template.primary_type.as_deref(),
            &template.mixin_types,
            &match children.as_slice() {
                [] => ChildNodesToWrite::Zero,
                [(name, record)] => ChildNodesToWrite::One {
                    name: name.clone(),
                    node: *record,
                },
                many => ChildNodesToWrite::Many(many.to_vec()),
            },
            &properties,
        )
        .expect("write the definition")
}

/// One existing property, re-written value for value with its stored type.
fn rewritten_property<Sink: SegmentSink>(
    property: &froe::PropertyState,
    writer: &mut RecordWriter<Sink>,
) -> PropertyToWrite {
    let write = |text: &str, writer: &mut RecordWriter<Sink>| {
        writer.write_string(text).expect("write a property value")
    };
    let values = match &property.values {
        froe::PropertyValues::Single(value) => PropertyValuesToWrite::Single(write(
            &value.as_text().expect("a non-binary property value"),
            writer,
        )),
        froe::PropertyValues::Multiple(values) => PropertyValuesToWrite::Multiple(
            values
                .iter()
                .map(|value| {
                    write(
                        &value.as_text().expect("a non-binary property value"),
                        writer,
                    )
                })
                .collect(),
        ),
    };
    PropertyToWrite {
        name: property.name.clone(),
        property_type: property.property_type,
        values,
    }
}
