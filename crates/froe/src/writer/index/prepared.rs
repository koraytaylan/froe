//! Planning and the lock boundary.
//!
//! `plan_reindex` is read-only and lockless: it opens the store read-only,
//! selects, and counts what each definition would produce **without writing
//! anything**. That is what an operator confirms.
//!
//! `PreparedReindex::prepare` is the lock boundary, and it follows
//! compaction's open protocol step for step — not approximately, and not by
//! reimplementation: every gate it calls is the one compaction calls, widened
//! for this module by task 0715.
//!
//! The order matters, and the safety case says why:
//!
//! 1. `validate_repository_shape`, then the two pre-lock identity gates.
//! 2. `RepositoryLock::acquire`, then `validate_path_identity`.
//! 3. **Both gates again**, because the path can change between the lockless
//!    check and the lock.
//! 4. A replan from disk — **no record identity from the lockless plan
//!    survives**, which is what makes the preview safe to show and unsafe to
//!    use.
//! 5. The directory fingerprint and the certified archive number.
//! 6. `validate_metadata_source_apply_identity`, which refuses a store whose
//!    newest active archive could not be re-owned under `open_prepared`'s
//!    `preserve_file_metadata` — that runs inside `flush`, so without this
//!    gate the failure would surface only after every record was written.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::index::inventory::IndexInventory;
use crate::progress::{DiscardedProgress, ProgressObserver};
use crate::store::Repository;
use crate::writer::index::ReindexOptions;
use crate::writer::index::counter_builder::CounterBuilder;
use crate::writer::index::plan::{
    NoWorkReason, ReindexAction, ReindexPlan, ReindexWarning, work_directory_estimate,
};
use crate::writer::index::property_collector::{
    CollectedEntries, CollectedReferences, EntrySink, PropertyCollector, ReferenceCollector,
};
use crate::writer::index::selection::{IndexingState, SelectionOptions, select};
use crate::writer::maintenance::{
    DirectoryFingerprint, available_filesystem_bytes, canonical_repository_directory,
    directory_fingerprint, validate_apply_environment, validate_apply_identity,
    validate_metadata_source_apply_identity, validate_repository_shape,
};
use crate::writer::repository_lock::RepositoryLock;

/// The prefix a run's own subdirectory carries, so residue is recognizable
/// as froe's rather than as something the operator put there.
pub(crate) const RUN_DIRECTORY_PREFIX: &str = "froe-reindex-";

/// Plans a reindex without acquiring the lock or changing a byte.
pub fn plan_reindex(directory: &Path, options: &ReindexOptions) -> Result<ReindexPlan> {
    plan_reindex_with_progress(directory, options, &mut DiscardedProgress)
}

