//! The walk that reproduces what Oak's editors observe during a reindex.
//!
//! A reindex runs the definition's editor over a diff from the *missing*
//! state, so every node and every property arrives as an addition. froe
//! performs the same computation as a depth-first walk of the same state, and
//! this module is that walk.
//!
//! Four rules decide what is visited and what is indexed, each of them Oak's:
//!
//! * **Visible-editor semantics.** Every editor runs inside
//!   `VisibleEditor`, so a hidden child name and a hidden property name are
//!   never visited. `docs/analysis/index-property-storage.md` §6.3.
//! * **The path filter**, per node: `Exclude` prunes the subtree, `Traverse`
//!   descends without indexing, `Include` indexes.
//! * **The node-type predicate** from `declaringNodeTypes`, resolved against
//!   **the state root's own** `/jcr:system/jcr:nodeTypes` — as Oak's own
//!   cycle builds it, from the state it indexes and never from the head.
//! * **Key derivation**, which is `keys_for_property` and the per-node union
//!   below.
//!
//! # The reference collector is not the property collector
//!
//! `ReferenceEditorProvider.getIndexEditor` constructs its editor with no
//! `PathFilter` and no `TypePredicate` — neither class appears in it — so a
//! reference definition's `includedPaths`, `excludedPaths` and
//! `declaringNodeTypes` are **ignored**, and the only restrictions are the
//! version-store test and the visible-editor wrap (§8.2). A budget derived
//! from such a definition's `includedPaths` would bound a walk that in fact
//! covers the store, which is why the safety case derives it from `/`.

use std::collections::{BTreeSet, HashSet};

use crate::content::node::NodeState;
use crate::content::property::{PropertyType, PropertyValue};
use crate::error::{Error, Result};
use crate::index::definition::IndexDefinition;
use crate::index::path_filter::PathVerdict;
use crate::index::property::key_encoding::keys_for_property;
use crate::index::property::type_predicate::TypePredicate;
use crate::progress::{ProgressObserver, Step, WorkUnit, count, observe};
use crate::segment::record::RecordIdentifier;
use crate::writer::index::{IndexEntry, RunLocation, SortBudget};

/// The step the collector opens while it walks.
const COLLECT_STEP: &str = "collecting index entries";

/// The step the collector opens around the merge.
const SORT_STEP: &str = "sorting index entries";

/// `VersionConstants.VERSION_STORE_PATH`.
///
/// Compared with `startsWith` — a **plain string prefix test, not a path
/// ancestry test** — so a sibling named `/jcr:system/jcr:versionStorage2` is
/// excluded too. That is Oak's quirk, not a design, and froe reproduces it.
const VERSION_STORE_PATH: &str = "/jcr:system/jcr:versionStorage";

/// Where a collector puts the entries it derives.
///
/// The plan only *counts* — nothing is written, which is what the plan's
/// read-only contract needs — while the apply feeds the sort.
pub enum EntrySink {
    /// Count entries and their bytes, writing nothing.
    Count,
    /// Spill into runs under `location`, charged against `budget`.
    Runs {
        /// Where the runs are written.
        location: RunLocation,
        /// The budget every run set of this definition shares.
        budget: SortBudget,
    },
}

/// What a property collection produced.
pub enum CollectedEntries {
    /// From [`EntrySink::Count`].
    Counted {
        /// Entries the walk would have emitted.
        entries: u64,
        /// Their total resident bytes, for the work-directory estimate.
        bytes: u64,
    },
    /// From [`EntrySink::Runs`], sorted and ready to build from.
    Sorted(SortedEntries),
}

/// The sorted entries, as a public iterator over a public record.
///
/// The sort itself is crate-internal, so it never appears in a signature: a
/// consumer sees entries in order and nothing about how they got there.
pub struct SortedEntries {
    inner: crate::external_sort::SortedPass<'static, IndexEntry>,
    /// How many entries the walk emitted, which the builder reports and the
    /// verification budget uses.
    emitted: u64,
}

impl SortedEntries {
    /// How many entries the walk emitted.
    #[must_use]
    pub fn emitted(&self) -> u64 {
        self.emitted
    }
}

