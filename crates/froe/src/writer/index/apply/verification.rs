//! The tail: what an apply proves about a subtree before it publishes it,
//! and the test-only seam that proves the proving is live.
//!
//! Split out of `apply.rs` because it is a second subject — the operation
//! decides what to write, this decides whether to keep it — and because the
//! two together exceed the thousand-line gate.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::{Error, Result};
use crate::progress::{ProgressObserver, count};
use crate::segment::record::RecordIdentifier;
#[cfg(test)]
use crate::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use crate::writer::store_writer::WritableRepository;

use super::{RebuiltDefinition, Verification, index_error_to_store_error};

/// Every new subtree, verified through the open session before anything is
/// published.
///
/// Two halves, and neither is `check`'s: the node tree, which proves every
/// record the subtree names is readable, and then an entry-side pass that
/// proves the entries agree with the content they name. The covered-node
/// half — a walk of every node the definition covers — is deliberately not
/// run here. It is what the rebuild just did, at the same cost, and running
/// it again would double a reindex's walk to re-derive the input it was
/// given.
pub(super) fn verify_before_publication(
    store: &WritableRepository,
    rebuilt: &[RebuiltDefinition],
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let mut verified = 0usize;
    for definition in rebuilt {
        crate::tooling::check::verify_node_tree(store, definition.rebuilt_record)?;
        verify_one_definition(store, definition)?;
        verified += 1;
        observer.step_advanced(count(verified));
    }
    Ok(())
}

/// The entry-side half for one rebuilt definition.
fn verify_one_definition(store: &WritableRepository, definition: &RebuiltDefinition) -> Result<()> {
    match &definition.verification {
        Verification::NodeTreeOnly => Ok(()),
        Verification::Entries {
            state_root,
            entries,
        } => verify_entries(store, definition, *state_root, *entries),
        Verification::Counter {
            state_root,
            credited_by_path,
        } => verify_counter(store, definition, *state_root, credited_by_path),
    }
}

/// A test-only way to damage a subtree between the builders and the tail.
///
/// The tail refuses when what was written disagrees with the content it
/// indexes or with the build's own record of what it counted. A suite that
/// only ever runs healthy reindexes cannot tell that apart from a tail that
/// returns `Ok(())` unconditionally — both are green — so this seam rewrites
/// one subtree after its builder returns and before the tail runs, and the
/// regressions below assert the refusal fires, names what it found, and
/// moves no head.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SubtreePerturbation {
    /// Splice `count` forged key nodes into the mirror, each carrying
    /// `match` under a path that is not in the content.
    ///
    /// One is a stale entry the pass reports. Two exceed the budget, which
    /// is the collected count plus one and accepts at its limit — so two is
    /// the smallest number that proves the budget is the collected count
    /// rather than something looser.
    SpliceForgedEntries(u32),
    /// Add one to the counter mirror's root `:cnt`.
    AlterOneCount,
}

#[cfg(test)]
std::thread_local! {
    static PERTURBATION: std::cell::RefCell<Option<SubtreePerturbation>> =
        const { std::cell::RefCell::new(None) };
}

/// Makes the next reindex on this thread damage what it built.
#[cfg(test)]
pub(crate) fn perturb_subtree(perturbation: Option<SubtreePerturbation>) {
    PERTURBATION.with(|cell| *cell.borrow_mut() = perturbation);
}

/// Damages every rebuilt definition, in a writer finished before the tail.
#[cfg(test)]
pub(super) fn perturb_all(
    store: &WritableRepository,
    rebuilt: &mut [RebuiltDefinition],
) -> Result<()> {
    if PERTURBATION.with(|cell| cell.borrow().is_none()) {
        return Ok(());
    }
    let mut writer = store.record_writer(store.writing_generation()?);
    for built in rebuilt {
        perturb(store, &mut writer, built)?;
    }
    writer.finish()?;
    Ok(())
}

/// Does nothing outside a test build.
#[cfg(not(test))]
#[inline]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the sibling this stands in for can fail, and the caller is the same either way"
)]
pub(super) fn perturb_all(
    store: &WritableRepository,
    rebuilt: &mut [RebuiltDefinition],
) -> Result<()> {
    let _ = (store, rebuilt);
    Ok(())
}