/// Plans exactly like [`plan_reindex`], reporting the counting walk — the
/// slow part — to `observer`.
pub fn plan_reindex_with_progress(
    directory: &Path,
    options: &ReindexOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<ReindexPlan> {
    let directory = canonical_repository_directory(directory)?;
    validate_repository_shape(&directory)?;
    let repository = Repository::open(&directory)?;
    build_plan(&directory, &repository, options, observer)
}

/// The plan, from a store already open read-only.
fn build_plan(
    directory: &Path,
    repository: &Repository,
    options: &ReindexOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<ReindexPlan> {
    let super_root = repository.head();
    let inventory =
        IndexInventory::collect(repository, &super_root).map_err(index_error_to_store_error)?;
    let selection = select(
        repository,
        &super_root,
        &inventory,
        &SelectionOptions {
            requested_paths: options.requested_paths().to_vec(),
            from_head: options.from_head(),
        },
    )?;

    let mut actions = Vec::with_capacity(selection.selected.len());
    let mut peak_estimate = 0u64;
    for selected in &selection.selected {
        let action = plan_one(repository, selected, options, observer)?;
        if let ReindexAction::Rebuild { entry_bytes, .. } = &action {
            peak_estimate = peak_estimate.max(work_directory_estimate(
                *entry_bytes,
                options.sort_budget_bytes() as u64,
            ));
        }
        actions.push(action);
    }

    let mut warnings: Vec<ReindexWarning> = selection
        .refused
        .into_iter()
        .map(|refusal| ReindexWarning::Skipped { refusal })
        .collect();

    let work_directory = options.work_directory().path();
    // Residue: a refusal in a directory the operator named, a warning under
    // the default. A subdirectory whose lock file is held belongs to a live
    // run and is neither.
    for residue in run_directory_residue(&work_directory)? {
        if options.work_directory().is_operator_named() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} is left over from an earlier froe reindex; remove it, or name a \
                     different --work-directory",
                    residue.display()
                ),
            });
        }
        warnings.push(ReindexWarning::ResidueUnderDefaultWorkDirectory { directory: residue });
    }

    if peak_estimate > 0
        && let Some(available) = available_filesystem_bytes(&work_directory)
        && available < peak_estimate
    {
        warnings.push(ReindexWarning::WorkDirectoryMayBeTooSmall {
            directory: work_directory.clone(),
            estimated_bytes: peak_estimate,
            available_bytes: available,
        });
    }

    Ok(ReindexPlan {
        directory: directory.to_path_buf(),
        actions,
        warnings,
        work_directory,
        work_directory_estimate_bytes: peak_estimate,
    })
}

/// One definition's action, counted rather than built.
fn plan_one(
    repository: &Repository,
    selected: &crate::writer::index::selection::SelectedIndex,
    options: &ReindexOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<ReindexAction> {
    let path = selected.definition.path.clone();

    if let IndexingState::ResetForReplay { lane } = &selected.state {
        let definition =
            crate::content::node::NodeState::new(repository, selected.definition_record);
        let removable = removable_hidden_children(&definition)?;
        if removable.is_empty() {
            return Ok(ReindexAction::NothingToDo {
                path,
                reason: NoWorkReason::NoRemovableHiddenChild,
            });
        }
        return Ok(ReindexAction::Reset {
            path,
            lane: lane.clone(),
            hidden_children: removable,
        });
    }

    let Some(state_record) = selected.state_root else {
        return Ok(ReindexAction::NothingToDo {
            path,
            reason: NoWorkReason::NoRemovableHiddenChild,
        });
    };
    let state_root = crate::content::node::NodeState::new(repository, state_record);

    let (entries, entry_bytes) = match selected.definition.index_type.as_ref() {
        Some(crate::index::IndexType::Counter) => {
            let builder = CounterBuilder::new(&selected.definition);
            let hits = builder.count_hits(&state_root)?;
            // A counter writes one node per credited path; its "entries" are
            // those nodes, and it spills nothing.
            (hits.credited_nodes as u64, 0)
        }
        Some(crate::index::IndexType::Reference) => {
            let (collected, _) =
                ReferenceCollector::collect(&state_root, &EntrySink::Count, observer)?;
            match collected {
                CollectedReferences::Counted {
                    strong,
                    weak,
                    bytes,
                } => (strong + weak, bytes),
                CollectedReferences::Sorted(_) => {
                    unreachable!("the counting sink never sorts")
                }
            }
        }
        _ => {
            let (collected, _) = PropertyCollector::collect(
                &state_root,
                &selected.definition,
                &EntrySink::Count,
                observer,
            )?;
            match collected {
                CollectedEntries::Counted { entries, bytes } => (entries, bytes),
                CollectedEntries::Sorted(_) => unreachable!("the counting sink never sorts"),
            }
        }
    };

    let _ = options;
    Ok(ReindexAction::Rebuild {
        path,
        state: selected.state.clone(),
        entries,
        entry_bytes,
    })
}

/// The hidden children a reindex would remove: every one not flagged
/// `retainNodeInReindex`.
fn removable_hidden_children(
    definition: &crate::content::node::NodeState<'_>,
) -> Result<Vec<String>> {
    let mut removable = Vec::new();
    for (name, child) in definition.child_node_entries()? {
        if !name.starts_with(':') {
            continue;
        }
        if crate::index::strict_boolean(child.property("retainNodeInReindex")?.as_ref()) {
            continue;
        }
        removable.push(name);
    }
    Ok(removable)
}

/// froe-named subdirectories of `directory` that no live run holds.
fn run_directory_residue(directory: &Path) -> Result<Vec<PathBuf>> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        // A work directory that does not exist yet has no residue; the run
        // creates it.
        return Ok(Vec::new());
    };
    let mut residue = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(RUN_DIRECTORY_PREFIX) || !entry.path().is_dir() {
            continue;
        }
        // A subdirectory whose lock file is held belongs to a live run.
        if RepositoryLock::acquire(&entry.path()).is_err() {
            continue;
        }
        residue.push(entry.path());
    }
    residue.sort();
    Ok(residue)
}

