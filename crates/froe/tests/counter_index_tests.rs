//! The counter index: Oak's `SipHash` variant, the `:cnt` map and the
//! estimated node count.
//!
//! The hash is pinned against 72 vectors a real JDK produced by calling
//! **Oak's own `SipHash` class**, from the `oak-core-1.90.0.jar` inside the
//! digest-pinned Sling image the interop suite runs against — so this is not
//! froe checked against froe's reading of the Java, but froe checked against
//! the Java running.
//!
//! The estimate's rules each get a test of their own, because Oak's own
//! method reaches six different answers through five branches and a port that
//! collapsed any two of them would still pass a single end-to-end assertion.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::PathBuf;

use froe::index::IndexDefinition;
use froe::index::counter::{
    CountBound, CounterIndex, NodeCountEstimate, SipHash, estimated_node_count, hash_for_path,
    narrowed_seed,
};
use froe::store::Repository;
use support::property_index_layout::{Node, Property, write_repository_with_tree};

/// The vectors, produced by Oak's own class inside the pinned image; the
/// file's header records the exact command and the whole program.
const VECTORS: &str = include_str!("fixtures/oak-sip-hash-vectors.tsv");

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-counter-index-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test repository directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn every_vector_from_oaks_own_class_matches() {
    let mut replayed = 0usize;
    for line in VECTORS.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let seed: i64 = fields
            .next()
            .expect("a seed column")
            .parse()
            .expect("a decimal seed");
        let elements = fields.next().expect("an elements column");
        let expected: i32 = fields
            .next()
            .expect("an expected column")
            .parse()
            .expect("a decimal hash code");
        assert!(fields.next().is_none(), "vector has extra columns");
        let mut hash = SipHash::seeded(seed);
        if !elements.is_empty() {
            for element in elements.split('|') {
                hash = hash.for_child(element);
            }
        }
        assert_eq!(
            hash.hash_code(),
            expected,
            "seed {seed}, elements {elements:?}"
        );
        replayed += 1;
    }
    assert!(
        replayed >= 72,
        "expected the committed vectors, replayed {replayed}"
    );
}

#[test]
fn the_path_spelling_of_the_chain_agrees_with_the_element_spelling() {
    // `hash_for_path` is the convenience the rest of the crate calls; the
    // vectors pin the element chain, so this is what ties the two together.
    let seed = -584_039_110i64;
    let mut chained = SipHash::seeded(seed);
    for element in ["content", "interop", "files"] {
        chained = chained.for_child(element);
    }
    assert_eq!(
        hash_for_path(seed, "/content/interop/files").hash_code(),
        chained.hash_code()
    );
    assert_eq!(
        hash_for_path(seed, "/").hash_code(),
        SipHash::seeded(seed).hash_code()
    );
}

#[test]
fn the_roots_hash_code_is_the_same_for_every_seed() {
    // Both key halves cancel in the fold, so a port with the seeding wrong
    // would still pass a root-only assertion. This pins the property rather
    // than relying on it.
    let root = hash_for_path(0, "/").hash_code();
    for seed in [1i64, -1, i64::MIN, i64::MAX, -7_610_761_686_379_641_542] {
        assert_eq!(hash_for_path(seed, "/").hash_code(), root);
    }
    assert_ne!(
        hash_for_path(0, "/a").hash_code(),
        hash_for_path(1, "/a").hash_code(),
        "one step down, the seed must matter"
    );
}

/// A counter definition node, with whatever data child the caller built.
fn counter_definition(seed: Option<i64>, resolution: Option<Property>, data: Option<Node>) -> Node {
    let mut definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("counter".to_owned()))
        .with("async", Property::Text("async".to_owned()));
    if let Some(seed) = seed {
        definition = definition.with("seed", Property::Long(seed));
    }
    if let Some(resolution) = resolution {
        definition = definition.with("resolution", resolution);
    }
    if let Some(data) = data {
        definition = definition.with_child(":index", data);
    }
    definition
}

/// Builds a store whose `/oak:index/counter` is `definition`, plus whatever
/// content the caller supplies.
fn store(name: &str, definition: Node, content: Vec<(&str, Node)>) -> (TestDirectory, Repository) {
    let directory = TestDirectory::new(name);
    let mut root =
        Node::new().with_child("oak:index", Node::new().with_child("counter", definition));
    for (child_name, node) in content {
        root = root.with_child(child_name, node);
    }
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open the repository");
    (directory, repository)
}

fn read_counter(repository: &Repository) -> (froe::content::NodeState<'_>, IndexDefinition) {
    let node = repository
        .node_at_path("/oak:index/counter")
        .expect("resolve")
        .expect("exists");
    let definition =
        IndexDefinition::read(&node, "/oak:index/counter").expect("the definition reads");
    (node, definition)
}

