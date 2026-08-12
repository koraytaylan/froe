//! Depth-first content tree traversal.
//!
//! [`DepthFirstTraversal`] visits a subtree in document order — each node
//! before its children, children in storage order — and hands every
//! consumer the same hardening: an explicit stack instead of recursion,
//! so tree depth is bounded by memory rather than stack size; a depth
//! limit that turns node cycles in corrupt repositories into errors; and
//! a node budget that stops corrupt records shaped as a wide DAG, whose
//! distinct paths grow exponentially while staying shallow.
//!
//! The traversal maintains one shared path buffer, so visiting a node
//! allocates nothing beyond its child list:
//!
//! ```no_run
//! use froe::content::traversal::DepthFirstTraversal;
//! use froe::store::Repository;
//!
//! fn main() -> froe::Result<()> {
//!     let repository = Repository::open(std::path::Path::new("/path/to/segmentstore"))?;
//!     if let Some(root) = repository.node_at_path("/content")? {
//!         let mut traversal = DepthFirstTraversal::new(root, "/content", None);
//!         while let Some(visited) = traversal.next_node()? {
//!             println!("{} ({} properties)", visited.path, visited.node.properties()?.len());
//!         }
//!     }
//!     Ok(())
//! }
//! ```

use crate::content::node::NodeState;
use crate::content::path::normalized_path;
use crate::error::{Error, Result};

/// Nodes deeper than this indicate a cycle in a corrupt repository;
/// real content trees are nowhere near this deep.
const MAXIMUM_TRAVERSAL_DEPTH: usize = 16_384;

/// The total nodes one traversal may visit. A depth bound alone cannot
/// stop corrupt records shaped as a wide DAG, whose distinct paths grow
/// exponentially while staying shallow; real repositories stay far below
/// this.
const MAXIMUM_TRAVERSAL_NODES: u64 = 1_000_000_000;

/// One unit of traversal work.
enum WorkItem<'provider> {
    /// Visit this node and schedule its children.
    Visit {
        node: NodeState<'provider>,
        name: String,
        depth: usize,
    },
    /// Restore the path buffer after a subtree completes.
    RestorePathLength(usize),
}

#[derive(Clone, Copy)]
struct SchedulingLimits {
    children: u64,
    child_name_bytes: u64,
    work: u64,
    pending_nodes: u64,
}

/// One visited node, valid until the traversal advances.
pub struct VisitedNode<'traversal, 'provider> {
    /// The node's content path.
    pub path: &'traversal str,
    /// The node itself.
    pub node: NodeState<'provider>,
    /// How many levels below the traversal root the node sits.
    pub depth: usize,
}

/// One visited node plus the resource accounting for scheduling its children.
///
/// Keeping this separate from [`VisitedNode`] preserves that type's compact,
/// destructurable compatibility surface while making the bounded traversal
/// and its typed budget errors available to custom diagnostic callers.
#[non_exhaustive]
pub struct BoundedVisitedNode<'traversal, 'provider> {
    /// The node, path, and depth returned by an ordinary traversal.
    pub visited: VisitedNode<'traversal, 'provider>,
    /// Children scheduled by this step.
    pub scheduled_children: u64,
    /// Stored child-name bytes materialized by this step.
    pub scheduled_child_name_bytes: u64,
    /// Child-map records inspected by this step.
    pub scheduled_child_map_records: u64,
}

/// A depth-first walk over a subtree, in document order.
pub struct DepthFirstTraversal<'provider> {
    stack: Vec<WorkItem<'provider>>,
    path_buffer: String,
    descent_limit: Option<usize>,
    visited_nodes: u64,
    pending_nodes: u64,
}

impl<'provider> DepthFirstTraversal<'provider> {
    /// Creates a traversal of the subtree rooted at `root`, whose content
    /// path is `root_path` (any spelling; it is normalized). A descent
    /// limit of `Some(limit)` visits nodes at most `limit` levels below
    /// the root; `None` visits the whole subtree.
    #[must_use]
    pub fn new(root: NodeState<'provider>, root_path: &str, descent_limit: Option<usize>) -> Self {
        let mut path_buffer = normalized_path(root_path);
        if path_buffer == "/" {
            path_buffer.clear();
        }
        Self {
            stack: vec![WorkItem::Visit {
                node: root,
                name: String::new(),
                depth: 0,
            }],
            path_buffer,
            descent_limit,
            visited_nodes: 0,
            pending_nodes: 1,
        }
    }

