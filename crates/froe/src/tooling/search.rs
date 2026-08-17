//! Node search: finding nodes by property, child, or value across every
//! data segment.
//!
//! `search_nodes` scans every node record in the store — not only those
//! reachable from the current head, so it finds nodes in old revisions
//! and orphaned by garbage — and reports those matching all of the
//! query's predicates.

use crate::content::node::{NodeState, PropertyValues};
use crate::content::property::PropertyValue;
use crate::content::provider::SegmentProvider;
use crate::error::Result;
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::store::{ArchiveSet, open_all_archives_with_progress};

/// The predicates a node must satisfy to match.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    /// Property names the node must have.
    pub has_properties: Vec<String>,
    /// Child names the node must have.
    pub has_children: Vec<String>,
    /// `(property name, string value)` pairs the node must carry (as a
    /// single value or a member of a multi-valued property).
    pub property_values: Vec<(String, String)>,
}

impl SearchQuery {
    /// Whether the query has any predicate.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.has_properties.is_empty()
            && self.has_children.is_empty()
            && self.property_values.is_empty()
    }
}

/// A matching node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeMatch {
    /// The node's record identifier.
    pub record: RecordIdentifier,
    /// The node's stable identifier.
    pub stable_identifier: String,
}

/// The result of a node search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    /// The matching nodes, in scan order.
    pub matches: Vec<NodeMatch>,
    /// How many node records could not be read and were skipped —
    /// widespread corruption must be visible, not silently absent from
    /// the results.
    pub unreadable_nodes: u64,
}

/// Searches every node record in the store for nodes matching `query`.
/// A limit of zero means no limit.
pub fn search_nodes(
    directory: &std::path::Path,
    query: &SearchQuery,
    limit: usize,
) -> Result<SearchOutcome> {
    search_nodes_with_progress(directory, query, limit, &mut DiscardedProgress)
}