impl Iterator for SortedEntries {
    type Item = Result<IndexEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// What the walk cost, for the safety case's statement of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WalkAccounting {
    /// Nodes entered, whether or not they were indexed.
    pub nodes_visited: u64,
}

/// Collects the entries a property, unique or node-type definition indexes.
pub struct PropertyCollector;

impl PropertyCollector {
    /// Walks `state_root` and derives `definition`'s entries.
    ///
    /// `state_root` is the head for a synchronous definition and the lane's
    /// checkpoint root for an asynchronous one — the caller decides, because
    /// only the caller knows the lane.
    pub fn collect(
        state_root: &NodeState<'_>,
        definition: &IndexDefinition,
        sink: &EntrySink,
        observer: &mut dyn ProgressObserver,
    ) -> Result<(CollectedEntries, WalkAccounting)> {
        // Oak's own cycle builds the predicate from the state it indexes.
        let predicate = TypePredicate::new(state_root, &definition.property.declaring_node_types)
            .map_err(index_error_to_store_error)?;

        let mut accounting = WalkAccounting::default();
        let mut counted = (0u64, 0u64);
        let mut runs = match sink {
            EntrySink::Count => None,
            EntrySink::Runs { location, budget } => Some(crate::external_sort::SortedRuns::<
                IndexEntry,
            >::new(
                location.clone(), budget.clone()
            )),
        };

        let step = Step::new(COLLECT_STEP, WorkUnit::Nodes);
        observe(observer, &step, |observer| {
            walk_visible(state_root, |node, path| {
                accounting.nodes_visited += 1;
                observer.step_advanced(count(accounting.nodes_visited as usize));

                let verdict = definition.path_filter.filter(path);
                if verdict != PathVerdict::Include {
                    return Ok(());
                }
                if !predicate.is_empty()
                    && !predicate.test(node).map_err(index_error_to_store_error)?
                {
                    return Ok(());
                }
                for key in keys_for_node(node, definition)? {
                    let entry = IndexEntry::new(key, path);
                    match runs.as_mut() {
                        None => {
                            counted.0 += 1;
                            counted.1 += entry_bytes(&entry);
                        }
                        Some(runs) => {
                            counted.0 += 1;
                            runs.push(entry)?;
                        }
                    }
                }
                Ok(())
            })
        })?;

        let collected = match runs {
            None => CollectedEntries::Counted {
                entries: counted.0,
                bytes: counted.1,
            },
            Some(runs) => {
                let step = Step::new(SORT_STEP, WorkUnit::IndexEntries).with_total(counted.0);
                let sorted = observe(observer, &step, |_| runs.into_sorted())?;
                CollectedEntries::Sorted(SortedEntries {
                    inner: sorted,
                    emitted: counted.0,
                })
            }
        };
        Ok((collected, accounting))
    }
}

/// One node's key set.
///
/// Exactly the per-node set Oak's editor derives: `keys_for_property` over
/// every name in `propertyNames`, **unioned**, so a node whose two indexed
/// properties carry the same value contributes one entry rather than two.
/// Binaries and empty multi-valued properties contribute nothing, which
/// `keys_for_property` already decides.
fn keys_for_node(node: &NodeState<'_>, definition: &IndexDefinition) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for name in &definition.property.property_names {
        // Hidden property names are never visited: the visible-editor wrap
        // filters them before the editor sees them.
        if name.starts_with(':') {
            continue;
        }
        let Some(property) = node.property(name)? else {
            continue;
        };
        keys.extend(
            keys_for_property(
                &property,
                &definition.property.value_pattern,
                &definition.path,
            )
            .map_err(index_error_to_store_error)?,
        );
    }
    Ok(keys)
}

/// What a collection of references produced.
pub enum CollectedReferences {
    /// From [`EntrySink::Count`].
    Counted {
        /// Strong-reference entries.
        strong: u64,
        /// Weak-reference entries.
        weak: u64,
        /// Their total resident bytes.
        bytes: u64,
    },
    /// From [`EntrySink::Runs`].
    ///
    /// Boxed: the sorted variant carries two run sets and the counted one
    /// three integers, and an enum sized for the larger would make every
    /// plan-time count pay for the apply-time state.
    Sorted(Box<SortedReferenceSets>),
}

