//! The ordered mutation sequence, and the verification that precedes it.
//!
//! Every step here has a place in the safety case's mutation table, and the
//! order is the argument:
//!
//! 1. **The run's subdirectory**, created before the first spill. Files
//!    outside the store, removed on return.
//! 2. **Index records appended** into fresh archives at a number above every
//!    physical name — additive, so nothing existing is touched and the store
//!    is unchanged plus unreferenced archives if anything fails.
//! 3. **The definition rewritten**, then the spine to a new super-root.
//! 4. **Everything verified through the open session**, before publication:
//!    `verify_node_tree` over each new subtree, and an entry-side pass that
//!    resolves every entry against the state root.
//! 5. **One `compare_and_set_head`, one `flush`.** The first changes nothing
//!    on disk; the second fsyncs the archive and the directory and only then
//!    appends the single journal line. So the cutpoints on either side
//!    observe the same on-disk prefix, and there is no third state.
//! 6. **A fresh reopen**, verifying the head identity, the single new
//!    journal line, each subtree again, and that the published head
//!    *reaches* each rebuilt definition — a readable copy somewhere in the
//!    store is not the same claim as a spine that resolves to it.
//!
//! The tail itself lives in the private `verification` submodule, which is
//! where the seam that proves it is not vacuous lives too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::progress::{ProgressObserver, Step, WorkUnit, observe};
use crate::segment::record::RecordIdentifier;
use crate::writer::index::counter_builder::CounterBuilder;
use crate::writer::index::definition_update::{
    DefinitionEdits, DisablerVerdict, rewrite_definition,
};
use crate::writer::index::plan::{NoWorkReason, ReindexAction};
use crate::writer::index::prepared::{PreparedReindex, RUN_DIRECTORY_PREFIX};
use crate::writer::index::property_builder::{MirrorBuilder, UniqueBuilder};
use crate::writer::index::property_collector::{
    CollectedEntries, CollectedReferences, EntrySink, PropertyCollector, ReferenceCollector,
    SortedEntries,
};
use crate::writer::index::selection::IndexingState;
use crate::writer::index::{IndexEntry, RunLocation, SortBudget};
use crate::writer::record_writer::{
    PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use crate::writer::store_writer::WritableRepository;

/// The step opened while index records are written.
const WRITE_STEP: &str = "writing index records";

/// The step opened while the rebuilt subtrees are verified.
const VERIFY_STEP: &str = "verifying the rebuilt indexes";

/// Fired once the sort has returned its iterator and before the first index
/// record is appended: every spill file is on disk and the store is
/// untouched. The sort itself takes no cutpoint, which is what keeps it
/// independent of the segment-store write path.
const BEFORE_SPILL_CLEANUP: &str = "index-reindex.before-spill-cleanup";

/// Fired after every record and the spine are written and verified, and
/// before the head moves: the store is unchanged plus unreferenced archives.
const BEFORE_HEAD_PUBLISH: &str = "index-reindex.before-head-publish";

/// Fired between `compare_and_set_head`, which changes nothing on disk, and
/// `flush`, which seals, fsyncs and appends the journal line. The on-disk
/// prefix here is the same one [`BEFORE_HEAD_PUBLISH`] observes.
const AFTER_HEAD_PUBLISH_BEFORE_FLUSH: &str = "index-reindex.after-head-publish-before-flush";

/// Fired after the store is final and before it is read back: a failure here
/// is reported, never repaired.
const BEFORE_APPLIED_VERIFICATION: &str = "index-reindex.before-applied-verification";

/// One durability boundary of the mutation table, for the fault probes.
#[cfg(test)]
fn probe(cutpoint: &str) -> Result<()> {
    crate::writer::fault_injection::fail_if_armed(cutpoint)?;
    crate::writer::fault_injection::crash_if_armed(cutpoint);
    Ok(())
}

/// Outside a test build there are no cutpoints and this compiles to nothing.
#[cfg(not(test))]
#[inline]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the sibling this stands in for can fail, and the caller is the same either way"
)]
fn probe(cutpoint: &str) -> Result<()> {
    let _ = cutpoint;
    Ok(())
}

