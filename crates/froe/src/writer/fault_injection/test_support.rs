//! The child process every probe forks: the scenarios it can be armed
//! with, how it is run, and the snapshot the parent compares the store
//! against afterwards.

use super::{
    ABSENCE_MODE, CHILD_ENVIRONMENT, CUTPOINT_ENVIRONMENT, ERROR_MODE, MODE_ENVIRONMENT,
    SUBSTITUTE_MODE,
};
#[cfg(unix)]
use super::{CRASH_EXIT_CODE, CRASH_MODE, VERIFIED_EXIT_CODE};
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
    "writer::fault_injection::test_support::cleanup_fault_child";

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
        COMPACTION_PUBLICATION_SCENARIO => {
            return CompactionOptions::default()
                .with_tasks([MaintenanceTask::Segments])
                .with_compaction(CompactionKind::Full);
        }
        PURGING_PUBLICATION_SCENARIO => {
            return CompactionOptions::default()
                .with_tasks([MaintenanceTask::Segments])
                .with_compaction(CompactionKind::Full)
                .with_orphaned_version_history_purge();
        }
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
    if scenario == REINDEX_SCENARIO {
        run_reindex_child(&directory, &cutpoint, &mode);
        return;
    }
    if scenario == super::lucene_fixture::LUCENE_REINDEX_SCENARIO {
        super::lucene_fixture::run_lucene_reindex_child(&directory, &cutpoint, &mode);
        return;
    }
    if scenario == LUCENE_IMPORT_SCENARIO {
        run_lucene_import_child(&directory, &cutpoint, &mode);
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

pub(crate) const COMPACTION_PUBLICATION_SCENARIO: &str = "compaction-publication";

pub(crate) const PURGING_PUBLICATION_SCENARIO: &str = "purging-publication";

/// A repository carrying one orphaned version history, so a purging run has
/// a real omission to perform when a probe interrupts it.
pub(crate) fn write_orphaned_history_fixture(directory: &Path) {
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    let store = WritableRepository::open(directory).expect("bootstrap the purge fixture");
    let generation = store.writing_generation().expect("writing generation");
    let mut writer = store.record_writer(generation);
    let versionable = writer
        .write_string("bbbbbbbb-2222-4222-8222-222222222222")
        .expect("versionable identifier");
    let history = writer
        .write_node(
            Some("nt:versionHistory"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:versionableUuid".to_owned(),
                property_type: crate::content::property::PropertyType::String,
                values: PropertyValuesToWrite::Single(versionable),
            }],
        )
        .expect("history");
    let version_storage = writer
        .write_node(
            Some("rep:versionStorage"),
            &[],
            &ChildNodesToWrite::One {
                name: "bbbbbbbb-2222-4222-8222-222222222222".to_owned(),
                node: history,
            },
            &[],
        )
        .expect("version storage");
    let jcr_system = writer
        .write_node(
            Some("rep:system"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:versionStorage".to_owned(),
                node: version_storage,
            },
            &[],
        )
        .expect("jcr:system");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:system".to_owned(),
                node: jcr_system,
            },
            &[],
        )
        .expect("root");
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
        .expect("super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush");
    store.close().expect("close the purge fixture");
}

pub(crate) const REINDEX_SCENARIO: &str = "reindex";

/// A single-valued property of `property_type`, written from its text.
pub(crate) fn single_valued<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    name: &str,
    property_type: crate::content::property::PropertyType,
    value: &str,
) -> crate::writer::record_writer::PropertyToWrite {
    let value = writer.write_string(value).expect("write a string");
    crate::writer::record_writer::PropertyToWrite {
        name: name.to_owned(),
        property_type,
        values: crate::writer::record_writer::PropertyValuesToWrite::Single(value),
    }
}

/// A single-entry multi-valued `NAME` property, which is what
/// `propertyNames` is.
pub(crate) fn one_name<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    name: &str,
    value: &str,
) -> crate::writer::record_writer::PropertyToWrite {
    let value = writer.write_string(value).expect("write a string");
    crate::writer::record_writer::PropertyToWrite {
        name: name.to_owned(),
        property_type: crate::content::property::PropertyType::Name,
        values: crate::writer::record_writer::PropertyValuesToWrite::Multiple(vec![value]),
    }
}

