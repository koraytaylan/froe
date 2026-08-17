//! The lock-protected session between a plan and its application, and
//! the four entry points callers reach maintenance through.

use super::apply::apply_prepared;
use super::apply_identity::{
    current_apply_credentials, metadata_source_apply_identity_issue, possible_created_group_ids,
    validate_apply_environment, validate_apply_identity, validate_plan_apply_identity,
};
use super::options::{CompactionOptions, MaintenanceTask};
use super::plan::{CompactionOutcome, CompactionPlan};
use super::planning::{
    ManifestUpgradeOnFirstInstall, attach_completed_repairs, build_plan,
    canonical_repository_directory, validate_options, validate_repository_shape,
};
use crate::error::{Error, Result};
use crate::progress::{DiscardedProgress, ProgressObserver};
use crate::writer::repository_lock::RepositoryLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// An authoritative cleanup plan protected by the held repository lock.
pub struct PreparedCompaction {
    pub(super) directory: PathBuf,
    pub(super) options: CompactionOptions,
    pub(super) plan: CompactionPlan,
    /// Archives whose index this preparation already rebuilt, before the
    /// plan was built. Carried so the outcome can report work that, by
    /// construction, happened before there was a plan to record it in.
    pub(super) repaired: Vec<crate::writer::store_writer::RepairedArchive>,
    pub(super) repository_lock: Arc<RepositoryLock>,
}

impl PreparedCompaction {
    /// Resolves the repository path once to its canonical absolute target,
    /// validates it without mutation, acquires `repo.lock`, and rebuilds an
    /// authoritative plan while holding that lock.
    pub fn prepare(directory: &Path, options: CompactionOptions) -> Result<Self> {
        Self::prepare_with_progress(directory, options, &mut DiscardedProgress)
    }

    /// Prepares exactly like [`PreparedCompaction::prepare`], reporting the
    /// authoritative replan — the slow part, repeated under the lock — to
    /// `observer`.
    pub fn prepare_with_progress(
        directory: &Path,
        options: CompactionOptions,
        observer: &mut dyn ProgressObserver,
    ) -> Result<Self> {
        validate_options(&options)?;
        let directory = canonical_repository_directory(directory)?;
        validate_repository_shape(&directory)?;
        // The store version, before anything is written. `build_plan` reaches
        // this through `Repository::open`, but the repair below runs first, so
        // without it here froe would rewrite the archives of a store it then
        // declares itself unable to read — and a caller using the library API
        // directly never gets the lockless preview that would have refused.
        crate::store::check_manifest(&directory, crate::store::ArchivePresence::Present)?;
        validate_apply_environment(&directory)?;
        validate_apply_identity(&directory)?;
        let repository_lock = Arc::new(RepositoryLock::acquire(&directory)?);
        // The path may have changed between the lockless shape check and lock
        // acquisition. Revalidate every managed type while the cooperative
        // repository lock is held before reading the authoritative plan.
        validate_repository_shape(&directory)?;
        crate::store::check_manifest(&directory, crate::store::ArchivePresence::Present)?;
        validate_apply_environment(&directory)?;
        validate_apply_identity(&directory)?;
        repository_lock.validate_path_identity(&directory)?;
        let repaired = Self::repair_before_planning(&directory, &options, observer)?;
        let now = SystemTime::now();
        let plan = build_plan(&directory, &options, now, observer).map_err(|error| {
            // The repair already happened and is durable. Every gate below it
            // — the duplicate-segment check, the generation invariant, the
            // segment plan — is evaluating this store for the first time in
            // its history, precisely because the index-less state suppressed
            // them before. So this path is ordinary, not exotic, and a
            // refusal that did not mention the rewrite would leave the
            // operator believing nothing moved.
            attach_completed_repairs(error, &repaired)
        })?;
        // Inside the same guard: this is the one identity gate that can only
        // fire after the repair, because the archives it inspects are the
        // ones the repair just wrote.
        validate_plan_apply_identity(&directory, &plan)
            .map_err(|error| attach_completed_repairs(error, &repaired))?;
        Ok(Self {
            directory,
            options,
            plan,
            repaired,
            repository_lock,
        })
    }

