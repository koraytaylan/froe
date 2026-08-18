//! Process-crash probes for cleanup durability boundaries.
//!
//! This entire module exists only in unit-test builds. Production binaries do
//! not contain the environment checks or the cutpoint calls.

use std::ffi::OsStr;

const CHILD_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_CHILD";
const CUTPOINT_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_CUTPOINT";
const MODE_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_MODE";
#[cfg(unix)]
const CRASH_EXIT_CODE: i32 = 86;
#[cfg(unix)]
const VERIFIED_EXIT_CODE: i32 = 87;
#[cfg(unix)]
const CRASH_MODE: &str = "crash";
const ERROR_MODE: &str = "error";
const SUBSTITUTE_MODE: &str = "substitute";
const ABSENCE_MODE: &str = "absence";

fn is_armed(cutpoint: &str, mode: &str) -> bool {
    std::env::var_os(CHILD_ENVIRONMENT).as_deref() == Some(OsStr::new("1"))
        && std::env::var_os(CUTPOINT_ENVIRONMENT).as_deref() == Some(OsStr::new(cutpoint))
        && std::env::var_os(MODE_ENVIRONMENT).as_deref() == Some(OsStr::new(mode))
}

/// Whether an in-memory consistency probe is explicitly armed in the
/// isolated fault-test child.
pub(super) fn is_substitution_armed(cutpoint: &str) -> bool {
    is_armed(cutpoint, SUBSTITUTE_MODE)
}

/// Terminates an explicitly armed child process at `cutpoint` without running
/// destructors. Ordinary unit-test processes never set the child marker.
pub(super) fn crash_if_armed(cutpoint: &str) {
    #[cfg(unix)]
    if is_armed(cutpoint, CRASH_MODE) {
        // SAFETY: `_exit` has no memory-safety preconditions. It is used only
        // in an isolated test child specifically to model abrupt process death
        // without Rust unwinding or guard cleanup.
        unsafe { libc::_exit(CRASH_EXIT_CODE) }
    }
    #[cfg(not(unix))]
    let _ = cutpoint;
}

/// Returns a deterministic synthetic I/O error from an explicitly armed test
/// child. Callers place this immediately before or after a real syscall to
/// exercise both old-state and completed-syscall error handling.
pub(super) fn fail_if_armed(cutpoint: &str) -> crate::error::Result<()> {
    if is_armed(cutpoint, ERROR_MODE) {
        return Err(
            std::io::Error::other(format!("injected cleanup I/O failure at {cutpoint}")).into(),
        );
    }
    Ok(())
}

/// Replaces an armed staging/source pathname exactly once while leaving its
/// previously validated inode under a diagnostic non-active name. Isolated
/// child-process tests use this to prove destructive syscalls are bound to the
/// descriptor that was certified, not merely to a reusable pathname.
pub(super) fn substitute_path_if_armed(
    cutpoint: &str,
    path: &std::path::Path,
) -> crate::error::Result<()> {
    if !is_armed(cutpoint, SUBSTITUTE_MODE) {
        return Ok(());
    }
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("staging");
    let displaced = path.with_file_name(format!("{file_name}.validated-inode"));
    std::fs::rename(path, displaced)?;
    std::fs::write(path, b"substituted pathname\n")?;
    Ok(())
}