/// The fixture's content: enough titled pages that a small sort budget
/// spills and merges rather than sorting one resident run.
fn write_flagged_index_content<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
) -> RecordIdentifier {
    use crate::content::property::PropertyType;
    use crate::writer::record_writer::ChildNodesToWrite;

    let mut pages = Vec::new();
    for serial in 0..64u32 {
        let properties = vec![
            single_valued(
                writer,
                "jcr:primaryType",
                PropertyType::Name,
                "nt:unstructured",
            ),
            single_valued(
                writer,
                "jcr:title",
                PropertyType::String,
                &format!("title-{serial:04}"),
            ),
        ];
        let page = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)
            .expect("write a page");
        pages.push((format!("page-{serial:04}"), page));
    }
    let content_properties = vec![single_valued(
        writer,
        "jcr:primaryType",
        PropertyType::Name,
        "nt:unstructured",
    )];
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(pages),
            &content_properties,
        )
        .expect("write the content")
}

/// The fixture's `/oak:index`: the flagged `title` definition a run
/// rebuilds, and the `nodetype` definition the path service reads.
fn write_flagged_index_definitions<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
) -> RecordIdentifier {
    use crate::content::property::PropertyType;
    use crate::writer::record_writer::ChildNodesToWrite;

    let title_definition = vec![
        single_valued(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        single_valued(writer, "type", PropertyType::String, "property"),
        single_valued(writer, "reindex", PropertyType::Boolean, "true"),
        one_name(writer, "propertyNames", "jcr:title"),
    ];
    let title = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &title_definition)
        .expect("write the flagged definition");

    let nodetype_definition = vec![
        single_valued(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        single_valued(writer, "type", PropertyType::String, "property"),
        one_name(writer, "propertyNames", "jcr:primaryType"),
    ];
    let nodetype = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &nodetype_definition)
        .expect("write the nodetype definition");

    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("nodetype".to_owned(), nodetype),
                ("title".to_owned(), title),
            ]),
            &[],
        )
        .expect("write oak:index")
}

/// A store with enough indexed content that the sort spills, plus the one
/// flagged definition a run rebuilds.
///
/// Laid out with the store one level below `root`, so the run's work
/// directory is its sibling rather than a subdirectory of the store: a
/// probe has to be able to look at the spill files without looking inside
/// the repository.
pub(crate) fn write_flagged_index_fixture(root: &Path) -> PathBuf {
    use crate::writer::record_writer::ChildNodesToWrite;

    let directory = root.join("store");
    std::fs::create_dir_all(&directory).expect("create the reindex fixture store directory");
    std::fs::create_dir_all(reindex_work_directory(&directory))
        .expect("create the reindex fixture work directory");

    let store = WritableRepository::open(&directory).expect("bootstrap the reindex fixture");
    let generation = store.writing_generation().expect("writing generation");
    let mut writer = store.record_writer(generation);

    let content = write_flagged_index_content(&mut writer);
    let oak_index = write_flagged_index_definitions(&mut writer);
    let root_node = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("content".to_owned(), content),
                (crate::index::INDEX_DEFINITIONS_NAME.to_owned(), oak_index),
            ]),
            &[],
        )
        .expect("write the root");
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root_node,
            },
            &[],
        )
        .expect("write the super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush");
    store.close().expect("close the reindex fixture");
    directory
}

/// The work directory a reindex probe's child uses: a sibling of the store,
/// so the parent knows where to look for the run's spill files without
/// being told.
pub(crate) fn reindex_work_directory(store: &Path) -> PathBuf {
    store
        .parent()
        .expect("the fixture store always has a parent")
        .join("work")
}

