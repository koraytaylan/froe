//! Revision differencing: what changed between two content states.
//!
//! `diff_revisions` compares the content tree at two revisions and
//! reports added, changed, and removed nodes and properties. Nodes are
//! compared by record identifier first — an unchanged subtree shares its
//! record and is skipped in one step — so the diff visits only the
//! divergent spine, exactly as Oak's stable-identifier diff does.

use crate::content::node::{NodeState, PropertyState, PropertyValues};
use crate::content::provider::SegmentProvider;
use crate::error::Result;
use crate::journal::parse_record_identifier_text;
use crate::segment::record::RecordIdentifier;
use crate::store::{ArchiveSet, open_all_archives};

/// A content tree is never this deep; beyond it the node records form a
/// cycle in a corrupt store, and the diff walk stops.
const MAXIMUM_DIFF_DEPTH: usize = 4000;

/// The total node pairs one diff may visit. A depth bound alone cannot
/// stop corrupt records shaped as a wide DAG, whose distinct paths grow
/// exponentially while staying shallow; real diffs — even a whole-tree
/// diff across a compaction — stay far below this.
const MAXIMUM_DIFF_VISITS: u64 = 1_000_000_000;

/// A property-level change within a node.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyChange {
    /// A property present only in the newer state.
    Added(PropertyState),
    /// A property present only in the older state.
    Removed(PropertyState),
    /// A property whose value or type changed. The states carry the
    /// type as well as the values: a `String` retyped to a `Name`, or
    /// an empty `String[]` to an empty `Long[]`, changes no value bytes
    /// but is a change — the exported type column depends on it.
    Changed {
        /// The property in the older state.
        before: PropertyState,
        /// The property in the newer state.
        after: PropertyState,
    },
}

/// A node-level difference.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeDifference {
    /// A node added in the newer state (its whole subtree is new).
    NodeAdded {
        /// The node's path.
        path: String,
    },
    /// A node removed in the newer state.
    NodeRemoved {
        /// The node's path.
        path: String,
    },
    /// A property change on an existing node.
    PropertyChanged {
        /// The node's path.
        path: String,
        /// The change.
        change: PropertyChange,
    },
}

/// Computes the differences between two revisions of the content tree
/// under `filter_path`. The revisions are record identifier strings in
/// either journal form (`<uuid>:<decimal>`) or diagnostic form
/// (`<uuid>.<hex8>`); `"head"` resolves to the newest journal revision.
pub fn diff_revisions(
    directory: &std::path::Path,
    before_revision: &str,
    after_revision: &str,
    filter_path: &str,
) -> Result<Vec<NodeDifference>> {
    let archives = open_all_archives(directory)?;
    let provider = ArchiveSet::new(archives);

    let before_head = resolve_revision(directory, before_revision, &provider)?;
    let after_head = resolve_revision(directory, after_revision, &provider)?;

    let before_node = content_node_at(&provider, before_head, filter_path)?;
    let after_node = content_node_at(&provider, after_head, filter_path)?;

    let base_path = normalized_filter_path(filter_path);
    let mut differences = Vec::new();
    let mut visits = 0u64;
    diff_nodes(
        &provider,
        before_node.as_ref(),
        after_node.as_ref(),
        &base_path,
        0,
        &mut differences,
        &mut visits,
    )?;
    Ok(differences)
}

/// Resolves a revision string to a head record identifier. `"head"`
/// rewinds past journal lines whose segment is missing, exactly like the
/// repository open — a syntactically valid line pointing at a lost
/// segment must not shadow the newest revision that actually resolves.
fn resolve_revision(
    directory: &std::path::Path,
    revision: &str,
    provider: &ArchiveSet,
) -> Result<RecordIdentifier> {
    if revision.eq_ignore_ascii_case("head") {
        let entries = crate::journal::read_journal(&directory.join("journal.log"))?;
        return entries
            .iter()
            .filter_map(crate::journal::JournalEntry::record_identifier)
            .find(|identifier| provider.segment(identifier.segment).is_ok())
            .ok_or_else(|| crate::error::Error::InvalidFormat {
                details: "the journal has no resolvable head".to_owned(),
            });
    }
    parse_record_identifier_text(revision).ok_or_else(|| crate::error::Error::InvalidFormat {
        details: format!("{revision:?} is not a valid record identifier"),
    })
}

