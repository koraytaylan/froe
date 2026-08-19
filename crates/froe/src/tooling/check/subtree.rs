//! Verifying a whole subtree: an explicit stack over the node graph, a
//! cache of the descendants already proven, and the exact path of the
//! first corrupt record.

use super::{
    BinaryCheck, DiscardedProgress, Error, HashSet, NodeState, PackedRecordSet, ProgressObserver,
    RecordIdentifier, Result, SegmentProvider, StrideCounter, check_node_shallow, display_relative,
};
use crate::content::node::PropertyState;

/// Receives every distinct node a verification walk certifies, exactly once
/// per walk, with the properties the walk already decoded for its checks.
///
/// The walk skips subtrees its certificate memo already holds, so a node is
/// observed under the path of its *first* visit and never again — which is
/// exactly the deduplication a census wants, and the reason a collector's
/// memory is bounded by distinct content rather than by path multiplicity.
/// Like a [`ProgressObserver`], an implementation is strictly additive: it
/// can never influence the walk, and it must not mutate the repository.
pub(crate) trait VerifiedContentObserver {
    /// One node passed its shallow checks. `path` is relative to the walked
    /// root — empty for the root itself — and `properties` is the node's
    /// full decoded property list, synthesized type properties included.
    /// The provider is the one the walk reads through, so a collector can
    /// resolve what a property references — binary block lists above all —
    /// without a provider of its own.
    fn node_verified(
        &mut self,
        provider: &dyn SegmentProvider,
        path: &str,
        node: &NodeState<'_>,
        properties: &[PropertyState],
    );
}

/// The observer that ignores every node, for walks that only verify.
pub(crate) struct DiscardedVerifiedContent;

impl VerifiedContentObserver for DiscardedVerifiedContent {
    fn node_verified(
        &mut self,
        _provider: &dyn SegmentProvider,
        _path: &str,
        _node: &NodeState<'_>,
        _properties: &[PropertyState],
    ) {
    }
}

/// A corrupt location inside a checked subtree: the relative path of the
/// node where verification failed (empty for the checked node itself)
/// and the reason.
pub(crate) struct CorruptLocation {
    pub(crate) path: String,
    pub(crate) reason: String,
}

/// Verifies one complete node subtree, including every stable-identifier
/// record and inline binary.
///
/// `root` is the exact node record to check; the provider may be a full
/// repository or any other segment source. External binaries are verified
/// only as references because their content is outside the segment store.
/// A corrupt root or descendant is reported as [`Error::InvalidFormat`]
/// whose details include its path relative to `root` (`/` denotes `root`
/// itself).
/// This deliberately classifies every traversal failure as invalid repository
/// data so path context is retained; the original error text is preserved in
/// the details even when the underlying failure was I/O or a missing segment.
pub fn verify_node_tree(provider: &dyn SegmentProvider, root: RecordIdentifier) -> Result<()> {
    NodeTreeVerifier::new(provider).verify(root)
}

/// A provider-bound verifier which reuses certificates for fully verified
/// immutable node subtrees across multiple roots.
///
/// Segment providers expose immutable record bytes, so a node that completed
/// every shallow, stable-identifier, inline-binary, and descendant check can
/// be safely reused while this verifier remains bound to the same provider.
/// Failed and cyclic subtrees are never certified.
pub struct NodeTreeVerifier<'provider> {
    pub(crate) provider: &'provider dyn SegmentProvider,
    /// Records whose subtree completed every check this walk performs.
    ///
    /// Exact and non-evicting, because a miss here is not an optimization
    /// question: re-walking a shared subtree re-walks every miss inside it,
    /// and the walk reports one node per certificate it issues, so an
    /// evicting memo made both the running time and the reported node count
    /// functions of the cache size rather than of the store. A 58 GB AEM
    /// repository whose head holds 18,796,598 nodes was reported as
    /// 56,389,743 — one full walk per root the super-root reaches — because
    /// the byte budget held a sixth of the tree.
    pub(crate) verified_subtrees: PackedRecordSet,
    /// Nodes certified across every `verify_with_progress` call, so a caller
    /// verifying several roots inside one reported step sees one running
    /// total rather than a count that restarts per root.
    pub(crate) verified_nodes: u64,
}