/// What one definition's rebuild produced.
pub struct RebuiltDefinition {
    /// The definition's path.
    pub path: String,
    /// Its record before the rewrite.
    pub previous_record: RecordIdentifier,
    /// Its record after.
    pub rebuilt_record: RecordIdentifier,
    /// What to report about it.
    pub report: DefinitionReport,
    /// What the tail has to check, and against what.
    pub verification: Verification,
}

/// What verifying one rebuilt definition needs that the written subtree does
/// not carry.
///
/// The entry half reads the content the entries name, so it needs the state
/// root the rebuild walked — the *old* one, which is right: a reindex does
/// not change content, and the new root is still in an unfinished segment
/// when the tail runs. The counter's arm needs more than a state root, since
/// what a node was credited cannot be recovered from the tree that records
/// it; task 0705's `credited_by_path` carries it here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Verification {
    /// Only the node tree: a reset writes no entries to check.
    NodeTreeOnly,
    /// The entry half of plan 0006's check, charged per entry.
    Entries {
        /// The state root the rebuild walked.
        state_root: RecordIdentifier,
        /// How many entries the walk produced, which is the budget.
        entries: u64,
    },
    /// The counter's own arm: paths resolve, and every `:cnt` adds up.
    Counter {
        /// The state root the rebuild walked.
        state_root: RecordIdentifier,
        /// What each node was credited, from the build.
        credited_by_path: BTreeMap<String, i64>,
    },
}

/// The per-definition half of a [`ReindexOutcome`].
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum DefinitionReport {
    /// Rebuilt, with what was written.
    Rebuilt {
        /// Entries the walk produced.
        entries: u64,
        /// Distinct keys, counted exactly by the builder.
        distinct_keys: u64,
        /// Index nodes written.
        nodes_written: u64,
    },
    /// Reset: hidden children removed, nothing built.
    Reset {
        /// The hidden children that were removed.
        removed_hidden_children: Vec<String>,
        /// The hidden children kept because they carry
        /// `retainNodeInReindex`.
        retained_hidden_children: Vec<String>,
    },
    /// Nothing to do; the head did not move for this definition.
    NothingToDo {
        /// Why.
        reason: NoWorkReason,
    },
}

/// What a run did.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct ReindexOutcome {
    /// One report per selected definition, in plan order.
    pub definitions: Vec<(String, DefinitionReport)>,
    /// The head before the run.
    pub head_before: RecordIdentifier,
    /// The head after. Equal to `head_before` when nothing was written.
    pub head_after: RecordIdentifier,
}

impl ReindexOutcome {
    /// Whether the run moved the head.
    #[must_use]
    pub fn moved_the_head(&self) -> bool {
        self.head_before != self.head_after
    }
}

/// Applies a prepared reindex.
pub(crate) fn apply_prepared(
    prepared: &PreparedReindex,
    observer: &mut dyn ProgressObserver,
) -> Result<ReindexOutcome> {
    // A plan with no work never opens the store and never moves the head.
    if prepared.plan.is_empty() {
        let repository = crate::store::Repository::open(&prepared.directory)?;
        let head = repository.head_record_identifier();
        return Ok(ReindexOutcome {
            definitions: prepared
                .plan
                .actions
                .iter()
                // An empty plan holds nothing but no-ops, and each carries
                // its own reason — reporting one it does not carry would be
                // a lie the renderer repeats.
                .filter_map(|action| match action {
                    ReindexAction::NothingToDo { path, reason } => Some((
                        path.clone(),
                        DefinitionReport::NothingToDo { reason: *reason },
                    )),
                    _ => None,
                })
                .collect(),
            head_before: head,
            head_after: head,
        });
    }

    prepared.recheck_before_mutation()?;

    let run = RunDirectory::create(prepared)?;
    let outcome = apply_under_the_lock(prepared, &run, observer);
    // The subdirectory is removed on return, whatever the outcome, so the
    // only thing a failed run leaves outside the store is nothing.
    run.remove();
    outcome
}