/// Navigates to `filter_path` under the content root of a super-root.
fn content_node_at<'provider>(
    provider: &'provider ArchiveSet,
    head: RecordIdentifier,
    filter_path: &str,
) -> Result<Option<NodeState<'provider>>> {
    let super_root = NodeState::new(provider, head);
    let Some(mut current) = super_root.child_node("root")? else {
        return Ok(None);
    };
    for name in filter_path.split('/').filter(|segment| !segment.is_empty()) {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// Normalizes a filter path to a leading-slash form for reporting.
fn normalized_filter_path(path: &str) -> String {
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        String::new()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Diffs two node states, appending differences.
#[allow(
    clippy::too_many_arguments,
    reason = "the walk threads its path, depth, output, and work budget"
)]
fn diff_nodes(
    provider: &dyn SegmentProvider,
    before: Option<&NodeState<'_>>,
    after: Option<&NodeState<'_>>,
    path: &str,
    depth: usize,
    differences: &mut Vec<NodeDifference>,
    visits: &mut u64,
) -> Result<()> {
    if depth > MAXIMUM_DIFF_DEPTH {
        return Err(crate::error::Error::InvalidFormat {
            details: format!(
                "node tree exceeds depth {MAXIMUM_DIFF_DEPTH}; the records probably form a cycle"
            ),
        });
    }
    *visits += 1;
    if *visits > MAXIMUM_DIFF_VISITS {
        return Err(crate::error::Error::InvalidFormat {
            details: format!(
                "diff exceeds {MAXIMUM_DIFF_VISITS} visited nodes; \
                 the records probably form a pathological graph"
            ),
        });
    }
    match (before, after) {
        (None, None) => {}
        (None, Some(_)) => differences.push(NodeDifference::NodeAdded {
            path: display_path(path),
        }),
        (Some(_), None) => differences.push(NodeDifference::NodeRemoved {
            path: display_path(path),
        }),
        (Some(before_node), Some(after_node)) => {
            // Identical records mean an identical subtree — skip it whole.
            if before_node.record_identifier() == after_node.record_identifier() {
                return Ok(());
            }
            diff_properties(provider, before_node, after_node, path, differences)?;
            diff_children(
                provider,
                before_node,
                after_node,
                path,
                depth,
                differences,
                visits,
            )?;
        }
    }
    Ok(())
}

/// Diffs the properties of two nodes.
fn diff_properties(
    provider: &dyn SegmentProvider,
    before: &NodeState<'_>,
    after: &NodeState<'_>,
    path: &str,
    differences: &mut Vec<NodeDifference>,
) -> Result<()> {
    let before_properties = before.properties()?;
    let after_properties = after.properties()?;

    for after_property in &after_properties {
        match before_properties
            .iter()
            .find(|property| property.name == after_property.name)
        {
            None => differences.push(NodeDifference::PropertyChanged {
                path: display_path(path),
                change: PropertyChange::Added(after_property.clone()),
            }),
            Some(before_property)
                if before_property.property_type != after_property.property_type
                    || !property_values_equal(
                        provider,
                        &before_property.values,
                        &after_property.values,
                    )? =>
            {
                differences.push(NodeDifference::PropertyChanged {
                    path: display_path(path),
                    change: PropertyChange::Changed {
                        before: before_property.clone(),
                        after: after_property.clone(),
                    },
                });
            }
            Some(_) => {}
        }
    }
    for before_property in &before_properties {
        if !after_properties
            .iter()
            .any(|property| property.name == before_property.name)
        {
            differences.push(NodeDifference::PropertyChanged {
                path: display_path(path),
                change: PropertyChange::Removed(before_property.clone()),
            });
        }
    }
    Ok(())
}

