//! The deep copy itself: an explicit heap stack over the source graph, so
//! tree depth is a property of the repository rather than a limit this
//! code imposes, and a cycle is refused at the record that closes it.

use super::{
    BinaryValue, BulkBlockSharing, ChildNodesToWrite, Error, NodeState, ProgressObserver,
    PropertyState, PropertyToWrite, PropertyType, PropertyValue, PropertyValues,
    PropertyValuesToWrite, RecordIdentifier, RecordWriter, Result, RewrittenNodes, SegmentInterner,
    SegmentProvider, SegmentSink, sort_properties_for_template,
};

/// How many nodes a deep copy rewrites between progress reports.
pub(crate) const COPIED_NODE_REPORT_STRIDE: u64 = 512;

/// Deep-copies nodes into a fresh generation, sharing rewritten records
/// through an exact source-record memo.
pub(crate) struct Compactor<'writer, Sink: SegmentSink> {
    pub(crate) source: &'writer dyn SegmentProvider,
    pub(crate) writer: &'writer mut RecordWriter<Sink>,
    /// Whether bulk blocks may be referenced in place or must be copied.
    pub(crate) bulk_sharing: BulkBlockSharing,
    /// Children of the super-root's `checkpoints` container this copy never
    /// enters. Empty for every copy that is not a maintenance run's.
    pub(crate) omitted_checkpoints: &'writer std::collections::BTreeSet<String>,
    /// Subtree roots this copy never enters *outside checkpoint snapshots*
    /// — the confirmed version-history purge. The checkpoint scoping is the
    /// point: a checkpoint's snapshot keeps its own version storage, so the
    /// same record reached through a retained checkpoint is still copied,
    /// and only the head loses the subtree. Empty for every copy that is
    /// not a purging maintenance run's.
    pub(crate) omitted_subtree_records:
        &'writer std::collections::HashSet<crate::segment::record::RecordIdentifier>,
    /// Records whose subtree contains an omission point — the ancestors on
    /// the path from the content root down to each omitted record. Their
    /// rewritten form depends on the scope: the head's copy loses the
    /// omitted subtrees, a checkpoint snapshot's copy keeps them. They are
    /// therefore memoized per scope rather than globally, so a copy shared
    /// through the memo can never leak one scope's shape into the other —
    /// whichever order the walk reaches them in.
    pub(crate) context_dependent_records:
        &'writer std::collections::HashSet<crate::segment::record::RecordIdentifier>,
    /// The per-scope memos for context-dependent records, keyed like the
    /// global memo: `[head scope, checkpoint scope]`.
    pub(crate) scoped_rewrites: [std::collections::HashMap<u64, u64>; 2],
    /// Interns the segments both the memo and the path set name.
    pub(crate) segments: SegmentInterner,
    /// Source record to its rewritten copy — exact, so each distinct node is
    /// copied once and `compacted_nodes` equals the distinct reachable count.
    pub(crate) rewritten_nodes: RewrittenNodes,
    /// The records currently being expanded — the ancestor path, packed the
    /// same way the memo packs. Exact and unbounded: it carries termination,
    /// so it is never budgeted. One entry per live level, not per node.
    pub(crate) nodes_on_path: std::collections::HashSet<u64>,
    pub(crate) compacted_nodes: u64,
    /// The count at the last progress report, so the observer is called
    /// once per stride rather than once per node.
    pub(crate) reported_nodes: u64,
    pub(crate) observer: &'writer mut dyn ProgressObserver,
}

/// The outcome of resolving one node reference.
pub(crate) enum Entered {
    /// Not yet copied; descend into this frame.
    Fresh(CompactionFrame),
    /// Already copied; the memo holds the rewritten record.
    Memoized(RecordIdentifier),
}