/// The run's own subdirectory, named from the store so residue is per store.
struct RunDirectory {
    path: PathBuf,
    _lock: crate::writer::repository_lock::RepositoryLock,
}

impl RunDirectory {
    fn create(prepared: &PreparedReindex) -> Result<Self> {
        let name = format!(
            "{RUN_DIRECTORY_PREFIX}{:016x}",
            store_name_hash(&prepared.directory)
        );
        let path = prepared.plan.work_directory.join(name);
        std::fs::create_dir_all(&path)?;
        // Held for the run's duration, so a concurrent run on *another*
        // store is neither refused nor warned about, and this one's
        // subdirectory is recognizably live rather than residue.
        let lock = crate::writer::repository_lock::RepositoryLock::acquire(&path)?;
        Ok(Self { path, _lock: lock })
    }

    fn remove(self) {
        let path = self.path.clone();
        drop(self);
        let _ = std::fs::remove_dir_all(path);
    }
}

/// A stable hash of the canonical store directory, so two stores never share
/// a run subdirectory and one store's residue is attributable.
fn store_name_hash(directory: &Path) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in directory.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// What a run published, for the tail that runs after the store is closed.
struct PublishedRun {
    rebuilt: Vec<RebuiltDefinition>,
    head_before: RecordIdentifier,
    head_after: RecordIdentifier,
    journal_lines_before: usize,
}

/// The whole mutating sequence, with the store open.
///
/// The session is always closed, on every path. That matters because a
/// failed run has appended records: closing writes the archive's graph,
/// catalog and index trailers, so what it leaves behind is an *unreferenced*
/// archive rather than a damaged one — the difference between a store a
/// later `froe compact` quietly retires and a store it refuses until an
/// operator authorizes an index repair.
fn apply_under_the_lock(
    prepared: &PreparedReindex,
    run: &RunDirectory,
    observer: &mut dyn ProgressObserver,
) -> Result<ReindexOutcome> {
    let store = WritableRepository::open_prepared(
        &prepared.directory,
        prepared.repository_lock.clone(),
        prepared.certified_archive_number,
    )?;
    let head_before = store.head();

    let published = build_and_publish(&store, prepared, run, observer, head_before);

    // On the way out of a failure, put the session's head back where it was
    // found. The head lives in memory until `flush` appends the journal
    // line, so a run that failed *after* `compare_and_set_head` must undo it
    // before closing — otherwise closing publishes the very run that failed.
    if published.is_err() {
        let current = store.head();
        if current != head_before {
            store.compare_and_set_head(current, head_before);
        }
    }
    let closed = store.close();

    let published = published?;
    closed?;

    if published.head_after == published.head_before {
        return Ok(ReindexOutcome {
            definitions: Vec::new(),
            head_before,
            head_after: head_before,
        });
    }

    probe(BEFORE_APPLIED_VERIFICATION)?;
    verification::verify_after_reopen(
        &prepared.directory,
        published.head_after,
        &published.rebuilt,
        published.journal_lines_before,
    )?;

    Ok(ReindexOutcome {
        definitions: published
            .rebuilt
            .iter()
            .map(|definition| (definition.path.clone(), definition.report.clone()))
            .collect(),
        head_before: published.head_before,
        head_after: published.head_after,
    })
}