impl<'provider> NodeTreeVerifier<'provider> {
    /// Binds a reusable verifier to one immutable segment provider.
    #[must_use]
    pub fn new(provider: &'provider dyn SegmentProvider) -> Self {
        Self {
            provider,
            verified_subtrees: PackedRecordSet::new(),
            verified_nodes: 0,
        }
    }

    /// Distinct node records this verifier has certified across every call.
    ///
    /// Exact by construction: the walk counts where it issues a certificate,
    /// and `verify_with_progress` asserts the two agree.
    #[must_use]
    pub fn verified_nodes(&self) -> u64 {
        self.verified_nodes
    }

    /// Verifies `root`, reusing only subtrees which a previous call completed
    /// successfully against this same provider.
    pub fn verify(&mut self, root: RecordIdentifier) -> Result<()> {
        self.verify_with_progress(root, &mut DiscardedProgress)
    }

    /// Verifies exactly like [`NodeTreeVerifier::verify`], reporting the
    /// number of *distinct* nodes certified so far to `observer`. A node
    /// reached again through a second parent, a second root, or a second call
    /// is never counted twice.
    ///
    /// # Panics
    ///
    /// Panics if a completed walk reported a different number of nodes than
    /// the number of certificates it issued. That is a logic error in this
    /// module rather than a property of the repository, and it is the defect
    /// the exact certificate set exists to make impossible.
    pub fn verify_with_progress(
        &mut self,
        root: RecordIdentifier,
        observer: &mut dyn ProgressObserver,
    ) -> Result<()> {
        self.verify_collecting_with_progress(root, &mut DiscardedVerifiedContent, observer)
    }

    /// Verifies exactly like [`NodeTreeVerifier::verify_with_progress`],
    /// handing every first-visit node to `content` with the properties the
    /// checks already decoded. A collector therefore sees each distinct node
    /// exactly once per verifier — the walk's certificate memo is its
    /// deduplication — at no additional decoding cost.
    ///
    /// # Panics
    ///
    /// Panics on the same certificate-count divergence as
    /// [`NodeTreeVerifier::verify_with_progress`].
    pub(crate) fn verify_collecting_with_progress(
        &mut self,
        root: RecordIdentifier,
        content: &mut dyn VerifiedContentObserver,
        observer: &mut dyn ProgressObserver,
    ) -> Result<()> {
        self.verify_collecting_pruned_with_progress(root, content, &[], observer)
    }

    /// Verifies exactly like
    /// [`NodeTreeVerifier::verify_collecting_with_progress`], never
    /// descending into the subtrees whose exact root-relative paths are
    /// listed. The pruned subtrees stay uncertified and unobserved, so a
    /// caller can walk them afterwards with a different collector and
    /// still have every record certified exactly once across its calls.
    ///
    /// # Panics
    ///
    /// Panics on the same certificate-count divergence as
    /// [`NodeTreeVerifier::verify_with_progress`].
    pub(crate) fn verify_collecting_pruned_with_progress(
        &mut self,
        root: RecordIdentifier,
        content: &mut dyn VerifiedContentObserver,
        pruned_subtree_paths: &[&str],
        observer: &mut dyn ProgressObserver,
    ) -> Result<()> {
        let mut progress = VerifiedNodeCount::resuming(observer, self.verified_nodes);
        let verified = verify_subtree_pruned_with_cache(
            self.provider,
            root,
            SubtreeChecks {
                binaries: BinaryCheck::EveryBlock,
                stable_identifiers: true,
            },
            &mut self.verified_subtrees,
            &mut progress,
            content,
            pruned_subtree_paths,
        );
        progress.finish();
        self.verified_nodes = progress.completed();
        // The reported number is the certificate count by construction, the
        // same coupling that makes compaction's copied-node count provable.
        // Asserted rather than argued, and only after a successful walk: a
        // failed one stops mid-tree with certificates it never issued.
        if verified.is_ok() {
            assert_eq!(
                self.verified_subtrees.len() as u64,
                self.verified_nodes,
                "the reported node count diverged from the number of certified records"
            );
        }
        verified.map_err(|corrupt| node_tree_error(&corrupt))
    }
}