/// Diffs the children of two nodes.
#[allow(
    clippy::too_many_arguments,
    reason = "the walk threads its path, depth, output, and work budget"
)]
fn diff_children(
    provider: &dyn SegmentProvider,
    before: &NodeState<'_>,
    after: &NodeState<'_>,
    path: &str,
    depth: usize,
    differences: &mut Vec<NodeDifference>,
    visits: &mut u64,
) -> Result<()> {
    let before_children = before.child_node_entries()?;
    let after_children = after.child_node_entries()?;

    for (name, after_child) in &after_children {
        let before_child = before_children
            .iter()
            .find(|(before_name, _)| before_name == name)
            .map(|(_, node)| node);
        let child_path = format!("{path}/{name}");
        diff_nodes(
            provider,
            before_child,
            Some(after_child),
            &child_path,
            depth + 1,
            differences,
            visits,
        )?;
    }
    for (name, before_child) in &before_children {
        if !after_children
            .iter()
            .any(|(after_name, _)| after_name == name)
        {
            let child_path = format!("{path}/{name}");
            diff_nodes(
                provider,
                Some(before_child),
                None,
                &child_path,
                depth + 1,
                differences,
                visits,
            )?;
        }
    }
    Ok(())
}

/// Whether two property value sets are equal by *content*. Oak compares
/// binaries by bytes with a same-record fast path, so byte-identical
/// binaries that compaction rewrote to different records must not be
/// reported as changed. Everything else compares structurally.
fn property_values_equal(
    provider: &dyn SegmentProvider,
    before: &PropertyValues,
    after: &PropertyValues,
) -> Result<bool> {
    match (before, after) {
        (PropertyValues::Single(before_value), PropertyValues::Single(after_value)) => {
            property_value_equal(provider, before_value, after_value)
        }
        (PropertyValues::Multiple(before_values), PropertyValues::Multiple(after_values)) => {
            if before_values.len() != after_values.len() {
                return Ok(false);
            }
            for (before_value, after_value) in before_values.iter().zip(after_values) {
                if !property_value_equal(provider, before_value, after_value)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Whether two single property values are equal by content.
fn property_value_equal(
    provider: &dyn SegmentProvider,
    before: &crate::content::property::PropertyValue,
    after: &crate::content::property::PropertyValue,
) -> Result<bool> {
    use crate::content::property::PropertyValue;
    use crate::content::value::BinaryValue;
    if let (
        PropertyValue::Binary(BinaryValue::Inline {
            length: before_length,
            record_identifier: before_record,
        }),
        PropertyValue::Binary(BinaryValue::Inline {
            length: after_length,
            record_identifier: after_record,
        }),
    ) = (before, after)
    {
        if before_length != after_length {
            return Ok(false);
        }
        return crate::content::value::inline_binary_contents_equal(
            provider,
            *before_record,
            *after_record,
            *before_length,
        );
    }
    // Doubles compare like Java's Double.equals — by bits, so NaN equals
    // NaN and -0.0 differs from 0.0 — where the derived f64 equality
    // would report a NaN property as perpetually changed and a sign flip
    // of zero as unchanged.
    if let (PropertyValue::Double(before_value), PropertyValue::Double(after_value)) =
        (before, after)
    {
        return Ok(
            double_bits_for_equality(*before_value) == double_bits_for_equality(*after_value)
        );
    }
    Ok(before == after)
}

/// The bit pattern Java's `Double.equals` compares: `doubleToLongBits`,
/// which collapses every NaN payload to the canonical quiet NaN. Stored
/// text can only ever parse to the canonical NaN, but exactness costs
/// nothing.
fn double_bits_for_equality(value: f64) -> u64 {
    if value.is_nan() {
        0x7FF8_0000_0000_0000
    } else {
        value.to_bits()
    }
}

/// Renders a path, using `/` for the empty root path.
fn display_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeDifference, PropertyChange, diff_revisions};
    use crate::content::node::PropertyValues;
    use crate::content::property::PropertyValue;
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-diff-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Writes a `/content` node with the given title and child names,
    /// returns the head revision string.
    fn write_revision(directory: &std::path::Path, title: &str, children: &[&str]) -> String {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let mut child_nodes = Vec::new();
        for name in children {
            let child = writer
                .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
                .expect("child");
            child_nodes.push(((*name).to_owned(), child));
        }
        let value = writer.write_string(title).expect("value");
        let child_structure = if child_nodes.is_empty() {
            ChildNodesToWrite::Zero
        } else {
            ChildNodesToWrite::Many(child_nodes)
        };
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &child_structure,
                &[PropertyToWrite {
                    name: "title".to_owned(),
                    property_type: crate::content::property::PropertyType::String,
                    values: PropertyValuesToWrite::Single(value),
                }],
            )
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content,
                },
                &[],
            )
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        store.close().expect("close");
        format!("{}:{}", head.segment, head.record_number as i32)
    }

    #[test]
    fn detects_property_and_child_changes() {
        let directory = TestDirectory::new("changes");
        let before = write_revision(&directory.path, "original", &["alpha"]);
        let after = write_revision(&directory.path, "updated", &["alpha", "beta"]);

        let differences =
            diff_revisions(&directory.path, &before, &after, "/content").expect("diff");

        // The title changed.
        let title_changed = differences.iter().any(|difference| {
            matches!(
                difference,
                NodeDifference::PropertyChanged {
                    change: PropertyChange::Changed { before, after },
                    ..
                } if before.name == "title"
                    && before.values
                        == PropertyValues::Single(PropertyValue::String("original".to_owned()))
                    && after.values
                        == PropertyValues::Single(PropertyValue::String("updated".to_owned()))
            )
        });
        assert!(
            title_changed,
            "the title change is reported: {differences:?}"
        );

        // The "beta" child was added.
        let beta_added = differences.iter().any(|difference| {
            matches!(difference, NodeDifference::NodeAdded { path } if path == "/content/beta")
        });
        assert!(beta_added, "the added child is reported: {differences:?}");
    }

    #[test]
    fn identical_revisions_have_no_differences() {
        let directory = TestDirectory::new("identical");
        let revision = write_revision(&directory.path, "same", &["child"]);
        let differences =
            diff_revisions(&directory.path, &revision, &revision, "/content").expect("diff");
        assert!(differences.is_empty(), "no differences: {differences:?}");
    }

    /// Writes a `/content` node carrying one single-valued property
    /// (`single_prop = "x"`) and one empty multi-valued property
    /// (`multi_prop`), both of the given types.
    fn write_typed_revision(
        directory: &std::path::Path,
        single_type: crate::content::property::PropertyType,
        multiple_type: crate::content::property::PropertyType,
    ) -> String {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let single_value = writer.write_string("x").expect("value");
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[
                    PropertyToWrite {
                        name: "single_prop".to_owned(),
                        property_type: single_type,
                        values: PropertyValuesToWrite::Single(single_value),
                    },
                    PropertyToWrite {
                        name: "multi_prop".to_owned(),
                        property_type: multiple_type,
                        values: PropertyValuesToWrite::Multiple(Vec::new()),
                    },
                ],
            )
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content,
                },
                &[],
            )
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        store.close().expect("close");
        format!("{}:{}", head.segment, head.record_number as i32)
    }

    #[test]
    fn detects_type_only_property_changes() {
        use crate::content::property::PropertyType;
        let directory = TestDirectory::new("type-changes");
        let before =
            write_typed_revision(&directory.path, PropertyType::String, PropertyType::String);
        let after = write_typed_revision(&directory.path, PropertyType::Name, PropertyType::Long);

        let differences =
            diff_revisions(&directory.path, &before, &after, "/content").expect("diff");

        let type_of = |name: &str| {
            differences.iter().find_map(|difference| match difference {
                NodeDifference::PropertyChanged {
                    change: PropertyChange::Changed { before, after },
                    ..
                } if before.name == name => Some((before.property_type, after.property_type)),
                _ => None,
            })
        };
        assert_eq!(
            type_of("single_prop"),
            Some((PropertyType::String, PropertyType::Name)),
            "a same-bytes String-to-Name retype is reported: {differences:?}"
        );
        assert_eq!(
            type_of("multi_prop"),
            Some((PropertyType::String, PropertyType::Long)),
            "an empty String[]-to-Long[] retype is reported: {differences:?}"
        );
    }

    #[test]
    fn head_resolves_to_the_newest_revision() {
        let directory = TestDirectory::new("head");
        let before = write_revision(&directory.path, "first", &[]);
        write_revision(&directory.path, "second", &[]);
        let differences =
            diff_revisions(&directory.path, &before, "head", "/content").expect("diff");
        assert!(
            !differences.is_empty(),
            "the title differs between the revisions"
        );
    }
}
