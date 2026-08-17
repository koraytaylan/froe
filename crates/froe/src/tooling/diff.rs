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
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::record::RecordIdentifier;
use crate::store::{ArchiveSet, open_all_archives_with_progress};

/// The total node pairs one diff may visit. A depth bound alone cannot
/// stop corrupt records shaped as a wide DAG, whose distinct paths grow
/// exponentially while staying shallow; real diffs — even a whole-tree
/// diff across a compaction — stay far below this.
const MAXIMUM_DIFF_VISITS: u64 = 1_000_000_000;

/// How many node pairs the walk compares between progress reports.
const DIFF_VISIT_REPORT_STRIDE: u64 = 512;

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
    diff_revisions_with_progress(
        directory,
        before_revision,
        after_revision,
        filter_path,
        &mut DiscardedProgress,
    )
}

/// Compares exactly like [`diff_revisions`], reporting the archive scan
/// and the comparison walk to `observer`.
pub fn diff_revisions_with_progress(
    directory: &std::path::Path,
    before_revision: &str,
    after_revision: &str,
    filter_path: &str,
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<NodeDifference>> {
    let mut differences = Vec::new();
    diff_revisions_visiting(
        directory,
        before_revision,
        after_revision,
        filter_path,
        observer,
        &mut |difference| differences.push(difference),
    )?;
    Ok(differences)
}

/// Compares exactly like [`diff_revisions_with_progress`], handing each
/// difference to `emit` as the walk finds it instead of collecting them.
///
/// Prefer this wherever differences are folded into something smaller than
/// themselves. The collecting form holds the entire change set, and each
/// entry carries full before and after property state, so a diff against a
/// stale base — a refresh weeks behind, a repository-wide property rewrite —
/// buffers a result the caller was only going to reduce anyway.
pub fn diff_revisions_visiting(
    directory: &std::path::Path,
    before_revision: &str,
    after_revision: &str,
    filter_path: &str,
    observer: &mut dyn ProgressObserver,
    emit: &mut dyn FnMut(NodeDifference),
) -> Result<()> {
    let archives = open_all_archives_with_progress(directory, observer)?;
    let provider = ArchiveSet::new(archives);

    let before_head = resolve_revision(directory, before_revision, &provider)?;
    let after_head = resolve_revision(directory, after_revision, &provider)?;

    let before_node = content_node_at(&provider, before_head, filter_path)?;
    let after_node = content_node_at(&provider, after_head, filter_path)?;

    let base_path = normalized_filter_path(filter_path);
    let mut visits = 0u64;
    crate::progress::observe(
        observer,
        &Step::new("comparing revisions", WorkUnit::Nodes),
        |observer| {
            let compared = diff_nodes(
                &provider,
                before_node.as_ref(),
                after_node.as_ref(),
                &base_path,
                emit,
                &mut visits,
                observer,
            );
            // The stride suppressed the last partial batch; report the
            // exact number of pairs the walk reached, failure included.
            observer.step_advanced(visits);
            compared
        },
    )?;
    Ok(())
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

/// One pair of records on the current path, so a self-referential graph is
/// refused exactly instead of walked until a work budget notices.
type PairOnPath = (Option<RecordIdentifier>, Option<RecordIdentifier>);

/// One unit of diff work.
enum DiffWork<'provider> {
    /// Compare this pair and schedule its children.
    Visit {
        before: Option<NodeState<'provider>>,
        after: Option<NodeState<'provider>>,
        path: String,
    },
    /// The pair's subtree finished; take it off the ancestor set.
    Leave(PairOnPath),
}

/// Diffs two node states, handing each difference to `emit`.
///
/// The walk carries its own stack on the heap and imposes no depth limit:
/// depth is a property of the repositories being compared, not something
/// this code may choose. Two mutually recursive frames per level used to
/// exhaust an ordinary thread stack long before any bound could refuse.
fn diff_nodes<'provider>(
    provider: &'provider dyn SegmentProvider,
    before: Option<&NodeState<'provider>>,
    after: Option<&NodeState<'provider>>,
    path: &str,
    emit: &mut dyn FnMut(NodeDifference),
    visits: &mut u64,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let mut ancestors: std::collections::HashSet<PairOnPath> = std::collections::HashSet::new();
    let mut stack = vec![DiffWork::Visit {
        before: before.copied(),
        after: after.copied(),
        path: path.to_owned(),
    }];

    while let Some(work) = stack.pop() {
        let (before, after, path) = match work {
            DiffWork::Leave(pair) => {
                ancestors.remove(&pair);
                continue;
            }
            DiffWork::Visit {
                before,
                after,
                path,
            } => (before, after, path),
        };
        // Reported before the increment: the pairs behind this one are the
        // ones actually compared, and this one has not been yet.
        if (*visits).is_multiple_of(DIFF_VISIT_REPORT_STRIDE) {
            observer.step_advanced(*visits);
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
            (None, Some(_)) => emit(NodeDifference::NodeAdded {
                path: display_path(&path),
            }),
            (Some(_), None) => emit(NodeDifference::NodeRemoved {
                path: display_path(&path),
            }),
            (Some(before_node), Some(after_node)) => {
                // Identical records mean an identical subtree — skip it whole.
                if before_node.record_identifier() == after_node.record_identifier() {
                    continue;
                }
                let pair = (
                    Some(before_node.record_identifier()),
                    Some(after_node.record_identifier()),
                );
                // A pair reachable from itself is corruption in one of the
                // two stores, refused at the records that close the cycle.
                if !ancestors.insert(pair) {
                    return Err(crate::error::Error::InvalidFormat {
                        details: format!(
                            "node records {} and {} are contained in their own subtree; \
                             the records form a cycle",
                            pair.0.expect("a matched pair"),
                            pair.1.expect("a matched pair"),
                        ),
                    });
                }
                diff_properties(provider, &before_node, &after_node, &path, emit)?;
                stack.push(DiffWork::Leave(pair));
                // Pushed in reverse so they pop in the order the recursive
                // walk visited them, keeping the emitted sequence identical.
                for child in child_pairs_to_visit(&before_node, &after_node, &path)?
                    .into_iter()
                    .rev()
                {
                    stack.push(child);
                }
            }
        }
    }
    Ok(())
}