/// Searches exactly like [`search_nodes`], reporting the archive scan and
/// the segment sweep to `observer`. A search that stops at `limit` ends
/// its step where it stopped, not at the segment total.
pub fn search_nodes_with_progress(
    directory: &std::path::Path,
    query: &SearchQuery,
    limit: usize,
    observer: &mut dyn ProgressObserver,
) -> Result<SearchOutcome> {
    let mut matches = Vec::new();
    let unreadable_nodes = search_nodes_visiting(directory, query, observer, &mut |found| {
        matches.push(found);
        if limit != 0 && matches.len() >= limit {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    })?;
    Ok(SearchOutcome {
        matches,
        unreadable_nodes,
    })
}

/// Searches exactly like [`search_nodes_with_progress`], handing each match
/// to `visit` as it is found instead of collecting them.
///
/// This is the form to prefer for anything that consumes matches one at a
/// time — printing them, counting them, streaming them onward. The collecting
/// form has to hold every match until the scan ends, and the scan covers every
/// node record in the store including dead revisions, so a broad predicate on
/// a large repository buffers far more than the caller ever needed at once.
/// Returning [`std::ops::ControlFlow::Break`] stops the scan.
///
/// Returns the number of node records that could not be read, the same count
/// [`SearchOutcome::unreadable_nodes`] carries.
pub fn search_nodes_visiting(
    directory: &std::path::Path,
    query: &SearchQuery,
    observer: &mut dyn ProgressObserver,
    visit: &mut dyn FnMut(NodeMatch) -> std::ops::ControlFlow<()>,
) -> Result<u64> {
    let archives = open_all_archives_with_progress(directory, observer)?;
    let provider = ArchiveSet::new(archives);

    let mut unreadable_nodes = 0u64;
    observer.step_began(
        &Step::new("searching segments", WorkUnit::Segments)
            .with_total(crate::progress::count(provider.segment_identifier_count())),
    );
    let mut searched_segments = 0usize;
    for (searched, segment_identifier) in provider.distinct_segment_identifiers().enumerate() {
        observer.step_advanced(crate::progress::count(searched));
        searched_segments = searched + 1;
        if segment_identifier.is_bulk_segment() {
            continue;
        }
        let Ok(view) = provider.segment(segment_identifier) else {
            continue;
        };
        // An unknown type byte means the record table itself is suspect;
        // Java's search dies on it, froe keeps scanning but the skipped
        // entries must be visible in the outcome.
        let mut node_records: Vec<u32> = Vec::new();
        for entry in view.structure.record_table() {
            match entry.record_type() {
                Some(RecordType::Node) => node_records.push(entry.record_number),
                Some(_) => {}
                None => unreadable_nodes += 1,
            }
        }
        for record_number in node_records {
            let record = RecordIdentifier::new(segment_identifier, record_number);
            match node_matches(&provider, record, query) {
                Ok(false) => {}
                Ok(true) => {
                    let stable_identifier = NodeState::new(&provider, record)
                        .stable_identifier()
                        .unwrap_or_else(|_| record.to_string());
                    let found = NodeMatch {
                        record,
                        stable_identifier,
                    };
                    if visit(found).is_break() {
                        observer.step_advanced(crate::progress::count(searched_segments));
                        observer.step_ended();
                        return Ok(unreadable_nodes);
                    }
                }
                Err(_) => unreadable_nodes += 1,
            }
        }
    }
    observer.step_advanced(crate::progress::count(searched_segments));
    observer.step_ended();
    Ok(unreadable_nodes)
}

/// Whether one node satisfies every predicate of the query.
fn node_matches(
    provider: &dyn SegmentProvider,
    record: RecordIdentifier,
    query: &SearchQuery,
) -> Result<bool> {
    let node = NodeState::new(provider, record);

    for child_name in &query.has_children {
        if node.child_node(child_name)?.is_none() {
            return Ok(false);
        }
    }

    if query.has_properties.is_empty() && query.property_values.is_empty() {
        return Ok(true);
    }
    let properties = node.properties()?;
    for property_name in &query.has_properties {
        if !properties
            .iter()
            .any(|property| property.name == *property_name)
        {
            return Ok(false);
        }
    }
    for (property_name, expected_value) in &query.property_values {
        let Some(property) = properties
            .iter()
            .find(|property| property.name == *property_name)
        else {
            return Ok(false);
        };
        if !property_has_string_value(&property.values, expected_value) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a property carries the given string value, as a single value
/// or a member of a multi-valued property.
fn property_has_string_value(values: &PropertyValues, expected: &str) -> bool {
    let matches_value =
        |value: &PropertyValue| value.as_text().is_some_and(|text| text == expected);
    match values {
        PropertyValues::Single(value) => matches_value(value),
        PropertyValues::Multiple(values) => values.iter().any(matches_value),
    }
}

#[cfg(test)]
mod tests {
    use super::{SearchQuery, search_nodes};
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-search-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn populate(directory: &std::path::Path) {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let marked_value = writer.write_string("target").expect("value");
        let marked = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "marker".to_owned(),
                    property_type: crate::content::property::PropertyType::String,
                    values: PropertyValuesToWrite::Single(marked_value),
                }],
            )
            .expect("marked");
        let plain = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("plain");
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("marked".to_owned(), marked),
                    ("plain".to_owned(), plain),
                ]),
                &[],
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
    }

    #[test]
    fn finds_nodes_by_property_presence() {
        let directory = TestDirectory::new("by-property");
        populate(&directory.path);
        let query = SearchQuery {
            has_properties: vec!["marker".to_owned()],
            ..SearchQuery::default()
        };
        let outcome = search_nodes(&directory.path, &query, 0).expect("search");
        assert_eq!(outcome.matches.len(), 1, "one node has the marker property");
        assert_eq!(outcome.unreadable_nodes, 0);
    }

    #[test]
    fn finds_nodes_by_property_value() {
        let directory = TestDirectory::new("by-value");
        populate(&directory.path);
        let query = SearchQuery {
            property_values: vec![("marker".to_owned(), "target".to_owned())],
            ..SearchQuery::default()
        };
        assert_eq!(
            search_nodes(&directory.path, &query, 0)
                .expect("search")
                .matches
                .len(),
            1
        );

        let wrong_value = SearchQuery {
            property_values: vec![("marker".to_owned(), "other".to_owned())],
            ..SearchQuery::default()
        };
        assert!(
            search_nodes(&directory.path, &wrong_value, 0)
                .expect("search")
                .matches
                .is_empty()
        );
    }

    #[test]
    fn finds_nodes_by_child_presence() {
        let directory = TestDirectory::new("by-child");
        populate(&directory.path);
        let query = SearchQuery {
            has_children: vec!["marked".to_owned()],
            ..SearchQuery::default()
        };
        let outcome = search_nodes(&directory.path, &query, 0).expect("search");
        assert_eq!(
            outcome.matches.len(),
            1,
            "the content node has the marked child"
        );
    }

    #[test]
    fn the_limit_bounds_the_result_count() {
        let directory = TestDirectory::new("limit");
        populate(&directory.path);
        // Every node has nt:unstructured as its primary type, so all match.
        let query = SearchQuery {
            has_properties: vec!["jcr:primaryType".to_owned()],
            ..SearchQuery::default()
        };
        let limited = search_nodes(&directory.path, &query, 2).expect("search");
        assert_eq!(limited.matches.len(), 2, "the limit caps the results");
    }
}