/// The two sorted reference sets, merged one at a time.
///
/// `weak()` does not open its merge until the iterator `strong()` returned
/// has been dropped, which is what keeps the open-file bound at the fan-in
/// plus one rather than twice it — the same discipline plan 0009 uses to
/// merge one format at a time.
pub struct SortedReferenceSets {
    strong_runs: Option<crate::external_sort::SortedRuns<IndexEntry>>,
    weak_runs: Option<crate::external_sort::SortedRuns<IndexEntry>>,
    /// Whether each set received an entry. Oak creates `:references` and
    /// `:weakreferences` only on the first insert, so a set that stayed
    /// empty must produce no hidden child at all.
    strong_seen: bool,
    weak_seen: bool,
}

impl SortedReferenceSets {
    /// Whether the strong set received any entry.
    #[must_use]
    pub fn has_strong(&self) -> bool {
        self.strong_seen
    }

    /// Whether the weak set received any entry.
    #[must_use]
    pub fn has_weak(&self) -> bool {
        self.weak_seen
    }

    /// The strong set, sorted. Call before [`Self::weak`].
    pub fn strong(&mut self) -> Result<Option<SortedEntries>> {
        let Some(runs) = self.strong_runs.take() else {
            return Ok(None);
        };
        Ok(Some(SortedEntries {
            inner: runs.into_sorted()?,
            emitted: 0,
        }))
    }

    /// The weak set, sorted.
    ///
    /// Merged only now, so the strong set's files are closed first.
    ///
    /// # Panics
    ///
    /// If [`Self::strong`] has not been called. The order is what keeps the
    /// open-file bound at the fan-in plus one rather than twice it, so a
    /// caller that skips the strong set is asking for a bound this type
    /// promises not to exceed — an API misuse rather than a runtime
    /// condition, which is why it is an assertion and not an error.
    pub fn weak(&mut self) -> Result<Option<SortedEntries>> {
        assert!(
            self.strong_runs.is_none(),
            "the weak set is merged only after the strong set, so the open-file \
             bound stays at the fan-in plus one"
        );
        let Some(runs) = self.weak_runs.take() else {
            return Ok(None);
        };
        Ok(Some(SortedEntries {
            inner: runs.into_sorted()?,
            emitted: 0,
        }))
    }
}

/// Collects the reference index's two entry sets.
pub struct ReferenceCollector;

/// The two run sets and their counters, so the per-node emission is one
/// function rather than four levels of nesting inside the walk.
struct ReferenceEmitter {
    strong_runs: Option<crate::external_sort::SortedRuns<IndexEntry>>,
    weak_runs: Option<crate::external_sort::SortedRuns<IndexEntry>>,
    strong: u64,
    weak: u64,
    bytes: u64,
}

impl ReferenceEmitter {
    fn new(sink: &EntrySink) -> Self {
        let (strong_runs, weak_runs) = match sink {
            EntrySink::Count => (None, None),
            EntrySink::Runs { location, budget } => (
                Some(crate::external_sort::SortedRuns::new(
                    RunLocation::new(
                        location.directory(),
                        format!("{}-strong", location.name_prefix()),
                    ),
                    budget.clone(),
                )),
                Some(crate::external_sort::SortedRuns::new(
                    RunLocation::new(
                        location.directory(),
                        format!("{}-weak", location.name_prefix()),
                    ),
                    budget.clone(),
                )),
            ),
        };
        Self {
            strong_runs,
            weak_runs,
            strong: 0,
            weak: 0,
            bytes: 0,
        }
    }

    /// Emits every reference one node carries.
    fn emit_node(&mut self, node: &NodeState<'_>, path: &str) -> Result<()> {
        // Strong references under the version store are skipped; weak ones
        // never are.
        let in_version_store = path.starts_with(VERSION_STORE_PATH);
        for property in node.properties()? {
            // Hidden property names are never visited: the visible-editor
            // wrap filters them before the editor sees them.
            if property.name.starts_with(':') {
                continue;
            }
            let strong = match property.property_type {
                PropertyType::Reference => true,
                PropertyType::WeakReference => false,
                _ => continue,
            };
            if strong && in_version_store {
                continue;
            }
            self.emit_property(&property, path, strong)?;
        }
        Ok(())
    }