/// One suspended node in the deep copy: its children have to be rewritten
/// before it can be written itself, so a node is visited twice — once to
/// enumerate its children, once to emit it after they are all rewritten.
pub(crate) struct CompactionFrame {
    pub(crate) source: RecordIdentifier,
    pub(crate) packed: u64,
    /// The name this node has in its parent, so the rewritten record can be
    /// attached when the frame pops. `None` for the root.
    pub(crate) name_in_parent: Option<String>,
    /// Whether this node lies inside a checkpoint snapshot, where subtree
    /// omissions never apply.
    pub(crate) within_checkpoints: bool,
    /// Remaining children to descend into, in reverse order so the next one
    /// is a `pop`.
    pub(crate) pending_children: Vec<(String, RecordIdentifier)>,
    /// Children already rewritten, in enumeration order.
    pub(crate) rewritten_children: Vec<(String, RecordIdentifier)>,
}

impl<Sink: SegmentSink> Compactor<'_, Sink> {
    /// Rewrites `source_root` and everything it reaches.
    ///
    /// The walk carries its own stack on the heap, so how deep a tree it can
    /// copy is bounded by memory rather than by the thread it happens to run
    /// on. There is no depth limit: depth is a property of the repository,
    /// not something this code can choose, and a bound on it would refuse
    /// valid stores. Termination on a corrupt self-referential graph is
    /// `nodes_on_path`, which decides it exactly.
    pub(crate) fn compact_tree(
        &mut self,
        source_root: RecordIdentifier,
    ) -> Result<RecordIdentifier> {
        let mut stack = match self.enter(source_root, false)? {
            Entered::Fresh(root) => vec![root],
            Entered::Memoized(rewritten) => return Ok(rewritten),
        };

        loop {
            let next = stack
                .last_mut()
                .expect("the loop returns before the stack empties")
                .pending_children
                .pop();
            if let Some((name, child)) = next {
                let parent = stack.last().expect("the parent frame is on the stack");
                let within_checkpoints =
                    parent.within_checkpoints || (stack.len() == 1 && name == "checkpoints");
                // The purge: an omitted subtree root is simply never
                // entered, exactly like a retired checkpoint — except
                // inside a checkpoint snapshot, which keeps everything it
                // froze.
                if !within_checkpoints && self.omitted_subtree_records.contains(&child) {
                    continue;
                }
                match self.enter(child, within_checkpoints)? {
                    Entered::Fresh(mut frame) => {
                        // The `checkpoints` container directly under the
                        // super-root is the one node whose child *names* this
                        // copy reads. A retired checkpoint is dropped here,
                        // before the frame is pushed, so the walk never enters
                        // it. A memo hit cannot bypass this: the container is
                        // reached exactly once per copy, and `Memoized` means
                        // the node was already emitted — which for the
                        // container can only happen after this filter ran.
                        if stack.len() == 1
                            && name == "checkpoints"
                            && !self.omitted_checkpoints.is_empty()
                        {
                            frame.pending_children.retain(|(checkpoint, _)| {
                                !self.omitted_checkpoints.contains(checkpoint)
                            });
                        }
                        frame.name_in_parent = Some(name);
                        frame.within_checkpoints = within_checkpoints;
                        stack.push(frame);
                    }
                    Entered::Memoized(rewritten) => stack
                        .last_mut()
                        .expect("the parent frame is still on the stack")
                        .rewritten_children
                        .push((name, rewritten)),
                }
                continue;
            }
            let finished = stack.pop().expect("a frame was just inspected");
            let rewritten = self.emit(
                finished.source,
                finished.packed,
                finished.within_checkpoints,
                finished.rewritten_children,
            )?;
            match stack.last_mut() {
                Some(parent) => parent.rewritten_children.push((
                    finished
                        .name_in_parent
                        .expect("only the root frame has no name"),
                    rewritten,
                )),
                None => return Ok(rewritten),
            }
        }
    }

    /// Which memo serves a record: the global one for the shared majority,
    /// a per-scope one for the ancestors of omission points.
    fn scope_index(within_checkpoints: bool) -> usize {
        usize::from(within_checkpoints)
    }

    /// Resolves one node: either a memo already holds its rewritten copy,
    /// or a frame to descend into.
    pub(crate) fn enter(
        &mut self,
        source_node: RecordIdentifier,
        within_checkpoints: bool,
    ) -> Result<Entered> {
        let packed = self.segments.pack(source_node);
        if self.context_dependent_records.contains(&source_node) {
            if let Some(rewritten) = self.scoped_rewrites[Self::scope_index(within_checkpoints)]
                .get(&packed)
                .copied()
            {
                return Ok(Entered::Memoized(self.segments.unpack(rewritten)));
            }
        } else if let Some(rewritten) = self.rewritten_nodes.get(packed) {
            return Ok(Entered::Memoized(self.segments.unpack(rewritten)));
        }
        // A node reachable from itself is corruption — valid records only
        // reference already-written records — and is refused exactly, at the
        // record that closes the cycle. The memo cannot mask it: a memo hit
        // returns above, so a memoized node is never on the path.
        if !self.nodes_on_path.insert(packed) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "node record {source_node} is contained in its own subtree; \
                     the source records form a cycle"
                ),
            });
        }
        let node = NodeState::new(self.source, source_node);
        let mut pending_children: Vec<(String, RecordIdentifier)> = node
            .child_node_entries()?
            .into_iter()
            .map(|(name, child)| (name, child.record_identifier()))
            .collect();
        // Reversed so `pop` yields enumeration order.
        pending_children.reverse();
        Ok(Entered::Fresh(CompactionFrame {
            source: source_node,
            packed,
            name_in_parent: None,
            within_checkpoints: false,
            pending_children,
            rewritten_children: Vec::new(),
        }))
    }

    /// Writes one node whose children have all been rewritten.
    pub(crate) fn emit(
        &mut self,
        source_node: RecordIdentifier,
        packed_source: u64,
        within_checkpoints: bool,
        mut child_entries: Vec<(String, RecordIdentifier)>,
    ) -> Result<RecordIdentifier> {
        let node = NodeState::new(self.source, source_node);
        let template = node.template()?;
        let stable_identifier = node.stable_identifier_bytes()?;

        let children = match child_entries.len() {
            0 => ChildNodesToWrite::Zero,
            1 => {
                let (name, node) = child_entries.pop().expect("one child");
                ChildNodesToWrite::One { name, node }
            }
            _ => ChildNodesToWrite::Many(child_entries),
        };

        // Rewrite the *stored* property values into fresh records — never
        // the synthesized jcr:primaryType/jcr:mixinTypes, and never a
        // name filter (which would drop an ordinary property of one of
        // those names). The head types come from the template.
        let mut properties = Vec::new();
        for property in node.stored_properties()? {
            properties.push(self.rewrite_property(&property)?);
        }
        sort_properties_for_template(&mut properties);

        let rewritten = self.writer.write_node_with_stable_identifier(
            template.primary_type.as_deref(),
            &template.mixin_types,
            &children,
            &properties,
            Some(stable_identifier),
        )?;
        self.nodes_on_path.remove(&packed_source);
        let packed_rewritten = self.segments.pack(rewritten);
        if self.context_dependent_records.contains(&source_node) {
            self.scoped_rewrites[Self::scope_index(within_checkpoints)]
                .insert(packed_source, packed_rewritten);
        } else {
            self.rewritten_nodes.insert(packed_source, packed_rewritten);
        }
        self.compacted_nodes += 1;
        if self.compacted_nodes - self.reported_nodes >= COPIED_NODE_REPORT_STRIDE {
            self.reported_nodes = self.compacted_nodes;
            self.observer.step_advanced(self.compacted_nodes);
        }
        Ok(rewritten)
    }

    /// Rewrites one property's values into fresh value records.
    pub(crate) fn rewrite_property(&mut self, property: &PropertyState) -> Result<PropertyToWrite> {
        let values = match &property.values {
            PropertyValues::Single(value) => {
                PropertyValuesToWrite::Single(self.rewrite_value(property.property_type, value)?)
            }
            PropertyValues::Multiple(values) => {
                let mut rewritten = Vec::with_capacity(values.len());
                for value in values {
                    rewritten.push(self.rewrite_value(property.property_type, value)?);
                }
                PropertyValuesToWrite::Multiple(rewritten)
            }
        };
        Ok(PropertyToWrite {
            name: property.name.clone(),
            property_type: property.property_type,
            values,
        })
    }

    /// Writes a fresh value record for one decoded property value.
    pub(crate) fn rewrite_value(
        &mut self,
        property_type: PropertyType,
        value: &PropertyValue,
    ) -> Result<RecordIdentifier> {
        if property_type == PropertyType::Binary {
            return match value {
                PropertyValue::Binary(BinaryValue::External { blob_identifier }) => self
                    .writer
                    .write_external_binary_identifier(blob_identifier),
                PropertyValue::Binary(BinaryValue::Inline {
                    record_identifier, ..
                }) => {
                    // Copy the binary streaming, block by block, so a
                    // multi-gigabyte inline binary never has to fit in
                    // memory at once.
                    self.writer.copy_binary_value(
                        self.source,
                        *record_identifier,
                        self.bulk_sharing,
                    )
                }
                _ => Err(Error::InvalidFormat {
                    details: "binary property did not decode to a binary value".to_owned(),
                }),
            };
        }
        // Every non-binary value is stored as its string form.
        let text = value.as_text().ok_or_else(|| Error::InvalidFormat {
            details: format!("property value {value:?} has no string form"),
        })?;
        self.writer.write_string(&text)
    }
}