/// The child pairs of one matched node pair, in the order the diff reports
/// them: every child of `after` (added or possibly changed), then the
/// children only `before` has (removed).
fn child_pairs_to_visit<'provider>(
    before: &NodeState<'provider>,
    after: &NodeState<'provider>,
    path: &str,
) -> Result<Vec<DiffWork<'provider>>> {
    let before_children = before.child_node_entries()?;
    let after_children = after.child_node_entries()?;
    let before_by_name: std::collections::HashMap<&str, &NodeState<'provider>> = before_children
        .iter()
        .map(|(name, node)| (name.as_str(), node))
        .collect();

    let mut pairs = Vec::with_capacity(after_children.len());
    for (name, after_child) in &after_children {
        pairs.push(DiffWork::Visit {
            before: before_by_name.get(name.as_str()).map(|node| **node),
            after: Some(*after_child),
            path: format!("{path}/{name}"),
        });
    }
    let after_names: std::collections::HashSet<&str> = after_children
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    for (name, before_child) in &before_children {
        if !after_names.contains(name.as_str()) {
            pairs.push(DiffWork::Visit {
                before: Some(*before_child),
                after: None,
                path: format!("{path}/{name}"),
            });
        }
    }
    Ok(pairs)
}

/// Diffs the properties of two nodes.
fn diff_properties(
    provider: &dyn SegmentProvider,
    before: &NodeState<'_>,
    after: &NodeState<'_>,
    path: &str,
    emit: &mut dyn FnMut(NodeDifference),
) -> Result<()> {
    let before_properties = before.properties()?;
    let after_properties = after.properties()?;

    for after_property in &after_properties {
        match before_properties
            .iter()
            .find(|property| property.name == after_property.name)
        {
            None => emit(NodeDifference::PropertyChanged {
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
                emit(NodeDifference::PropertyChanged {
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
            emit(NodeDifference::PropertyChanged {
                path: display_path(path),
                change: PropertyChange::Removed(before_property.clone()),
            });
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
        assert!(store.compare_and_set_head(previous, head));
        store.close().expect("close");
        format!("{}:{}", head.segment, head.record_number as i32)
    }

    /// Writes a revision whose content is one chain `levels` deep, with the
    /// deepest node carrying `title`, and returns its revision string.
    fn write_deep_revision(directory: &std::path::Path, title: &str, levels: usize) -> String {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let value = writer.write_string(title).expect("value");
        let mut node = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "title".to_owned(),
                    property_type: crate::content::property::PropertyType::String,
                    values: PropertyValuesToWrite::Single(value),
                }],
            )
            .expect("deepest");
        for _ in 0..levels {
            node = writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &ChildNodesToWrite::One {
                        name: "down".to_owned(),
                        node,
                    },
                    &[],
                )
                .expect("level");
        }
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node,
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
        assert!(store.compare_and_set_head(previous, head));
        store.close().expect("close");
        format!("{}:{}", head.segment, head.record_number as i32)
    }

    #[test]
    fn a_tree_deeper_than_any_call_stack_diffs_whole() {
        // On the 2 MiB stack a spawned thread gets by default. The diff used
        // to recurse twice a level and overflowed here around 1600.
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let directory = TestDirectory::new("deep-chain");
                let before = write_deep_revision(&directory.path, "original", 60_000);
                let after = write_deep_revision(&directory.path, "changed", 60_000);
                let differences = diff_revisions(&directory.path, &before, &after, "/")
                    .expect("a deep diff completes rather than aborting");
                // The chains are identical except for the title on the
                // deepest node, so exactly one difference is correct — and
                // reaching it means the walk descended the whole chain and
                // built the path the whole way down.
                let [NodeDifference::PropertyChanged { path, .. }] = differences.as_slice() else {
                    panic!("expected one property change, got {differences:?}");
                };
                assert_eq!(
                    path.matches('/').count(),
                    60_001,
                    "the change is reported at the bottom of the chain, at {path}"
                );
            })
            .expect("spawn");
        handle.join().expect("the walk stays off the call stack");
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
        assert!(store.compare_and_set_head(previous, head));
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
