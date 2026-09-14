//! `froe index`: the three read-only subcommands.
//!
//! What these pin is the contract a runbook depends on — the exit codes, the
//! stream split, the refusals and the read-only guarantee — rather than the
//! readers underneath, which have their own tests in `froe`. The store is
//! written by froe's own writer, so every shape a test needs can be built
//! exactly, including the ones Oak would never produce.

use std::path::Path;

use froe::PropertyType;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use froe::writer::store_writer::WritableRepository;

use super::TestDirectory;
use crate::filesystem_snapshot::directory_snapshot;

/// The lane checkpoint the asynchronous definition resumes from.
const CHECKPOINT_NAME: &str = "8b3d5f2a-1c4e-4a7b-9f01-2d3e4f5a6b7c";

/// A checkpoint `/:async` names that `/checkpoints` does not hold.
const DANGLING_CHECKPOINT_NAME: &str = "0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";

fn text<Sink: SegmentSink>(
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

fn names<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    name: &str,
    values: &[&str],
) -> PropertyToWrite {
    let written = values
        .iter()
        .map(|value| writer.write_string(value).expect("name value"))
        .collect();
    PropertyToWrite {
        name: name.to_owned(),
        property_type: PropertyType::Name,
        values: PropertyValuesToWrite::Multiple(written),
    }
}

/// How the `title` definition is maintained.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Maintenance {
    /// No `async` property: the index corresponds to the head.
    Synchronous,
    /// `async = async`, and `/:async` resumes from a checkpoint
    /// `/checkpoints` holds.
    LaneResolves,
    /// `async = async`, and `/:async` names a checkpoint that is gone.
    LaneDangles,
}

impl Maintenance {
    fn asynchronous(self) -> bool {
        self != Maintenance::Synchronous
    }

    fn resume_point(self) -> Option<&'static str> {
        match self {
            Maintenance::Synchronous => None,
            Maintenance::LaneResolves => Some(CHECKPOINT_NAME),
            Maintenance::LaneDangles => Some(DANGLING_CHECKPOINT_NAME),
        }
    }
}

/// Which definitions `/oak:index` holds beside `title`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Definitions {
    /// `nodetype` and `counter`, which is what a real store has.
    Complete,
    /// No `nodetype`, so Oak's index path service would refuse the store.
    WithoutNodeType,
}

/// How the store under test is shaped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StoreShape {
    maintenance: Maintenance,
    /// Whether the mirror's entry names a node that does not exist, which is
    /// the `stale` case.
    forge_stale_entry: bool,
    definitions: Definitions,
}

impl Default for StoreShape {
    fn default() -> Self {
        Self {
            maintenance: Maintenance::Synchronous,
            forge_stale_entry: false,
            definitions: Definitions::Complete,
        }
    }
}

/// The content the index covers: `/content/page` carrying `jcr:title`.
fn write_content<Sink: SegmentSink>(writer: &mut RecordWriter<Sink>) -> froe::RecordIdentifier {
    let page_properties = [text(writer, "jcr:title", PropertyType::String, "Alpha")];
    let page = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Zero,
            &page_properties,
        )
        .expect("write /content/page");
    writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::One {
                name: "page".to_owned(),
                node: page,
            },
            &[],
        )
        .expect("write /content")
}

/// The mirror: `:index/Alpha/content/<leaf>` with `match = true`. When
/// `leaf` is not `page` the entry names a node that does not exist, which is
/// the `stale` case.
fn write_mirror<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    leaf: &str,
) -> froe::RecordIdentifier {
    let match_properties = [text(writer, "match", PropertyType::Boolean, "true")];
    let matched = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &match_properties)
        .expect("write the match node");
    let entry_content = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: leaf.to_owned(),
                node: matched,
            },
            &[],
        )
        .expect("write the entry's content level");
    let key = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "content".to_owned(),
                node: entry_content,
            },
            &[],
        )
        .expect("write the key node");
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "Alpha".to_owned(),
                node: key,
            },
            &[],
        )
        .expect("write :index")
}

/// The `title` definition over `jcr:title`, with its mirror as `:index`.
fn write_title_definition<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    shape: StoreShape,
) -> froe::RecordIdentifier {
    let index_storage = write_mirror(
        writer,
        if shape.forge_stale_entry {
            "ghost"
        } else {
            "page"
        },
    );
    let mut properties = vec![
        text(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        text(writer, "type", PropertyType::String, "property"),
        names(writer, "propertyNames", &["jcr:title"]),
        text(writer, "reindex", PropertyType::Boolean, "false"),
        text(writer, "reindexCount", PropertyType::Long, "1"),
    ];
    if shape.maintenance.asynchronous() {
        properties.push(text(writer, "async", PropertyType::String, "async"));
    }
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: ":index".to_owned(),
                node: index_storage,
            },
            &properties,
        )
        .expect("write the title definition")
}