#[cfg(test)]
mod tests {
    use crate::content::provider::SegmentProvider;
    use crate::store::Repository;
    use crate::writer::compaction::test_support::*;
    use crate::writer::compaction::{
        deep_copy_super_root_with_progress, deep_copy_tree_with_progress,
    };
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::store_writer::WritableRepository;

    /// A copy that omits a checkpoint must land on the same tree as removing
    /// that checkpoint from the head first and then copying — two independent
    /// mechanisms, one of which (`remove_checkpoints`) is the commit path Oak's
    /// own checkpoint removal mirrors. Both run against copies of one store,
    /// because checkpoint names are random and a fixture built twice carries
    /// different ones.
    #[test]
    fn a_copy_that_omits_a_checkpoint_reproduces_every_other_child_exactly() {
        let source = TestDirectory::new("omit-checkpoint-source");
        let names = build_store_with_checkpoints(&source);
        let dropped = names[1].clone();

        let omitting = TestDirectory::new("omit-checkpoint-copy");
        copy_repository(&source.path, &omitting.path);
        let removing = TestDirectory::new("omit-checkpoint-removal");
        copy_repository(&source.path, &removing.path);

        let (omitted_head, omitted_nodes) = {
            let store = WritableRepository::open(&omitting.path).expect("open the omitting store");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (head, nodes) = deep_copy_super_root_with_progress(
                &store,
                &mut writer,
                store.head(),
                &std::collections::BTreeSet::from([dropped.clone()]),
                &mut crate::progress::DiscardedProgress,
            )
            .expect("copy while omitting one checkpoint");
            writer.finish().expect("finish");
            assert!(store.compare_and_set_head(store.head(), head));
            store.flush().expect("flush");
            store.close().expect("close the omitting store");
            (head, nodes)
        };

        let (removed_head, removed_nodes) = {
            let store = WritableRepository::open(&removing.path).expect("open the removing store");
            crate::writer::commit::remove_checkpoints(&store, std::slice::from_ref(&dropped))
                .expect("remove the checkpoint from the live head");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (head, nodes) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                store.head(),
                &mut crate::progress::DiscardedProgress,
            )
            .expect("copy the already-reduced head");
            writer.finish().expect("finish");
            assert!(store.compare_and_set_head(store.head(), head));
            store.flush().expect("flush");
            store.close().expect("close the removing store");
            (head, nodes)
        };