/// The reindex scenario: rebuild the fixture's one flagged definition with
/// the cutpoint armed.
pub(crate) fn run_reindex_child(store: &Path, cutpoint: &str, mode: &str) {
    use crate::writer::index::{ReindexOptions, WorkDirectory, reindex};

    let options = ReindexOptions::new()
        .with_work_directory(WorkDirectory::OperatorNamed(reindex_work_directory(store)))
        // Small, so the sort spills and the boundary before the spill
        // cleanup has files to observe.
        .with_sort_budget_bytes(64);
    let outcome = reindex(store, options);
    match mode {
        ERROR_MODE => {
            let error = outcome.expect_err("the reindex completed without the injected error");
            assert!(
                error.to_string().contains(cutpoint),
                "the reindex failed before {cutpoint}: {error}"
            );
        }
        #[cfg(unix)]
        CRASH_MODE => match outcome {
            Ok(_) => panic!("the reindex completed without reaching {cutpoint}"),
            Err(error) => panic!("the reindex failed before {cutpoint}: {error}"),
        },
        other => panic!("unsupported reindex fault mode {other}"),
    }
    // SAFETY: `_exit` has no memory-safety preconditions and this is an
    // isolated child whose error path was checked above.
    #[cfg(unix)]
    unsafe {
        libc::_exit(VERIFIED_EXIT_CODE)
    }
}

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

pub(crate) const LUCENE_IMPORT_SCENARIO: &str = "lucene-import";

/// The blob size the import fixture's definition declares, and the one its
/// `:data` is written with. They must agree: the reader takes the size from
/// the definition, and a writer that chose its own would produce a
/// directory the reader cuts into the wrong blocks.
const IMPORT_FIXTURE_BLOB_SIZE: i64 = 32_768;

/// The files the import fixture's index holds: the committed sample index,
/// written by Oak itself.
///
/// They have to be a *coherent* Lucene index rather than plausible bytes,
/// because the plan refuses an incoherent directory before it copies one —
/// so a fixture of invented files would be refused before any cutpoint
/// could fire. The largest of them is what `mid-file-copy` interrupts.
fn import_fixture_files() -> Vec<(String, Vec<u8>)> {
    let directory =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lucene-4-7-sample-index");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&directory)
        .expect("read the sample index")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("read a sample index file"),
            )
        })
        .filter(|(name, _)| name != "README.md")
        .collect();
    files.sort();
    files
}

/// A store holding one asynchronous `lucene` definition with a `:data`
/// directory, a resolvable lane checkpoint, and a dump of it beside the
/// store for the import to read.
///
/// Returns the store directory; the input directory is
/// [`lucene_import_input`] of it, so the child finds it without being told.
/// The `/oak:index` node of the import fixture: one asynchronous `lucene`
/// definition whose `:data` holds the committed sample index.
fn write_import_fixture_definitions<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
) -> RecordIdentifier {
    use crate::content::property::PropertyType;
    use crate::writer::index::lucene_directory::{DirectoryListing, OakDirectoryWriter};
    use crate::writer::record_writer::ChildNodesToWrite;

    let data = {
        let mut builder =
            OakDirectoryWriter::new(writer, IMPORT_FIXTURE_BLOB_SIZE, DirectoryListing::Saved);
        for (name, bytes) in import_fixture_files() {
            builder
                .add_file(&name, std::io::Cursor::new(bytes))
                .expect("write an index file");
        }
        builder.finish().expect("finish :data")
    };

    let definition_properties = [
        single_valued(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        single_valued(writer, "type", PropertyType::String, "lucene"),
        single_valued(writer, "async", PropertyType::String, "async"),
        single_valued(
            writer,
            "blobSize",
            PropertyType::Long,
            &IMPORT_FIXTURE_BLOB_SIZE.to_string(),
        ),
    ];
    let definition = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: ":data".to_owned(),
                node: data,
            },
            &definition_properties,
        )
        .expect("write the definition");
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "lucene".to_owned(),
                node: definition,
            },
            &[],
        )
        .expect("write /oak:index")
}