/// `/oak:index/nodetype`, which Oak's index path service requires to exist
/// and to read as a `property` index before it enumerates anything.
fn write_node_type_definition<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
) -> froe::RecordIdentifier {
    let properties = [
        text(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        text(writer, "type", PropertyType::String, "property"),
        names(
            writer,
            "propertyNames",
            &["jcr:primaryType", "jcr:mixinTypes"],
        ),
    ];
    writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)
        .expect("write the nodetype definition")
}

/// A counter whose `:index` carries a real count, so a derived node budget
/// is a number rather than the unbudgeted form.
fn write_counter_definition<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
) -> froe::RecordIdentifier {
    let counted_properties = [text(writer, ":cnt", PropertyType::Long, "1024")];
    let counted = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &counted_properties)
        .expect("write the counter's :index");
    let properties = [
        text(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        text(writer, "type", PropertyType::String, "counter"),
    ];
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: ":index".to_owned(),
                node: counted,
            },
            &properties,
        )
        .expect("write the counter definition")
}

/// The checkpoint the lane resumes from, sharing the content root — which is
/// what a real one does right after a lane completes a run.
fn write_checkpoints<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    root: Option<froe::RecordIdentifier>,
) -> froe::RecordIdentifier {
    let Some(root) = root else {
        return writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("write the empty checkpoints container");
    };
    let checkpoint_properties = [text(writer, "created", PropertyType::Long, "1750000000000")];
    let checkpoint = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &checkpoint_properties,
        )
        .expect("write the checkpoint");
    writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: CHECKPOINT_NAME.to_owned(),
                node: checkpoint,
            },
            &[],
        )
        .expect("write the checkpoints container")
}

