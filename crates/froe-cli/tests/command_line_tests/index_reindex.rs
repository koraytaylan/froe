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
    /// A flagged `lucene` definition on a lane whose checkpoint resolves,
    /// with the analyzed catch-all rule the fixture's default definition
    /// has.
    FlaggedLucene,
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

/// The `lucene` definition: one `nt:base` rule whose catch-all property
/// definition is analyzed, which is what makes it an `oakCodec` index.
fn write_lucene_definition<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
) -> froe::RecordIdentifier {
    let catch_all = [
        single(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "nt:unstructured",
        ),
        single(writer, "name", PropertyType::String, r"^[^\/]*$"),
        single(writer, "isRegexp", PropertyType::Boolean, "true"),
        single(writer, "analyzed", PropertyType::Boolean, "true"),
        single(writer, "nodeScopeIndex", PropertyType::Boolean, "true"),
    ];
    let all = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &catch_all)
        .expect("write the catch-all property definition");
    let properties = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "all".to_owned(),
                node: all,
            },
            &[],
        )
        .expect("write the properties node");
    let rule = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "properties".to_owned(),
                node: properties,
            },
            &[],
        )
        .expect("write the rule");
    let rules = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "nt:base".to_owned(),
                node: rule,
            },
            &[],
        )
        .expect("write indexRules");
    let definition = [
        single(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        single(writer, "type", PropertyType::String, "lucene"),
        single(writer, "async", PropertyType::String, "async"),
        single(writer, "reindex", PropertyType::Boolean, "true"),
    ];
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "indexRules".to_owned(),
                node: rules,
            },
            &definition,
        )
        .expect("write the lucene definition")
}

