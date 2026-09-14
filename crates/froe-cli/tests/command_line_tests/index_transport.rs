//! `froe index dump`: moving Lucene index data out to the filesystem.
//!
//! What these pin is the operator's contract — the store is untouched, the
//! output goes where oak-run's importer looks, an existing dump is never
//! written over, and the reporting never reaches standard output — rather
//! than the layout itself, which has its own tests in `froe`.

use std::path::{Path, PathBuf};

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

/// The `:data` child holding two files in the streaming encoding.
fn write_data_directory<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
) -> froe::RecordIdentifier {
    let mut files = Vec::new();
    for (name, bytes) in [("segments_1", &b"commit"[..]), ("_0.si", &b"segment"[..])] {
        let blob = writer.write_binary_content(bytes).expect("write a blob");
        let properties = [
            single(writer, "blobSize", PropertyType::Long, "1047552"),
            PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(blob),
            },
        ];
        let node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)
            .expect("write a file node");
        files.push((name.to_owned(), node));
    }

    let listing: Vec<_> = files
        .iter()
        .map(|(name, _)| writer.write_string(name).expect("listing entry"))
        .collect();
    let data_properties = [PropertyToWrite {
        name: "dirListing".to_owned(),
        property_type: PropertyType::String,
        values: PropertyValuesToWrite::Multiple(listing),
    }];
    writer
        .write_node(None, &[], &ChildNodesToWrite::Many(files), &data_properties)
        .expect("write :data")
}

/// `/:async` and `/checkpoints`, so the lane's checkpoint resolves.
fn write_lane_state<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
) -> (froe::RecordIdentifier, froe::RecordIdentifier) {
    let lane_properties = [single(
        writer,
        "async",
        PropertyType::String,
        "checkpoint-1",
    )];
    let async_node = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &lane_properties)
        .expect("write /:async");
    let checkpoint = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
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
        .expect("write /checkpoints");
    (async_node, checkpoints)
}