#[test]
fn a_string_resolution_is_read_as_its_number() {
    let (_directory, repository) = store(
        "string-resolution",
        counter_definition(
            Some(7),
            Some(Property::Text("500".to_owned())),
            Some(Node::new()),
        ),
        Vec::new(),
    );
    let (node, definition) = read_counter(&repository);
    let index = CounterIndex::open(node, &definition);
    assert_eq!(index.resolution(), 500, "resolution is read converting");
    assert_eq!(
        index.bit_mask(),
        511,
        "highestOneBit(500) = 256, doubled less one"
    );
}

#[test]
fn an_absent_resolution_takes_the_default_and_its_bit_mask() {
    let (_directory, repository) = store(
        "default-resolution",
        counter_definition(Some(7), None, Some(Node::new())),
        Vec::new(),
    );
    let (node, definition) = read_counter(&repository);
    let index = CounterIndex::open(node, &definition);
    assert_eq!(index.resolution(), 1000);
    assert_eq!(index.bit_mask(), 1023, "the increment is 1024");
}

#[test]
fn the_stored_seed_is_narrowed_before_it_is_hashed_with() {
    let stored = -7_610_761_686_379_641_542i64;
    let (_directory, repository) = store(
        "seed",
        counter_definition(Some(stored), None, Some(Node::new())),
        Vec::new(),
    );
    let (node, definition) = read_counter(&repository);
    assert_eq!(
        definition.seed,
        Some(stored),
        "the model keeps what is stored"
    );
    let index = CounterIndex::open(node, &definition);
    assert_eq!(
        index.seed(),
        narrowed_seed(stored),
        "every run after the creating one uses the narrowed value"
    );
    assert_ne!(index.seed(), stored);
}

/// A `:cnt` map: `/` with 10240, `/libs` with 9216, and `/var` carrying no
/// count at all — the state an incrementally maintained index reaches after
/// deletions, which `leaveNew` produces by removing the property and leaving
/// the node.
fn sample_counter_data() -> Node {
    Node::new()
        .with(":cnt", Property::Long(10_240))
        .with_child("libs", Node::new().with(":cnt", Property::Long(9_216)))
        .with_child("var", Node::new())
}

#[test]
fn the_reader_enumerates_the_counter_map_including_a_node_with_no_count() {
    let (_directory, repository) = store(
        "entries",
        counter_definition(Some(7), None, Some(sample_counter_data())),
        Vec::new(),
    );
    let (node, definition) = read_counter(&repository);
    let index = CounterIndex::open(node, &definition);
    let entries: Vec<(String, Option<i64>)> = index
        .entries()
        .expect("enumerate")
        .into_iter()
        .map(|entry| (entry.path, entry.count))
        .collect();
    assert_eq!(
        entries,
        [
            ("/".to_owned(), Some(10_240)),
            ("/libs".to_owned(), Some(9_216)),
            ("/var".to_owned(), None),
        ],
        "a node with neither :cnt nor :count reports an absent count, not a zero"
    );
}

#[test]
fn an_old_counters_count_property_is_added_rather_than_ignored() {
    let data = Node::new()
        .with(":cnt", Property::Long(1_024))
        .with(":count", Property::Long(500));
    let (_directory, repository) = store(
        "combined",
        counter_definition(Some(7), None, Some(data)),
        Vec::new(),
    );
    let (node, definition) = read_counter(&repository);
    let index = CounterIndex::open(node, &definition);
    assert_eq!(index.entries().expect("enumerate")[0].count, Some(1_524));
}

#[test]
fn a_counter_definition_with_no_data_child_reads_as_empty_and_estimates_unknown() {
    let (_directory, repository) = store(
        "no-data",
        counter_definition(Some(7), None, None),
        Vec::new(),
    );
    let (node, definition) = read_counter(&repository);
    let index = CounterIndex::open(node, &definition);
    assert!(!index.has_data_node().expect("check"));
    assert!(index.entries().expect("enumerate").is_empty());
    assert_eq!(
        estimated_node_count(
            &repository.content_root().expect("content root"),
            "/",
            CountBound::Expected
        )
        .expect("estimate"),
        NodeCountEstimate::Unknown
    );
}

#[test]
fn a_store_with_no_counter_definition_estimates_unknown() {
    let directory = TestDirectory::new("no-counter");
    write_repository_with_tree(
        &directory.path,
        &Node::new().with_child("content", Node::new()),
    );
    let repository = Repository::open(&directory.path).expect("open");
    assert_eq!(
        estimated_node_count(
            &repository.content_root().expect("content root"),
            "/",
            CountBound::Expected
        )
        .expect("estimate"),
        NodeCountEstimate::Unknown
    );
}

#[test]
fn a_path_that_does_not_exist_estimates_zero() {
    let (_directory, repository) = store(
        "absent-path",
        counter_definition(Some(7), None, Some(sample_counter_data())),
        Vec::new(),
    );
    assert_eq!(
        estimated_node_count(
            &repository.content_root().expect("content root"),
            "/gone",
            CountBound::Expected
        )
        .expect("estimate"),
        NodeCountEstimate::Count(0)
    );
}