/// Rewrites one rebuilt definition's `:index`, in place of the record the
/// builder produced.
#[cfg(test)]
fn perturb<Sink: SegmentSink>(
    store: &WritableRepository,
    writer: &mut RecordWriter<Sink>,
    built: &mut RebuiltDefinition,
) -> Result<()> {
    let Some(perturbation) = PERTURBATION.with(|cell| *cell.borrow()) else {
        return Ok(());
    };
    let definition_node = crate::content::node::NodeState::new(store, built.rebuilt_record);
    let Some(index) = definition_node.child_node(crate::index::INDEX_CONTENT_NODE_NAME)? else {
        return Ok(());
    };

    let new_index = match perturbation {
        SubtreePerturbation::SpliceForgedEntries(count) => {
            let mut edits = crate::writer::commit::ChildEdits::new();
            for serial in 0..count {
                let flag = writer.write_string("true")?;
                let leaf = writer.write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "match".to_owned(),
                        property_type: crate::PropertyType::Boolean,
                        values: PropertyValuesToWrite::Single(flag),
                    }],
                )?;
                let key = writer.write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "nowhere".to_owned(),
                        node: leaf,
                    },
                    &[],
                )?;
                edits.insert(format!("froe-forged-{serial}"), Some(key));
            }
            crate::writer::commit::rewrite_node_with_child_edits(
                store,
                writer,
                Some(index.record_identifier()),
                &edits,
            )?
        }
        SubtreePerturbation::AlterOneCount => {
            let own = counter_value(&index, "/", &built.path)?;
            let value = writer.write_string(&(own + 1).to_string())?;
            crate::writer::commit::rewrite_node_with_edits(
                store,
                writer,
                Some(index.record_identifier()),
                &crate::writer::commit::NodeEdits {
                    property_replacements: vec![PropertyToWrite {
                        name: ":cnt".to_owned(),
                        property_type: crate::PropertyType::Long,
                        values: PropertyValuesToWrite::Single(value),
                    }],
                    property_removals: Vec::new(),
                    child_edits: crate::writer::commit::ChildEdits::new(),
                },
            )?
        }
    };

    let mut edits = crate::writer::commit::ChildEdits::new();
    edits.insert(
        crate::index::INDEX_CONTENT_NODE_NAME.to_owned(),
        Some(new_index),
    );
    built.rebuilt_record = crate::writer::commit::rewrite_node_with_child_edits(
        store,
        writer,
        Some(built.rebuilt_record),
        &edits,
    )?;
    Ok(())
}

/// Plan 0006's entry half over the subtree just written.
///
/// The budget is the collected entry count plus one: one more than the walk
/// produced, so a correct index is never cut short and an index that somehow
/// holds more entries than were collected exhausts the budget rather than
/// passing quietly.
fn verify_entries(
    store: &WritableRepository,
    definition: &RebuiltDefinition,
    state_root: RecordIdentifier,
    entries: u64,
) -> Result<()> {
    let definition_node = crate::content::node::NodeState::new(store, definition.rebuilt_record);
    let state_root = crate::content::node::NodeState::new(store, state_root);
    let model = crate::index::IndexDefinition::read(&definition_node, &definition.path)
        .map_err(index_error_to_store_error)?;
    let report = crate::index::property::consistency::check_entries(
        &definition_node,
        &model,
        &state_root,
        crate::index::property::consistency::EntryCheckBudget::of_entries(
            entries.saturating_add(1),
        ),
    )
    .map_err(index_error_to_store_error)?;
    // Every fault is definite here. `check`'s "missing" category, which is
    // the one an operator cannot attribute, is the covered-node half's, and
    // that half is not run.
    if report.has_definite_faults() {
        return Err(Error::InvalidFormat {
            details: format!(
                "the index just rebuilt at {} does not agree with the content it indexes: \
                 {} stale, {} mismatched and {} duplicate entries",
                definition.path,
                report.stale_entries.len(),
                report.mismatched_entries.len(),
                report.duplicate_entries.len(),
            ),
        });
    }
    Ok(())
}

/// The counter's arm, which no general checker can run.
///
/// Three claims. Every node of the mirror resolves in the state root, so the
/// counter describes content that exists. Every `:cnt` is exactly what task
/// 0705's `credited_by_path` recorded for that path, and the mirror holds a
/// node for every credited path and no others — the map is the independent
/// record of what the walk counted, and no walk of the written tree can
/// recover it. And every `:cnt` is at least the sum of its children's, which
/// is the one claim about the counting itself rather than its transcription:
/// a credit reaches a node and all of its ancestors, so a parent's total can
/// never fall below what its children hold.
fn verify_counter(
    store: &WritableRepository,
    definition: &RebuiltDefinition,
    state_root: RecordIdentifier,
    credited_by_path: &BTreeMap<String, i64>,
) -> Result<()> {
    let definition_node = crate::content::node::NodeState::new(store, definition.rebuilt_record);
    let Some(index) = definition_node.child_node(crate::index::INDEX_CONTENT_NODE_NAME)? else {
        // No `:index` is a real outcome, not an empty tree: nothing hit.
        return Ok(());
    };
    let state_root = crate::content::node::NodeState::new(store, state_root);
    let mut seen = 0usize;
    verify_counter_node(
        &index,
        &state_root,
        "",
        credited_by_path,
        &definition.path,
        &mut seen,
    )?;
    if seen != credited_by_path.len() {
        return Err(Error::InvalidFormat {
            details: format!(
                "the counter just rebuilt at {} holds {seen} counted nodes where the \
                 build credited {}",
                definition.path,
                credited_by_path.len(),
            ),
        });
    }
    Ok(())
}