/// A store with one asynchronous `lucene` definition whose `:data` holds two
/// files, and a resolvable lane checkpoint.
fn build_store(directory: &Path) {
    let store = WritableRepository::open(directory).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    let data = write_data_directory(&mut writer);
    let definition_properties = [
        single(
            &mut writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        single(&mut writer, "type", PropertyType::String, "lucene"),
        single(&mut writer, "async", PropertyType::String, "async"),
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
    let oak_index = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "lucene".to_owned(),
                node: definition,
            },
            &[],
        )
        .expect("write /oak:index");

    let (async_node, checkpoints) = write_lane_state(&mut writer);
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                (":async".to_owned(), async_node),
                ("checkpoints".to_owned(), checkpoints),
                ("oak:index".to_owned(), oak_index),
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

fn froe_dump(store: &Path, arguments: &[&str]) -> Run {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_froe"));
    command.arg("index").arg("dump").arg(store);
    for argument in arguments {
        command.arg(argument);
    }
    let output = command.output().expect("run froe index dump");
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The store and an output directory beside it.
fn fixture(name: &str) -> (TestDirectory, PathBuf, PathBuf) {
    let directory = TestDirectory::new(name);
    let store = directory.path.join("segmentstore");
    let output = directory.path.join("out");
    std::fs::create_dir_all(&store).expect("create the store directory");
    build_store(&store);
    (directory, store, output)
}

#[test]
pub(crate) fn a_dump_writes_oak_runs_layout_and_leaves_the_store_untouched() {
    let (_directory, store, output) = fixture("dump-layout");
    let before = directory_snapshot(&store);

    let run = froe_dump(&store, &["--output", output.to_str().expect("utf-8")]);

    assert_eq!(run.status.code(), Some(0), "{}", run.stderr);
    assert!(
        run.stdout.contains("/oak:index/lucene -> lucene/"),
        "the dump names what it wrote where: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("recorded checkpoint checkpoint-1"),
        "{}",
        run.stdout
    );

    // oak-run's importer scans this directory's direct children.
    let dumps = output.join("index-dumps");
    assert!(dumps.join("indexer-info.properties").is_file());
    assert!(dumps.join("index-definitions.json").is_file());
    assert!(dumps.join("lucene").join("index-details.txt").is_file());
    assert!(
        dumps
            .join("lucene")
            .join("data")
            .join("segments_1")
            .is_file()
    );

    assert_eq!(
        directory_snapshot(&store),
        before,
        "a dump must not write a byte inside the store"
    );

    // The `repo.lock` inode is left behind by whoever wrote the store, so
    // its presence proves nothing. What proves the dump takes no lock is
    // that it succeeds while this process holds it.
    let held = froe::writer::RepositoryLock::acquire(&store).expect("hold the lock");
    let under_lock = froe_dump(
        &store,
        &["--output", output.join("second").to_str().expect("utf-8")],
    );
    assert_eq!(
        under_lock.status.code(),
        Some(0),
        "a dump must run while another writer holds the lock: {}",
        under_lock.stderr
    );
    drop(held);
}

#[test]
pub(crate) fn an_output_directory_inside_the_store_is_refused_by_the_command() {
    let (_directory, store, _output) = fixture("dump-inside");
    let inside = store.join("dump-here");

    let run = froe_dump(&store, &["--output", inside.to_str().expect("utf-8")]);

    assert_ne!(run.status.code(), Some(0));
    assert!(
        run.stderr.contains("inside the repository directory"),
        "{}",
        run.stderr
    );
    assert!(
        !inside.exists(),
        "the refusal must land before anything is created"
    );
}

#[test]
pub(crate) fn an_existing_dump_is_refused_with_its_remedy() {
    let (_directory, store, output) = fixture("dump-twice");
    let first = froe_dump(&store, &["--output", output.to_str().expect("utf-8")]);
    assert_eq!(first.status.code(), Some(0), "{}", first.stderr);

    let second = froe_dump(&store, &["--output", output.to_str().expect("utf-8")]);
    assert_ne!(second.status.code(), Some(0));
    assert!(
        second.stderr.contains("already holds a dump")
            && second.stderr.contains("Delete it and rerun"),
        "{}",
        second.stderr
    );
}

#[test]
pub(crate) fn a_partially_populated_output_is_refused_rather_than_completed() {
    let (_directory, store, output) = fixture("dump-partial");
    std::fs::create_dir_all(output.join("index-dumps").join("lucene"))
        .expect("plant a partial dump");

    let run = froe_dump(&store, &["--output", output.to_str().expect("utf-8")]);
    assert_ne!(run.status.code(), Some(0));
    assert!(
        run.stderr.contains("already holds a dump"),
        "{}",
        run.stderr
    );
}

#[test]
pub(crate) fn a_definition_that_is_not_lucene_is_refused_by_name() {
    let (_directory, store, output) = fixture("dump-not-lucene");

    let run = froe_dump(
        &store,
        &[
            "--output",
            output.to_str().expect("utf-8"),
            "--index",
            "/oak:index/nothing-here",
        ],
    );
    assert_ne!(run.status.code(), Some(0));
    assert!(
        run.stderr.contains("/oak:index/nothing-here")
            && run.stderr.contains("names no lucene definition"),
        "the refusal names the path rather than reporting an empty dump: {}",
        run.stderr
    );
}

/// Progress is a report, and a report never mixes with the data an operator
/// pipes.
#[test]
pub(crate) fn reporting_never_reaches_the_standard_output_of_a_dump() {
    for progress in ["always", "never", "auto"] {
        let (_directory, store, output) = fixture(&format!("dump-reporting-{progress}"));
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_froe"));
        command
            .arg("--progress")
            .arg(progress)
            .arg("index")
            .arg("dump")
            .arg(&store)
            .arg("--output")
            .arg(&output);
        let result = command.output().expect("run froe index dump");
        let stdout = String::from_utf8_lossy(&result.stdout).into_owned();

        assert!(
            stdout.starts_with("dumped to"),
            "--progress {progress} put something before the data: {stdout}"
        );
        for reported in ["dumping index files", "\r"] {
            assert!(
                !stdout.contains(reported),
                "--progress {progress} leaked {reported:?} into standard output: {stdout}"
            );
        }
    }
}