/// Everything that needs the store open: build, rewrite, verify, publish.
fn build_and_publish(
    store: &WritableRepository,
    prepared: &PreparedReindex,
    run: &RunDirectory,
    observer: &mut dyn ProgressObserver,
    head_before: RecordIdentifier,
) -> Result<PublishedRun> {
    let journal_lines_before =
        crate::journal::read_journal(&prepared.directory.join("journal.log"))?.len();

    let mut rebuilt = {
        let mut writer = store.record_writer(store.writing_generation()?);
        let step = Step::new(WRITE_STEP, WorkUnit::Nodes);
        let rebuilt = observe(observer, &step, |observer| {
            rebuild_every_definition(store, &mut writer, prepared, run, observer)
        })?;
        // The index records have to be in the store before the spine that
        // references them is written: the spine writer's segment carries
        // references into this writer's, and a reference can only name a
        // segment the store already holds.
        writer.finish()?;
        rebuilt
    };

    if rebuilt.is_empty() {
        return Ok(PublishedRun {
            rebuilt,
            head_before,
            head_after: head_before,
            journal_lines_before,
        });
    }

    // Test-only damage, in a writer of its own that is finished before the
    // tail runs — so the tail reads a perturbed subtree exactly the way it
    // reads a builder's, out of the store rather than out of an open
    // segment. Compiles to nothing outside a test build.
    verification::perturb_all(store, &mut rebuilt)?;

    // One spine rewrite over every rebuilt definition, then one publication.
    let mut writer = store.record_writer(store.writing_generation()?);
    let super_root = rewrite_the_spine(store, &mut writer, head_before, &rebuilt)?;

    let step = Step::new(VERIFY_STEP, WorkUnit::Nodes);
    observe(observer, &step, |observer| {
        verification::verify_before_publication(store, &rebuilt, observer)
    })?;

    writer.finish()?;

    probe(BEFORE_HEAD_PUBLISH)?;
    if !store.compare_and_set_head(head_before, super_root) {
        return Err(Error::InvalidFormat {
            details: "the head moved while the reindex held the lock, which cannot happen \
                      and means the lock did not hold"
                .to_owned(),
        });
    }
    // `compare_and_set_head` changed nothing on disk, so a fault here
    // observes exactly the prefix the boundary before it observes. There is
    // no third state.
    probe(AFTER_HEAD_PUBLISH_BEFORE_FLUSH)?;

    Ok(PublishedRun {
        rebuilt,
        head_before,
        head_after: super_root,
        journal_lines_before,
    })
}

