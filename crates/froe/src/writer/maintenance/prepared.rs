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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Repository;
    use crate::tar_archive::file_name::ArchiveFileName;
    use crate::writer::commit::create_checkpoint;

    use crate::writer::maintenance::options::*;

    use crate::writer::maintenance::test_support::*;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::record_writer::PropertyToWrite;
    use crate::writer::record_writer::PropertyValuesToWrite;
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::store_writer::WritableRepository;
    use std::num::NonZeroUsize;

    /// An archive number that cannot be rebuilt dooms the run however it is
    /// retried, so it is refused where nothing has been touched — not after
    /// paying a durable rewrite of every repairable archive to discover it.
    #[test]
    fn an_unrepairable_archive_refuses_before_anything_is_rewritten() {
        let directory = TestDirectory::new("repair-unrepairable");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            writer
                .write_string("forces a second archive")
                .expect("string");
            writer.finish().expect("finish");
            store.close().expect("close");
        }
        // A repairable archive, and — at a higher number, so the repair loop
        // would have reached it second — bytes no scan can recover.
        break_index_magic(&directory.path.join("data00000a.tar"));
        std::fs::write(directory.path.join("data00500a.tar"), vec![0x5au8; 4096])
            .expect("unrecoverable residue");
        let before = file_bytes(&directory.path);

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);

        // The read-only preview says so, before any authorization.
        let preview = plan_compaction(&directory.path, &options)
            .expect_err("the preview must refuse an unrepairable archive");
        assert!(
            preview.to_string().contains("data00500a.tar"),
            "the preview names the archive that dooms the run: {preview}"
        );

        // And a library caller skipping the preview pays no rewrite either.
        match PreparedCompaction::prepare(&directory.path, options) {
            Ok(_) => panic!("prepare must refuse too"),
            Err(error) => assert!(
                error.to_string().contains("data00500a.tar"),
                "prepare names it as well: {error}"
            ),
        }
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "and neither path rewrote a single archive"
        );
    }
    #[cfg(unix)]
    #[test]
    fn repository_root_symlink_is_resolved_to_the_canonical_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::repository("root-symlink-target");
        let link = directory.path.with_extension("repository-link");
        let _ = std::fs::remove_file(&link);
        symlink(&directory.path, &link).expect("create repository root symlink");

        let expected = std::fs::canonicalize(&directory.path).expect("canonical target");
        let plan =
            plan_compaction(&link, &CompactionOptions::default()).expect("plan through alias");
        assert_eq!(plan.directory(), expected);
        let prepared = PreparedCompaction::prepare(
            &link,
            CompactionOptions::default().with_tasks(std::iter::empty()),
        )
        .expect("prepare through alias");
        assert_eq!(prepared.plan().directory(), expected);
        drop(prepared);
        std::fs::remove_file(link).expect("remove repository link");
    }
    #[test]
    fn journal_cleanup_preserves_mixed_physical_terminators_exactly() {
        let directory = TestDirectory::repository("mixed-journal-terminators");
        let head = Repository::open(&directory.path)
            .expect("repository")
            .head_record_identifier();
        let retained = format!("{head} tag-lf 1\n{head} tag-crlf 2\r\n{head} tag-cr 3\r");
        let source = format!("{retained}parser-skipped\n");
        std::fs::write(directory.path.join("journal.log"), source.as_bytes())
            .expect("write mixed journal fixture");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Journal]);

        let plan = plan_compaction(&directory.path, &options).expect("plan mixed cleanup");
        assert_eq!(plan.journal_line_removals().len(), 1);
        assert_eq!(plan.journal_line_removals()[0].line_number(), 4);
        compact(&directory.path, options).expect("apply mixed cleanup");

        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("read rewritten journal"),
            retained.as_bytes(),
            "LF, CRLF, and bare-CR terminators must remain byte-exact"
        );
        Repository::open(&directory.path).expect("mixed-terminator repository remains healthy");
    }
    #[test]
    fn prepared_cleanup_is_excluded_by_an_existing_writer_lock() {
        let directory = TestDirectory::repository("lock-exclusion");
        let writer = WritableRepository::open(&directory.path).expect("hold writer lock");
        plan_compaction(&directory.path, &CompactionOptions::default())
            .expect("lock-free preview remains read-only");

        assert!(
            PreparedCompaction::prepare(&directory.path, CompactionOptions::default()).is_err()
        );
        writer.close().expect("close writer");
        Repository::open(&directory.path).expect("repository healthy");
    }
    #[test]
    fn redundant_journal_staging_is_removed_and_second_run_is_a_true_noop() {
        let directory = TestDirectory::repository("temporary-idempotence");
        let staging = directory.path.join("journal.log.compacting");
        std::fs::copy(directory.path.join("journal.log"), &staging).expect("copy staging");
        let forensic_staging = directory.path.join("journal.log.recovered");
        std::fs::write(&forensic_staging, b"unterminated recovery evidence")
            .expect("write ambiguous staging journal");
        std::fs::write(directory.path.join("gc.log"), b"operator gc state\n").expect("seed gc log");

        compact(&directory.path, CompactionOptions::default()).expect("first cleanup");
        assert!(!staging.exists());
        assert!(forensic_staging.exists());
        assert_eq!(
            std::fs::read(directory.path.join("gc.log")).expect("gc log"),
            b"operator gc state\n"
        );
        let before = file_bytes(&directory.path);
        let second =
            plan_compaction(&directory.path, &CompactionOptions::default()).expect("second plan");
        assert!(second.is_empty(), "second plan: {:?}", second.actions());
        let outcome = PreparedCompaction::prepare(&directory.path, CompactionOptions::default())
            .expect("prepare no-op")
            .apply()
            .expect("apply no-op");
        assert_eq!(outcome.head_before, outcome.head_after);
        assert_eq!(file_bytes(&directory.path), before);
    }
    #[test]
    fn checkpoint_cleanup_allocates_after_zero_byte_next_archive_residue() {
        let directory = TestDirectory::repository("checkpoint-zero-byte-next-archive");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));

        let repository = Repository::open(&directory.path).expect("open checkpoint repository");
        let active_maximum = repository
            .archives()
            .iter()
            .filter_map(|archive| ArchiveFileName::parse(archive.file_name()))
            .map(|name| name.archive_number)
            .max()
            .expect("fixture has an active archive");
        drop(repository);
        let occupied_number = active_maximum.checked_add(1).expect("fixture namespace");
        let certified_number = occupied_number.checked_add(1).expect("fixture namespace");
        let occupied_name = format!("data{occupied_number:05}a.tar");
        std::fs::write(directory.path.join(&occupied_name), b"")
            .expect("install zero-byte otherwise-next residue");
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let plan = plan_compaction(&directory.path, &options).expect("plan checkpoint cleanup");
        assert_eq!(plan.checkpoint_archive_number, Some(certified_number));
        let outcome = compact(&directory.path, options).expect("apply checkpoint cleanup");

        assert_eq!(outcome.removed_checkpoints, 1);
        assert_eq!(
            std::fs::read(directory.path.join(&occupied_name)).expect("read zero-byte residue"),
            b"",
            "cleanup must neither truncate nor reuse physical residue"
        );
        assert!(
            directory
                .path
                .join(format!("data{certified_number:05}a.tar"))
                .exists()
        );
        let repository = Repository::open(&directory.path).expect("healthy repository");
        assert!(repository.checkpoints().expect("checkpoints").is_empty());
    }
    #[test]
    fn the_history_price_counts_the_bulk_segments_held_behind_the_data_ones() {
        // The veto holds bulk segments only indirectly: a vetoed data segment
        // keeps seeding its references, so its binary content stays too.
        // Counting protected *data* segments alone therefore prices a store
        // full of inline binaries at a rounding error, and an operator
        // reading that figure would decline a run worth most of the store.
        let directory = TestDirectory::repository("history-price-bulk");
        {
            let store = WritableRepository::open(&directory.path).expect("open binary writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            // Comfortably past the 256 KiB segment limit, so the content
            // lands in bulk segments rather than inline in the data segment.
            let content: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
            let binary = writer.write_binary_content(&content).expect("binary");
            let file = writer
                .write_node(
                    Some("nt:file"),
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "data".to_owned(),
                        property_type: crate::content::property::PropertyType::Binary,
                        values: PropertyValuesToWrite::Single(binary),
                    }],
                )
                .expect("file node");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: file,
                    },
                    &[],
                )
                .expect("binary super root");
            writer.finish().expect("finish binary segments");
            assert!(store.compare_and_set_head(store.head(), head));
            store.close().expect("close binary writer");
        }
        // An independent generation-two head that reaches none of it.
        {
            let store = WritableRepository::open(&directory.path).expect("open new head writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            });
            let root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("new content root");
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
                .expect("new super root");
            writer.finish().expect("finish new head");
            assert!(store.compare_and_set_head(store.head(), head));
            store.close().expect("close new head writer");
        }

        let priced = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("priced plan");
        let (_priced_segments, priced_bytes) = priced.history_protected_reclaimable();

        // Now actually retire the history and compare. The quoted price must
        // be what the operation delivers, not the data-segment fraction of it.
        let outcome = compact(
            &directory.path,
            CompactionOptions::default()
                .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
                .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision")),
        )
        .expect("bounded cleanup");
        let freed = outcome.archive_bytes_before - outcome.archive_bytes_after;
        assert!(
            freed > 1024 * 1024,
            "retiring the history must free the binary content: {freed}"
        );
        assert_eq!(
            priced_bytes, freed,
            "the quoted price must be what retiring the history delivers"
        );
    }
    #[test]
    fn an_unselected_task_reports_no_step_of_its_own() {
        let (directory, _old_head, _new_head) = history_veto_fixture("unselected-task-step");
        let mut observer = StepNameObserver { names: Vec::new() };
        // recovery-backups is not among the defaults, so announcing its
        // removal step told the operator froe had considered backups it was
        // never asked to touch.
        compact_with_progress(
            &directory.path,
            CompactionOptions::default()
                .with_tasks([
                    MaintenanceTask::Segments,
                    MaintenanceTask::Journal,
                    MaintenanceTask::StaleTemporaries,
                ])
                .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision")),
            &mut observer,
        )
        .expect("bounded cleanup");
        for unselected in ["removing old recovery backups", "removing stale archives"] {
            assert!(
                !observer.names.iter().any(|name| name == unselected),
                "unselected task reported {unselected:?}: {:?}",
                observer.names
            );
        }
        assert!(
            observer
                .names
                .iter()
                .any(|name| name == "removing stale temporary files"),
            "a selected task still reports its step: {:?}",
            observer.names
        );
    }
}