#[test]
fn the_root_estimate_sums_the_data_childs_count_and_the_bound_adds_the_resolution() {
    let (_directory, repository) = store(
        "root-estimate",
        counter_definition(Some(7), None, Some(sample_counter_data())),
        Vec::new(),
    );
    let content_root = repository.content_root().expect("content root");
    assert_eq!(
        estimated_node_count(&content_root, "/", CountBound::Expected).expect("estimate"),
        NodeCountEstimate::Count(10_240)
    );
    assert_eq!(
        estimated_node_count(&content_root, "/", CountBound::Maximum).expect("estimate"),
        NodeCountEstimate::Count(10_340),
        "the maximum bound adds the approximate counter's own resolution of 100"
    );
}

#[test]
fn a_non_root_path_answers_for_that_subtree_rather_than_the_store() {
    let (_directory, repository) = store(
        "subtree-estimate",
        counter_definition(Some(7), None, Some(sample_counter_data())),
        vec![("libs", Node::new())],
    );
    assert_eq!(
        estimated_node_count(
            &repository.content_root().expect("content root"),
            "/libs",
            CountBound::Expected
        )
        .expect("estimate"),
        NodeCountEstimate::Count(9_216)
    );
}

#[test]
fn a_path_the_sampling_counter_never_recorded_answers_fallback() {
    // `/var` exists in the counter map with no count, and in the content
    // tree with nothing of its own, so the sum is zero — the case Oak
    // answers with a placeholder number that counts nothing.
    let (_directory, repository) = store(
        "fallback",
        counter_definition(Some(7), None, Some(sample_counter_data())),
        vec![("var", Node::new())],
    );
    let content_root = repository.content_root().expect("content root");
    assert_eq!(
        estimated_node_count(&content_root, "/var", CountBound::Expected).expect("estimate"),
        NodeCountEstimate::Fallback
    );
    assert_eq!(
        estimated_node_count(&content_root, "/var", CountBound::Maximum).expect("estimate"),
        NodeCountEstimate::Fallback,
        "both bounds answer Fallback; Oak's 0 and 2000 are placeholders, not counts"
    );
}

#[test]
fn a_target_nodes_own_approximate_count_answers_under_the_expected_bound_alone() {
    // This is the branch that answers for a property index's `:index` node:
    // the node carries `:count_*` properties of its own.
    let (_directory, repository) = store(
        "own-approximate",
        counter_definition(Some(7), None, Some(sample_counter_data())),
        vec![(
            "content",
            Node::new()
                .with(":count_0daeb465", Property::Long(200))
                .with(":count_65f31f1f", Property::Long(400)),
        )],
    );
    let content_root = repository.content_root().expect("content root");
    assert_eq!(
        estimated_node_count(&content_root, "/content", CountBound::Expected).expect("estimate"),
        NodeCountEstimate::Count(600),
        "max(added / 2, added - removed) over 200 and 400"
    );
    // Under the maximum bound that branch is skipped entirely, so the answer
    // comes from the counter map instead — where `/content` is absent.
    assert_eq!(
        estimated_node_count(&content_root, "/content", CountBound::Maximum).expect("estimate"),
        NodeCountEstimate::Fallback
    );
}

#[test]
fn a_target_nodes_own_combined_count_answers_under_both_bounds() {
    let (_directory, repository) = store(
        "own-combined",
        counter_definition(Some(7), None, Some(sample_counter_data())),
        vec![("content", Node::new().with(":cnt", Property::Long(2_048)))],
    );
    let content_root = repository.content_root().expect("content root");
    assert_eq!(
        estimated_node_count(&content_root, "/content", CountBound::Expected).expect("estimate"),
        NodeCountEstimate::Count(2_048)
    );
    assert_eq!(
        estimated_node_count(&content_root, "/content", CountBound::Maximum).expect("estimate"),
        NodeCountEstimate::Count(2_148)
    );
}

#[test]
fn the_sampling_test_uses_the_narrowed_seed_and_the_definitions_bit_mask() {
    let (_directory, repository) = store(
        "sampled",
        counter_definition(Some(7), None, Some(Node::new())),
        Vec::new(),
    );
    let (node, definition) = read_counter(&repository);
    let index = CounterIndex::open(node, &definition);
    // Exercising the test rather than asserting a particular hit: the hash
    // and the mask both have their own pinned tests, and which paths a seed
    // of 7 happens to sample is not a property anyone relies on.
    let sampled = ["/", "/a", "/a/b", "/content", "/content/interop"]
        .into_iter()
        .filter(|path| index.is_sampled(path))
        .count();
    assert!(
        sampled <= 5,
        "the test answers for every path without panicking"
    );
    assert_eq!(
        index.is_sampled("/a"),
        hash_for_path(narrowed_seed(7), "/a").hash_code() & index.bit_mask() == 0,
        "the test is the mask applied to the chained hash of the narrowed seed"
    );
}