    /// Advances to the next node in document order, or `Ok(None)` when
    /// the subtree is exhausted. The returned [`VisitedNode`] borrows the
    /// traversal's path buffer, so it lives until the next call.
    pub fn next_node(&mut self) -> Result<Option<VisitedNode<'_, 'provider>>> {
        Ok(self
            .next_node_internal(None)?
            .map(|bounded| bounded.visited))
    }

    /// Advances with independent per-node child-count and stored-name-byte
    /// caps, plus a combined scheduling-work cap checked before expansion.
    pub fn next_node_with_scheduling_limits(
        &mut self,
        maximum_scheduled_children: u64,
        maximum_scheduled_child_name_bytes: u64,
        maximum_scheduling_work: u64,
        maximum_pending_nodes: u64,
    ) -> Result<Option<BoundedVisitedNode<'_, 'provider>>> {
        self.next_node_internal(Some(SchedulingLimits {
            children: maximum_scheduled_children,
            child_name_bytes: maximum_scheduled_child_name_bytes,
            work: maximum_scheduling_work,
            pending_nodes: maximum_pending_nodes,
        }))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the traversal step keeps path restoration and bounded child scheduling in one state transition"
    )]
    fn next_node_internal(
        &mut self,
        limits: Option<SchedulingLimits>,
    ) -> Result<Option<BoundedVisitedNode<'_, 'provider>>> {
        loop {
            let Some(item) = self.stack.pop() else {
                return Ok(None);
            };
            let (node, name, depth) = match item {
                WorkItem::RestorePathLength(length) => {
                    self.path_buffer.truncate(length);
                    continue;
                }
                WorkItem::Visit { node, name, depth } => {
                    self.pending_nodes = self.pending_nodes.saturating_sub(1);
                    (node, name, depth)
                }
            };
            if depth >= MAXIMUM_TRAVERSAL_DEPTH {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "content tree exceeds depth {MAXIMUM_TRAVERSAL_DEPTH}; \
                         the node records probably form a cycle"
                    ),
                });
            }
            self.visited_nodes += 1;
            if self.visited_nodes > MAXIMUM_TRAVERSAL_NODES {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "traversal exceeds {MAXIMUM_TRAVERSAL_NODES} nodes; \
                         the node records probably form a pathological graph"
                    ),
                });
            }

            if !name.is_empty() {
                self.stack
                    .push(WorkItem::RestorePathLength(self.path_buffer.len()));
                self.path_buffer.push('/');
                self.path_buffer.push_str(&name);
            }

            let descend = match self.descent_limit {
                Some(limit) => depth < limit,
                None => true,
            };
            let (scheduled_children, scheduled_child_name_bytes, scheduled_child_map_records) =
                if descend {
                    let (child_entries, child_count, child_name_bytes, child_map_records) =
                        if let Some(limits) = limits {
                            let (child_count, count_map_records) = node
                                .child_node_count_with_maximum_work(limits.work)
                                .map_err(|error| match error {
                                    Error::MapTraversalWorkBudgetExceeded {
                                        attempted_work_units,
                                        ..
                                    } => Error::TraversalSchedulingWorkBudgetExceeded {
                                        maximum_scheduling_work: limits.work,
                                        attempted_scheduling_work: attempted_work_units,
                                    },
                                    other => other,
                                })?;
                            if child_count > limits.children {
                                return Err(Error::TraversalSchedulingBudgetExceeded {
                                    maximum_scheduled_children: limits.children,
                                    attempted_scheduled_children: child_count,
                                });
                            }
                            let reserved_work = child_count.saturating_add(count_map_records);
                            if reserved_work > limits.work {
                                return Err(Error::TraversalSchedulingWorkBudgetExceeded {
                                    maximum_scheduling_work: limits.work,
                                    attempted_scheduling_work: reserved_work,
                                });
                            }
                            let remaining_work = limits.work - reserved_work;
                            let maximum_name_bytes = limits.child_name_bytes.min(remaining_work);
                            let (entries, name_bytes, enumeration_map_records) = node
                                .child_node_entries_with_limits(
                                    child_count,
                                    maximum_name_bytes,
                                    remaining_work,
                                )
                                .map_err(|error| match error {
                                    Error::StringMaterializationBudgetExceeded {
                                        maximum_stored_bytes,
                                        attempted_stored_bytes,
                                        ..
                                    } => Error::TraversalChildNameBudgetExceeded {
                                        maximum_stored_child_name_bytes: maximum_stored_bytes,
                                        attempted_stored_child_name_bytes: attempted_stored_bytes,
                                        scheduled_children: child_count,
                                    },
                                    Error::MapTraversalWorkBudgetExceeded {
                                        attempted_work_units,
                                        ..
                                    } => Error::TraversalSchedulingWorkBudgetExceeded {
                                        maximum_scheduling_work: limits.work,
                                        attempted_scheduling_work: reserved_work
                                            .saturating_add(attempted_work_units),
                                    },
                                    Error::MapEntryBudgetExceeded {
                                        maximum_entries,
                                        attempted_entries,
                                    } => Error::InvalidFormat {
                                        details: format!(
                                            "child map declared {maximum_entries} entries but \
                                             enumerated at least {attempted_entries}"
                                        ),
                                    },
                                    other => other,
                                })?;
                            let enumerated_children =
                                u64::try_from(entries.len()).unwrap_or(u64::MAX);
                            if enumerated_children != child_count {
                                return Err(Error::InvalidFormat {
                                    details: format!(
                                        "child map declared {child_count} entries but enumerated \
                                         {enumerated_children}"
                                    ),
                                });
                            }
                            (
                                entries,
                                child_count,
                                name_bytes,
                                count_map_records.saturating_add(enumeration_map_records),
                            )
                        } else {
                            let entries = node.child_node_entries()?;
                            let child_count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
                            (entries, child_count, 0, 0)
                        };

                    let attempted_pending_nodes = self
                        .pending_nodes
                        .saturating_add(u64::try_from(child_entries.len()).unwrap_or(u64::MAX));
                    if let Some(limits) = limits
                        && attempted_pending_nodes > limits.pending_nodes
                    {
                        return Err(Error::TraversalPendingBudgetExceeded {
                            maximum_pending_nodes: limits.pending_nodes,
                            attempted_pending_nodes,
                        });
                    }
                    self.pending_nodes = attempted_pending_nodes;
                    // Push children in reverse so they pop in storage order.
                    for (child_name, child) in child_entries.into_iter().rev() {
                        self.stack.push(WorkItem::Visit {
                            node: child,
                            name: child_name,
                            depth: depth + 1,
                        });
                    }
                    (child_count, child_name_bytes, child_map_records)
                } else {
                    (0, 0, 0)
                };

            let path = if self.path_buffer.is_empty() {
                "/"
            } else {
                self.path_buffer.as_str()
            };
            return Ok(Some(BoundedVisitedNode {
                visited: VisitedNode { path, node, depth },
                scheduled_children,
                scheduled_child_name_bytes,
                scheduled_child_map_records,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DepthFirstTraversal;
    use crate::content::provider::tests::{CountingSegmentProvider, MemorySegmentProvider};
    use crate::content::template::tests::{TemplateArity, template_record};
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;
    use crate::store::Repository;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-traversal-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Writes a store whose content tree is `/content/{a, b/c}`.
    fn populate(directory: &std::path::Path) {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let leaf_a = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("a");
        let leaf_c = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("c");
        let branch_b = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::One {
                    name: "c".to_owned(),
                    node: leaf_c,
                },
                &[],
            )
            .expect("b");
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("a".to_owned(), leaf_a),
                    ("b".to_owned(), branch_b),
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
        assert!(store.set_head(previous, head));
        store.close().expect("close");
    }

    fn visited_paths(
        repository: &Repository,
        root_path: &str,
        descent_limit: Option<usize>,
    ) -> Vec<(String, usize)> {
        let root = repository
            .node_at_path(root_path)
            .expect("resolve")
            .expect("present");
        let mut traversal = DepthFirstTraversal::new(root, root_path, descent_limit);
        let mut visited = Vec::new();
        while let Some(visit) = traversal.next_node().expect("advance") {
            visited.push((visit.path.to_owned(), visit.depth));
        }
        visited
    }

    #[test]
    fn visits_nodes_in_document_order() {
        let directory = TestDirectory::new("document-order");
        populate(&directory.path);
        let repository = Repository::open(&directory.path).expect("open");

        // Sibling order under /content is the child map's storage order;
        // read it from the map rather than assuming an insertion order.
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        let mut expected = vec![("/".to_owned(), 0), ("/content".to_owned(), 1)];
        for (name, _) in content.child_node_entries().expect("children") {
            expected.push((format!("/content/{name}"), 2));
            if name == "b" {
                expected.push(("/content/b/c".to_owned(), 3));
            }
        }

        assert_eq!(visited_paths(&repository, "/", None), expected);
    }

    #[test]
    fn the_descent_limit_bounds_the_walk() {
        let directory = TestDirectory::new("descent-limit");
        populate(&directory.path);
        let repository = Repository::open(&directory.path).expect("open");

        assert_eq!(
            visited_paths(&repository, "/", Some(0)),
            [("/".to_owned(), 0)]
        );
        assert_eq!(
            visited_paths(&repository, "/", Some(1)),
            [("/".to_owned(), 0), ("/content".to_owned(), 1)]
        );
    }

    #[test]
    fn the_root_path_is_normalized() {
        let directory = TestDirectory::new("root-path");
        populate(&directory.path);
        let repository = Repository::open(&directory.path).expect("open");

        let visited = visited_paths(&repository, "/content//b/", None);
        assert_eq!(
            visited,
            [("/content/b".to_owned(), 0), ("/content/b/c".to_owned(), 1)]
        );
    }

    #[test]
    fn a_node_cycle_fails_instead_of_walking_forever() {
        fn identifier_bytes(record_number: u32) -> [u8; 6] {
            let mut bytes = [0u8; 6];
            bytes[2..6].copy_from_slice(&record_number.to_be_bytes());
            bytes
        }

        let segment = data_segment_identifier(1);
        let mut child_name = vec![4u8]; // small string "self"
        child_name.extend_from_slice(b"self");
        // Record 30 is its own single child: template arity One pointing
        // back at record 30.
        let mut node = Vec::new();
        node.extend_from_slice(&identifier_bytes(30)); // stable identifier
        node.extend_from_slice(&identifier_bytes(21)); // template
        node.extend_from_slice(&identifier_bytes(30)); // child: itself
        let records: Vec<(u32, u8, Vec<u8>)> = vec![
            (1, 4, child_name),
            (
                21,
                6,
                template_record(None, &[], &TemplateArity::One(1), None, &[]),
            ),
            (30, 7, node),
        ];
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));

        let root =
            crate::content::node::NodeState::new(&provider, RecordIdentifier::new(segment, 30));
        let mut traversal = DepthFirstTraversal::new(root, "/", None);
        let error = loop {
            match traversal.next_node() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("a cyclic tree must not end cleanly"),
                Err(error) => break error,
            }
        };
        assert!(
            error.to_string().contains("cycle"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn ordinary_traversal_uses_the_provider_template_surface() {
        let identifier_bytes = |record_number: u32| {
            let mut bytes = [0u8; 6];
            bytes[2..6].copy_from_slice(&record_number.to_be_bytes());
            bytes
        };
        let segment = data_segment_identifier(2);
        let mut child_name = vec![5u8];
        child_name.extend_from_slice(b"child");
        let mut root = Vec::new();
        root.extend_from_slice(&identifier_bytes(30));
        root.extend_from_slice(&identifier_bytes(21));
        root.extend_from_slice(&identifier_bytes(31));
        let mut child = Vec::new();
        child.extend_from_slice(&identifier_bytes(31));
        child.extend_from_slice(&identifier_bytes(22));
        let records = vec![
            (1, 4, child_name),
            (
                21,
                6,
                template_record(None, &[], &TemplateArity::One(1), None, &[]),
            ),
            (
                22,
                6,
                template_record(None, &[], &TemplateArity::Zero, None, &[]),
            ),
            (30, 7, root),
            (31, 7, child),
        ];
        let mut inner = MemorySegmentProvider::default();
        inner.insert(segment, synthetic_data_segment(&[], &records));
        let provider = CountingSegmentProvider::new(&inner);
        let node =
            crate::content::node::NodeState::new(&provider, RecordIdentifier::new(segment, 30));
        let mut traversal = DepthFirstTraversal::new(node, "/", None);

        assert!(traversal.next_node().expect("advance").is_some());
        assert_eq!(provider.template_reads(), 1);
    }

    fn single_child_fixture() -> (MemorySegmentProvider, crate::segment::SegmentIdentifier) {
        let identifier_bytes = |record_number: u32| {
            let mut bytes = [0u8; 6];
            bytes[2..6].copy_from_slice(&record_number.to_be_bytes());
            bytes
        };
        let segment = data_segment_identifier(5);
        let mut child_name = vec![5u8];
        child_name.extend_from_slice(b"child");
        let mut root = Vec::new();
        root.extend_from_slice(&identifier_bytes(30));
        root.extend_from_slice(&identifier_bytes(21));
        root.extend_from_slice(&identifier_bytes(31));
        let mut child = Vec::new();
        child.extend_from_slice(&identifier_bytes(31));
        child.extend_from_slice(&identifier_bytes(22));
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, child_name),
                    (
                        21,
                        6,
                        template_record(None, &[], &TemplateArity::One(1), None, &[]),
                    ),
                    (
                        22,
                        6,
                        template_record(None, &[], &TemplateArity::Zero, None, &[]),
                    ),
                    (30, 7, root),
                    (31, 7, child),
                ],
            ),
        );
        (provider, segment)
    }

    #[test]
    fn bounded_traversal_guards_every_scheduling_resource_at_exact_thresholds() {
        let (provider, segment) = single_child_fixture();
        let root =
            || crate::content::node::NodeState::new(&provider, RecordIdentifier::new(segment, 30));

        let mut traversal = DepthFirstTraversal::new(root(), "/", None);
        assert!(matches!(
            traversal.next_node_with_scheduling_limits(0, u64::MAX, u64::MAX, u64::MAX),
            Err(crate::Error::TraversalSchedulingBudgetExceeded {
                maximum_scheduled_children: 0,
                attempted_scheduled_children: 1,
            })
        ));

        let mut traversal = DepthFirstTraversal::new(root(), "/", None);
        assert!(matches!(
            traversal.next_node_with_scheduling_limits(1, 4, u64::MAX, u64::MAX),
            Err(crate::Error::TraversalChildNameBudgetExceeded {
                maximum_stored_child_name_bytes: 4,
                attempted_stored_child_name_bytes: 5,
                scheduled_children: 1,
            })
        ));

        let mut traversal = DepthFirstTraversal::new(root(), "/", None);
        assert!(matches!(
            traversal.next_node_with_scheduling_limits(1, u64::MAX, 0, u64::MAX),
            Err(crate::Error::TraversalSchedulingWorkBudgetExceeded {
                maximum_scheduling_work: 0,
                attempted_scheduling_work: 1,
            })
        ));

        let mut traversal = DepthFirstTraversal::new(root(), "/", None);
        assert!(matches!(
            traversal.next_node_with_scheduling_limits(1, 5, 6, 0),
            Err(crate::Error::TraversalPendingBudgetExceeded {
                maximum_pending_nodes: 0,
                attempted_pending_nodes: 1,
            })
        ));

        let mut traversal = DepthFirstTraversal::new(root(), "/", None);
        let visited = traversal
            .next_node_with_scheduling_limits(1, 5, 6, 1)
            .expect("the exact scheduling limits fit")
            .expect("root visit");
        assert_eq!(visited.scheduled_children, 1);
        assert_eq!(visited.scheduled_child_name_bytes, 5);
        assert_eq!(visited.scheduled_child_map_records, 0);
    }
}