/// One counter node and its descendants, counting the nodes it visits.
fn verify_counter_node(
    node: &crate::content::node::NodeState<'_>,
    state_root: &crate::content::node::NodeState<'_>,
    path: &str,
    credited_by_path: &BTreeMap<String, i64>,
    definition_path: &str,
    seen: &mut usize,
) -> Result<()> {
    let absolute = if path.is_empty() { "/" } else { path };
    *seen += 1;
    if resolve(state_root, path)?.is_none() {
        return Err(Error::InvalidFormat {
            details: format!(
                "the counter just rebuilt at {definition_path} counts {absolute}, \
                 which is not a node of the state it counted"
            ),
        });
    }

    let own = counter_value(node, absolute, definition_path)?;
    let Some(credited) = credited_by_path.get(absolute).copied() else {
        return Err(Error::InvalidFormat {
            details: format!(
                "the counter just rebuilt at {definition_path} counts {absolute}, \
                 which the build credited nothing"
            ),
        });
    };
    if own != credited {
        return Err(Error::InvalidFormat {
            details: format!(
                "the counter just rebuilt at {definition_path} holds {own} at \
                 {absolute} where the build credited it {credited}"
            ),
        });
    }

    let mut children_total = 0i64;
    for (name, child) in node.child_node_entries()? {
        let child_path = format!("{path}/{name}");
        children_total += counter_value(&child, &child_path, definition_path)?;
        verify_counter_node(
            &child,
            state_root,
            &child_path,
            credited_by_path,
            definition_path,
            seen,
        )?;
    }
    if own < children_total {
        return Err(Error::InvalidFormat {
            details: format!(
                "the counter just rebuilt at {definition_path} holds {own} at \
                 {absolute} where its children hold {children_total}, which a credit \
                 that reaches every ancestor cannot produce"
            ),
        });
    }
    Ok(())
}

/// One node's `:cnt`, which every node of a counter mirror carries.
fn counter_value(
    node: &crate::content::node::NodeState<'_>,
    path: &str,
    definition_path: &str,
) -> Result<i64> {
    let property = node.property(":cnt")?;
    let Some(value) = property
        .as_ref()
        .and_then(|property| crate::index::converting_long(Some(property)))
    else {
        return Err(Error::InvalidFormat {
            details: format!(
                "the counter just rebuilt at {definition_path} has no readable :cnt at \
                 {path}, which every node of a counter mirror carries"
            ),
        });
    };
    Ok(value)
}

/// A path resolved against a state root, where the empty path is the root.
fn resolve<'store>(
    state_root: &crate::content::node::NodeState<'store>,
    path: &str,
) -> Result<Option<crate::content::node::NodeState<'store>>> {
    let mut node = *state_root;
    for element in path.split('/').filter(|element| !element.is_empty()) {
        let Some(child) = node.child_node(element)? else {
            return Ok(None);
        };
        node = child;
    }
    Ok(Some(node))
}

/// The same verification after a fresh reopen, plus the head identity, the
/// spine that reaches each rebuilt definition, and the single journal line.
///
/// The reopen is what proves the run is durable rather than merely
/// consistent in the session that wrote it: every record is read again
/// through a repository opened from the directory, with none of the writing
/// session's caches.
pub(super) fn verify_after_reopen(
    directory: &Path,
    published: RecordIdentifier,
    rebuilt: &[RebuiltDefinition],
    journal_lines_before: usize,
) -> Result<()> {
    let journal_lines_after = crate::journal::read_journal(&directory.join("journal.log"))?.len();
    if journal_lines_after != journal_lines_before + 1 {
        return Err(Error::InvalidFormat {
            details: format!(
                "the reindex left {journal_lines_after} journal lines where it found \
                 {journal_lines_before}, and a run appends exactly one"
            ),
        });
    }

    let repository = crate::store::Repository::open(directory)?;
    if repository.head_record_identifier() != published {
        return Err(Error::InvalidFormat {
            details: format!(
                "the reopened store's head is {} rather than the {published} just \
                 published",
                repository.head_record_identifier()
            ),
        });
    }
    for definition in rebuilt {
        crate::tooling::check::verify_node_tree(&repository, definition.rebuilt_record)?;
        // The spine: the head has to *reach* the rebuilt definition, not
        // merely hold a readable copy of it somewhere in the store.
        let reached = repository
            .node_at_path(&definition.path)?
            .map(|node| node.record_identifier());
        if reached != Some(definition.rebuilt_record) {
            return Err(Error::InvalidFormat {
                details: match reached {
                    Some(record) => format!(
                        "the published head reaches {record} at {}, not the {} the run \
                         built",
                        definition.path, definition.rebuilt_record
                    ),
                    None => format!(
                        "the published head does not reach {} at all",
                        definition.path
                    ),
                },
            });
        }
    }
    Ok(())
}