/// A reindex planned under the repository lock, ready to apply.
pub struct PreparedReindex {
    pub(crate) directory: PathBuf,
    pub(crate) options: ReindexOptions,
    pub(crate) plan: ReindexPlan,
    pub(crate) fingerprint: DirectoryFingerprint,
    pub(crate) certified_archive_number: u32,
    pub(crate) repository_lock: Arc<RepositoryLock>,
}

impl PreparedReindex {
    /// Takes the lock, replans from disk and runs every gate.
    pub fn prepare(directory: &Path, options: ReindexOptions) -> Result<Self> {
        Self::prepare_with_progress(directory, options, &mut DiscardedProgress)
    }

    /// Prepares exactly like [`PreparedReindex::prepare`], reporting the
    /// counting walk to `observer`.
    pub fn prepare_with_progress(
        directory: &Path,
        options: ReindexOptions,
        observer: &mut dyn ProgressObserver,
    ) -> Result<Self> {
        let directory = canonical_repository_directory(directory)?;
        validate_repository_shape(&directory)?;
        validate_apply_environment(&directory)?;
        validate_apply_identity(&directory)?;

        let repository_lock = Arc::new(RepositoryLock::acquire(&directory)?);
        // The path may have changed between the lockless checks and the
        // lock, so every one of them runs again against the locked state.
        validate_repository_shape(&directory)?;
        validate_apply_environment(&directory)?;
        validate_apply_identity(&directory)?;
        repository_lock.validate_path_identity(&directory)?;

        // Replanned from disk: no record identity from a lockless plan
        // survives this point.
        let repository = Repository::open(&directory)?;
        let plan = build_plan(&directory, &repository, &options, observer)?;
        let fingerprint = directory_fingerprint(&directory)?;
        let certified_archive_number =
            crate::writer::store_writer::next_cleanup_archive_number(&directory)?;
        // Last, and before anything is written: `preserve_file_metadata`
        // runs inside `flush`, so a store this gate would refuse fails only
        // after every record is on disk without it.
        validate_metadata_source_apply_identity(&directory)?;

        drop(repository);
        Ok(Self {
            directory,
            options,
            plan,
            fingerprint,
            certified_archive_number,
            repository_lock,
        })
    }

    /// The plan this run will apply, replanned under the lock.
    #[must_use]
    pub fn plan(&self) -> &ReindexPlan {
        &self.plan
    }

    /// Rechecks the fingerprint and the path identity, immediately before
    /// the destructive step.
    ///
    /// The fingerprint deliberately skips `repo.lock`, so only the identity
    /// check catches a replaced lock file — which is why both run.
    pub(crate) fn recheck_before_mutation(&self) -> Result<()> {
        let current = directory_fingerprint(&self.directory)?;
        if current != self.fingerprint {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} changed between planning and applying; refusing before any \
                     index record is written",
                    self.directory.display()
                ),
            });
        }
        self.repository_lock.validate_path_identity(&self.directory)
    }
}

