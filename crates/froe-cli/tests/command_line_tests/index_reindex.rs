//! `froe index reindex`: the one `index` subcommand that writes.
//!
//! What these pin is the operator's contract — the dry run touches nothing,
//! the confirmation is real, the summary counts what happened, and the
//! reporting never reaches standard output — rather than the rebuild
//! itself, which has its own tests in `froe`.

use std::path::Path;

use froe::PropertyType;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use froe::writer::store_writer::WritableRepository;

use super::TestDirectory;
use crate::filesystem_snapshot::directory_snapshot;

fn single<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    name: &str,
    property_type: PropertyType,
    value: &str,
) -> PropertyToWrite {
    let written = writer.write_string(value).expect("property value");
    PropertyToWrite {
        name: name.to_owned(),
        property_type,
        values: PropertyValuesToWrite::Single(written),
    }
}

fn one_name<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    name: &str,
    value: &str,
) -> PropertyToWrite {
    let written = writer.write_string(value).expect("name value");
    PropertyToWrite {
        name: name.to_owned(),
        property_type: PropertyType::Name,
        values: PropertyValuesToWrite::Multiple(vec![written]),
    }
}

/// Which shape of definition the fixture carries beside `nodetype`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Subject {
    /// A flagged `jcr:title` property index over the head.
    FlaggedTitle,
    /// A flagged counter parked on a lane `/:async` does not hold, already
    /// carrying an `:index` from an earlier indexing run.
    CounterOnAnAbsentLane,
}

/// The content the definitions cover: `/content/page` carrying `jcr:title`.
fn write_content<Sink: SegmentSink>(writer: &mut RecordWriter<Sink>) -> froe::RecordIdentifier {
    let page_properties = [
        single(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "nt:unstructured",
        ),
        single(writer, "jcr:title", PropertyType::String, "Alpha"),
    ];
    let page = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &page_properties)
        .expect("write /content/page");
    let content_properties = [single(
        writer,
        "jcr:primaryType",
        PropertyType::Name,
        "nt:unstructured",
    )];
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "page".to_owned(),
                node: page,
            },
            &content_properties,
        )
        .expect("write /content")
}

/// The `nodetype` definition the index path service reads.
fn write_node_type_definition<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
) -> froe::RecordIdentifier {
    let properties = [
        single(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        single(writer, "type", PropertyType::String, "property"),
        one_name(writer, "propertyNames", "jcr:primaryType"),
    ];
    writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)
        .expect("write the nodetype definition")
}

/// The definition under test, and the lane `/:async` should name if any.
fn write_subject<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    subject: Subject,
) -> (froe::RecordIdentifier, Option<&'static str>) {
    match subject {
        Subject::FlaggedTitle => {
            let properties = [
                single(
                    writer,
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single(writer, "type", PropertyType::String, "property"),
                single(writer, "reindex", PropertyType::Boolean, "true"),
                one_name(writer, "propertyNames", "jcr:title"),
            ];
            (
                writer
                    .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)
                    .expect("write the flagged definition"),
                None,
            )
        }
        Subject::CounterOnAnAbsentLane => {
            let count = [single(writer, ":cnt", PropertyType::Long, "40")];
            let index = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &count)
                .expect("write the counter's existing :index");
            let properties = [
                single(
                    writer,
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single(writer, "type", PropertyType::String, "counter"),
                single(writer, "reindex", PropertyType::Boolean, "true"),
                single(writer, "async", PropertyType::String, "async"),
                single(writer, "resolution", PropertyType::Long, "8"),
                single(writer, "info", PropertyType::String, "kept verbatim"),
            ];
            (
                writer
                    .write_node(
                        None,
                        &[],
                        &ChildNodesToWrite::One {
                            name: ":index".to_owned(),
                            node: index,
                        },
                        &properties,
                    )
                    .expect("write the counter definition"),
                // `/:async` exists but names a different lane, so the
                // counter's own lane is absent from it.
                Some("other-lane"),
            )
        }
    }
}