        let omitted = Repository::open(&omitting.path).expect("reopen the omitting store");
        let removed = Repository::open(&removing.path).expect("reopen the removing store");
        let omitted_root = omitted.node(omitted_head);
        let removed_root = removed.node(removed_head);
        assert_eq!(
            child_names(&omitted_root),
            child_names(&removed_root),
            "the super-root carries the same children either way"
        );

        let omitted_checkpoints = omitted_root
            .child_node("checkpoints")
            .expect("read")
            .expect("present");
        let removed_checkpoints = removed_root
            .child_node("checkpoints")
            .expect("read")
            .expect("present");
        let mut surviving = child_names(&omitted_checkpoints);
        let mut expected = child_names(&removed_checkpoints);
        surviving.sort();
        expected.sort();
        assert_eq!(
            surviving, expected,
            "exactly the same checkpoints survive both mechanisms"
        );
        assert!(
            !surviving.contains(&dropped),
            "the retired checkpoint is gone: {surviving:?}"
        );
        // Record addresses differ between two stores by construction, so the
        // equivalence that means something is the shape: both mechanisms copy
        // the same number of distinct nodes, and every surviving checkpoint
        // still resolves its snapshot.
        assert_eq!(
            omitted_nodes, removed_nodes,
            "both mechanisms copy the same tree, so the same number of distinct nodes"
        );
        for name in &surviving {
            for (label, container) in [
                ("omitted", &omitted_checkpoints),
                ("removed", &removed_checkpoints),
            ] {
                let checkpoint = container
                    .child_node(name)
                    .expect("read the checkpoint")
                    .unwrap_or_else(|| panic!("{label} store keeps checkpoint {name}"));
                assert!(
                    checkpoint
                        .child_node("root")
                        .expect("read the snapshot")
                        .is_some(),
                    "{label} store resolves the snapshot of checkpoint {name}"
                );
            }
        }
    }

    /// A checkpoint whose subtree nothing else reaches costs exactly its own
    /// distinct records, and none of them reaches the output.
    #[test]
    fn an_omitted_checkpoint_leaves_its_exclusive_records_uncopied() {
        let directory = TestDirectory::new("omit-exclusive-records");
        let names = build_store_with_checkpoints(&directory);
        let store = WritableRepository::open(&directory.path).expect("open");

        let generation = store.writing_generation().expect("generation");
        let mut whole_writer = store.record_writer(generation);
        let (_, whole) = deep_copy_super_root_with_progress(
            &store,
            &mut whole_writer,
            store.head(),
            &std::collections::BTreeSet::new(),
            &mut crate::progress::DiscardedProgress,
        )
        .expect("copy everything");
        whole_writer.finish().expect("finish");

        let mut reduced_writer = store.record_writer(generation);
        let (_, reduced) = deep_copy_super_root_with_progress(
            &store,
            &mut reduced_writer,
            store.head(),
            &std::collections::BTreeSet::from([names[0].clone()]),
            &mut crate::progress::DiscardedProgress,
        )
        .expect("copy while omitting one checkpoint");
        reduced_writer.finish().expect("finish");

        assert!(
            reduced < whole,
            "omitting a checkpoint copies strictly fewer nodes: {reduced} against {whole}"
        );
        store.close().expect("close");
    }

    /// A subtree the retired checkpoint shares with the live content root is
    /// still copied — through the root. Omission drops a name, not a subtree.
    #[test]
    fn omitting_a_shared_subtree_still_copies_it_through_the_live_root() {
        let directory = TestDirectory::new("omit-shared-subtree");
        let names = build_store_with_checkpoints(&directory);
        let store = WritableRepository::open(&directory.path).expect("open");

        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let (head, _) = deep_copy_super_root_with_progress(
            &store,
            &mut writer,
            store.head(),
            &std::collections::BTreeSet::from([names[0].clone()]),
            &mut crate::progress::DiscardedProgress,
        )
        .expect("copy while omitting one checkpoint");
        writer.finish().expect("finish");
        assert!(store.compare_and_set_head(store.head(), head));
        store.flush().expect("flush");
        store.close().expect("close");

        // The checkpoints snapshot the same content root, so the content the
        // retired one pointed at is still fully readable through `root`.
        let repository = Repository::open(&directory.path).expect("reopen");
        let content = repository
            .node_at_path("/content")
            .expect("resolve the content")
            .expect("the shared content survives the omission");
        assert!(
            content.child_node_count().expect("count") > 0,
            "the shared subtree is intact"
        );
    }

    #[test]
    fn a_deep_copy_copies_each_distinct_node_exactly_once() {
        let directory = TestDirectory::new("memo-exact");
        build_populated_store(&directory);

        // The memo is exact, so the copy visits the shared subtree behind the
        // checkpoint and the live root once. `copied` is therefore the
        // distinct reachable node count, not merely at least it.
        let (copied, distinct) = {
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (root, copied) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                head,
                &mut crate::progress::DiscardedProgress,
            )
            .expect("deep copy");
            let distinct = distinct_reachable_nodes(&store, head);
            writer.finish().expect("finish");
            assert!(store.compare_and_set_head(head, root));
            store.close().expect("close");
            (copied, distinct)
        };
        assert_eq!(
            copied as usize, distinct,
            "every distinct node is copied exactly once"
        );

        assert_content_intact(&directory);
    }

    #[test]
    fn a_shared_subtree_is_copied_once_however_deep_the_sharing_nests() {
        // Every level references the same next-level node twice, so the
        // distinct root-to-leaf paths grow as 2^levels while the distinct
        // nodes grow linearly. A memo that can be starved turns those paths
        // into copies: at 14 levels this shape measured 557,024 copies
        // against 464 distinct nodes. An exact memo cannot, at any depth.
        for (levels, ballast) in [(4usize, 0usize), (14, 0), (14, 32), (24, 4)] {
            let directory = TestDirectory::new(&format!("diamond-{levels}-{ballast}"));
            build_diamond_chain(&directory, levels, ballast);
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let distinct = distinct_reachable_nodes(&store, head);
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (_root, copied) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                head,
                &mut crate::progress::DiscardedProgress,
            )
            .expect("deep copy");
            writer.finish().expect("finish");
            store.close().expect("close");
            assert_eq!(
                copied as usize, distinct,
                "levels={levels} ballast={ballast}: copied must equal the distinct node count"
            );
        }
    }

    /// Throughput of the exact copy, for extrapolating to a field-scale head.
    /// Ignored by default: it writes about a million nodes.
    #[test]
    fn a_tree_deeper_than_any_call_stack_copies_whole() {
        // 100k levels on the 2 MiB stack a spawned thread gets by default.
        // The recursive walk aborted the process at 2900 levels here; there
        // is no depth this can refuse, because depth is the repository's
        // property and not this code's to bound.
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let directory = TestDirectory::new("deep-chain");
                build_diamond_chain(&directory, 100_000, 0);
                let store = WritableRepository::open(&directory.path).expect("open");
                let head = store.head();
                let distinct = distinct_reachable_nodes(&store, head);
                let generation = store.writing_generation().expect("generation");
                let mut writer = store.record_writer(generation);
                let (_root, copied) = deep_copy_tree_with_progress(
                    &store,
                    &mut writer,
                    head,
                    &mut crate::progress::DiscardedProgress,
                )
                .expect("a deep tree copies rather than aborting");
                writer.finish().expect("finish");
                assert_eq!(copied as usize, distinct);
                assert!(distinct > 100_000, "the chain really is that deep");
                // The post-write health traversal walks the same tree, so a
                // depth limit there would only move the failure.
                crate::tooling::verify_node_tree(&store, head)
                    .expect("the verifier has no depth limit either");
                store.close().expect("close");
            })
            .expect("spawn");
        handle.join().expect("the walk stays off the call stack");
    }

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn measure_deep_chain_walk_footprint() {
        for levels in [100_000usize, 400_000] {
            let directory = TestDirectory::new(&format!("deep-footprint-{levels}"));
            build_diamond_chain(&directory, levels, 0);
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let before = resident_bytes();
            crate::tooling::verify_node_tree(&store, head).expect("verifies");
            let after_verify = resident_bytes();
            store.close().expect("close");
            println!(
                "levels={levels:>7} verify_rss_delta={:>6} MiB = {:>4} B/level",
                after_verify.saturating_sub(before) / 1024 / 1024,
                after_verify.saturating_sub(before) / levels,
            );
        }
    }

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn measure_copy_throughput() {
        let directory = TestDirectory::new("throughput");
        let build_started = std::time::Instant::now();
        build_wide_store(&directory, 1000);
        let built = build_started.elapsed();
        let store = WritableRepository::open(&directory.path).expect("open");
        let head = store.head();
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let started = std::time::Instant::now();
        let (_root, copied) = deep_copy_tree_with_progress(
            &store,
            &mut writer,
            head,
            &mut crate::progress::DiscardedProgress,
        )
        .expect("deep copy");
        let elapsed = started.elapsed();
        writer.finish().expect("finish");
        store.close().expect("close");
        let per_second =
            u32::try_from(copied).map_or(f64::INFINITY, f64::from) / elapsed.as_secs_f64();
        println!(
            "built {copied} nodes in {:.1}s; copied in {:.2}s = {per_second:.0} nodes/s; \
             18.8M nodes extrapolates to {:.1} min",
            built.as_secs_f64(),
            elapsed.as_secs_f64(),
            18_800_000.0 / per_second / 60.0,
        );
    }

    #[test]
    fn every_random_shape_copies_each_distinct_node_exactly_once() {
        // The count is cross-checked against `distinct_reachable_nodes`, which
        // walks with a `HashSet<RecordIdentifier>` and shares no code with the
        // interner or the memo — so agreement is two independent answers
        // matching, not one implementation agreeing with itself.
        for seed in 1..=200u64 {
            let directory = TestDirectory::new(&format!("random-dag-{seed}"));
            build_random_dag(&directory, seed);
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let distinct = distinct_reachable_nodes(&store, head);
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (_root, copied) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                head,
                &mut crate::progress::DiscardedProgress,
            )
            .expect("deep copy");
            writer.finish().expect("finish");
            store.close().expect("close");
            assert_eq!(
                copied as usize, distinct,
                "seed {seed}: copied {copied} but {distinct} distinct nodes are reachable"
            );
        }
    }

    #[test]
    fn a_cyclic_source_is_refused_at_the_record_that_closes_the_cycle() {
        use crate::content::provider::tests::MemorySegmentProvider;
        use crate::error::Error;
        use crate::writer::segment_builder::SegmentBufferBuilder;

        let directory = TestDirectory::new("compaction-cycle");
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

        // Point the root's only child slot back at the root itself.
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

        let mut sink_writer = store.record_writer(generation);
        let error = deep_copy_tree_with_progress(
            &memory,
            &mut sink_writer,
            root,
            &mut crate::progress::DiscardedProgress,
        )
        .expect_err("a cyclic source is refused");
        let Error::InvalidFormat { details } = &error else {
            panic!("a cycle is a format error, got {error:?}");
        };
        // Exactly, at the closing record — not "probably a cycle" after 4000
        // wasted levels, and naming the record so the store can be repaired.
        assert!(
            details.contains("contained in its own subtree"),
            "unexpected detail: {details}"
        );
        assert!(
            details.contains(&root.to_string()),
            "the error names the offending record: {details}"
        );
        store.close().expect("close");
    }
}