impl PreparedReindex {
    /// Applies the prepared reindex.
    pub fn apply(self) -> Result<crate::writer::index::apply::ReindexOutcome> {
        crate::writer::index::apply::apply_prepared(&self, &mut DiscardedProgress)
    }

    /// Applies exactly like [`PreparedReindex::apply`], reporting each step
    /// to `observer`. Reporting cannot alter the mutation sequence: the
    /// observer is told what has already been done and never decides
    /// anything.
    pub fn apply_with_progress(
        self,
        observer: &mut dyn ProgressObserver,
    ) -> Result<crate::writer::index::apply::ReindexOutcome> {
        crate::writer::index::apply::apply_prepared(&self, observer)
    }
}

/// Prepares under the lock and applies immediately.
///
/// The non-interactive convenience. An interactive caller should use
/// [`plan_reindex`] and [`PreparedReindex`], so it can show the plan and
/// take a confirmation while the lock is held.
pub fn reindex(
    directory: &Path,
    options: ReindexOptions,
) -> Result<crate::writer::index::apply::ReindexOutcome> {
    reindex_with_progress(directory, options, &mut DiscardedProgress)
}

/// Prepares and applies exactly like [`reindex`], reporting both phases.
pub fn reindex_with_progress(
    directory: &Path,
    options: ReindexOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<crate::writer::index::apply::ReindexOutcome> {
    PreparedReindex::prepare_with_progress(directory, options, observer)?
        .apply_with_progress(observer)
}

fn index_error_to_store_error(error: crate::index::IndexError) -> Error {
    match error {
        crate::index::IndexError::Record(source) => source,
        other => Error::InvalidFormat {
            details: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::PreparedReindex;
    use crate::writer::index::ReindexOptions;
    use crate::writer::maintenance::gate_observation::{recorded, reset};
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn repository(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "froe-reindex-gates-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the test directory");
            WritableRepository::open(&path)
                .expect("bootstrap")
                .close()
                .expect("close the bootstrap");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A reindex's `prepare` runs the same open protocol compaction's does.
    ///
    /// The second user of task 0706's seam, and the reason it exists: a
    /// `prepare` that dropped a gate would plan correctly and apply
    /// correctly against a healthy store, and only fail to refuse an
    /// unhealthy one — an absence nothing downstream can observe. Asserting
    /// the wiring directly is the only way to pin it.
    #[test]
    fn a_prepare_runs_the_whole_open_protocol() {
        let directory = TestDirectory::repository("open-protocol");
        reset();
        let prepared = PreparedReindex::prepare(&directory.path, ReindexOptions::new())
            .expect("prepare against a healthy store");

        let calls = recorded();
        let gates: Vec<&str> = calls.iter().map(|(gate, _)| *gate).collect();
        for expected in [
            "validate_repository_shape",
            "validate_apply_environment",
            "validate_apply_identity",
            "validate_metadata_source_apply_identity",
        ] {
            assert!(
                gates.contains(&expected),
                "{expected} was not called during prepare: {gates:?}"
            );
        }

        // Before *and* again after the lock: a check that ran only before it
        // proves nothing about the state the apply will act on.
        for repeated in [
            "validate_repository_shape",
            "validate_apply_environment",
            "validate_apply_identity",
        ] {
            assert!(
                gates.iter().filter(|gate| **gate == repeated).count() >= 2,
                "{repeated} must run before and again after the lock: {gates:?}"
            );
        }

        let canonical =
            std::fs::canonicalize(&directory.path).expect("canonicalize the repository");
        for (gate, seen) in &calls {
            assert_eq!(
                seen,
                &canonical,
                "{gate} received {} rather than the canonicalized directory",
                seen.display()
            );
        }
        drop(prepared);
    }
}
