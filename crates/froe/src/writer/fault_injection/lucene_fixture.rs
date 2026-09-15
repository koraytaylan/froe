//! The Lucene reindex scenario's fixture and child, which
//! `test_support.rs` would otherwise carry past the thousand-line gate.
//!
//! The store this writes is the smallest one the arm's boundaries are
//! observable in: a flagged `lucene` definition with an analyzed catch-all
//! rule, the lane and the checkpoint it names, the node types the rule
//! resolves through, and enough pages that the writer spills at the small
//! budget the child runs with.

use std::path::{Path, PathBuf};

use super::ERROR_MODE;
use super::test_support::{one_name, reindex_work_directory, single_valued};
#[cfg(unix)]
use super::{CRASH_MODE, VERIFIED_EXIT_CODE};
use crate::segment::record::RecordIdentifier;
use crate::writer::store_writer::WritableRepository;

pub(crate) const LUCENE_REINDEX_SCENARIO: &str = "lucene-reindex";

/// A store with one flagged `lucene` definition, its lane's checkpoint, the
/// node types its rule resolves through, and content to index.
pub(crate) fn write_lucene_reindex_fixture(root: &Path) -> PathBuf {
    use crate::content::property::PropertyType;
    use crate::writer::record_writer::ChildNodesToWrite;

    let directory = root.join("store");
    std::fs::create_dir_all(&directory).expect("create the Lucene reindex fixture store");
    std::fs::create_dir_all(reindex_work_directory(&directory))
        .expect("create the Lucene reindex fixture work directory");

    let store = WritableRepository::open(&directory).expect("bootstrap the fixture");
    let generation = store.writing_generation().expect("writing generation");
    let mut writer = store.record_writer(generation);

    let content = write_lucene_fixture_content(&mut writer);

    let node_types = write_lucene_fixture_node_types(&mut writer);
    let oak_index = write_lucene_fixture_definitions(&mut writer);
    let state = |writer: &mut crate::writer::record_writer::RecordWriter<_>| {
        writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("content".to_owned(), content),
                    ("jcr:system".to_owned(), node_types),
                ]),
                &[],
            )
            .expect("write a state root")
    };
    let pinned = state(&mut writer);
    let checkpoint = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: pinned,
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
    let lane_properties = vec![single_valued(
        &mut writer,
        "async",
        PropertyType::String,
        "lane-checkpoint",
    )];
    let lanes = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &lane_properties)
        .expect("write /:async");
    let root_node = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("content".to_owned(), content),
                ("jcr:system".to_owned(), node_types),
                (":async".to_owned(), lanes),
                (crate::index::INDEX_DEFINITIONS_NAME.to_owned(), oak_index),
            ]),
            &[],
        )
        .expect("write the root");
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("root".to_owned(), root_node),
                ("checkpoints".to_owned(), checkpoints),
            ]),
            &[],
        )
        .expect("write the super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush");
    store.close().expect("close the fixture");
    directory
}

/// The content the fixture indexes: enough pages that the writer spills at
/// the small budget the child runs with, so the boundary before `finish`
/// has files to observe.
fn write_lucene_fixture_content<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
) -> RecordIdentifier {
    use crate::content::property::PropertyType;
    use crate::writer::record_writer::ChildNodesToWrite;

    let mut pages = Vec::new();
    for number in 0..24 {
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
                &format!("page number {number} of the fixture"),
            ),
        ];
        let node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)
            .expect("write a page");
        pages.push((format!("page{number}"), node));
    }
    let properties = vec![single_valued(
        writer,
        "jcr:primaryType",
        PropertyType::Name,
        "nt:unstructured",
    )];
    writer
        .write_node(None, &[], &ChildNodesToWrite::Many(pages), &properties)
        .expect("write the content")
}

/// `/jcr:system/jcr:nodeTypes` with `nt:unstructured` below `nt:base`.
fn write_lucene_fixture_node_types<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
) -> RecordIdentifier {
    use crate::writer::record_writer::ChildNodesToWrite;

    let subtypes = vec![one_name(writer, "rep:primarySubtypes", "nt:unstructured")];
    let base = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &subtypes)
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

/// `/oak:index` with the flagged `lucene` definition and the `nodetype` one
/// the path service requires.
fn write_lucene_fixture_definitions<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
) -> RecordIdentifier {
    use crate::content::property::PropertyType;
    use crate::writer::record_writer::ChildNodesToWrite;

    let catch_all = vec![
        single_valued(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "nt:unstructured",
        ),
        single_valued(writer, "name", PropertyType::String, r"^[^\/]*$"),
        single_valued(writer, "isRegexp", PropertyType::Boolean, "true"),
        single_valued(writer, "analyzed", PropertyType::Boolean, "true"),
        single_valued(writer, "nodeScopeIndex", PropertyType::Boolean, "true"),
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
    let definition = vec![
        single_valued(
            writer,
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        single_valued(writer, "type", PropertyType::String, "lucene"),
        single_valued(writer, "async", PropertyType::String, "async"),
        single_valued(writer, "reindex", PropertyType::Boolean, "true"),
    ];
    let lucene = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "indexRules".to_owned(),
                node: rules,
            },
            &definition,
        )
        .expect("write the lucene definition");

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
                ("lucene".to_owned(), lucene),
                ("nodetype".to_owned(), nodetype),
            ]),
            &[],
        )
        .expect("write oak:index")
}

/// The Lucene-reindex scenario: rebuild the fixture's flagged definition
/// with the cutpoint armed and a binary-text policy in force.
pub(crate) fn run_lucene_reindex_child(store: &Path, cutpoint: &str, mode: &str) {
    use crate::index::lucene::documents::binaries::{BinaryTextFallback, BinaryTextPolicy};
    use crate::writer::index::{ReindexOptions, WorkDirectory, reindex};

    let options = ReindexOptions::new()
        .with_work_directory(WorkDirectory::OperatorNamed(reindex_work_directory(store)))
        .with_binary_text_policy(BinaryTextPolicy::new(BinaryTextFallback::Marker))
        // Small, so the writer spills and the boundary before `finish` has
        // files to observe.
        .with_sort_budget_bytes(1024);
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
        other => panic!("unsupported Lucene reindex fault mode {other}"),
    }
    // SAFETY: `_exit` has no memory-safety preconditions and this is an
    // isolated child whose error path was checked above.
    #[cfg(unix)]
    unsafe {
        libc::_exit(VERIFIED_EXIT_CODE)
    }
}