/// The per-definition dispatch.
fn rebuild_every_definition<Sink: SegmentSink>(
    store: &WritableRepository,
    writer: &mut RecordWriter<Sink>,
    prepared: &PreparedReindex,
    run: &RunDirectory,
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<RebuiltDefinition>> {
    let super_root = store.head_node();
    let inventory = crate::index::inventory::IndexInventory::collect(store, &super_root)
        .map_err(index_error_to_store_error)?;
    let selection = crate::writer::index::selection::select(
        store,
        &super_root,
        &inventory,
        &crate::writer::index::selection::SelectionOptions {
            requested_paths: prepared.options.requested_paths().to_vec(),
            from_head: prepared.options.from_head(),
        },
    )?;

    let mut rebuilt = Vec::new();
    for selected in &selection.selected {
        let definition = crate::content::node::NodeState::new(store, selected.definition_record);
        if matches!(selected.state, IndexingState::ResetForReplay { .. }) {
            if let Some(built) = reset_one(store, writer, selected, &definition)? {
                rebuilt.push(built);
            }
            continue;
        }
        let built = rebuild_one(
            store,
            writer,
            selected,
            &definition,
            prepared,
            run,
            observer,
        )?;
        rebuilt.push(built);
    }
    Ok(rebuilt)
}

/// A reset: hidden children removed, nothing built, no visible property
/// touched.
fn reset_one<Sink: SegmentSink>(
    store: &WritableRepository,
    writer: &mut RecordWriter<Sink>,
    selected: &crate::writer::index::selection::SelectedIndex,
    definition: &crate::content::node::NodeState<'_>,
) -> Result<Option<RebuiltDefinition>> {
    let mut removed = Vec::new();
    let mut retained = Vec::new();
    for (name, child) in definition.child_node_entries()? {
        if !name.starts_with(':') {
            continue;
        }
        if crate::index::strict_boolean(child.property("retainNodeInReindex")?.as_ref()) {
            retained.push(name);
        } else {
            removed.push(name);
        }
    }
    if removed.is_empty() {
        // Nothing to remove: the head does not move for this definition.
        return Ok(None);
    }
    let rebuilt_record = rewrite_definition(store, writer, definition, &DefinitionEdits::reset())?;
    Ok(Some(RebuiltDefinition {
        path: selected.definition.path.clone(),
        previous_record: selected.definition_record,
        rebuilt_record,
        report: DefinitionReport::Reset {
            removed_hidden_children: removed,
            retained_hidden_children: retained,
        },
        verification: Verification::NodeTreeOnly,
    }))
}

/// A rebuild: collect, sort, build, rewrite.
fn rebuild_one<Sink: SegmentSink>(
    store: &WritableRepository,
    writer: &mut RecordWriter<Sink>,
    selected: &crate::writer::index::selection::SelectedIndex,
    definition: &crate::content::node::NodeState<'_>,
    prepared: &PreparedReindex,
    run: &RunDirectory,
    observer: &mut dyn ProgressObserver,
) -> Result<RebuiltDefinition> {
    let Some(state_record) = selected.state_root else {
        return Err(Error::InvalidFormat {
            details: format!(
                "{} was selected for a rebuild with no state root, which selection does \
                 not produce",
                selected.definition.path
            ),
        });
    };
    let state_root = crate::content::node::NodeState::new(store, state_record);
    let budget = SortBudget::of_bytes(prepared.options.sort_budget_bytes());
    let location = RunLocation::new(&run.path, sanitized_prefix(&selected.definition.path));

    let mut created_seed = None;
    let (hidden_children, report, verification) = match selected.definition.index_type.as_ref() {
        Some(crate::index::IndexType::Counter) => {
            let builder = CounterBuilder::new(&selected.definition);
            let built = builder.build(&state_root, writer)?;
            created_seed = built.created_seed;
            let children = built
                .index_record
                .map(|record| vec![(crate::index::INDEX_CONTENT_NODE_NAME.to_owned(), record)])
                .unwrap_or_default();
            let nodes = built.credited_by_path.len() as u64;
            (
                children,
                DefinitionReport::Rebuilt {
                    entries: nodes,
                    distinct_keys: nodes,
                    nodes_written: nodes,
                },
                Verification::Counter {
                    state_root: state_record,
                    credited_by_path: built.credited_by_path,
                },
            )
        }
        Some(crate::index::IndexType::Lucene) => {
            // Selection refuses a Lucene definition, so this arm is
            // unreachable through `select`. It is here so the fall-through
            // below cannot one day treat a Lucene definition as a property
            // one — a silent wrong rebuild — and so plan 0010 has the place
            // to fill.
            return Err(Error::InvalidFormat {
                details: crate::writer::index::selection::SelectionRefusal::LuceneNotYetSupported {
                    path: selected.definition.path.clone(),
                }
                .to_string(),
            });
        }
        Some(crate::index::IndexType::Reference) => {
            let (children, report) =
                build_reference_index(&state_root, writer, &location, &budget, observer)?;
            let verification = entry_verification(state_record, &report);
            (children, report, verification)
        }
        _ => {
            let (children, report) = build_property_index(
                &state_root,
                &selected.definition,
                writer,
                &location,
                &budget,
                observer,
            )?;
            let verification = entry_verification(state_record, &report);
            (children, report, verification)
        }
    };

    let mut edits = DefinitionEdits::reindexed(
        // The verdict is selection's, computed over the head's other
        // definitions; the rewrite never evaluates it.
        DisablerVerdict::Leave,
        hidden_children,
    );
    if let Some(seed) = created_seed {
        let value = writer.write_string(&seed.to_string())?;
        edits.property_replacements.push(PropertyToWrite {
            name: "seed".to_owned(),
            property_type: crate::PropertyType::Long,
            values: PropertyValuesToWrite::Single(value),
        });
    }
    let rebuilt_record = rewrite_definition(store, writer, definition, &edits)?;

    Ok(RebuiltDefinition {
        path: selected.definition.path.clone(),
        previous_record: selected.definition_record,
        rebuilt_record,
        report,
        verification,
    })
}

/// The entry-half verification for a report that wrote entries.
fn entry_verification(state_root: RecordIdentifier, report: &DefinitionReport) -> Verification {
    match report {
        DefinitionReport::Rebuilt { entries, .. } => Verification::Entries {
            state_root,
            entries: *entries,
        },
        _ => Verification::NodeTreeOnly,
    }
}

/// The mirror or unique `:index`, from the sorted entries.
fn build_property_index<Sink: SegmentSink>(
    state_root: &crate::content::node::NodeState<'_>,
    definition: &crate::index::IndexDefinition,
    writer: &mut RecordWriter<Sink>,
    location: &RunLocation,
    budget: &SortBudget,
    observer: &mut dyn ProgressObserver,
) -> Result<(Vec<(String, RecordIdentifier)>, DefinitionReport)> {
    let (collected, _) = PropertyCollector::collect(
        state_root,
        definition,
        &EntrySink::Runs {
            location: location.clone(),
            budget: budget.clone(),
        },
        observer,
    )?;
    // The sort has returned its iterator and every spill file is on disk;
    // not one index record has been appended.
    probe(BEFORE_SPILL_CLEANUP)?;
    let CollectedEntries::Sorted(sorted) = collected else {
        unreachable!("the run sink sorts");
    };
    let entries = sorted.emitted();
    let (record, accounting, distinct_keys) = if definition.property.unique {
        let mut builder = UniqueBuilder::new(writer);
        let keys = feed(sorted, |key, path| builder.push(key, path))?;
        let (record, accounting) = builder.finish()?;
        (record, accounting, keys)
    } else {
        let mut builder = MirrorBuilder::new(writer);
        let keys = feed(sorted, |key, path| builder.push(key, path))?;
        let (record, accounting) = builder.finish()?;
        (record, accounting, keys)
    };
    Ok((
        vec![(crate::index::INDEX_CONTENT_NODE_NAME.to_owned(), record)],
        DefinitionReport::Rebuilt {
            entries,
            distinct_keys,
            nodes_written: accounting.nodes_written,
        },
    ))
}

/// The reference index's two hidden children, each written only when its set
/// received an entry — Oak creates them on the first insert and not before.
fn build_reference_index<Sink: SegmentSink>(
    state_root: &crate::content::node::NodeState<'_>,
    writer: &mut RecordWriter<Sink>,
    location: &RunLocation,
    budget: &SortBudget,
    observer: &mut dyn ProgressObserver,
) -> Result<(Vec<(String, RecordIdentifier)>, DefinitionReport)> {
    let (collected, _) = ReferenceCollector::collect(
        state_root,
        &EntrySink::Runs {
            location: location.clone(),
            budget: budget.clone(),
        },
        observer,
    )?;
    probe(BEFORE_SPILL_CLEANUP)?;
    let CollectedReferences::Sorted(mut sets) = collected else {
        unreachable!("the run sink sorts");
    };

    let mut children = Vec::new();
    let mut entries = 0u64;
    let mut nodes_written = 0u64;
    let mut distinct_keys = 0u64;

    // Strong first, then weak: the second merge opens only after the first
    // iterator is dropped, which is what keeps the open-file bound at the
    // fan-in plus one.
    let had_strong = sets.has_strong();
    if let Some(sorted) = sets.strong()? {
        let mut builder = MirrorBuilder::new(writer);
        let keys = feed(sorted, |key, path| builder.push(key, path))?;
        let (record, accounting) = builder.finish()?;
        if had_strong {
            children.push((":references".to_owned(), record));
            nodes_written += accounting.nodes_written;
            distinct_keys += keys;
            entries += keys;
        }
    }
    let had_weak = sets.has_weak();
    if let Some(sorted) = sets.weak()? {
        let mut builder = MirrorBuilder::new(writer);
        let keys = feed(sorted, |key, path| builder.push(key, path))?;
        let (record, accounting) = builder.finish()?;
        if had_weak {
            children.push((":weakreferences".to_owned(), record));
            nodes_written += accounting.nodes_written;
            distinct_keys += keys;
            entries += keys;
        }
    }

    Ok((
        children,
        DefinitionReport::Rebuilt {
            entries,
            distinct_keys,
            nodes_written,
        },
    ))
}

/// Feeds every sorted entry to `push`, returning the distinct-key count.
fn feed(sorted: SortedEntries, mut push: impl FnMut(&str, &str) -> Result<()>) -> Result<u64> {
    let mut distinct_keys = 0u64;
    let mut previous: Option<String> = None;
    for entry in sorted {
        let IndexEntry { key, path } = entry?;
        if previous.as_deref() != Some(key.as_str()) {
            distinct_keys += 1;
            previous = Some(key.clone());
        }
        push(&key, &path)?;
    }
    Ok(distinct_keys)
}

/// Rewrites `/oak:index` and the root up to a new super-root.
fn rewrite_the_spine<Sink: SegmentSink>(
    store: &WritableRepository,
    writer: &mut RecordWriter<Sink>,
    head: RecordIdentifier,
    rebuilt: &[RebuiltDefinition],
) -> Result<RecordIdentifier> {
    let head_node = crate::content::node::NodeState::new(store, head);
    let content_root = head_node
        .child_node("root")?
        .ok_or_else(|| Error::InvalidFormat {
            details: "the super-root has no \"root\" child node".to_owned(),
        })?;
    let oak_index = content_root
        .child_node(crate::index::INDEX_DEFINITIONS_NAME)?
        .ok_or_else(|| Error::InvalidFormat {
            details: "the content root has no /oak:index".to_owned(),
        })?;

    let mut definition_edits = crate::writer::commit::ChildEdits::new();
    for definition in rebuilt {
        let name = definition
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&definition.path)
            .to_owned();
        definition_edits.insert(name, Some(definition.rebuilt_record));
    }
    let new_oak_index = crate::writer::commit::rewrite_node_with_child_edits(
        store,
        writer,
        Some(oak_index.record_identifier()),
        &definition_edits,
    )?;

    let mut root_edits = crate::writer::commit::ChildEdits::new();
    root_edits.insert(
        crate::index::INDEX_DEFINITIONS_NAME.to_owned(),
        Some(new_oak_index),
    );
    let new_root = crate::writer::commit::rewrite_node_with_child_edits(
        store,
        writer,
        Some(content_root.record_identifier()),
        &root_edits,
    )?;

    let mut super_edits = crate::writer::commit::ChildEdits::new();
    super_edits.insert("root".to_owned(), Some(new_root));
    crate::writer::commit::rewrite_node_with_child_edits(store, writer, Some(head), &super_edits)
}

/// A run-file prefix derived from a definition path, so two definitions of
/// one run never share spill names.
fn sanitized_prefix(path: &str) -> String {
    let mut prefix = String::with_capacity(path.len());
    for character in path.chars() {
        if character.is_ascii_alphanumeric() {
            prefix.push(character);
        } else {
            prefix.push('-');
        }
    }
    prefix
}

fn index_error_to_store_error(error: crate::index::IndexError) -> Error {
    match error {
        crate::index::IndexError::Record(source) => source,
        other => Error::InvalidFormat {
            details: other.to_string(),
        },
    }
}

mod verification;

#[cfg(test)]
mod tests;
