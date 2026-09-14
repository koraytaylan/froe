//! The counter builder against Oak's own editor.
//!
//! The counter is the one rebuild whose output cannot be checked against
//! content: its `:cnt` values are a function of a hash chain, a seed and a
//! resolution, so a chain that is subtly wrong produces a plausible map that
//! disagrees with Oak's next cycle in a way that only grows. The oracle is
//! therefore Oak's editor itself, captured as
//! `tests/fixtures/oak-counter-index-vectors.tsv` by the judge running
//! inside the pinned image.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use froe::index::IndexDefinition;
use froe::store::Repository;
use froe::writer::index::counter_builder::CounterBuilder;
use froe::writer::record_writer::ChildNodesToWrite;
use froe::writer::store_writer::WritableRepository;
use support::property_index_layout::{Node, Property, write_repository_with_tree};

/// The tree the vector was generated over: 12 children per level, 3 deep.
const FAN_OUT: u32 = 12;
const DEPTH: u32 = 3;

/// The seed and resolution the vector was generated with.
const VECTOR_SEED: i64 = -7_610_761_686_379_641_542;
const VECTOR_RESOLUTION: i64 = 8;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-counter-builder-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The same tree `CounterVectors.java` populates.
fn vector_tree(remaining: u32) -> Node {
    let mut node = Node::new().with(
        "jcr:primaryType",
        Property::Name("nt:unstructured".to_owned()),
    );
    if remaining == 0 {
        return node;
    }
    for index in 0..FAN_OUT {
        node = node.with_child(&format!("n{index}"), vector_tree(remaining - 1));
    }
    node
}

/// A counter definition with the given seed and resolution.
fn counter_definition(seed: Option<i64>, resolution: Option<i64>) -> Node {
    let mut node = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("counter".to_owned()));
    if let Some(seed) = seed {
        node = node.with("seed", Property::Long(seed));
    }
    if let Some(resolution) = resolution {
        node = node.with("resolution", Property::Long(resolution));
    }
    node
}

/// Oak's vector, as a path-to-count map.
fn oak_vector() -> BTreeMap<String, i64> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/oak-counter-index-vectors.tsv"
    ))
    .expect("read the counter vector fixture");
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let (path, count) = line.split_once('\t').expect("path<TAB>cnt");
            (path.to_owned(), count.parse::<i64>().expect("a count"))
        })
        .collect()
}