pub(crate) fn node_tree_error(corrupt: &CorruptLocation) -> Error {
    Error::InvalidFormat {
        details: format!(
            "node tree verification failed at {}: {}",
            display_relative(&corrupt.path),
            corrupt.reason
        ),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SubtreeChecks {
    pub(crate) binaries: BinaryCheck,
    pub(crate) stable_identifiers: bool,
}

/// Traverses a subtree, resolving every node, property, and — when asked
/// — binary content and stable identifier. Returns the first corrupt
/// location, which the caller remembers and re-probes at older revisions.
pub(crate) fn verify_subtree(
    provider: &dyn SegmentProvider,
    root: RecordIdentifier,
    checks: SubtreeChecks,
    verified: &mut PackedRecordSet,
    progress: &mut VerifiedNodeCount<'_>,
) -> std::result::Result<(), CorruptLocation> {
    verify_subtree_with_cache(
        provider,
        root,
        checks,
        verified,
        progress,
        &mut DiscardedVerifiedContent,
    )
}

/// Counts verified nodes for a [`ProgressObserver`], reporting on a stride
/// so a million-node tree does not become a million observer calls.
pub(crate) struct VerifiedNodeCount<'observer> {
    pub(crate) observer: &'observer mut dyn ProgressObserver,
    pub(crate) counter: StrideCounter,
}

impl<'observer> VerifiedNodeCount<'observer> {
    pub(in crate::tooling) fn new(observer: &'observer mut dyn ProgressObserver) -> Self {
        Self::resuming(observer, 0)
    }

    /// A counter continuing from `already`, so a step that verifies
    /// several roots keeps one running total instead of restarting — and
    /// the second root, which the subtree cache makes cheaper than the
    /// first, cannot report a smaller number than the first did.
    pub(in crate::tooling) fn resuming(
        observer: &'observer mut dyn ProgressObserver,
        already: u64,
    ) -> Self {
        Self {
            observer,
            counter: StrideCounter::resuming(VERIFIED_NODE_REPORT_STRIDE, already),
        }
    }

    /// How many nodes this counter has seen, including the ones it
    /// resumed from.
    pub(in crate::tooling) fn completed(&self) -> u64 {
        self.counter.completed()
    }

    pub(in crate::tooling) fn advance(&mut self) {
        self.counter.advance(self.observer);
    }

    /// Reports the exact number of nodes resolved, including the last
    /// partial stride.
    pub(in crate::tooling) fn finish(&mut self) {
        self.counter.finish(self.observer);
    }
}

/// How many nodes a verification walk resolves between progress reports.
pub(crate) const VERIFIED_NODE_REPORT_STRIDE: u64 = 512;

/// One suspended node: the children it has still to descend into, and
/// the path length to restore when it completes.
pub(crate) struct VerificationFrame {
    pub(crate) record: RecordIdentifier,
    pub(crate) pending_children: Vec<(String, RecordIdentifier)>,
    pub(crate) parent_path_length: usize,
}

/// Checks one node and enumerates its children. `None` means the memo
/// already holds it, so the subtree needs no walking.
pub(crate) fn open(
    provider: &dyn SegmentProvider,
    record: RecordIdentifier,
    checks: SubtreeChecks,
    verified: &PackedRecordSet,
    ancestors: &mut HashSet<RecordIdentifier>,
    path: &str,
    content: &mut dyn VerifiedContentObserver,
) -> std::result::Result<Option<Vec<(String, RecordIdentifier)>>, CorruptLocation> {
    let corrupt_here = |reason: String| CorruptLocation {
        path: path.to_owned(),
        reason,
    };
    // A node reachable from itself is corruption (valid records only
    // reference already-written records), and must fail the check —
    // whereas meeting an already-*completed* node again is ordinary
    // shared-subtree deduplication and verifies for free. Tested before
    // the memo so a cycle can never be served from it.
    if ancestors.contains(&record) {
        return Err(corrupt_here(format!(
            "node record {record} is contained in its own subtree"
        )));
    }
    if verified.contains(record) {
        return Ok(None);
    }
    ancestors.insert(record);
    // Not `map_err(corrupt_here)`: different clippy versions disagree
    // about the borrow there, and an explicit struct keeps both quiet.
    let properties = match check_node_shallow(provider, record, checks.binaries) {
        Ok(properties) => properties,
        Err(reason) => {
            return Err(CorruptLocation {
                path: path.to_owned(),
                reason,
            });
        }
    };
    let node = NodeState::new(provider, record);
    if checks.stable_identifiers
        && let Err(error) = node.stable_identifier_bytes()
    {
        return Err(CorruptLocation {
            path: path.to_owned(),
            reason: error.to_string(),
        });
    }
    // Observed only after every shallow check passed, so a collector never
    // counts content the walk is about to refuse.
    content.node_verified(provider, path, &node, &properties);
    let mut children: Vec<(String, RecordIdentifier)> = node
        .child_node_entries()
        .map_err(|error| corrupt_here(error.to_string()))?
        .into_iter()
        .map(|(name, child)| (name, child.record_identifier()))
        .collect();
    // Reversed so `pop` yields enumeration order.
    children.reverse();
    Ok(Some(children))
}

/// Verifies a subtree. A record enters `verified` only after every
/// descendant completed successfully.
///
/// The walk carries its own stack on the heap, so how deep a tree it can
/// verify is bounded by memory rather than by the thread it runs on. There is
/// no depth limit: depth is a property of the repository, not something this
/// code may choose. Termination on a self-referential record graph is the
/// exact `ancestors` set.
pub(crate) fn verify_subtree_with_cache(
    provider: &dyn SegmentProvider,
    root: RecordIdentifier,
    checks: SubtreeChecks,
    verified: &mut PackedRecordSet,
    progress: &mut VerifiedNodeCount<'_>,
    content: &mut dyn VerifiedContentObserver,
) -> std::result::Result<(), CorruptLocation> {
    verify_subtree_pruned_with_cache(provider, root, checks, verified, progress, content, &[])
}

/// Verifies exactly like [`verify_subtree_with_cache`], never descending
/// into the subtrees whose exact root-relative paths are listed — which
/// therefore also stay uncertified and unobserved, for a caller that walks
/// them separately with a different collector.
#[allow(
    clippy::too_many_arguments,
    reason = "the walk's one loop needs every one of these, and a builder for a crate-private function would only move the list"
)]
pub(crate) fn verify_subtree_pruned_with_cache(
    provider: &dyn SegmentProvider,
    root: RecordIdentifier,
    checks: SubtreeChecks,
    verified: &mut PackedRecordSet,
    progress: &mut VerifiedNodeCount<'_>,
    content: &mut dyn VerifiedContentObserver,
    pruned_subtree_paths: &[&str],
) -> std::result::Result<(), CorruptLocation> {
    let mut ancestors = HashSet::new();
    let mut path = String::new();
    let Some(children) = open(
        provider,
        root,
        checks,
        verified,
        &mut ancestors,
        &path,
        content,
    )?
    else {
        return Ok(());
    };
    let mut stack = vec![VerificationFrame {
        record: root,
        pending_children: children,
        parent_path_length: 0,
    }];

    loop {
        let next = stack
            .last_mut()
            .expect("the loop returns before the stack empties")
            .pending_children
            .pop();
        if let Some((name, child)) = next {
            let parent_path_length = path.len();
            path.push('/');
            path.push_str(&name);
            if pruned_subtree_paths.contains(&path.as_str()) {
                path.truncate(parent_path_length);
                continue;
            }
            match open(
                provider,
                child,
                checks,
                verified,
                &mut ancestors,
                &path,
                content,
            )? {
                Some(children) => stack.push(VerificationFrame {
                    record: child,
                    pending_children: children,
                    parent_path_length,
                }),
                None => path.truncate(parent_path_length),
            }
            continue;
        }
        let finished = stack.pop().expect("a frame was just inspected");
        ancestors.remove(&finished.record);
        // Counted here rather than where the node was opened: what an
        // operator reads is then the number of records this walk certified,
        // and a node re-reached through a second parent or a second root is
        // certified — and so counted — exactly once.
        verified.insert(finished.record);
        progress.advance();
        if stack.is_empty() {
            return Ok(());
        }
        path.truncate(finished.parent_path_length);
    }
}