    /// Rebuilds index-less archives, when the task is selected and there is
    /// something to rebuild.
    ///
    /// This is the only mutation that precedes planning, because every
    /// index-dependent decision is impossible until it has run — the preview
    /// could name it and nothing more. It is gated on the *scan*, never on
    /// task selection alone: a store with nothing to repair must come out of
    /// a repair run byte-identical, including its manifest.
    fn repair_before_planning(
        directory: &Path,
        options: &CompactionOptions,
        observer: &mut dyn ProgressObserver,
    ) -> Result<Vec<crate::writer::store_writer::RepairedArchive>> {
        if !options.contains(MaintenanceTask::RepairArchives) {
            return Ok(Vec::new());
        }
        // One predicate for the whole decision. `repairable` — not merely
        // "index-less" — is what gates the irreversible steps below, because
        // a number that cannot be rebuilt makes the run fail however it is
        // retried, and paying a manifest upgrade or a durable rewrite to
        // discover that is exactly the trade this ordering exists to avoid.
        let survey = crate::writer::store_writer::survey_indexless_archive_numbers(directory)?;
        if !survey.unrepairable.is_empty() {
            return Err(crate::writer::store_writer::unrepairable_archives_refusal(
                &survey.unrepairable,
            ));
        }
        if survey.repairable == 0 {
            return Ok(Vec::new());
        }
        // Duplicate `(number, letter)` pairs before the upgrade, not after:
        // the repair refuses them, and a refusal must not cost a one-way
        // manifest transition. `Repository::open` rejects such a store, so
        // only a library caller skipping the preview reaches this.
        crate::writer::store_writer::reject_duplicate_archive_generations(directory)?;
        // Ownership of the archives about to be rewritten, from stat(2),
        // before anything is touched. A rebuild ends in
        // `preserve_file_metadata`, whose `fchown` fails EPERM when the
        // target belongs to another uid — and the newest archive of a store
        // whose Oak ran as root is exactly the killed-writer artifact this
        // task exists to repair. Ownership does not change on retry, so
        // discovering it after the rewrite means no rerun ever converges.
        // Nothing else covers these files: `journal_service_user_issue`
        // stats only `journal.log`, and `planned_metadata_sources` is
        // consulted after the repair has already run.
        Self::validate_repair_target_identity(directory)?;
        // A rebuilt archive carries a version-2 trailer, so a version-1 store
        // is raised first — but only at the instant one is about to become
        // visible, never merely because a rebuild was predicted. A repair can
        // still fail per archive for reasons no survey models (a full disk, a
        // blob catalog that will not resolve), and paying an irreversible
        // format transition for a run that then rebuilds nothing would leave
        // the store damaged *and* unopenable by an older Oak.
        let manifest_upgrade = &mut ManifestUpgradeOnFirstInstall::new(directory);
        crate::writer::store_writer::repair_indexless_archive_numbers(
            directory,
            observer,
            manifest_upgrade,
        )
    }

    /// Refuses before the first rewrite when an archive the repair would
    /// replace cannot have its metadata preserved by this process.
    fn validate_repair_target_identity(directory: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

            let credentials = current_apply_credentials()?;
            let directory_metadata = std::fs::symlink_metadata(directory)?;
            let possible_created_gids = possible_created_group_ids(
                directory_metadata.gid(),
                directory_metadata.permissions().mode(),
                &credentials,
            );
            for name in crate::writer::store_writer::repair_target_names(directory)? {
                let path = directory.join(&name);
                let metadata = std::fs::symlink_metadata(&path)?;
                if let Some(issue) = metadata_source_apply_identity_issue(
                    &path,
                    metadata.uid(),
                    metadata.gid(),
                    metadata.permissions().mode(),
                    &possible_created_gids,
                    &credentials,
                ) {
                    return Err(Error::InvalidFormat { details: issue });
                }
            }
        }
        #[cfg(not(unix))]
        let _ = directory;
        Ok(())
    }

    /// The lock-protected plan callers should display and confirm.
    #[must_use]
    pub fn plan(&self) -> &CompactionPlan {
        &self.plan
    }

    /// Archive indexes this preparation already rebuilt.
    ///
    /// Non-zero only for a `repair-archives` run, and non-zero *before*
    /// anything is applied: the rebuild is what made the plan computable, so
    /// it is durable by the time a caller sees this. A caller that declines
    /// the plan has still had these archives rewritten, and should say so.
    #[must_use]
    pub fn repaired_archives(&self) -> usize {
        self.repaired.len()
    }

    /// Applies exactly this authoritative plan, failing before the first
    /// mutation if any directory entry changed after planning.
    pub fn apply(self) -> Result<CompactionOutcome> {
        apply_prepared(self, &mut DiscardedProgress)
    }

    /// Applies exactly like [`PreparedCompaction::apply`], reporting the
    /// archive rewrites and file removals to `observer`. Reporting cannot
    /// alter the mutation sequence: the observer is told what has already
    /// been done and never decides anything.
    pub fn apply_with_progress(
        self,
        observer: &mut dyn ProgressObserver,
    ) -> Result<CompactionOutcome> {
        apply_prepared(self, observer)
    }
}

/// Resolves `directory` once to its canonical absolute target, then builds a
/// cleanup plan without acquiring a lock or changing any byte. Interactive
/// callers should pass [`CompactionPlan::directory`] to
/// [`PreparedCompaction::prepare`] so an alias cannot redirect lock acquisition
/// after the preview.
pub fn plan_compaction(directory: &Path, options: &CompactionOptions) -> Result<CompactionPlan> {
    plan_compaction_with_progress(directory, options, &mut DiscardedProgress)
}

/// Plans exactly like [`plan_compaction`] — still strictly read-only, still
/// without acquiring the lock — reporting each planning step to
/// `observer`. Planning a large store is the phase that takes minutes:
/// it verifies the whole head tree, replays the journal, and traces the
/// reachable segment closure before it can say anything at all.
pub fn plan_compaction_with_progress(
    directory: &Path,
    options: &CompactionOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionPlan> {
    validate_options(options)?;
    let directory = canonical_repository_directory(directory)?;
    validate_repository_shape(&directory)?;
    build_plan(&directory, options, SystemTime::now(), observer)
}

/// Convenience non-interactive API: prepares under lock and immediately
/// applies the authoritative plan. Interactive callers should use
/// [`plan_compaction`] and [`PreparedCompaction`] so they can display/reconfirm.
pub fn compact(directory: &Path, options: CompactionOptions) -> Result<CompactionOutcome> {
    compact_with_progress(directory, options, &mut DiscardedProgress)
}

/// Prepares and applies exactly like [`compact`], reporting both phases
/// to `observer`.
pub fn compact_with_progress(
    directory: &Path,
    options: CompactionOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionOutcome> {
    PreparedCompaction::prepare_with_progress(directory, options, observer)?
        .apply_with_progress(observer)
}
