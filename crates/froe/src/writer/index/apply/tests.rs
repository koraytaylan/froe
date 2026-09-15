//! The reindex apply's in-crate regressions.
//!
//! In-crate because a `#[cfg(test)]` item is absent from the library an
//! integration test links against, and the seam these three use is one:
//! the damage they do to a built subtree has to happen inside the apply,
//! between the builders and the tail. In a file of their own because
//! `apply.rs` carries the operation itself and is near the thousand-line
//! gate without them.

use super::verification::{SubtreePerturbation, perturb_subtree};
use crate::content::property::PropertyType;
use crate::writer::index::{ReindexOptions, WorkDirectory, reindex};
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::store_writer::WritableRepository;

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-reindex-tail-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    fn store(&self) -> std::path::PathBuf {
        let store = self.path.join("store");
        std::fs::create_dir_all(&store).expect("create the store directory");
        store
    }

    fn options(&self) -> ReindexOptions {
        ReindexOptions::new().with_work_directory(WorkDirectory::OperatorNamed(self.path.clone()))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A single-valued String property.
fn text<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    name: &str,
    value: &str,
) -> PropertyToWrite {
    let value = writer.write_string(value).expect("write a string");
    PropertyToWrite {
        name: name.to_owned(),
        property_type: PropertyType::String,
        values: PropertyValuesToWrite::Single(value),
    }
}

/// A single-valued Name property.
fn name_of<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    name: &str,
    value: &str,
) -> PropertyToWrite {
    let value = writer.write_string(value).expect("write a string");
    PropertyToWrite {
        name: name.to_owned(),
        property_type: PropertyType::Name,
        values: PropertyValuesToWrite::Single(value),
    }
}

/// A single-valued Boolean property.
fn boolean<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    name: &str,
    value: bool,
) -> PropertyToWrite {
    let value = writer
        .write_string(if value { "true" } else { "false" })
        .expect("write a string");
    PropertyToWrite {
        name: name.to_owned(),
        property_type: PropertyType::Boolean,
        values: PropertyValuesToWrite::Single(value),
    }
}

/// The definition's own properties, by type.
fn definition_properties<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    index_type: &str,
) -> Vec<PropertyToWrite> {
    let mut properties = vec![
        name_of(writer, "jcr:primaryType", "oak:QueryIndexDefinition"),
        text(writer, "type", index_type),
        boolean(writer, "reindex", true),
    ];
    if index_type == "property" {
        properties.push(names(writer, "propertyNames", &["jcr:title"]));
    } else {
        properties.push(long(writer, "seed", "-7610761686379641542"));
        properties.push(long(writer, "resolution", "1"));
    }
    properties
}

/// A multi-valued Name property.
fn names<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    name: &str,
    values: &[&str],
) -> PropertyToWrite {
    let values = values
        .iter()
        .map(|value| writer.write_string(value).expect("write a string"))
        .collect();
    PropertyToWrite {
        name: name.to_owned(),
        property_type: PropertyType::Name,
        values: PropertyValuesToWrite::Multiple(values),
    }
}

/// A single-valued Long property, written from its decimal text.
fn long<Sink: crate::writer::SegmentSink>(
    writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    name: &str,
    value: &str,
) -> PropertyToWrite {
    let value = writer.write_string(value).expect("write a string");
    PropertyToWrite {
        name: name.to_owned(),
        property_type: PropertyType::Long,
        values: PropertyValuesToWrite::Single(value),
    }
}

/// A store holding one indexed node and one definition of `index_type`,
/// flagged for reindex.
///
/// Written through the production writer rather than the independent
/// encoder, because what these three pin is the tail's refusal, not an
/// encoding — and an in-crate test cannot reach the test-only encoder.
fn store_with_a_flagged_definition(
    directory: &TestDirectory,
    index_type: &str,
) -> std::path::PathBuf {
    let path = directory.store();
    let store = WritableRepository::open(&path).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    let page_properties = vec![
        name_of(&mut writer, "jcr:primaryType", "nt:unstructured"),
        text(&mut writer, "jcr:title", "Alpha"),
    ];
    let page = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &page_properties)
        .expect("write the page");
    let content_properties = vec![name_of(&mut writer, "jcr:primaryType", "nt:unstructured")];
    let content = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "page".to_owned(),
                node: page,
            },
            &content_properties,
        )
        .expect("write the content");

    let subject = definition_properties(&mut writer, index_type);
    let subject = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &subject)
        .expect("write the definition");

    // The path service reads a `nodetype` definition, so every fixture
    // carries one whether or not the run rebuilds it.
    let nodetype_properties = vec![
        name_of(&mut writer, "jcr:primaryType", "oak:QueryIndexDefinition"),
        text(&mut writer, "type", "property"),
        names(&mut writer, "propertyNames", &["jcr:primaryType"]),
    ];
    let nodetype = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &nodetype_properties)
        .expect("write the nodetype definition");

    let oak_index = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("nodetype".to_owned(), nodetype),
                ("subject".to_owned(), subject),
            ]),
            &[],
        )
        .expect("write oak:index");
    let root = writer
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
                node: root,
            },
            &[],
        )
        .expect("write the super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
    path
}