    fn emit_property(
        &mut self,
        property: &crate::content::node::PropertyState,
        path: &str,
        strong: bool,
    ) -> Result<()> {
        // Oak collects the values into a set, so a multi-valued property
        // listing one identifier twice is one entry.
        let identifiers: BTreeSet<String> = crate::index::values_of(property)
            .iter()
            .filter_map(PropertyValue::as_text)
            .collect();
        let property_path = relative_property_path(path, &property.name);
        for identifier in identifiers {
            // The key is the referenced identifier **unencoded** — the
            // property's own value, with no URL encoding and no truncation.
            let entry = IndexEntry::new(identifier, &property_path);
            self.bytes += entry_bytes(&entry);
            let runs = if strong {
                self.strong += 1;
                self.strong_runs.as_mut()
            } else {
                self.weak += 1;
                self.weak_runs.as_mut()
            };
            if let Some(runs) = runs {
                runs.push(entry)?;
            }
        }
        Ok(())
    }

    fn finish(self) -> CollectedReferences {
        let (strong_seen, weak_seen) = (self.strong > 0, self.weak > 0);
        match (self.strong_runs, self.weak_runs) {
            (Some(strong_runs), Some(weak_runs)) => {
                CollectedReferences::Sorted(Box::new(SortedReferenceSets {
                    strong_runs: Some(strong_runs),
                    weak_runs: Some(weak_runs),
                    strong_seen,
                    weak_seen,
                }))
            }
            _ => CollectedReferences::Counted {
                strong: self.strong,
                weak: self.weak,
                bytes: self.bytes,
            },
        }
    }
}

impl ReferenceCollector {
    /// Walks `state_root` and derives both reference sets.
    ///
    /// No path filter and no type predicate: see the module documentation.
    pub fn collect(
        state_root: &NodeState<'_>,
        sink: &EntrySink,
        observer: &mut dyn ProgressObserver,
    ) -> Result<(CollectedReferences, WalkAccounting)> {
        let mut accounting = WalkAccounting::default();
        let mut emitter = ReferenceEmitter::new(sink);

        let step = Step::new(COLLECT_STEP, WorkUnit::Nodes);
        observe(observer, &step, |observer| {
            walk_visible(state_root, |node, path| {
                accounting.nodes_visited += 1;
                observer.step_advanced(count(accounting.nodes_visited as usize));
                emitter.emit_node(node, path)
            })
        })?;

        Ok((emitter.finish(), accounting))
    }
}

/// The property's path with the leading `/` stripped, as Oak's `put` stores
/// it: an entry for `/content/a/@ref` is stored at
/// `:references/<uuid>/content/a/ref`.
fn relative_property_path(node_path: &str, property_name: &str) -> String {
    let absolute = if node_path == "/" {
        format!("/{property_name}")
    } else {
        format!("{node_path}/{property_name}")
    };
    absolute.strip_prefix('/').unwrap_or(&absolute).to_owned()
}

fn entry_bytes(entry: &IndexEntry) -> u64 {
    (entry.key.len() + entry.path.len()) as u64
}