/// Builds the counter over `content` with `definition`, returning what froe
/// wrote as a path-to-count map and whether an `:index` node exists at all.
fn build(
    directory: &TestDirectory,
    content: Node,
    definition: Node,
) -> (BTreeMap<String, i64>, bool, Option<i64>) {
    let source = directory.path.join("source");
    std::fs::create_dir_all(&source).expect("create the source store");
    let root = content.with_child("oak:index", Node::new().with_child("counter", definition));
    write_repository_with_tree(&source, &root);

    let repository = Repository::open(&source).expect("open the source");
    let content_root = repository.content_root().expect("content root");
    let definition_state = repository
        .node_at_path("/oak:index/counter")
        .expect("resolve")
        .expect("the definition exists");
    let definition = IndexDefinition::read(&definition_state, "/oak:index/counter").expect("model");

    let target = directory.path.join("target");
    std::fs::create_dir_all(&target).expect("create the target store");
    let store = WritableRepository::open(&target).expect("open the target for writing");
    let generation = store.writing_generation().expect("generation");
    let (index_record, created_seed) = {
        let mut writer = store.record_writer(generation);
        let builder = CounterBuilder::new(&definition);
        let built = builder.build(&content_root, &mut writer).expect("build");
        let root = writer
            .write_node(
                None,
                &[],
                &match built.index_record {
                    Some(index) => ChildNodesToWrite::One {
                        name: "counter".to_owned(),
                        node: index,
                    },
                    None => ChildNodesToWrite::Zero,
                },
                &[],
            )
            .expect("write the content root");
        let checkpoints = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("checkpoints");
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
            .expect("super-root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.compare_and_set_head(previous, super_root));
        (built.index_record.is_some(), built.created_seed)
    };
    store.close().expect("close");

    let written = Repository::open(&target).expect("open the target");
    let mut counts = BTreeMap::new();
    if index_record {
        let index = written
            .node_at_path("/counter")
            .expect("resolve")
            .expect("the :index node was written");
        read_counts(&index, "/", &mut counts);
    }
    (counts, index_record, created_seed)
}

fn read_counts(
    node: &froe::content::node::NodeState<'_>,
    path: &str,
    counts: &mut BTreeMap<String, i64>,
) {
    if let Some(property) = node.property(":cnt").expect("read :cnt") {
        let value = match &property.values {
            froe::content::PropertyValues::Single(value) => Some(value),
            froe::content::PropertyValues::Multiple(values) => values.first(),
        };
        if let Some(text) = value.and_then(froe::content::property::PropertyValue::as_text) {
            counts.insert(path.to_owned(), text.parse().expect("a count"));
        }
    }
    for (name, child) in node.child_node_entries().expect("children") {
        let child_path = if path == "/" {
            format!("/{name}")
        } else {
            format!("{path}/{name}")
        };
        read_counts(&child, &child_path, counts);
    }
}

#[test]
fn the_builder_reproduces_oaks_counter_map_exactly() {
    // The seed is outside the `int` range, so this also pins the 32-bit
    // narrowing every run after the first applies: a builder that used the
    // stored 64-bit value would place hits at different paths.
    let directory = TestDirectory::new("vector");
    let (built, has_index, created) = build(
        &directory,
        vector_tree(DEPTH),
        counter_definition(Some(VECTOR_SEED), Some(VECTOR_RESOLUTION)),
    );
    assert!(has_index, "the vector tree produces hits");
    assert_eq!(created, None, "the definition carried a seed already");

    let expected = oak_vector();
    assert!(!expected.is_empty(), "the fixture is empty");
    assert_eq!(
        built, expected,
        "froe's counter map differs from Oak's own editor"
    );
}

#[test]
fn a_store_where_nothing_hits_produces_no_index_node() {
    // Oak's editor returns before creating `:index`, so its own reindex of a
    // small store leaves the counter definition without one — and that is
    // the shape the interop oracle compares against.
    let directory = TestDirectory::new("no-hit");
    let content = Node::new().with_child(
        "content",
        Node::new().with("jcr:title", Property::Text("one node".to_owned())),
    );
    // The default resolution's mask is 1023, so one child almost never hits.
    let (built, has_index, _) = build(&directory, content, counter_definition(Some(1), None));
    assert!(!has_index, "no hit means no :index child at all");
    assert!(built.is_empty());
}

#[test]
fn a_definition_without_a_seed_gets_one_that_survives_the_narrowing() {
    // Oak draws 64 bits and reads back 32 on every run after the first, so
    // the creating run and every later one disagree about where hits fall.
    // froe draws a value that already fits, so both readings agree.
    let directory = TestDirectory::new("seed-creation");
    let (_, _, created) = build(
        &directory,
        vector_tree(2),
        counter_definition(None, Some(VECTOR_RESOLUTION)),
    );
    let seed = created.expect("a definition with no seed gets one");
    assert_eq!(
        seed,
        i64::from(seed as i32),
        "the created seed must read back identically through the 32-bit narrowing"
    );
}

#[test]
fn a_resolution_other_than_the_default_changes_the_mask() {
    let directory = TestDirectory::new("resolution");
    let (coarse, _, _) = build(
        &directory,
        vector_tree(DEPTH),
        counter_definition(Some(VECTOR_SEED), Some(1024)),
    );
    let fine = TestDirectory::new("resolution-fine");
    let (dense, _, _) = build(
        &fine,
        vector_tree(DEPTH),
        counter_definition(Some(VECTOR_SEED), Some(VECTOR_RESOLUTION)),
    );
    assert!(
        dense.len() > coarse.len(),
        "a finer resolution credits more nodes: {} against {}",
        dense.len(),
        coarse.len()
    );
}

#[test]
fn hidden_children_are_never_counted() {
    // A shallower tree than the vector's: the fixture encoder puts a whole
    // store in one segment, and doubling the vector tree with a hidden copy
    // of itself overflows it. Two levels is enough to produce hits at more
    // than one depth, which is what the exclusion has to survive.
    let shallow = 2;
    let plain = TestDirectory::new("hidden-without");
    let (without, _, _) = build(
        &plain,
        vector_tree(shallow),
        counter_definition(Some(VECTOR_SEED), Some(VECTOR_RESOLUTION)),
    );
    assert!(
        without.len() > 1,
        "the fixture must credit more than the root, or the exclusion proves nothing"
    );

    let directory = TestDirectory::new("hidden-with");
    let (with_hidden, _, _) = build(
        &directory,
        vector_tree(shallow).with_child(":storage", vector_tree(shallow)),
        counter_definition(Some(VECTOR_SEED), Some(VECTOR_RESOLUTION)),
    );
    assert_eq!(
        with_hidden, without,
        "a hidden subtree must not change a single count"
    );
}