/// The head, so a refusal can be shown to have moved nothing.
fn head_of(store: &std::path::Path) -> crate::segment::record::RecordIdentifier {
    crate::store::Repository::open(store)
        .expect("open")
        .head_record_identifier()
}

/// A guard that clears the perturbation even when the test panics, so
/// one failure cannot make the next test in this thread fail too.
struct Perturbed;

impl Perturbed {
    fn to(perturbation: SubtreePerturbation) -> Self {
        perturb_subtree(Some(perturbation));
        Self
    }
}

impl Drop for Perturbed {
    fn drop(&mut self) {
        perturb_subtree(None);
    }
}

/// A forged entry naming a path that is not in the content is refused
/// by the entry arm, and the head does not move.
#[test]
fn a_forged_entry_the_content_does_not_carry_is_refused() {
    let directory = TestDirectory::new("forged-entry");
    let store = store_with_a_flagged_definition(&directory, "property");
    let before = head_of(&store);

    let _perturbed = Perturbed::to(SubtreePerturbation::SpliceForgedEntries(1));
    let error = reindex(&store, directory.options()).expect_err("the tail must refuse");
    assert!(
        error
            .to_string()
            .contains("does not agree with the content it indexes"),
        "the refusal says what it found: {error}"
    );
    assert_eq!(
        head_of(&store),
        before,
        "a refused reindex moved the head anyway"
    );
}

/// Two forged entries exceed the tail's budget, which is the collected
/// count plus one and accepts at its limit.
///
/// One more entry than was collected is what a correct index can never
/// hold and what the budget still admits, so the pass reports on it
/// rather than stopping; two is the smallest number that proves the
/// budget is the collected count rather than something looser.
#[test]
fn a_subtree_holding_more_entries_than_were_collected_exhausts_the_budget() {
    let directory = TestDirectory::new("budget");
    let store = store_with_a_flagged_definition(&directory, "property");
    let before = head_of(&store);

    let _perturbed = Perturbed::to(SubtreePerturbation::SpliceForgedEntries(2));
    let error = reindex(&store, directory.options()).expect_err("the tail must refuse");
    let message = error.to_string();
    assert!(
        message.contains("would examine more than") && message.contains("entries"),
        "the refusal names the limit it reached: {message}"
    );
    assert_eq!(head_of(&store), before);
}

/// A `:cnt` that is not what the build credited is refused by the
/// counter arm.
#[test]
fn a_counter_count_the_build_did_not_credit_is_refused() {
    let directory = TestDirectory::new("counter-count");
    let store = store_with_a_flagged_definition(&directory, "counter");
    let before = head_of(&store);

    let _perturbed = Perturbed::to(SubtreePerturbation::AlterOneCount);
    let error = reindex(&store, directory.options()).expect_err("the tail must refuse");
    assert!(
        error.to_string().contains("where the build credited it"),
        "the refusal says what it found: {error}"
    );
    assert_eq!(head_of(&store), before);
}

/// Without a perturbation the same fixtures pass, so the three above
/// fail for the reason they name rather than because the fixture is
/// broken.
#[test]
fn the_same_fixtures_pass_unperturbed() {
    for index_type in ["property", "counter"] {
        let directory = TestDirectory::new(&format!("unperturbed-{index_type}"));
        let store = store_with_a_flagged_definition(&directory, index_type);
        let outcome = reindex(&store, directory.options()).expect("reindex");
        assert!(outcome.moved_the_head(), "{index_type} rebuilt nothing");
    }
}

/// A Lucene selection runs through `PreparedReindex` and therefore through
/// the `index-reindex.` cutpoints task 0708 armed.
///
/// The assertion is the wiring itself: the publication and verification
/// boundaries are `apply.rs`'s, shared with every other definition type, so
/// what this plan has to show is that a Lucene definition *reaches* them
/// rather than taking a path of its own. A dropped arm would rebuild
/// correctly and simply never be interrupted where the case says it is.
// Unix-only for the same reason the module's other Lucene case is: the
// fixture comes from the fault-injection harness, which forks.
#[cfg(unix)]
#[test]
fn a_lucene_selection_runs_through_the_prepared_wrapper() {
    use crate::index::lucene::documents::binaries::{BinaryTextFallback, BinaryTextPolicy};

    let directory = TestDirectory::new("lucene-wiring");
    let store = crate::writer::fault_injection::lucene_fixture::write_lucene_reindex_fixture(
        &directory.path,
    );
    let options = ReindexOptions::new()
        .with_work_directory(crate::writer::index::WorkDirectory::OperatorNamed(
            crate::writer::fault_injection::test_support::reindex_work_directory(&store),
        ))
        .with_binary_text_policy(BinaryTextPolicy::new(BinaryTextFallback::Marker));

    // `reindex` is the shared entry: it prepares, applies and publishes.
    // A Lucene definition that did not reach it would come back with no
    // report at all.
    let outcome = crate::writer::index::reindex(&store, options).expect("reindex");
    assert!(outcome.moved_the_head(), "the wrapper published nothing");
    let (path, report) = outcome
        .definitions
        .first()
        .expect("the Lucene definition reached the shared tail");
    assert_eq!(path, "/oak:index/lucene");
    assert!(
        matches!(
            report,
            crate::writer::index::DefinitionReport::RebuiltIndex { .. }
        ),
        "{report:?}"
    );
}