/// Removes an armed pathname immediately before production retries the same
/// unlink, modelling an external actor winning that exact deletion race.
pub(super) fn remove_path_if_armed(
    cutpoint: &str,
    path: &std::path::Path,
) -> crate::error::Result<()> {
    if is_armed(cutpoint, ABSENCE_MODE) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Omits the final retained item from an explicitly armed in-memory
/// post-mutation analysis. This changes no repository byte; it makes the
/// production retained-root verification call load-bearing in tests.
pub(super) fn omit_last_if_armed<T>(cutpoint: &str, items: &mut Vec<T>) {
    if is_armed(cutpoint, SUBSTITUTE_MODE) {
        items.pop();
    }
}

/// Adds a physical line that cannot exist in the final journal to an armed
/// verifier's expected set. This changes no repository byte; it makes the
/// production byte-exact retained-line verification call load-bearing.
pub(super) fn append_missing_journal_line_if_armed(cutpoint: &str, expected: &mut Vec<Vec<u8>>) {
    if is_armed(cutpoint, SUBSTITUTE_MODE) {
        expected.push(b"froe injected missing retained journal line\n".to_vec());
    }
}

#[cfg(unix)]
mod tests {
    use std::ffi::OsStr;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        ABSENCE_MODE, CHILD_ENVIRONMENT, CRASH_EXIT_CODE, CRASH_MODE, CUTPOINT_ENVIRONMENT,
        ERROR_MODE, MODE_ENVIRONMENT, SUBSTITUTE_MODE, VERIFIED_EXIT_CODE,
    };
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::record::RecordIdentifier;
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::tar_archive::file_name::ArchiveFileName;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::compaction::CompactionKind;
    use crate::writer::maintenance::{
        CompactionAction, CompactionOptions, MaintenanceTask, compact, plan_compaction,
    };
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::repository_lock::RepositoryLock;
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::store_writer::WritableRepository;

    const REPOSITORY_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_REPOSITORY";
    const SCENARIO_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_SCENARIO";
    const CHILD_TEST_NAME: &str = "writer::maintenance_fault_injection::tests::cleanup_fault_child";

    const CHECKPOINT_SCENARIO: &str = "checkpoint";
    const SWEEP_SCENARIO: &str = "sweep";
    const POSTCOMPACTION_SWEEP_SCENARIO: &str = "postcomp-sweep";
    const JOURNAL_SCENARIO: &str = "journal";
    const MANIFEST_SCENARIO: &str = "manifest";
    const REMOVAL_SCENARIO: &str = "removal";
    const STALE_ARCHIVE_SCENARIO: &str = "stale-archive";

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "froe-cleanup-fault-{name}-{}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("create fault-injection repository directory");
            Self { path }
        }

        fn repository(name: &str) -> Self {
            let directory = Self::new(name);
            WritableRepository::open(&directory.path)
                .expect("bootstrap fault-injection repository")
                .close()
                .expect("close bootstrap writer");
            directory
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Debug)]
    struct RepositorySnapshot {
        head: RecordIdentifier,
        readable_journal_roots: Vec<RecordIdentifier>,
        journal_bytes: Vec<u8>,
    }

    fn snapshot_repository(directory: &Path) -> RepositorySnapshot {
        let repository = Repository::open(directory).expect("open repository before crash");
        let readable_journal_roots = readable_journal_roots(&repository);
        assert!(
            !readable_journal_roots.is_empty(),
            "the fixture must have at least one readable journal root"
        );
        RepositorySnapshot {
            head: repository.head_record_identifier(),
            readable_journal_roots,
            journal_bytes: std::fs::read(directory.join("journal.log"))
                .expect("read journal before crash"),
        }
    }

    fn readable_journal_roots(repository: &Repository) -> Vec<RecordIdentifier> {
        repository
            .journal_entries()
            .iter()
            .filter_map(crate::journal::JournalEntry::record_identifier)
            .filter(|identifier| repository.contains_segment(identifier.segment))
            .inspect(|identifier| {
                crate::tooling::verify_node_tree(repository, *identifier)
                    .expect("every segment-resolving fixture journal root must traverse");
            })
            .collect()
    }

    fn assert_exact_snapshot_reopens(directory: &Path, expected: &RepositorySnapshot) {
        // Deliberately use the read-only opener: the writable opener performs
        // archive recovery and could conceal unsafe crash residue.
        let repository = Repository::open(directory).expect("fresh read-only reopen after crash");
        assert_eq!(
            repository.head_record_identifier(),
            expected.head,
            "these cutpoints all precede a new durable journal head"
        );
        assert_eq!(
            readable_journal_roots(&repository),
            expected.readable_journal_roots,
            "every previously readable revision, including duplicate multiplicity and order, must remain readable"
        );
        drop(repository);

        // Kernel advisory locks must be released by abrupt process death even
        // though the persistent repo.lock inode remains in place.
        drop(RepositoryLock::acquire(directory).expect("child crash releases repository lock"));
    }

    fn cleanup_child_output(
        directory: &Path,
        scenario: &str,
        cutpoint: &str,
        mode: &str,
    ) -> std::process::Output {
        Command::new(std::env::current_exe().expect("locate unit-test binary"))
            .arg("--exact")
            .arg(CHILD_TEST_NAME)
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_ENVIRONMENT, "1")
            .env(CUTPOINT_ENVIRONMENT, cutpoint)
            .env(MODE_ENVIRONMENT, mode)
            .env(REPOSITORY_ENVIRONMENT, directory)
            .env(SCENARIO_ENVIRONMENT, scenario)
            .output()
            .expect("spawn fault-injection child")
    }

    fn run_crash_child(directory: &Path, scenario: &str, cutpoint: &str) {
        let output = cleanup_child_output(directory, scenario, cutpoint, CRASH_MODE);
        assert_eq!(
            output.status.code(),
            Some(CRASH_EXIT_CODE),
            "child did not reach {cutpoint}; stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_error_child(directory: &Path, scenario: &str, cutpoint: &str) {
        let output = cleanup_child_output(directory, scenario, cutpoint, ERROR_MODE);
        assert_eq!(
            output.status.code(),
            Some(VERIFIED_EXIT_CODE),
            "child did not return the injected error at {cutpoint}; stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_substitution_child(directory: &Path, scenario: &str, cutpoint: &str) {
        let output = cleanup_child_output(directory, scenario, cutpoint, SUBSTITUTE_MODE);
        assert_eq!(
            output.status.code(),
            Some(VERIFIED_EXIT_CODE),
            "child did not reject the substituted path at {cutpoint}; stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_absence_child(directory: &Path, scenario: &str, cutpoint: &str) {
        let output = cleanup_child_output(directory, scenario, cutpoint, ABSENCE_MODE);
        assert_eq!(
            output.status.code(),
            Some(VERIFIED_EXIT_CODE),
            "child did not observe the already-absent unlink at {cutpoint}; stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn scenario_options(scenario: &str) -> CompactionOptions {
        let task = match scenario {
            CHECKPOINT_SCENARIO | MANIFEST_SCENARIO => MaintenanceTask::UnreferencedCheckpoints,
            SWEEP_SCENARIO => MaintenanceTask::Segments,
            JOURNAL_SCENARIO => MaintenanceTask::Journal,
            REMOVAL_SCENARIO => MaintenanceTask::StaleTemporaries,
            STALE_ARCHIVE_SCENARIO => {
                return CompactionOptions::default().with_tasks([
                    MaintenanceTask::StaleArchives,
                    MaintenanceTask::ExpiredCheckpoints,
                ]);
            }
            other => panic!("unknown fault-injection scenario {other}"),
        };
        CompactionOptions::default().with_tasks([task])
    }

    /// Child entrypoint. A normal `cargo test` invocation leaves the marker
    /// unset, so this registered test is a no-op outside its parent harness.
    #[test]
    fn cleanup_fault_child() {
        if std::env::var_os(CHILD_ENVIRONMENT).as_deref() != Some(OsStr::new("1")) {
            return;
        }
        let directory = PathBuf::from(
            std::env::var_os(REPOSITORY_ENVIRONMENT)
                .expect("fault child repository path must be supplied"),
        );
        let scenario =
            std::env::var(SCENARIO_ENVIRONMENT).expect("fault child scenario must be supplied");
        let cutpoint =
            std::env::var(CUTPOINT_ENVIRONMENT).expect("fault child cutpoint must be supplied");
        let mode = std::env::var(MODE_ENVIRONMENT).expect("fault child mode must be supplied");
        if scenario == POSTCOMPACTION_SWEEP_SCENARIO {
            run_postcompaction_sweep_child(&directory, &cutpoint, &mode);
            return;
        }
        run_compaction_child(&directory, &scenario, &cutpoint, &mode);
    }

    /// The post-compaction sweep scenario: compact first, then arm the
    /// cutpoint and reclaim, so the fault lands in the sweep rather than in
    /// the copy that precedes it.
    fn run_postcompaction_sweep_child(directory: &Path, cutpoint: &str, mode: &str) {
        let mut store =
            WritableRepository::open(directory).expect("open post-compaction boundary fixture");
        let reference = store
            .writing_generation()
            .expect("read post-compaction reference generation");
        let outcome = store.reclaim_old_generations(reference, CompactionKind::Full);
        match mode {
            ERROR_MODE => {
                let error =
                    outcome.expect_err("post-compaction reclaim completed without injected error");
                assert!(
                    error.to_string().contains(cutpoint),
                    "post-compaction reclaim failed before {cutpoint}: {error}"
                );
            }
            CRASH_MODE => match outcome {
                Ok(()) => {
                    panic!("post-compaction reclaim completed without reaching {cutpoint}")
                }
                Err(error) => {
                    panic!("post-compaction reclaim failed before {cutpoint}: {error}")
                }
            },
            other => panic!("unsupported post-compaction fault mode {other}"),
        }
        // SAFETY: `_exit` has no memory-safety preconditions and this is
        // an isolated child whose error path was checked above.
        unsafe { libc::_exit(VERIFIED_EXIT_CODE) }
    }

    /// What a path substituted at `cutpoint` must have left behind: either a
    /// refusal, or a partial outcome naming the file it declined to touch.
    fn assert_substitution_outcome(
        scenario: &str,
        cutpoint: &str,
        outcome: crate::error::Result<crate::writer::maintenance::CompactionOutcome>,
    ) {
        if cutpoint == "remove-planned-file.before-final-identity" && scenario == REMOVAL_SCENARIO {
            let outcome =
                outcome.expect("a late planned-file identity refusal is a partial outcome");
            assert!(!outcome.is_complete());
            assert!(outcome.deletion_failures().iter().any(|failure| {
                failure.file_name() == "journal.log.cleaning.000"
                    && failure.error().contains("changed after")
            }));
        } else {
            let error = outcome.expect_err("cleanup accepted injected post-mutation inconsistency");
            if cutpoint == "checkpoint.tar-durable-before-journal" {
                assert!(
                    error.to_string().contains("finalized session archive"),
                    "unexpected checkpoint TAR identity refusal: {error}"
                );
            }
            if cutpoint == "sweep.staging-validated-before-publish" {
                assert!(
                    error.to_string().contains("validated archive staging file"),
                    "unexpected staging identity refusal: {error}"
                );
            }
            if cutpoint == "sweep.remove-before-source-identity" {
                assert!(
                    error.to_string().contains("certified removal source"),
                    "unexpected archive-source identity refusal: {error}"
                );
            }
            if cutpoint == "cleanup.before-final-retained-root-verification" {
                assert!(
                    error
                        .to_string()
                        .contains("previously readable journal root"),
                    "unexpected retained-root refusal: {error}"
                );
            }
            if cutpoint == "cleanup.before-prospective-retained-root-verification" {
                assert!(
                    error
                        .to_string()
                        .contains("segment cleanup would make retained journal root"),
                    "unexpected prospective retained-root refusal: {error}"
                );
            }
            if cutpoint == "cleanup.before-final-retained-line-verification" {
                assert!(
                    error
                        .to_string()
                        .contains("previously readable physical journal line byte-for-byte"),
                    "unexpected retained-line refusal: {error}"
                );
            }
            if cutpoint == "remove-planned-file.before-final-identity"
                && scenario == STALE_ARCHIVE_SCENARIO
            {
                assert!(
                    error
                        .to_string()
                        .contains("planned cleanup deletion of data00000a.tar failed"),
                    "unexpected strict stale-archive refusal: {error}"
                );
            }
        }
    }

    /// Every other scenario: arm the cutpoint, run the maintenance, and hold
    /// the store to what the mode says a fault there must leave behind.
    fn run_compaction_child(directory: &Path, scenario: &str, cutpoint: &str, mode: &str) {
        let outcome = compact(directory, scenario_options(scenario));
        match mode {
            ERROR_MODE => {
                let error = outcome.expect_err("cleanup completed without the injected error");
                assert!(
                    error.to_string().contains(cutpoint),
                    "cleanup failed at an unexpected seam before {cutpoint}: {error}"
                );
            }
            SUBSTITUTE_MODE => {
                assert_substitution_outcome(scenario, cutpoint, outcome);
            }
            ABSENCE_MODE => {
                let outcome = outcome.expect("cleanup lost the already-absent segment outcome");
                let failures: Vec<_> = outcome
                    .deletion_failures()
                    .iter()
                    .filter(|failure| failure.target_was_already_absent())
                    .collect();
                assert_eq!(
                    failures.len(),
                    1,
                    "the segment unlink race must have one typed already-absent result: {outcome:?}"
                );
                assert!(
                    Path::new(failures[0].file_name()).extension() == Some(OsStr::new("tar")),
                    "the typed absence must come from the archive segment pass"
                );
            }
            CRASH_MODE => match outcome {
                Ok(_) => panic!("cleanup completed without reaching the armed crash cutpoint"),
                Err(error) => {
                    panic!("cleanup failed before the armed crash cutpoint {cutpoint}: {error}")
                }
            },
            other => panic!("unknown fault mode {other}"),
        }
        // A missing `--exact` child test exits zero, so the parent must not
        // treat libtest success as proof that this entrypoint ran. Error and
        // substitution children exit with a distinctive code only after all
        // mode-specific assertions above have completed.
        // SAFETY: `_exit` has no memory-safety preconditions and this is an
        // isolated child whose repository state has already been checked.
        unsafe { libc::_exit(VERIFIED_EXIT_CODE) }
    }

    #[test]
    fn checkpoint_tar_is_readable_when_process_dies_before_journal_append() {
        let directory = TestDirectory::repository("checkpoint-before-journal");
        {
            let store = WritableRepository::open(&directory.path).expect("open checkpoint writer");
            create_checkpoint(&store, 60_000, &[]).expect("create retained checkpoint fixture");
            store.close().expect("close checkpoint writer");
        }
        let snapshot = snapshot_repository(&directory.path);
        assert_eq!(snapshot.readable_journal_roots.len(), 2);
        let archives_before = archive_file_names(&directory.path);

        run_crash_child(
            &directory.path,
            CHECKPOINT_SCENARIO,
            "checkpoint.tar-durable-before-journal",
        );

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("read journal after crash"),
            snapshot.journal_bytes,
            "the old journal must be byte-exact before the head append begins"
        );
        let archives_after = archive_file_names(&directory.path);
        assert_eq!(archives_after.len(), archives_before.len() + 1);
        let new_archives: Vec<_> = archives_after
            .iter()
            .filter(|name| !archives_before.contains(name))
            .collect();
        assert_eq!(new_archives.len(), 1);
        let orphan = TarArchiveReader::open(&directory.path.join(new_archives[0]))
            .expect("pre-journal checkpoint TAR residue is fully finalized and readable");
        assert!(!orphan.is_recovered());
    }

    #[test]
    fn checkpoint_rejects_a_session_tar_substituted_before_journal_append() {
        let cutpoint = "checkpoint.tar-durable-before-journal";
        let directory = TestDirectory::repository("checkpoint-tar-substitution");
        {
            let store = WritableRepository::open(&directory.path).expect("open checkpoint writer");
            create_checkpoint(&store, 60_000, &[])
                .expect("create deterministically unreferenced checkpoint");
            store.close().expect("close checkpoint writer");
        }
        let snapshot = snapshot_repository(&directory.path);
        let archives_before = archive_file_names(&directory.path);

        run_substitution_child(&directory.path, CHECKPOINT_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log"))
                .expect("journal survives session TAR substitution"),
            snapshot.journal_bytes,
            "a substituted checkpoint TAR must be rejected before its head is journal-visible"
        );
        let new_archives: Vec<_> = archive_file_names(&directory.path)
            .into_iter()
            .filter(|name| !archives_before.contains(name))
            .collect();
        assert_eq!(new_archives.len(), 1);
        assert_eq!(
            std::fs::read(directory.path.join(&new_archives[0]))
                .expect("substituted active path remains for diagnosis"),
            b"substituted pathname\n"
        );
        let retained = directory
            .path
            .join(format!("{}.validated-inode", new_archives[0]));
        let validated = TarArchiveReader::open(&retained)
            .expect("descriptor-certified session inode remains readable");
        assert!(!validated.is_recovered());
    }

    fn archive_file_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(directory)
            .expect("list archive directory")
            .map(|entry| entry.expect("read archive entry").file_name())
            .filter_map(|name| name.to_str().map(str::to_owned))
            .filter(|name| ArchiveFileName::parse(name).is_some())
            .collect();
        names.sort();
        names
    }

    fn create_manifest_upgrade_fixture(
        directory: &TestDirectory,
    ) -> (RepositorySnapshot, Vec<u8>, Vec<u8>) {
        {
            let store = WritableRepository::open(&directory.path).expect("open manifest fixture");
            create_checkpoint(&store, 60_000, &[])
                .expect("create deterministically unreferenced checkpoint");
            store.close().expect("close manifest fixture writer");
        }
        let old_manifest = b"custom.property=kept\nstore.version=1\n".to_vec();
        std::fs::write(directory.path.join("manifest"), &old_manifest)
            .expect("install version-one manifest fixture");
        let mut upgraded_manifest = old_manifest.clone();
        upgraded_manifest.push(b'\n');
        upgraded_manifest
            .extend_from_slice(b"# upgraded atomically by froe cleanup\nstore.version=2\n");
        (
            snapshot_repository(&directory.path),
            old_manifest,
            upgraded_manifest,
        )
    }

    /// What a fault at a manifest-replacement cutpoint must leave on disk.
    #[derive(Clone, Copy)]
    struct ExpectedManifestResidue {
        /// The upgraded manifest is in place under its final name.
        replacement_installed: bool,
        /// The staging temporary is still present.
        temporary_exists: bool,
    }

    fn assert_manifest_residue(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        old_manifest: &[u8],
        upgraded_manifest: &[u8],
        residue: ExpectedManifestResidue,
        cutpoint: &str,
    ) {
        let ExpectedManifestResidue {
            replacement_installed,
            temporary_exists,
        } = residue;
        assert_exact_snapshot_reopens(directory, snapshot);
        assert_eq!(
            std::fs::read(directory.join("manifest")).expect("read canonical manifest"),
            if replacement_installed {
                upgraded_manifest
            } else {
                old_manifest
            },
            "canonical manifest must be one complete valid generation at {cutpoint}"
        );
        let temporary = directory.join("manifest.cleaning.000");
        assert_eq!(temporary.exists(), temporary_exists, "{cutpoint}");
        if temporary_exists {
            assert_eq!(
                std::fs::read(temporary).expect("read staged manifest residue"),
                upgraded_manifest,
                "manifest staging residue must contain the exact valid upgrade"
            );
        }
    }

    #[test]
    fn manifest_replacement_crash_boundaries_keep_an_exact_old_or_new_manifest() {
        let cutpoints = [
            ("manifest.temporary-durable", false, true),
            ("manifest.before-rename", false, true),
            ("manifest.renamed-before-directory-sync", true, false),
            ("manifest.before-post-rename-directory-sync", true, false),
            ("manifest.rename-durable", true, false),
        ];

        for (cutpoint, replacement_installed, temporary_exists) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, old_manifest, upgraded_manifest) =
                create_manifest_upgrade_fixture(&directory);

            run_crash_child(&directory.path, MANIFEST_SCENARIO, cutpoint);

            assert_manifest_residue(
                &directory.path,
                &snapshot,
                &old_manifest,
                &upgraded_manifest,
                ExpectedManifestResidue {
                    replacement_installed,
                    temporary_exists,
                },
                cutpoint,
            );
        }
    }

    #[test]
    fn manifest_replacement_errors_keep_an_exact_old_or_new_manifest() {
        let cutpoints = [
            ("manifest.temporary-durable", false),
            ("manifest.before-rename", false),
            ("manifest.renamed-before-directory-sync", true),
            ("manifest.before-post-rename-directory-sync", true),
            ("manifest.rename-durable", true),
        ];

        for (cutpoint, replacement_installed) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, old_manifest, upgraded_manifest) =
                create_manifest_upgrade_fixture(&directory);

            run_error_child(&directory.path, MANIFEST_SCENARIO, cutpoint);

            assert_manifest_residue(
                &directory.path,
                &snapshot,
                &old_manifest,
                &upgraded_manifest,
                ExpectedManifestResidue {
                    replacement_installed,
                    temporary_exists: false,
                },
                cutpoint,
            );
        }
    }

    fn create_partial_sweep_fixture(
        directory: &TestDirectory,
    ) -> (RepositorySnapshot, String, String) {
        let store = WritableRepository::open(&directory.path).expect("open sweep fixture writer");
        let obsolete_generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        for _ in 0..3 {
            let mut writer = store.record_writer(obsolete_generation);
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("write orphan node");
            writer.finish().expect("finish orphan segment");
        }

        let current_generation = GarbageCollectionGeneration {
            generation: 3,
            full_generation: 3,
            is_compacted: false,
        };
        let old_head = store.head();
        let mut writer = store.record_writer(current_generation);
        let content_root = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("write current content root");
        let new_head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("write current super-root");
        writer.finish().expect("finish current head segment");
        assert!(store.compare_and_set_head(old_head, new_head));
        store.close().expect("close sweep fixture writer");

        let options = scenario_options(SWEEP_SCENARIO);
        let plan = plan_compaction(&directory.path, &options).expect("plan partial sweep fixture");
        let (source, replacement) = plan
            .actions()
            .iter()
            .find_map(|action| match action {
                CompactionAction::RewriteArchive {
                    file_name,
                    replacement_name,
                    ..
                } => Some((file_name.clone(), replacement_name.clone())),
                _ => None,
            })
            .expect("fixture must require a partial archive rewrite");
        (snapshot_repository(&directory.path), source, replacement)
    }

    fn create_whole_archive_sweep_fixture(
        directory: &TestDirectory,
    ) -> (RepositorySnapshot, String, Vec<u8>) {
        {
            let store = WritableRepository::open(&directory.path).expect("open head writer");
            let old_head = store.head();
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 3,
                full_generation: 3,
                is_compacted: false,
            });
            let content_root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("write current content root");
            let new_head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: content_root,
                    },
                    &[],
                )
                .expect("write current super-root");
            writer.finish().expect("finish current head segment");
            assert!(store.compare_and_set_head(old_head, new_head));
            store.close().expect("close head writer");
        }
        {
            let store = WritableRepository::open(&directory.path).expect("open orphan writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("write whole-archive orphan");
            writer.finish().expect("finish whole-archive orphan");
            store.close().expect("close orphan writer");
        }

        let options = scenario_options(SWEEP_SCENARIO);
        let plan = plan_compaction(&directory.path, &options).expect("plan whole-archive sweep");
        let source = plan
            .actions()
            .iter()
            .find_map(|action| match action {
                CompactionAction::RemoveReclaimableArchive { file_name, .. } => {
                    Some(file_name.clone())
                }
                _ => None,
            })
            .expect("fixture must remove a wholly reclaimable archive");
        let source_bytes =
            std::fs::read(directory.path.join(&source)).expect("read whole-removal source");
        (snapshot_repository(&directory.path), source, source_bytes)
    }

    fn create_removal_then_rewrite_fixture(
        directory: &TestDirectory,
    ) -> (RepositorySnapshot, String, String, Vec<u8>, String) {
        let (_, rewrite_source, replacement) = create_partial_sweep_fixture(directory);
        {
            let store = WritableRepository::open(&directory.path)
                .expect("open whole-removal orphan writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("write whole-archive orphan");
            writer.finish().expect("finish whole-archive orphan");
            store.close().expect("close whole-removal orphan writer");
        }

        let options = scenario_options(SWEEP_SCENARIO);
        let plan = plan_compaction(&directory.path, &options)
            .expect("plan removal followed by rewrite fixture");
        let removal_source = plan
            .actions()
            .iter()
            .find_map(|action| match action {
                CompactionAction::RemoveReclaimableArchive { file_name, .. } => {
                    Some(file_name.clone())
                }
                _ => None,
            })
            .expect("fixture must contain a whole-archive removal");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RewriteArchive {
                file_name,
                replacement_name,
                ..
            } if file_name == &rewrite_source && replacement_name == &replacement
        )));
        let rewrite_bytes = std::fs::read(directory.path.join(&rewrite_source))
            .expect("read rewrite source before boundary fault");
        (
            snapshot_repository(&directory.path),
            removal_source,
            rewrite_source,
            rewrite_bytes,
            replacement,
        )
    }

    fn assert_removal_then_rewrite_prefix(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        removal_source: &str,
        rewrite_source: &str,
        rewrite_bytes: &[u8],
        replacement: &str,
    ) {
        assert_exact_snapshot_reopens(directory, snapshot);
        assert_eq!(
            std::fs::read(directory.join("journal.log"))
                .expect("read journal after phase-boundary fault"),
            snapshot.journal_bytes,
            "segment sweep prefix cannot alter retained journal history"
        );
        assert!(
            !directory.join(removal_source).exists(),
            "the boundary must be reached after the whole-removal phase"
        );
        assert_eq!(
            std::fs::read(directory.join(rewrite_source))
                .expect("rewrite source remains at the phase boundary"),
            rewrite_bytes,
            "the rewrite phase must not have begun"
        );
        assert!(
            !directory.join(replacement).exists(),
            "no replacement may publish before the rewrite phase begins"
        );
    }

    fn assert_postcomp_removal_then_rewrite_prefix(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        removal_source: &str,
        rewrite_source: &str,
        rewrite_bytes: &[u8],
        replacement: &str,
    ) {
        // Post-compaction reclamation is allowed to retire older journal
        // roots before the journal itself is compacted. Its healthy-prefix
        // oracle is therefore the still-durable current head, not byte-for-
        // byte preservation of every historical root.
        let repository = Repository::open(directory)
            .expect("fresh read-only reopen after post-compaction boundary fault");
        assert_eq!(repository.head_record_identifier(), snapshot.head);
        repository
            .content_root()
            .expect("the durable current head remains traversable");
        let readable_roots = readable_journal_roots(&repository);
        assert!(
            readable_roots.contains(&snapshot.head),
            "the durable current head must remain among the readable journal roots"
        );
        drop(repository);
        drop(RepositoryLock::acquire(directory).expect("child releases repository lock"));

        assert_eq!(
            std::fs::read(directory.join("journal.log"))
                .expect("read journal after post-compaction boundary fault"),
            snapshot.journal_bytes,
            "post-compaction archive sweeping cannot rewrite the journal"
        );
        assert!(
            !directory.join(removal_source).exists(),
            "the post-compaction boundary follows the whole-removal phase"
        );
        assert_eq!(
            std::fs::read(directory.join(rewrite_source))
                .expect("post-compaction rewrite source remains at the boundary"),
            rewrite_bytes,
            "the post-compaction rewrite phase must not have begun"
        );
        assert!(
            !directory.join(replacement).exists(),
            "no post-compaction replacement may publish before its rewrite phase"
        );
    }

    #[test]
    fn removal_to_rewrite_error_and_process_crash_leave_a_healthy_prefix() {
        const CUTPOINT: &str = "sweep.removals-complete-before-rewrites";
        for crash in [false, true] {
            let name = if crash {
                "removal-rewrite-boundary-crash"
            } else {
                "removal-rewrite-boundary-error"
            };
            let directory = TestDirectory::repository(name);
            let (snapshot, removal_source, rewrite_source, rewrite_bytes, replacement) =
                create_removal_then_rewrite_fixture(&directory);

            if crash {
                run_crash_child(&directory.path, SWEEP_SCENARIO, CUTPOINT);
            } else {
                run_error_child(&directory.path, SWEEP_SCENARIO, CUTPOINT);
            }

            assert_removal_then_rewrite_prefix(
                &directory.path,
                &snapshot,
                &removal_source,
                &rewrite_source,
                &rewrite_bytes,
                &replacement,
            );
        }
    }

    #[test]
    fn postcomp_removal_to_rewrite_error_and_process_crash_leave_a_healthy_prefix() {
        const CUTPOINT: &str = "postcomp-sweep.removals-complete-before-rewrites";
        for crash in [false, true] {
            let name = if crash {
                "postcomp-removal-rewrite-boundary-crash"
            } else {
                "postcomp-removal-rewrite-boundary-error"
            };
            let directory = TestDirectory::repository(name);
            let (snapshot, removal_source, rewrite_source, rewrite_bytes, replacement) =
                create_removal_then_rewrite_fixture(&directory);

            if crash {
                run_crash_child(&directory.path, POSTCOMPACTION_SWEEP_SCENARIO, CUTPOINT);
            } else {
                run_error_child(&directory.path, POSTCOMPACTION_SWEEP_SCENARIO, CUTPOINT);
            }

            assert_postcomp_removal_then_rewrite_prefix(
                &directory.path,
                &snapshot,
                &removal_source,
                &rewrite_source,
                &rewrite_bytes,
                &replacement,
            );
        }
    }

    #[test]
    fn segment_unlink_enoent_is_reported_with_a_typed_already_absent_disposition() {
        const CUTPOINT: &str = "sweep.remove-before-source-unlink-not-found";
        let directory = TestDirectory::repository("segment-unlink-already-absent");
        let (snapshot, source, _) = create_whole_archive_sweep_fixture(&directory);

        run_absence_child(&directory.path, SWEEP_SCENARIO, CUTPOINT);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert!(
            !directory.path.join(source).exists(),
            "the simulated competing unlink must leave the planned orphan absent"
        );
    }

    #[test]
    fn published_sweep_survives_process_death_before_source_unlink() {
        let directory = TestDirectory::repository("sweep-before-unlink");
        let (snapshot, source, replacement) = create_partial_sweep_fixture(&directory);

        run_crash_child(
            &directory.path,
            SWEEP_SCENARIO,
            "sweep.published-before-source-unlink",
        );

        assert_sweep_residue(
            &directory.path,
            &snapshot,
            &source,
            &replacement,
            (true, true, true),
        );
    }

    fn assert_sweep_residue(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        source: &str,
        replacement: &str,
        expected: (bool, bool, bool),
    ) {
        let (source_exists, replacement_exists, staging_exists) = expected;
        assert_exact_snapshot_reopens(directory, snapshot);
        assert_eq!(
            std::fs::read(directory.join("journal.log")).expect("read journal after fault"),
            snapshot.journal_bytes
        );
        let source_path = directory.join(source);
        let replacement_path = directory.join(replacement);
        let staging_path = directory.join(format!("{replacement}.cleaning.000"));
        assert_eq!(source_path.exists(), source_exists, "source archive state");
        assert_eq!(
            replacement_path.exists(),
            replacement_exists,
            "replacement archive state"
        );
        assert_eq!(
            staging_path.exists(),
            staging_exists,
            "staging archive state"
        );
        if source_exists {
            TarArchiveReader::open(&source_path).expect("old source residue remains readable");
        }
        if replacement_exists {
            let swept = TarArchiveReader::open(&replacement_path)
                .expect("published replacement is never a corrupt active winner");
            assert!(!swept.is_recovered());
            assert!(swept.contains_segment(snapshot.head.segment));
        }
        if staging_exists {
            let staged = TarArchiveReader::open(&staging_path)
                .expect("ignored staging residue contains a complete validated archive");
            assert!(!staged.is_recovered());
            assert!(staged.contains_segment(snapshot.head.segment));
        }
        if replacement_exists && staging_exists {
            assert_eq!(
                std::fs::read(&replacement_path).expect("read published replacement"),
                std::fs::read(&staging_path).expect("read staging hard link"),
                "the active winner and ignored staging name must expose the same validated bytes"
            );
        }
    }

    #[test]
    fn published_sweep_survives_process_death_after_source_unlink() {
        let directory = TestDirectory::repository("sweep-after-unlink");
        let (snapshot, source, replacement) = create_partial_sweep_fixture(&directory);

        run_crash_child(&directory.path, SWEEP_SCENARIO, "sweep.source-unlinked");

        assert_sweep_residue(
            &directory.path,
            &snapshot,
            &source,
            &replacement,
            (false, true, false),
        );
    }

    #[test]
    fn sweep_syscall_failures_leave_only_an_old_or_valid_new_winner() {
        let cutpoints = [
            ("sweep.staging-write-after-create", true, false, false),
            ("sweep.staging-close-before-trailers", true, false, false),
            ("sweep.staging-before-validation-open", true, false, false),
            ("sweep.before-publish-link", true, false, true),
            ("sweep.after-publish-link", true, true, true),
            ("sweep.before-publish-directory-sync", true, true, true),
            ("sweep.after-publish-directory-sync", true, true, true),
            ("sweep.before-staging-unlink", true, true, true),
            ("sweep.after-staging-unlink", true, true, false),
            ("sweep.before-source-unlink", true, true, false),
            ("sweep.after-source-unlink", false, true, false),
        ];

        for (cutpoint, source_exists, replacement_exists, staging_exists) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, source, replacement) = create_partial_sweep_fixture(&directory);

            run_error_child(&directory.path, SWEEP_SCENARIO, cutpoint);

            assert_sweep_residue(
                &directory.path,
                &snapshot,
                &source,
                &replacement,
                (source_exists, replacement_exists, staging_exists),
            );
        }
    }

    #[test]
    fn sweep_rejects_a_staging_path_substituted_after_validation() {
        let cutpoint = "sweep.staging-validated-before-publish";
        let directory = TestDirectory::repository("sweep-staging-substitution");
        let (snapshot, source, replacement) = create_partial_sweep_fixture(&directory);
        let source_path = directory.path.join(&source);
        let source_bytes = std::fs::read(&source_path).expect("read certified source archive");

        run_substitution_child(&directory.path, SWEEP_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(&source_path).expect("source archive survives substitution"),
            source_bytes,
            "descriptor/path mismatch must be rejected before source removal"
        );
        assert!(
            !directory.path.join(&replacement).exists(),
            "a substituted staging pathname must never become an active higher archive letter"
        );
        assert_eq!(
            std::fs::read(directory.path.join(format!("{replacement}.cleaning.000")))
                .expect("substituted non-active staging residue remains"),
            b"substituted pathname\n"
        );
    }

    #[test]
    fn sweep_does_not_unlink_a_source_path_substituted_after_certification() {
        let cutpoint = "sweep.remove-before-source-identity";
        let directory = TestDirectory::repository("sweep-source-substitution");
        let (snapshot, source, source_bytes) = create_whole_archive_sweep_fixture(&directory);

        run_substitution_child(&directory.path, SWEEP_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join(format!("{source}.validated-inode")))
                .expect("certified source inode remains available"),
            source_bytes,
            "the certified source bytes must not be lost"
        );
        assert_eq!(
            std::fs::read(directory.path.join(&source))
                .expect("substituted source pathname was not unlinked"),
            b"substituted pathname\n",
            "cleanup must not unlink an inode it did not certify"
        );
    }

    #[test]
    fn prospective_sweep_refuses_an_injected_retained_root_in_a_removed_segment() {
        let cutpoint = "cleanup.before-prospective-retained-root-verification";
        let directory = TestDirectory::repository("prospective-retained-root-verification");
        let (snapshot, source, source_bytes) = create_whole_archive_sweep_fixture(&directory);

        run_substitution_child(&directory.path, SWEEP_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join(source)).expect("planned source remains"),
            source_bytes,
            "prospective validation must refuse before any archive mutation"
        );
    }

    #[test]
    fn planned_file_removal_does_not_unlink_a_substituted_staging_inode() {
        let cutpoint = "remove-planned-file.before-final-identity";
        let directory = TestDirectory::repository("planned-removal-substitution");
        let snapshot = snapshot_repository(&directory.path);
        let staged_path = directory.path.join("journal.log.cleaning.000");
        let staged_bytes = std::fs::read(directory.path.join("journal.log"))
            .expect("read canonical journal for redundant staging fixture");
        std::fs::write(&staged_path, &staged_bytes).expect("write redundant journal staging file");
        let plan = plan_compaction(&directory.path, &scenario_options(REMOVAL_SCENARIO))
            .expect("plan redundant staging removal");
        assert!(plan.actions().iter().any(|action| {
            matches!(
                action,
                CompactionAction::RemoveTemporary { file_name, .. }
                    if file_name == "journal.log.cleaning.000"
            )
        }));

        run_substitution_child(&directory.path, REMOVAL_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(&staged_path).expect("substituted staging inode was not unlinked"),
            b"substituted pathname\n"
        );
        assert_eq!(
            std::fs::read(
                directory
                    .path
                    .join("journal.log.cleaning.000.validated-inode")
            )
            .expect("the planned staging inode remains available"),
            staged_bytes
        );
    }

    #[test]
    fn stale_archive_identity_refusal_is_fatal_before_checkpoint_mutation() {
        let cutpoint = "remove-planned-file.before-final-identity";
        let directory = TestDirectory::repository("strict-stale-archive-removal");
        {
            let store = WritableRepository::open(&directory.path)
                .expect("open expired-checkpoint fixture writer");
            create_checkpoint(&store, 1, &[]).expect("create expiring checkpoint");
            store
                .close()
                .expect("close expired-checkpoint fixture writer");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::copy(
            directory.path.join("data00000a.tar"),
            directory.path.join("data00000b.tar"),
        )
        .expect("create semantically identical higher archive generation");
        let options = scenario_options(STALE_ARCHIVE_SCENARIO);
        let plan = plan_compaction(&directory.path, &options).expect("plan combined cleanup");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveStaleArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )));
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveCheckpoints { expired: 1, .. }
        )));
        let snapshot = snapshot_repository(&directory.path);
        let checkpoints_before = Repository::open(&directory.path)
            .expect("repository before strict refusal")
            .checkpoints()
            .expect("checkpoints before strict refusal")
            .len();
        assert_eq!(checkpoints_before, 1);

        run_substitution_child(&directory.path, STALE_ARCHIVE_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log"))
                .expect("journal after strict stale refusal"),
            snapshot.journal_bytes,
            "checkpoint cleanup must not append a head after stale archive identity changed"
        );
        assert_eq!(
            Repository::open(&directory.path)
                .expect("repository after strict refusal")
                .checkpoints()
                .expect("checkpoints after strict refusal")
                .len(),
            checkpoints_before,
            "stale archive refusal must precede checkpoint mutation"
        );
    }

    #[test]
    fn final_reopen_refuses_a_missing_previously_readable_journal_root() {
        let cutpoint = "cleanup.before-final-retained-root-verification";
        let directory = TestDirectory::repository("final-retained-root-verification");
        let snapshot = snapshot_repository(&directory.path);
        let staged_path = directory.path.join("journal.log.cleaning.000");
        std::fs::write(&staged_path, &snapshot.journal_bytes)
            .expect("write redundant staging fixture");

        run_substitution_child(&directory.path, REMOVAL_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("journal after refusal"),
            snapshot.journal_bytes,
            "the in-memory verifier probe must not alter canonical journal bytes"
        );
        assert!(
            staged_path.exists(),
            "deferred deletion must remain pending when final root verification refuses"
        );
    }

    #[test]
    fn final_reopen_refuses_a_missing_byte_exact_retained_journal_line() {
        let cutpoint = "cleanup.before-final-retained-line-verification";
        let directory = TestDirectory::repository("final-retained-line-verification");
        let snapshot = snapshot_repository(&directory.path);
        let staged_path = directory.path.join("journal.log.cleaning.000");
        std::fs::write(&staged_path, &snapshot.journal_bytes)
            .expect("write redundant staging fixture");

        run_substitution_child(&directory.path, REMOVAL_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("journal after refusal"),
            snapshot.journal_bytes,
            "the in-memory verifier probe must not alter canonical journal bytes"
        );
        assert!(
            staged_path.exists(),
            "deferred deletion must remain pending when byte-exact verification refuses"
        );
    }

    fn create_journal_rewrite_fixture(directory: &TestDirectory) -> (RepositorySnapshot, Vec<u8>) {
        {
            let store = WritableRepository::open(&directory.path).expect("open journal fixture");
            create_checkpoint(&store, 60_000, &[]).expect("create second readable revision");
            store.close().expect("close journal fixture writer");
        }
        let expected_replacement =
            std::fs::read(directory.path.join("journal.log")).expect("read retained journal");
        let missing = SegmentIdentifier::new(7, 0xA000_0000_0000_0007);
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(directory.path.join("journal.log"))
                .expect("open journal for dangling line"),
            "{missing}:0 root 123"
        )
        .expect("append dangling journal line");
        let snapshot = snapshot_repository(&directory.path);
        assert_eq!(snapshot.readable_journal_roots.len(), 2);
        (snapshot, expected_replacement)
    }

    fn assert_journal_residue(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        expected_replacement: &[u8],
        cutpoint: &str,
        expected: (bool, bool, bool),
    ) {
        let (replacement_installed, temporary_exists, backup_exists) = expected;
        assert_exact_snapshot_reopens(directory, snapshot);
        let canonical = std::fs::read(directory.join("journal.log"))
            .expect("read canonical journal after fault");
        assert_eq!(
            canonical.as_slice(),
            if replacement_installed {
                expected_replacement
            } else {
                snapshot.journal_bytes.as_slice()
            },
            "canonical journal must be exactly the old or replacement byte sequence at {cutpoint}"
        );

        let temporary = directory.join("journal.log.cleaning.000");
        let backup = directory.join("journal.log.bak.000");
        assert_eq!(temporary.exists(), temporary_exists, "{cutpoint}");
        assert_eq!(backup.exists(), backup_exists, "{cutpoint}");
        if temporary_exists {
            assert_eq!(
                std::fs::read(temporary).expect("read staged journal"),
                expected_replacement,
                "staging residue must contain the exact replacement"
            );
        }
        if backup_exists {
            assert_eq!(
                std::fs::read(backup).expect("read journal backup"),
                snapshot.journal_bytes,
                "backup residue must contain the exact old journal"
            );
        }
    }

    #[test]
    fn journal_rewrite_crash_boundaries_preserve_exact_readable_roots() {
        let cutpoints = [
            ("journal.temporary-durable", false, true, false),
            ("journal.backup-file-durable", false, true, true),
            ("journal.pre-rename-directory-durable", false, true, true),
            ("journal.renamed-before-directory-sync", true, false, true),
            ("journal.rename-durable", true, false, true),
        ];

        for (cutpoint, replacement_installed, temporary_exists, backup_exists) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, expected_replacement) = create_journal_rewrite_fixture(&directory);

            run_crash_child(&directory.path, JOURNAL_SCENARIO, cutpoint);

            assert_journal_residue(
                &directory.path,
                &snapshot,
                &expected_replacement,
                cutpoint,
                (replacement_installed, temporary_exists, backup_exists),
            );
        }
    }

    #[test]
    fn journal_syscall_failures_leave_an_exact_old_or_new_canonical_file() {
        let cutpoints = [
            ("journal.temporary-durable", false, false, false),
            ("journal.backup-file-durable", false, false, false),
            (
                "journal.before-pre-rename-directory-sync",
                false,
                false,
                false,
            ),
            (
                "journal.after-pre-rename-directory-sync",
                false,
                false,
                false,
            ),
            ("journal.pre-rename-directory-durable", false, false, true),
            ("journal.before-rename", false, false, true),
            ("journal.after-rename", true, false, true),
            (
                "journal.before-post-rename-directory-sync",
                true,
                false,
                true,
            ),
            (
                "journal.after-post-rename-directory-sync",
                true,
                false,
                true,
            ),
        ];

        for (cutpoint, replacement_installed, temporary_exists, backup_exists) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, expected_replacement) = create_journal_rewrite_fixture(&directory);

            run_error_child(&directory.path, JOURNAL_SCENARIO, cutpoint);

            assert_journal_residue(
                &directory.path,
                &snapshot,
                &expected_replacement,
                cutpoint,
                (replacement_installed, temporary_exists, backup_exists),
            );
        }
    }
}