/// A depth-first walk of the visible tree, carrying its own stack.
///
/// Hidden child names are never entered, which is the visible-editor wrap.
/// The path set is exact rather than a depth bound, so a spliced
/// self-reference is a refusal rather than an unbounded walk — the same rule
/// `content/traversal.rs` applies.
pub(crate) fn walk_visible(
    root: &NodeState<'_>,
    mut visit: impl FnMut(&NodeState<'_>, &str) -> Result<()>,
) -> Result<()> {
    enum Step<'provider> {
        Visit {
            node: NodeState<'provider>,
            path: String,
        },
        Leave {
            record: RecordIdentifier,
        },
    }

    let mut stack = vec![Step::Visit {
        node: *root,
        path: String::new(),
    }];
    let mut ancestors: HashSet<RecordIdentifier> = HashSet::new();

    while let Some(step) = stack.pop() {
        match step {
            Step::Leave { record } => {
                ancestors.remove(&record);
            }
            Step::Visit { node, path } => {
                let record = node.record_identifier();
                if !ancestors.insert(record) {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "the node at {} is its own ancestor, so the index cannot be \
                             rebuilt from it; this is corruption, not deep content",
                            if path.is_empty() { "/" } else { &path }
                        ),
                    });
                }
                stack.push(Step::Leave { record });

                visit(&node, if path.is_empty() { "/" } else { &path })?;

                let mut entries = node.child_node_entries()?;
                entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                for (name, child) in entries.into_iter().rev() {
                    if name.starts_with(':') {
                        continue;
                    }
                    stack.push(Step::Visit {
                        node: child,
                        path: format!("{path}/{name}"),
                    });
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn index_error_to_store_error(error: crate::index::IndexError) -> Error {
    match error {
        crate::index::IndexError::Record(source) => source,
        other => Error::InvalidFormat {
            details: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{ReferenceEmitter, SortedReferenceSets};
    use crate::external_sort::{
        SortedRuns, merge_passes, peak_open_run_files, reset_sort_accounting,
    };
    use crate::writer::index::{IndexEntry, RunLocation, SortBudget};

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "froe-reference-merge-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the run directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// The two reference sets are merged one at a time, so the open-file
    /// bound stays at the fan-in plus one rather than twice it.
    ///
    /// In-crate rather than in the integration file because the accounting
    /// the external sort exposes is crate-internal — the same placement task
    /// 0707 gives its gate-seam test.
    #[test]
    fn the_two_reference_sets_merge_one_at_a_time() {
        let directory = TestDirectory::new("one-at-a-time");
        // One record per run, so both sets spill heavily and each merge
        // opens many files.
        let budget = SortBudget::of_bytes(8);
        let mut sets = SortedReferenceSets {
            strong_runs: Some(SortedRuns::new(
                RunLocation::new(&directory.path, "strong"),
                budget.clone(),
            )),
            weak_runs: Some(SortedRuns::new(
                RunLocation::new(&directory.path, "weak"),
                budget.clone(),
            )),
            strong_seen: true,
            weak_seen: true,
        };
        for index in 0..200u32 {
            let entry = IndexEntry::new(format!("{index:08}"), "/content/a");
            sets.strong_runs
                .as_mut()
                .expect("the strong set")
                .push(entry.clone())
                .expect("push");
            sets.weak_runs
                .as_mut()
                .expect("the weak set")
                .push(entry)
                .expect("push");
        }

        reset_sort_accounting();
        let strong: Vec<IndexEntry> = sets
            .strong()
            .expect("open the strong set")
            .expect("it exists")
            .collect::<crate::Result<Vec<_>>>()
            .expect("read the strong set");
        assert_eq!(strong.len(), 200);
        let after_strong = peak_open_run_files();
        assert!(
            after_strong <= crate::external_sort::MAXIMUM_FAN_IN + 1,
            "the strong merge held {after_strong} files"
        );

        let weak: Vec<IndexEntry> = sets
            .weak()
            .expect("open the weak set")
            .expect("it exists")
            .collect::<crate::Result<Vec<_>>>()
            .expect("read the weak set");
        assert_eq!(weak.len(), 200);
        assert!(
            peak_open_run_files() <= crate::external_sort::MAXIMUM_FAN_IN + 1,
            "merging the weak set after the strong one took the peak to {} — the two \
             sets must not be open at once",
            peak_open_run_files()
        );
        assert!(merge_passes() >= 2, "both sets needed a reduction pass");
    }

    /// A set with no entry reports itself empty, so the caller writes no
    /// hidden child — Oak creates `:references` and `:weakreferences` only
    /// on the first insert.
    #[test]
    fn an_emitter_that_saw_nothing_reports_both_sets_empty() {
        let emitter = ReferenceEmitter::new(&super::EntrySink::Count);
        match emitter.finish() {
            super::CollectedReferences::Counted { strong, weak, .. } => {
                assert_eq!((strong, weak), (0, 0));
            }
            super::CollectedReferences::Sorted(_) => panic!("the counting sink counts"),
        }
    }
}