/// Writes a store holding one property index over `jcr:title`, the content
/// it indexes, and optionally a counter definition and an `/:async` node.
fn build_index_store(directory: &Path, shape: StoreShape) {
    let store = WritableRepository::open(directory).expect("open the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    let content = write_content(&mut writer);
    let mut index_children = vec![(
        "title".to_owned(),
        write_title_definition(&mut writer, shape),
    )];
    if shape.definitions == Definitions::Complete {
        index_children.push((
            "nodetype".to_owned(),
            write_node_type_definition(&mut writer),
        ));
        index_children.push(("counter".to_owned(), write_counter_definition(&mut writer)));
    }
    let oak_index = writer
        .write_node(None, &[], &ChildNodesToWrite::Many(index_children), &[])
        .expect("write /oak:index");

    let mut root_children = vec![
        ("content".to_owned(), content),
        ("oak:index".to_owned(), oak_index),
    ];
    if let Some(resume) = shape.maintenance.resume_point() {
        let async_properties = [text(&mut writer, "async", PropertyType::String, resume)];
        let async_state = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &async_properties)
            .expect("write /:async");
        root_children.push((":async".to_owned(), async_state));
    }
    let root = writer
        .write_node(
            Some("rep:root"),
            &[],
            &ChildNodesToWrite::Many(root_children),
            &[],
        )
        .expect("write the content root");

    let checkpoints = write_checkpoints(
        &mut writer,
        (shape.maintenance == Maintenance::LaneResolves).then_some(root),
    );
    let super_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("root".to_owned(), root),
                ("checkpoints".to_owned(), checkpoints),
            ]),
            &[],
        )
        .expect("write the super-root");

    writer.finish().expect("finish the writer");
    let previous = store.head();
    assert!(
        store.compare_and_set_head(previous, super_root),
        "advance the head"
    );
    store.close().expect("close the store");
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn froe_index(store: &Path, arguments: &[&str]) -> Run {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_froe"));
    command.arg("index");
    command.arg(arguments[0]);
    command.arg(store);
    for argument in &arguments[1..] {
        command.arg(argument);
    }
    let output = command.output().expect("run froe index");
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn exit_code(run: &Run) -> i32 {
    run.status.code().expect("the process exited normally")
}

#[test]
pub(crate) fn list_prints_every_definition_and_check_exits_zero() {
    let directory = TestDirectory::new("index-list");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(&store, StoreShape::default());

    let run = froe_index(&store, &["list", "--silent"]);
    assert_eq!(exit_code(&run), 0, "{}", run.stderr);
    assert!(run.stdout.contains("/oak:index/title"), "{}", run.stdout);
    assert!(run.stdout.contains("/oak:index/counter"), "{}", run.stdout);
    assert!(
        run.stdout.contains("type              property"),
        "{}",
        run.stdout
    );

    let run = froe_index(&store, &["check", "--silent"]);
    assert_eq!(
        exit_code(&run),
        0,
        "a consistent store exits 0 — {}{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout
            .contains("/oak:index/title: consistent against the head"),
        "{}",
        run.stdout
    );
}

#[test]
pub(crate) fn a_counter_definition_alone_never_makes_a_run_exit_four() {
    // Every store carries a counter, and its type has no applicable check.
    // If that reached exit 4, no unnarrowed run could ever exit 0 and a
    // runbook gating on the command would never pass.
    let directory = TestDirectory::new("index-counter-only");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(&store, StoreShape::default());

    let run = froe_index(&store, &["check", "--silent"]);
    assert_eq!(exit_code(&run), 0, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout
            .contains("/oak:index/counter: no applicable check for type counter"),
        "the verdict is still reported — {}",
        run.stdout
    );
}

#[test]
pub(crate) fn a_stale_entry_makes_the_check_exit_three() {
    let directory = TestDirectory::new("index-stale");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(
        &store,
        StoreShape {
            forge_stale_entry: true,
            ..StoreShape::default()
        },
    );

    let run = froe_index(&store, &["check", "--silent"]);
    assert_eq!(
        exit_code(&run),
        3,
        "an inconsistent index exits 3 — {}{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("/oak:index/title: INCONSISTENT"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("/content/ghost"),
        "the entry that does not resolve is named — {}",
        run.stdout
    );
}

#[test]
pub(crate) fn an_asynchronous_definition_is_checked_against_its_lane_checkpoint() {
    let directory = TestDirectory::new("index-lane");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(
        &store,
        StoreShape {
            maintenance: Maintenance::LaneResolves,
            ..StoreShape::default()
        },
    );

    let run = froe_index(&store, &["check", "--silent"]);
    assert_eq!(exit_code(&run), 0, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout
            .contains(&format!("lane async at checkpoint {CHECKPOINT_NAME}")),
        "the verdict names the state it checked against — {}",
        run.stdout
    );
}

#[test]
pub(crate) fn a_dangling_lane_checkpoint_is_not_run_rather_than_inconsistent() {
    // Oak does not fail on one either: it reindexes from the missing state.
    // Calling the index inconsistent would send an operator to reindex
    // something that may be perfectly correct.
    let directory = TestDirectory::new("index-dangling-lane");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(
        &store,
        StoreShape {
            maintenance: Maintenance::LaneDangles,
            ..StoreShape::default()
        },
    );

    let run = froe_index(&store, &["check", "--silent"]);
    assert_eq!(
        exit_code(&run),
        4,
        "a check that could not be run exits 4, not 3 — {}{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("/oak:index/title: not checked"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(DANGLING_CHECKPOINT_NAME),
        "the checkpoint that no longer exists is named — {}",
        run.stdout
    );
}

#[test]
pub(crate) fn an_index_path_naming_no_definition_is_refused_by_name() {
    let directory = TestDirectory::new("index-unknown-path");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(&store, StoreShape::default());

    for subcommand in ["list", "definitions", "check"] {
        let run = froe_index(
            &store,
            &[subcommand, "--silent", "--index", "/oak:index/nope"],
        );
        assert_eq!(
            exit_code(&run),
            1,
            "{subcommand} refuses rather than listing nothing — {}{}",
            run.stdout,
            run.stderr
        );
        assert!(
            run.stderr
                .contains("no index definition at /oak:index/nope"),
            "{subcommand}: {}",
            run.stderr
        );
    }
}

#[test]
pub(crate) fn a_disabled_nodetype_index_refuses_definitions_but_never_list() {
    // Oak's own printer walks `IndexPathService.getIndexPaths()`, which
    // throws when `/oak:index/nodetype` is absent or does not read as a
    // `property` index. Falling back silently would make froe's definitions
    // file differ from Oak's on exactly the store where they must agree.
    let directory = TestDirectory::new("index-no-nodetype");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(
        &store,
        StoreShape {
            definitions: Definitions::WithoutNodeType,
            ..StoreShape::default()
        },
    );

    let run = froe_index(&store, &["list"]);
    assert_eq!(
        exit_code(&run),
        0,
        "a listing is froe's own and still runs — {}{}",
        run.stdout,
        run.stderr
    );
    assert!(run.stdout.contains("/oak:index/title"), "{}", run.stdout);
    assert!(
        run.stderr.contains("nodetype"),
        "the warning says what could not be enumerated — {}",
        run.stderr
    );

    for subcommand in ["definitions", "check"] {
        let run = froe_index(&store, &[subcommand, "--silent"]);
        assert_eq!(exit_code(&run), 1, "{subcommand}: {}", run.stderr);
        assert!(
            run.stderr.contains("--index"),
            "{subcommand} names the way past it — {}",
            run.stderr
        );

        // Naming the paths never consults the path service, so its nodetype
        // precondition is never evaluated — as with oak-run's --index-paths.
        let run = froe_index(
            &store,
            &[subcommand, "--silent", "--index", "/oak:index/title"],
        );
        assert_eq!(
            exit_code(&run),
            0,
            "{subcommand} --index: {}{}",
            run.stdout,
            run.stderr
        );
        assert!(run.stdout.contains("/oak:index/title"), "{}", run.stdout);
    }
}

#[test]
pub(crate) fn definitions_narrows_to_the_requested_paths() {
    let directory = TestDirectory::new("index-definitions");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(&store, StoreShape::default());

    let run = froe_index(&store, &["definitions", "--silent"]);
    assert_eq!(exit_code(&run), 0, "{}", run.stderr);
    assert!(
        run.stdout.contains("\"/oak:index/title\""),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("\"/oak:index/counter\""),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains(":index\""),
        "hidden children are not serialized — {}",
        run.stdout
    );

    let run = froe_index(
        &store,
        &["definitions", "--silent", "--index", "/oak:index/title"],
    );
    assert_eq!(exit_code(&run), 0, "{}", run.stderr);
    assert!(
        run.stdout.contains("\"/oak:index/title\""),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("\"/oak:index/counter\""),
        "{}",
        run.stdout
    );
}

#[test]
pub(crate) fn definitions_output_refuses_an_existing_file_and_one_inside_the_store() {
    let directory = TestDirectory::new("index-definitions-output");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(&store, StoreShape::default());

    let fresh = directory.path.join("definitions.json");
    let run = froe_index(
        &store,
        &[
            "definitions",
            "--silent",
            "--output",
            fresh.to_str().unwrap(),
        ],
    );
    assert_eq!(exit_code(&run), 0, "{}", run.stderr);
    let written = std::fs::read_to_string(&fresh).expect("the file was written");
    assert!(written.starts_with('{'), "{written}");
    assert!(
        written.ends_with("\n}"),
        "the file carries the printer's bytes exactly, ending at the closing brace — {written:?}"
    );
    assert!(
        run.stdout.is_empty(),
        "with --output nothing goes to standard output — {}",
        run.stdout
    );

    // Unlike `froe digest --output`, which truncates: a definitions file is
    // an artifact an operator keeps and re-imports.
    let run = froe_index(
        &store,
        &[
            "definitions",
            "--silent",
            "--output",
            fresh.to_str().unwrap(),
        ],
    );
    assert_eq!(exit_code(&run), 1, "{}", run.stderr);
    assert_eq!(
        std::fs::read_to_string(&fresh).expect("read"),
        written,
        "the refusal left the file untouched"
    );

    let inside = store.join("definitions.json");
    let run = froe_index(
        &store,
        &[
            "definitions",
            "--silent",
            "--output",
            inside.to_str().unwrap(),
        ],
    );
    assert_eq!(exit_code(&run), 1, "{}", run.stderr);
    assert!(
        run.stderr.contains("inside the repository directory"),
        "{}",
        run.stderr
    );
    assert!(!inside.exists(), "nothing was created inside the store");
}

#[test]
pub(crate) fn every_subcommand_leaves_the_store_byte_identical_and_takes_no_lock() {
    let directory = TestDirectory::new("index-read-only");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(&store, StoreShape::default());
    // The writer that built the fixture left its own lock file behind;
    // removing it first is what makes the assertion below mean that *these*
    // commands never take the lock.
    std::fs::remove_file(store.join("repo.lock")).expect("remove the bootstrap lock file");

    let before = directory_snapshot(&store);
    for arguments in [
        vec!["list", "--silent"],
        vec!["definitions", "--silent"],
        vec!["check", "--silent"],
    ] {
        let run = froe_index(&store, &arguments);
        assert!(
            exit_code(&run) == 0,
            "{arguments:?}: {}{}",
            run.stdout,
            run.stderr
        );
        assert_eq!(
            directory_snapshot(&store),
            before,
            "{arguments:?} modified the store"
        );
        assert!(
            !store.join("repo.lock").exists(),
            "{arguments:?} created repo.lock"
        );
    }
}

#[test]
pub(crate) fn the_data_stream_is_unaffected_by_the_progress_flags() {
    let directory = TestDirectory::new("index-streams");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    build_index_store(&store, StoreShape::default());

    let silent = froe_index(&store, &["list", "--silent"]);
    let always = froe_index(&store, &["list", "--progress", "always"]);
    assert_eq!(
        silent.stdout, always.stdout,
        "progress never reaches standard output"
    );
    assert!(
        silent.stderr.is_empty(),
        "--silent says nothing — {}",
        silent.stderr
    );
    assert!(
        always.stderr.contains("inventorying indexes"),
        "the step is announced on standard error — {}",
        always.stderr
    );
}