/// Resolves and reads every block of an inline binary without holding the
/// content in memory (a multi-gigabyte binary must not have to fit);
/// external binaries have no local content to check.
pub(crate) fn materialize_binary(
    provider: &dyn SegmentProvider,
    value: &crate::content::property::PropertyValue,
) -> Result<()> {
    use crate::content::property::PropertyValue;
    use crate::content::value::BinaryValue;
    if let PropertyValue::Binary(BinaryValue::Inline {
        record_identifier, ..
    }) = value
    {
        crate::content::value::verify_binary_content(provider, *record_identifier)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{NodeTreeVerifier, verify_node_tree};
    use crate::content::node::NodeState;
    use crate::content::provider::SegmentProvider;
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::error::Error;
    use crate::progress::{ProgressObserver, Step};
    use crate::segment::record::RecordIdentifier;
    use crate::tooling::check::test_support::{CountingProvider, HidingProvider, TestDirectory};
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::segment_builder::SegmentBufferBuilder;
    use crate::writer::store_writer::WritableRepository;

    #[test]
    fn node_tree_verifier_reports_the_corrupt_descendant_path() {
        let directory = TestDirectory::new("verify-corrupt-path");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        // Finish the child first so it occupies a different segment from
        // the parent. The wrapped provider can then make only that child
        // unavailable while leaving the parent perfectly readable.
        let mut child_writer = store.record_writer(generation);
        let child = child_writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("child");
        child_writer.finish().expect("finish child");
        let mut root_writer = store.record_writer(generation);
        let root = root_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "broken".to_owned(),
                    node: child,
                },
                &[],
            )
            .expect("root");
        root_writer.finish().expect("finish root");

        verify_node_tree(&store, root).expect("the complete tree verifies");
        let provider = HidingProvider {
            store: &store,
            exact: Some(child.segment),
            bulk: false,
        };
        let error = verify_node_tree(&provider, root).expect_err("hidden child must fail");
        let Error::InvalidFormat { details } = error else {
            panic!("verification must return a structured format error");
        };
        assert!(
            details.contains("at /broken:"),
            "the error identifies the corrupt relative path: {details}"
        );
        assert!(
            details.contains(&child.segment.to_string()),
            "the underlying failure remains useful: {details}"
        );

        let provider = HidingProvider {
            store: &store,
            exact: Some(root.segment),
            bulk: false,
        };
        let error = verify_node_tree(&provider, root).expect_err("hidden root must fail");
        let Error::InvalidFormat { details } = error else {
            panic!("verification must return a structured format error");
        };
        assert!(
            details.contains("at /:"),
            "root corruption uses the documented root path: {details}"
        );
        store.close().expect("close");
    }

    #[test]
    fn reusable_node_tree_verifier_reuses_fully_verified_shared_descendants() {
        let directory = TestDirectory::new("verify-shared-cache");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        let mut child_writer = store.record_writer(generation);
        let shared = child_writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("shared child");
        child_writer.finish().expect("finish shared child");

        let mut first_writer = store.record_writer(generation);
        let first_root = first_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "shared".to_owned(),
                    node: shared,
                },
                &[],
            )
            .expect("first root");
        first_writer.finish().expect("finish first root");

        let mut second_writer = store.record_writer(generation);
        let second_root = second_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "shared".to_owned(),
                    node: shared,
                },
                &[],
            )
            .expect("second root");
        second_writer.finish().expect("finish second root");

        let provider = CountingProvider::new(&store);
        let mut verifier = NodeTreeVerifier::new(&provider);
        verifier.verify(first_root).expect("first tree verifies");
        let shared_reads = provider.reads_of(shared.segment);
        assert!(shared_reads > 0, "the first root traverses the shared node");
        assert!(
            verifier.verified_subtrees.contains(shared),
            "only a completed shared subtree receives a certificate"
        );

        verifier.verify(second_root).expect("second tree verifies");
        assert_eq!(
            provider.reads_of(shared.segment),
            shared_reads,
            "the second root reuses the provider-bound subtree certificate"
        );
        store.close().expect("close");
    }

    /// Counts an observer's highest reported completion for one step.
    #[derive(Default)]
    struct HighestReportedCount {
        highest: u64,
    }

    impl ProgressObserver for HighestReportedCount {
        fn step_began(&mut self, _step: &Step<'_>) {}

        fn step_advanced(&mut self, completed: u64) {
            assert!(
                completed >= self.highest,
                "a running total must never go backwards: {completed} after {}",
                self.highest
            );
            self.highest = completed;
        }

        fn step_ended(&mut self) {}
    }

    /// The distinct node records a root reaches, walked with a plain
    /// `HashSet` that shares no code with the verifier's certificate set.
    fn distinct_nodes_below(
        provider: &dyn SegmentProvider,
        roots: &[RecordIdentifier],
    ) -> std::collections::HashSet<RecordIdentifier> {
        let mut seen = std::collections::HashSet::new();
        let mut pending: Vec<RecordIdentifier> = roots.to_vec();
        while let Some(record) = pending.pop() {
            if !seen.insert(record) {
                continue;
            }
            for (_, child) in NodeState::new(provider, record)
                .child_node_entries()
                .expect("enumerate the child nodes")
            {
                pending.push(child.record_identifier());
            }
        }
        seen
    }

    /// The production shape that inflated the reported count: a super-root
    /// whose `root` child and whose checkpoint snapshot roots are distinct
    /// records sharing nearly every descendant. An evicting memo re-walked
    /// the shared content once per root and counted it again each time — a
    /// 58 GB repository whose head held 18,796,598 nodes reported 56,389,743.
    #[test]
    fn a_super_root_with_checkpoint_snapshots_counts_each_distinct_node_once() {
        let directory = TestDirectory::new("verify-exact-count");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        // A content tree wide and deep enough that no plausible memo ceiling
        // could have held all of it.
        let mut writer = store.record_writer(generation);
        let mut level = Vec::new();
        for leaf in 0..64 {
            let value = writer
                .write_string(&format!("leaf-{leaf}"))
                .expect("leaf value");
            let node = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "data".to_owned(),
                        property_type: crate::content::property::PropertyType::String,
                        values: PropertyValuesToWrite::Single(value),
                    }],
                )
                .expect("leaf node");
            level.push((format!("leaf{leaf}"), node));
        }
        let mut content_root = writer
            .write_node(None, &[], &ChildNodesToWrite::Many(level), &[])
            .expect("content root");
        for depth in 0..8 {
            content_root = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: format!("level{depth}"),
                        node: content_root,
                    },
                    &[],
                )
                .expect("branch node");
        }

        // Two checkpoint snapshots over the same content root, each reached
        // through its own record, exactly as a live super-root holds them.
        let mut snapshots = Vec::new();
        for snapshot in 0..2 {
            snapshots.push((
                format!("checkpoint{snapshot}"),
                writer
                    .write_node(
                        None,
                        &[],
                        &ChildNodesToWrite::One {
                            name: "root".to_owned(),
                            node: content_root,
                        },
                        &[],
                    )
                    .expect("checkpoint snapshot"),
            ));
        }
        let checkpoints = writer
            .write_node(None, &[], &ChildNodesToWrite::Many(snapshots), &[])
            .expect("checkpoint container");
        let super_root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("root".to_owned(), content_root),
                    ("checkpoints".to_owned(), checkpoints),
                ]),
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");

        let expected = distinct_nodes_below(&store, &[super_root]).len();
        assert!(
            expected > 64,
            "the fixture must be large enough to matter, got {expected} nodes"
        );

        let mut reported = HighestReportedCount::default();
        NodeTreeVerifier::new(&store)
            .verify_with_progress(super_root, &mut reported)
            .expect("the super root verifies");
        assert_eq!(
            reported.highest, expected as u64,
            "every distinct node is reported once, however many roots reach it"
        );
        store.close().expect("close");
    }

    #[test]
    fn reusable_node_tree_verifier_never_caches_a_failed_subtree() {
        let directory = TestDirectory::new("verify-failed-not-cached");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        let mut child_writer = store.record_writer(generation);
        let child = child_writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("child");
        child_writer.finish().expect("finish child");
        let mut root_writer = store.record_writer(generation);
        let root = root_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "broken".to_owned(),
                    node: child,
                },
                &[],
            )
            .expect("root");
        root_writer.finish().expect("finish root");

        let provider = CountingProvider::hiding(&store, child.segment);
        let mut verifier = NodeTreeVerifier::new(&provider);
        let first = verifier.verify(root).expect_err("hidden child fails");
        let first_reads = provider.reads_of(child.segment);
        assert!(first_reads > 0);
        assert!(
            verifier.verified_subtrees.len() == 0,
            "neither the corrupt child nor its incomplete ancestor is cached"
        );
        let second = verifier
            .verify(root)
            .expect_err("the same hidden child must be re-read and fail again");
        assert!(provider.reads_of(child.segment) > first_reads);
        assert_eq!(first.to_string(), second.to_string());
        assert_eq!(verifier.verified_subtrees.len(), 0);
        store.close().expect("close");
    }

    #[test]
    fn reusable_node_tree_verifier_never_caches_a_cyclic_subtree() {
        let directory = TestDirectory::new("verify-cycle-not-cached");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let original_child = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("original child");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "loop".to_owned(),
                    node: original_child,
                },
                &[],
            )
            .expect("root");
        writer.finish().expect("finish segment");

        let view = store.segment(root.segment).expect("root segment");
        let root_position = view
            .record_position(root.record_number)
            .expect("root position");
        let mut cyclic_bytes = view.bytes.to_vec();
        let child_slot: &mut [u8; 6] = (&mut cyclic_bytes[root_position + 12..root_position + 18])
            .try_into()
            .expect("one child identifier slot");
        SegmentBufferBuilder::write_record_identifier_bytes(0, root.record_number, child_slot);
        let mut memory = MemorySegmentProvider::default();
        memory.insert(root.segment, cyclic_bytes);

        let provider = CountingProvider::new(&memory);
        let mut verifier = NodeTreeVerifier::new(&provider);
        let first = verifier.verify(root).expect_err("self-cycle fails");
        let first_reads = provider.reads_of(root.segment);
        let Error::InvalidFormat { details } = &first else {
            panic!("cycle verification returns a format error");
        };
        assert!(details.contains("at /loop:"));
        assert!(details.contains("contained in its own subtree"));
        assert_eq!(verifier.verified_subtrees.len(), 0);

        let second = verifier.verify(root).expect_err("cycle is never cached");
        assert!(provider.reads_of(root.segment) > first_reads);
        assert_eq!(first.to_string(), second.to_string());
        assert_eq!(verifier.verified_subtrees.len(), 0);
        store.close().expect("close");
    }

    #[test]
    fn node_tree_verifier_materializes_long_inline_binary_blocks() {
        let directory = TestDirectory::new("verify-inline-binary");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");
        let content: Vec<u8> = (0..300 * 1024).map(|index| (index % 251) as u8).collect();
        let mut writer = store.record_writer(generation);
        let binary = writer.write_binary_content(&content).expect("binary");
        let payload = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "data".to_owned(),
                    property_type: crate::content::property::PropertyType::Binary,
                    values: PropertyValuesToWrite::Single(binary),
                }],
            )
            .expect("payload");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "payload".to_owned(),
                    node: payload,
                },
                &[],
            )
            .expect("root");
        writer.finish().expect("finish");

        verify_node_tree(&store, root).expect("complete binary verifies");
        let provider = HidingProvider {
            store: &store,
            exact: None,
            bulk: true,
        };
        let error = verify_node_tree(&provider, root).expect_err("missing block must fail");
        let Error::InvalidFormat { details } = error else {
            panic!("verification must return a structured format error");
        };
        assert!(
            details.contains("at /payload:"),
            "binary corruption is attributed to its containing node: {details}"
        );
        assert!(
            details.contains("not found in any archive"),
            "the missing block reason is retained: {details}"
        );
        store.close().expect("close");
    }

    #[test]
    fn node_tree_verifier_resolves_preserved_stable_identifiers() {
        let directory = TestDirectory::new("verify-stable-identifier");
        let store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");

        let mut child_writer = store.record_writer(generation);
        let child = child_writer
            .write_node_with_stable_identifier(
                None,
                &[],
                &ChildNodesToWrite::Zero,
                &[],
                Some([0x5a; 20]),
            )
            .expect("child");
        child_writer.finish().expect("finish child");
        let mut root_writer = store.record_writer(generation);
        let root = root_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "stable".to_owned(),
                    node: child,
                },
                &[],
            )
            .expect("root");
        root_writer.finish().expect("finish root");
        verify_node_tree(&store, root).expect("valid stable identifier verifies");

        // Keep both node-shaped segments intact except for the child's slot
        // zero. Point that slot at a nonexistent record in its own segment:
        // templates, properties, and children still decode, so only a
        // verifier that resolves stable identifiers detects this corruption.
        let child_view = store.segment(child.segment).expect("child segment");
        let mut child_bytes = child_view.bytes.to_vec();
        let child_position = child_view
            .record_position(child.record_number)
            .expect("child position");
        child_bytes[child_position..child_position + 2].copy_from_slice(&0u16.to_be_bytes());
        child_bytes[child_position + 2..child_position + 6]
            .copy_from_slice(&u32::MAX.to_be_bytes());
        let root_view = store.segment(root.segment).expect("root segment");
        let mut provider = MemorySegmentProvider::default();
        provider.insert(child.segment, child_bytes);
        provider.insert(root.segment, root_view.bytes.to_vec());

        let error = verify_node_tree(&provider, root).expect_err("invalid stable id must fail");
        let Error::InvalidFormat { details } = error else {
            panic!("verification must return a structured format error");
        };
        assert!(
            details.contains("at /stable:"),
            "stable-id corruption is attributed to its node: {details}"
        );
        assert!(
            details.contains("record 4294967295 does not exist"),
            "the stable-id failure reason is retained: {details}"
        );
        store.close().expect("close");
    }
}
