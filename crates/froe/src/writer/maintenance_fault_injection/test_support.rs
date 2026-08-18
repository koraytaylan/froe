//! The child process every probe forks: the scenarios it can be armed
//! with, how it is run, and the snapshot the parent compares the store
//! against afterwards.

use super::{
    ABSENCE_MODE, CHILD_ENVIRONMENT, CRASH_EXIT_CODE, CRASH_MODE, CUTPOINT_ENVIRONMENT, ERROR_MODE,
    MODE_ENVIRONMENT, SUBSTITUTE_MODE, VERIFIED_EXIT_CODE,
};
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::compaction::CompactionKind;
use crate::writer::maintenance::{CompactionOptions, MaintenanceTask, compact};
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::store_writer::WritableRepository;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct TestDirectory {
    pub(crate) path: PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "froe-cleanup-fault-{name}-{}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create fault-injection repository directory");
        Self { path }
    }

    pub(crate) fn repository(name: &str) -> Self {
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

pub(crate) const REPOSITORY_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_REPOSITORY";

pub(crate) const SCENARIO_ENVIRONMENT: &str = "FROE_CLEANUP_FAULT_SCENARIO";

/// The child entrypoint's own test path, which `--exact` must match. A
/// probe that matched nothing would run no child at all, so
/// [`cleanup_fault_child`] asserts the marker it was passed rather than
/// silently succeeding.
pub(crate) const CHILD_TEST_NAME: &str =
    "writer::maintenance_fault_injection::test_support::cleanup_fault_child";

pub(crate) static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct RepositorySnapshot {
    pub(crate) head: RecordIdentifier,
    pub(crate) readable_journal_roots: Vec<RecordIdentifier>,
    pub(crate) journal_bytes: Vec<u8>,
}

pub(crate) fn snapshot_repository(directory: &Path) -> RepositorySnapshot {
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

pub(crate) fn readable_journal_roots(repository: &Repository) -> Vec<RecordIdentifier> {
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

pub(crate) fn assert_exact_snapshot_reopens(directory: &Path, expected: &RepositorySnapshot) {
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

pub(crate) fn cleanup_child_output(
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

pub(crate) fn run_crash_child(directory: &Path, scenario: &str, cutpoint: &str) {
    let output = cleanup_child_output(directory, scenario, cutpoint, CRASH_MODE);
    assert_eq!(
        output.status.code(),
        Some(CRASH_EXIT_CODE),
        "child did not reach {cutpoint}; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn run_error_child(directory: &Path, scenario: &str, cutpoint: &str) {
    let output = cleanup_child_output(directory, scenario, cutpoint, ERROR_MODE);
    assert_eq!(
        output.status.code(),
        Some(VERIFIED_EXIT_CODE),
        "child did not return the injected error at {cutpoint}; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn run_substitution_child(directory: &Path, scenario: &str, cutpoint: &str) {
    let output = cleanup_child_output(directory, scenario, cutpoint, SUBSTITUTE_MODE);
    assert_eq!(
        output.status.code(),
        Some(VERIFIED_EXIT_CODE),
        "child did not reject the substituted path at {cutpoint}; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn run_absence_child(directory: &Path, scenario: &str, cutpoint: &str) {
    let output = cleanup_child_output(directory, scenario, cutpoint, ABSENCE_MODE);
    assert_eq!(
        output.status.code(),
        Some(VERIFIED_EXIT_CODE),
        "child did not observe the already-absent unlink at {cutpoint}; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn scenario_options(scenario: &str) -> CompactionOptions {
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
pub(crate) fn cleanup_fault_child() {
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
pub(crate) fn run_postcompaction_sweep_child(directory: &Path, cutpoint: &str, mode: &str) {
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
pub(crate) fn assert_substitution_outcome(
    scenario: &str,
    cutpoint: &str,
    outcome: crate::error::Result<crate::writer::maintenance::CompactionOutcome>,
) {
    if cutpoint == "remove-planned-file.before-final-identity" && scenario == REMOVAL_SCENARIO {
        let outcome = outcome.expect("a late planned-file identity refusal is a partial outcome");
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
pub(crate) fn run_compaction_child(directory: &Path, scenario: &str, cutpoint: &str, mode: &str) {
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

pub(crate) const CHECKPOINT_SCENARIO: &str = "checkpoint";

pub(crate) const MANIFEST_SCENARIO: &str = "manifest";

pub(crate) const SWEEP_SCENARIO: &str = "sweep";

pub(crate) const REMOVAL_SCENARIO: &str = "removal";

pub(crate) const STALE_ARCHIVE_SCENARIO: &str = "stale-archive";

pub(crate) const POSTCOMPACTION_SWEEP_SCENARIO: &str = "postcomp-sweep";

pub(crate) const JOURNAL_SCENARIO: &str = "journal";

pub(crate) fn archive_file_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(directory)
        .expect("list archive directory")
        .map(|entry| entry.expect("read archive entry").file_name())
        .filter_map(|name| name.to_str().map(str::to_owned))
        .filter(|name| ArchiveFileName::parse(name).is_some())
        .collect();
    names.sort();
    names
}