/// A store with content and one flagged `jcr:title` property index, for
/// the compaction plan's pending-reindex warning.
pub(crate) fn build_flagged_store(directory: &Path) {
    build_store(directory, Subject::FlaggedTitle);
}

/// A store with content and one flagged definition of `subject`.
fn build_store(directory: &Path, subject: Subject) {
    let store = WritableRepository::open(directory).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    let content = write_content(&mut writer);
    let nodetype = write_node_type_definition(&mut writer);
    let (subject_record, lane) = write_subject(&mut writer, subject);

    let oak_index = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("nodetype".to_owned(), nodetype),
                ("subject".to_owned(), subject_record),
            ]),
            &[],
        )
        .expect("write /oak:index");

    let mut root_children = vec![
        ("content".to_owned(), content),
        ("oak:index".to_owned(), oak_index),
    ];
    if let Some(lane) = lane {
        let lane_properties = [single(&mut writer, lane, PropertyType::String, "c1")];
        let async_node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &lane_properties)
            .expect("write /:async");
        root_children.push((":async".to_owned(), async_node));
    }
    root_children.sort_by(|left, right| left.0.cmp(&right.0));

    let root = writer
        .write_node(None, &[], &ChildNodesToWrite::Many(root_children), &[])
        .expect("write the root");
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
        .expect("write the super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush");
    store.close().expect("close");
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn froe_reindex(store: &Path, arguments: &[&str]) -> Run {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_froe"));
    command.arg("index").arg("reindex").arg(store);
    for argument in arguments {
        command.arg(argument);
    }
    let output = command.output().expect("run froe index reindex");
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A test directory holding the store and the run's work directory as
/// siblings, so the work directory is never inside the repository.
fn fixture(
    name: &str,
    subject: Subject,
) -> (TestDirectory, std::path::PathBuf, std::path::PathBuf) {
    let directory = TestDirectory::new(name);
    let store = directory.path.join("segmentstore");
    let work = directory.path.join("work");
    std::fs::create_dir_all(&store).expect("create the store directory");
    std::fs::create_dir_all(&work).expect("create the work directory");
    build_store(&store, subject);
    (directory, store, work)
}

#[test]
pub(crate) fn a_dry_run_takes_no_lock_and_writes_nothing() {
    let (_directory, store, work) = fixture("reindex-dry-run", Subject::FlaggedTitle);
    let before = directory_snapshot(&store);

    let run = froe_reindex(
        &store,
        &[
            "--dry-run",
            "--work-directory",
            work.to_str().expect("utf-8"),
        ],
    );

    assert_eq!(run.status.code(), Some(0), "{}", run.stderr);
    assert!(
        run.stdout.contains("rebuild /oak:index/subject"),
        "the plan names what it would rebuild: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("dry-run: repository was not modified"),
        "{}",
        run.stdout
    );
    assert_eq!(
        directory_snapshot(&store),
        before,
        "a dry run must not write a byte"
    );

    // The `repo.lock` inode is left behind by whoever wrote the store, so
    // its presence proves nothing. What proves the dry run takes no lock is
    // that it succeeds while this process holds it.
    let held = froe::writer::RepositoryLock::acquire(&store).expect("hold the lock");
    let under_lock = froe_reindex(
        &store,
        &[
            "--dry-run",
            "--work-directory",
            work.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(
        under_lock.status.code(),
        Some(0),
        "a dry run must plan while another writer holds the lock: {}",
        under_lock.stderr
    );
    drop(held);
}

#[test]
pub(crate) fn a_scripted_run_without_yes_plans_and_cancels_naming_the_flag() {
    let (_directory, store, work) = fixture("reindex-cancel", Subject::FlaggedTitle);

    let run = froe_reindex(&store, &["--work-directory", work.to_str().expect("utf-8")]);

    assert_ne!(
        run.status.code(),
        Some(0),
        "a cancelled run does not succeed"
    );
    assert!(
        run.stdout.contains("rebuild /oak:index/subject"),
        "the plan is printed before the question: {}",
        run.stdout
    );
    assert!(
        run.stderr.contains("reindex cancelled") && run.stderr.contains("--yes"),
        "the cancellation names the flag that would supply an answer: {}",
        run.stderr
    );
}

#[test]
pub(crate) fn yes_applies_and_the_summary_counts_what_happened() {
    let (_directory, store, work) = fixture("reindex-apply", Subject::FlaggedTitle);

    let run = froe_reindex(
        &store,
        &["--yes", "--work-directory", work.to_str().expect("utf-8")],
    );

    assert_eq!(run.status.code(), Some(0), "{}", run.stderr);
    assert!(
        run.stdout.contains("reindexed 1 index"),
        "the summary counts the rebuild: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("head ") && run.stdout.contains(" -> "),
        "the summary names both heads: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("1 entry"),
        "the summary reports what was written: {}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("no checkpoint pins the replaced index records"),
        "the summary says what keeps the old records live: {}",
        run.stdout
    );

    // And the definition is no longer flagged, so a rerun has nothing to do.
    let rerun = froe_reindex(
        &store,
        &["--yes", "--work-directory", work.to_str().expect("utf-8")],
    );
    assert_eq!(rerun.status.code(), Some(0), "{}", rerun.stderr);
    assert!(
        rerun.stdout.contains("nothing to do"),
        "a rerun has nothing to do: {}",
        rerun.stdout
    );
}

#[test]
pub(crate) fn an_unresolvable_lane_refuses_a_counter_with_or_without_from_head() {
    let (_directory, store, work) = fixture("reindex-from-head", Subject::CounterOnAnAbsentLane);
    let work = work.to_str().expect("utf-8").to_owned();

    // Without the flag the definition is refused by name, and nothing is
    // rebuilt.
    let refused = froe_reindex(&store, &["--yes", "--work-directory", &work]);
    assert_eq!(refused.status.code(), Some(0), "{}", refused.stderr);
    assert!(
        refused.stdout.contains("nothing to do") && refused.stdout.contains("/oak:index/subject"),
        "the refusal names the definition and leaves nothing to do: {}",
        refused.stdout
    );

    // And **with** it the counter is refused too — the one case
    // `--from-head` does not authorize. froe will not rebuild a counter
    // (Oak's own replay would double it) and Oak will not rebuild one on a
    // lane whose checkpoint is gone, so removing its data would leave an
    // index nothing restores.
    let flagged = froe_reindex(&store, &["--yes", "--from-head", "--work-directory", &work]);
    assert_eq!(flagged.status.code(), Some(0), "{}", flagged.stderr);
    assert!(
        flagged.stdout.contains("nothing to do") && flagged.stdout.contains("/oak:index/subject"),
        "the refusal names the definition even under --from-head: {}",
        flagged.stdout
    );
    assert!(
        flagged.stdout.contains("does not rebuild a counter"),
        "the refusal gives the reason: {}",
        flagged.stdout
    );
    assert!(
        !flagged.stdout.contains("reset"),
        "a counter on an unresolvable lane is no longer reset: {}",
        flagged.stdout
    );
}

/// The reporting contract: progress is a report, and a report never mixes
/// with the data an operator pipes.
#[test]
pub(crate) fn reporting_never_reaches_the_standard_output_of_a_reindex_plan() {
    let (_directory, store, work) = fixture("reindex-reporting", Subject::FlaggedTitle);

    for progress in ["always", "never", "auto"] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_froe"));
        command
            .arg("--progress")
            .arg(progress)
            .arg("index")
            .arg("reindex")
            .arg(&store)
            .arg("--dry-run")
            .arg("--work-directory")
            .arg(&work);
        let output = command.output().expect("run froe index reindex");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

        assert!(
            stdout.starts_with("reindex plan for"),
            "--progress {progress} put something before the plan: {stdout}"
        );
        for reported in ["collecting index entries", "note:", "\r"] {
            assert!(
                !stdout.contains(reported),
                "--progress {progress} leaked {reported:?} into standard output: {stdout}"
            );
        }
    }
}