/// `/jcr:system/jcr:nodeTypes`, which the Lucene rule resolves through.
fn write_node_types<Sink: SegmentSink>(writer: &mut RecordWriter<Sink>) -> froe::RecordIdentifier {
    let base_properties = [one_name(writer, "rep:primarySubtypes", "nt:unstructured")];
    let base = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &base_properties)
        .expect("write nt:base");
    let types = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "nt:base".to_owned(),
                node: base,
            },
            &[],
        )
        .expect("write jcr:nodeTypes");
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:nodeTypes".to_owned(),
                node: types,
            },
            &[],
        )
        .expect("write jcr:system")
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
        Subject::FlaggedLucene => (write_lucene_definition(writer), Some("async")),
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
    // A Lucene definition is rebuilt from its lane's checkpoint, and the
    // rule resolves node types from the state it indexes.
    let lucene = subject == Subject::FlaggedLucene;
    if lucene {
        root_children.push(("jcr:system".to_owned(), write_node_types(&mut writer)));
    }
    if let Some(lane) = lane {
        let checkpoint_name = if lucene { "lane-checkpoint" } else { "c1" };
        let lane_properties = [single(
            &mut writer,
            lane,
            PropertyType::String,
            checkpoint_name,
        )];
        let async_node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &lane_properties)
            .expect("write /:async");
        root_children.push((":async".to_owned(), async_node));
    }
    root_children.sort_by(|left, right| left.0.cmp(&right.0));

    let root = writer
        .write_node(None, &[], &ChildNodesToWrite::Many(root_children), &[])
        .expect("write the root");
    let mut super_children = vec![("root".to_owned(), root)];
    if lucene {
        // The checkpoint pins the same state the head holds, which is what
        // a lane that has caught up looks like.
        let checkpoint = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("write the checkpoint");
        let checkpoints = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "lane-checkpoint".to_owned(),
                    node: checkpoint,
                },
                &[],
            )
            .expect("write the checkpoints container");
        super_children.push(("checkpoints".to_owned(), checkpoints));
    }
    let head = writer
        .write_node(None, &[], &ChildNodesToWrite::Many(super_children), &[])
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
pub(crate) fn an_unresolvable_lane_needs_from_head_and_then_resets_the_counter() {
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

    // With it, the counter is reset rather than rebuilt.
    let reset = froe_reindex(&store, &["--yes", "--from-head", "--work-directory", &work]);
    assert_eq!(reset.status.code(), Some(0), "{}", reset.stderr);
    assert!(
        reset.stdout.contains("reset"),
        "the plan and summary name the reset: {}",
        reset.stdout
    );
    assert!(
        reset.stdout.contains(":index"),
        "the summary names the hidden child it removed: {}",
        reset.stdout
    );

    // A rerun of the reset has nothing left to remove.
    let rerun = froe_reindex(&store, &["--yes", "--from-head", "--work-directory", &work]);
    assert_eq!(rerun.status.code(), Some(0), "{}", rerun.stderr);
    assert!(rerun.stdout.contains("nothing to do"), "{}", rerun.stdout);
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

/// A Lucene definition is refused until the operator says what a binary
/// property contributes, and the refusal names the flag.
#[test]
pub(crate) fn a_lucene_definition_without_the_binary_text_flag_is_refused() {
    let (_directory, store, work) = fixture("reindex-lucene-no-policy", Subject::FlaggedLucene);
    let run = froe_reindex(
        &store,
        &[
            "--dry-run",
            "--work-directory",
            work.to_str().expect("path"),
        ],
    );
    assert!(run.status.success(), "{}", run.stderr);
    assert!(
        run.stdout.contains("binary-text policy"),
        "the plan must name what is missing: {}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("rebuild /oak:index/subject"),
        "nothing may be planned: {}",
        run.stdout
    );
}

/// With the flag, the plan says what it will do and the run does it.
#[test]
pub(crate) fn the_binary_text_flag_reaches_the_library_and_the_run_rebuilds() {
    let (_directory, store, work) = fixture("reindex-lucene-policy", Subject::FlaggedLucene);
    let plan = froe_reindex(
        &store,
        &[
            "--dry-run",
            "--binary-text",
            "marker",
            "--work-directory",
            work.to_str().expect("path"),
        ],
    );
    assert!(plan.status.success(), "{}", plan.stderr);
    assert!(
        plan.stdout
            .contains("rebuild /oak:index/subject from lane async's checkpoint"),
        "{}",
        plan.stdout
    );
    assert!(plan.stdout.contains("indexing rule"), "{}", plan.stdout);
    assert!(
        plan.stdout
            .contains("binary text the extraction-error marker"),
        "the policy in force is on the plan line: {}",
        plan.stdout
    );
    assert!(
        plan.stdout.contains("as a proxy"),
        "the work-directory figure is labelled a proxy: {}",
        plan.stdout
    );

    let run = froe_reindex(
        &store,
        &[
            "--yes",
            "--binary-text",
            "skip",
            "--work-directory",
            work.to_str().expect("path"),
        ],
    );
    assert!(run.status.success(), "{}", run.stderr);
    assert!(
        run.stdout.contains("document") && run.stdout.contains("index file"),
        "the summary reports what happened: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("binary text was not extracted")
            && run
                .stdout
                .contains("do not match what Oak's own index matches"),
        "the summary states what the binary policy costs a query: {}",
        run.stdout
    );
}

/// A mistyped pre-extracted directory would otherwise produce an index
/// identical to one built without the flag, under a plan line naming a
/// directory nothing read.
#[test]
pub(crate) fn a_pre_extracted_directory_that_is_not_there_is_refused() {
    let (directory, store, work) = fixture("reindex-lucene-missing-store", Subject::FlaggedLucene);
    let missing = directory.path.join("no-such-extracted-text");
    let run = froe_reindex(
        &store,
        &[
            "--yes",
            "--binary-text",
            "marker",
            "--pre-extracted-text-directory",
            missing.to_str().expect("path"),
            "--work-directory",
            work.to_str().expect("path"),
        ],
    );
    assert!(!run.status.success(), "{}", run.stdout);
    assert!(
        run.stderr.contains("--pre-extracted-text-directory")
            && run.stderr.contains("is not a directory"),
        "the refusal names the flag and what is wrong with it: {}",
        run.stderr
    );
}

/// The prompt that authorizes taking the lock names the work, and a
/// Lucene rebuild is work.
#[test]
pub(crate) fn the_prompt_counts_a_lucene_rebuild_rather_than_saying_none() {
    let (_directory, store, work) = fixture("reindex-lucene-prompt", Subject::FlaggedLucene);
    // No `--yes`: the run plans, asks, and cancels on the empty answer a
    // non-interactive standard input gives it.
    let run = froe_reindex(
        &store,
        &[
            "--binary-text",
            "marker",
            "--work-directory",
            work.to_str().expect("path"),
        ],
    );
    let printed = format!("{}{}", run.stdout, run.stderr);
    assert!(
        printed.contains("about to rebuild 1 index in"),
        "the prompt counts the Lucene rebuild: {printed}"
    );
    assert!(
        !printed.contains("rebuild 0 indexes"),
        "a Lucene-only run is not a run that rebuilds nothing: {printed}"
    );
}

/// The pre-extracted directory is a refinement of the fallback, not a
/// replacement for it.
#[test]
pub(crate) fn a_pre_extracted_directory_without_the_policy_is_refused() {
    let (_directory, store, work) = fixture("reindex-lucene-pre-extracted", Subject::FlaggedLucene);
    let run = froe_reindex(
        &store,
        &[
            "--dry-run",
            "--pre-extracted-text-directory",
            work.to_str().expect("path"),
            "--work-directory",
            work.to_str().expect("path"),
        ],
    );
    assert!(!run.status.success(), "{}", run.stdout);
    assert!(
        run.stderr.contains("--binary-text"),
        "the refusal names the flag it requires: {}",
        run.stderr
    );
}