/// The super-root of the import fixture: the content root, and a
/// `checkpoint-1` pinning it by record identity.
///
/// Checkpoints hang off the **super-root**, which is where
/// `Repository::checkpoints` reads them, and a real checkpoint shares the
/// content root's record — which is what the state rule compares.
fn write_import_fixture_super_root<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    content_root: RecordIdentifier,
) -> RecordIdentifier {
    use crate::writer::record_writer::ChildNodesToWrite;

    let checkpoint = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: content_root,
            },
            &[],
        )
        .expect("write the checkpoint");
    let checkpoints = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "checkpoint-1".to_owned(),
                node: checkpoint,
            },
            &[],
        )
        .expect("write the checkpoints node");
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("checkpoints".to_owned(), checkpoints),
                ("root".to_owned(), content_root),
            ]),
            &[],
        )
        .expect("write the super root")
}

pub(crate) fn write_lucene_import_fixture(root: &Path) -> PathBuf {
    use crate::content::property::PropertyType;
    use crate::writer::record_writer::ChildNodesToWrite;

    let directory = root.join("store");
    std::fs::create_dir_all(&directory).expect("create the import fixture store directory");

    let store = WritableRepository::open(&directory).expect("bootstrap the import fixture");
    let generation = store.writing_generation().expect("writing generation");
    let mut writer = store.record_writer(generation);

    let oak_index = write_import_fixture_definitions(&mut writer);
    let lane_properties = [single_valued(
        &mut writer,
        "async",
        PropertyType::String,
        "checkpoint-1",
    )];
    let lane = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &lane_properties)
        .expect("write /:async");
    let content_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                (":async".to_owned(), lane),
                (crate::index::INDEX_DEFINITIONS_NAME.to_owned(), oak_index),
            ]),
            &[],
        )
        .expect("write the content root");
    let head = write_import_fixture_super_root(&mut writer, content_root);

    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush");
    store.close().expect("close the import fixture");

    // The input directory is the store's own dump, so the file bytes the
    // import reads are exactly the bytes the store holds and a round trip
    // is the expected post-state.
    let repository = crate::store::Repository::open(&directory).expect("open the import fixture");
    crate::index::lucene::dump::dump_lucene_indexes(
        &repository,
        &crate::index::lucene::dump::DumpOptions::new(Vec::new(), lucene_import_dump(&directory)),
    )
    .expect("dump the import fixture");
    drop(repository);

    directory
}

/// Where the import fixture's dump is written: a sibling of the store.
fn lucene_import_dump(store: &Path) -> PathBuf {
    store
        .parent()
        .expect("the fixture store always has a parent")
        .join("dump")
}

/// The directory an import reads, which is what the dump wrote.
pub(crate) fn lucene_import_input(store: &Path) -> PathBuf {
    lucene_import_dump(store).join("index-dumps")
}

/// The import scenario: import the fixture's own dump back over it with the
/// cutpoint armed.
pub(crate) fn run_lucene_import_child(store: &Path, cutpoint: &str, mode: &str) {
    use crate::writer::index::lucene_import::{LuceneImportOptions, lucene_import};

    let outcome = lucene_import(store, &LuceneImportOptions::new(lucene_import_input(store)));
    match mode {
        ERROR_MODE => {
            let error = outcome.expect_err("the import completed without the injected error");
            let text = error.to_string();
            assert!(
                text.contains(cutpoint),
                "the import failed before {cutpoint}: {error}"
            );
            if cutpoint == "lucene-import.mid-file-copy" {
                // The one cutpoint that lands inside a file has to say
                // which file and how far in, because that is what an
                // operator whose filesystem filled up needs.
                assert!(
                    text.contains("the copy of ") && text.contains(" stopped at byte "),
                    "a mid-copy failure names the file and the offset: {text}"
                );
            }
        }
        #[cfg(unix)]
        CRASH_MODE => match outcome {
            Ok(_) => panic!("the import completed without reaching {cutpoint}"),
            Err(error) => panic!("the import failed before {cutpoint}: {error}"),
        },
        other => panic!("unsupported import fault mode {other}"),
    }
    // SAFETY: `_exit` has no memory-safety preconditions and this is an
    // isolated child whose error path was checked above.
    #[cfg(unix)]
    unsafe {
        libc::_exit(VERIFIED_EXIT_CODE)
    }
}
