//! The mirror and unique builders against an independent encoder.
//!
//! The comparison is deliberately not against froe's own readers: they share
//! the builder's assumptions about the layout, so a shared misreading would
//! pass. Each fixture is built twice — once by the builder through
//! `RecordWriter`, once by the independent in-memory encoder of
//! `tests/support/property_index_layout.rs` — written into one store under
//! two paths, and the two subtrees compared through the lines of
//! `digest_repository_excluding` after normalizing the path prefix away.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::{Path, PathBuf};

use froe::store::Repository;
use froe::tooling::digest::digest_repository_excluding;
use froe::writer::index::property_builder::{BuilderAccounting, MirrorBuilder, UniqueBuilder};
use froe::writer::record_writer::ChildNodesToWrite;
use froe::writer::store_writer::WritableRepository;
use support::property_index_layout::{Node, mirror_storage, unique_storage};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-property-builder-{name}-{}-{:?}",
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

/// Which strategy a fixture is built with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Strategy {
    Mirror,
    Unique,
}

/// Writes `entries` through the builder into its own store, and `expected`
/// through the independent encoder into another.
///
/// Two stores rather than one, because the independent encoder shares no
/// encoding code with production — which is the whole reason it exists — and
/// so cannot write through the same `RecordWriter`. Each store's `:index`
/// subtree is digested and the two line sets compared.
fn build_and_compare(
    directory: &Path,
    strategy: Strategy,
    entries: &[(&str, &str)],
    expected: &Node,
) -> (Vec<String>, Vec<String>, BuilderAccounting) {
    let built_store = directory.join("built");
    let expected_store = directory.join("expected");
    std::fs::create_dir_all(&built_store).expect("create the builder's store");
    std::fs::create_dir_all(&expected_store).expect("create the encoder's store");

    let accounting = write_through_the_builder(&built_store, strategy, entries);
    support::property_index_layout::write_repository_with_tree(
        &expected_store,
        &Node::new().with_child("index", expected.clone()),
    );

    (
        index_lines(&built_store),
        index_lines(&expected_store),
        accounting,
    )
}

/// Writes the builder's `:index` under `/index` of a fresh store.
fn write_through_the_builder(
    directory: &Path,
    strategy: Strategy,
    entries: &[(&str, &str)],
) -> BuilderAccounting {
    let store = WritableRepository::open(directory).expect("open the store");
    let generation = store.writing_generation().expect("the writing generation");
    let accounting;
    {
        let mut writer = store.record_writer(generation);
        let (index, reported) = match strategy {
            Strategy::Mirror => {
                let mut builder = MirrorBuilder::new(&mut writer);
                for (key, path) in entries {
                    builder.push(key, path).expect("push an entry");
                }
                builder.finish().expect("finish the mirror")
            }
            Strategy::Unique => {
                let mut builder = UniqueBuilder::new(&mut writer);
                for (key, path) in entries {
                    builder.push(key, path).expect("push an entry");
                }
                builder.finish().expect("finish the unique index")
            }
        };
        accounting = reported;
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "index".to_owned(),
                    node: index,
                },
                &[],
            )
            .expect("write the content root");
        let checkpoints = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("write the checkpoints container");
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
    }
    store.close().expect("close the store");
    accounting
}

/// The digest lines of `/index` and everything below it, without the
/// approximate counters.
///
/// `docs/analysis/index-property-storage.md` §11: the counter's name, its
/// presence and its value are each drawn from a random generator, so two
/// Oak reindexes of one tree disagree on them and no expectation can name
/// one. Excluding the prefix is what that section says a comparison must
/// do — and nothing else is excluded, so every other property is still
/// held to the independent encoder's bytes. Their *shape* is asserted
/// separately by `the_counters_have_the_shape_oaks_algorithm_produces`.
fn index_lines(directory: &Path) -> Vec<String> {
    let repository = Repository::open(directory).expect("open the repository");
    let mut rendered = Vec::new();
    digest_repository_excluding(
        &repository,
        &[],
        &[froe::writer::index::approximate_counter::COUNT_PROPERTY_PREFIX.to_owned()],
        &mut rendered,
    )
    .expect("digest");
    let digest = String::from_utf8(rendered).expect("the digest is UTF-8");
    digest
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("/index")?;
            (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\t'))
                .then(|| rest.to_owned())
        })
        .collect()
}

fn assert_matches_the_independent_encoder(
    name: &str,
    strategy: Strategy,
    entries: &[(&str, &str)],
    expected: &Node,
) -> BuilderAccounting {
    let directory = TestDirectory::new(name);
    let (built, encoded, accounting) =
        build_and_compare(&directory.path, strategy, entries, expected);
    assert_eq!(
        built, encoded,
        "{name}: the builder's subtree differs from the independent encoder's"
    );
    assert!(!built.is_empty(), "{name}: the comparison found no lines");
    accounting
}

#[test]
fn an_empty_index_still_has_an_index_node_with_child_arity_zero() {
    // Oak's uniqueness check creates `:index` unconditionally, so a property
    // definition always has one even with nothing indexed.
    let accounting =
        assert_matches_the_independent_encoder("empty", Strategy::Mirror, &[], &Node::new());
    assert_eq!(accounting.nodes_written, 1, "only the :index node");

    // `Zero`, not `Many(vec![])`. The difference is a child arity of 0
    // against 2 plus an empty map record — a shape Oak's own segment writer
    // never produces, and one the digest lines do not show, so it is
    // asserted on the written template.
    let directory = TestDirectory::new("empty-arity");
    let store = directory.path.join("built");
    std::fs::create_dir_all(&store).expect("create the store");
    write_through_the_builder(&store, Strategy::Mirror, &[]);
    let repository = Repository::open(&store).expect("open the repository");
    let index = repository
        .node_at_path("/index")
        .expect("resolve")
        .expect("the :index node exists");
    assert!(
        index.child_node_entries().expect("children").is_empty(),
        "an empty :index has no children at all"
    );
}

#[test]
fn an_empty_value_is_a_key_like_any_other() {
    // The empty token is `:`, a hidden name — and a reader that filtered
    // hidden names at the key level would lose every entry indexed under it.
    let entries = [(":", "/content/page")];
    assert_matches_the_independent_encoder(
        "empty-token",
        Strategy::Mirror,
        &entries,
        &mirror_storage(&entries),
    );
}

#[test]
fn the_root_path_addresses_the_key_node_itself() {
    let entries = [("Alpha", "/")];
    assert_matches_the_independent_encoder(
        "root-path",
        Strategy::Mirror,
        &entries,
        &mirror_storage(&entries),
    );
}

#[test]
fn two_keys_sharing_a_path_prefix_each_get_their_own_trie() {
    let entries = [
        ("Alpha", "/content/a/x"),
        ("Alpha", "/content/a/y"),
        ("Beta", "/content/a/x"),
    ];
    assert_matches_the_independent_encoder(
        "shared-prefix",
        Strategy::Mirror,
        &entries,
        &mirror_storage(&entries),
    );
}

#[test]
fn an_ancestor_and_its_descendant_under_one_key_both_carry_match() {
    // The insert sets `match` unconditionally after descending every
    // element, so an interior node a shorter indexed path also addresses
    // carries it too. Oak's `nodetype` index hits this for nested
    // `rep:AuthorizableFolder` nodes.
    let entries = [("Alpha", "/content/a"), ("Alpha", "/content/a/b/c")];
    assert_matches_the_independent_encoder(
        "ancestor",
        Strategy::Mirror,
        &entries,
        &mirror_storage(&entries),
    );
}

#[test]
fn a_deep_path_is_one_node_per_element() {
    let deep = "/a/b/c/d/e/f/g/h/i/j/k/l";
    let entries = [("Key", deep)];
    let accounting = assert_matches_the_independent_encoder(
        "deep",
        Strategy::Mirror,
        &entries,
        &mirror_storage(&entries),
    );
    // `:index`, the key node, and one per element.
    assert_eq!(accounting.nodes_written, 2 + 12);
    assert_eq!(
        accounting.peak_resident_children, 1,
        "a single deep path holds one completed child at a time"
    );
}

/// How many siblings the compared wide fixture writes.
///
/// Over the 32-entry leaf limit, so the child map branches — which is the
/// shape this fixture is for — and under the width at which the independent
/// encoder's one level of branching would overflow a bucket. That encoder
/// refuses past its limit by name rather than producing a segment no reader
/// can walk, so this number is a property of the *fixture builder*, not of
/// the builder under test. The residency claim is asserted separately, at a
/// width no encoder is involved in.
const COMPARED_FAN_OUT: u32 = 400;

/// How many siblings the residency assertion writes.
///
/// Thousands, because the claim is about what the builder holds and nothing
/// here has to encode it a second time.
const RESIDENT_FAN_OUT: u32 = 5_000;

#[test]
fn a_branching_child_map_renders_the_way_the_independent_encoder_writes_it() {
    let paths: Vec<String> = (0..COMPARED_FAN_OUT)
        .map(|index| format!("/content/{index:05}"))
        .collect();
    let entries: Vec<(&str, &str)> = paths.iter().map(|path| ("Alpha", path.as_str())).collect();
    let accounting = assert_matches_the_independent_encoder(
        "wide",
        Strategy::Mirror,
        &entries,
        &mirror_storage(&entries),
    );
    assert_eq!(
        accounting.peak_resident_children, COMPARED_FAN_OUT as usize,
        "the widest node is /content under the key"
    );
}

#[test]
fn thousands_of_siblings_hold_only_the_widest_fan_out_on_one_path() {
    // The one key-proportional term the safety case admits, asserted by the
    // builder's own accounting rather than by the process's resident set
    // size — which would measure the allocator, not the algorithm.
    let directory = TestDirectory::new("resident");
    let store = directory.path.join("built");
    std::fs::create_dir_all(&store).expect("create the store");
    let paths: Vec<String> = (0..RESIDENT_FAN_OUT)
        .map(|index| format!("/content/{index:05}"))
        .collect();
    let entries: Vec<(&str, &str)> = paths.iter().map(|path| ("Alpha", path.as_str())).collect();
    let accounting = write_through_the_builder(&store, Strategy::Mirror, &entries);
    assert_eq!(
        accounting.peak_resident_children, RESIDENT_FAN_OUT as usize,
        "the builder holds the widest fan-out on one path and no more"
    );
    // `:index`, the key node, `/content`, and one node per sibling.
    assert_eq!(accounting.nodes_written, u64::from(RESIDENT_FAN_OUT) + 3);
}

#[test]
fn a_unique_index_is_one_node_per_key_with_an_entry_array() {
    let entries = [("Alpha", "/content/a"), ("Beta", "/content/b")];
    let expected = unique_storage(&[
        ("Alpha", &["/content/a"][..]),
        ("Beta", &["/content/b"][..]),
    ]);
    assert_matches_the_independent_encoder("unique", Strategy::Unique, &entries, &expected);
}

#[test]
fn a_repeated_entry_under_one_unique_key_is_one_entry() {
    // A multi-valued property whose values encode to the same key produces
    // the same `(key, path)` twice. That is one entry, not a duplicate.
    let entries = [("Alpha", "/content/a"), ("Alpha", "/content/a")];
    let expected = unique_storage(&[("Alpha", &["/content/a"][..])]);
    assert_matches_the_independent_encoder("unique-repeat", Strategy::Unique, &entries, &expected);
}

#[test]
fn two_nodes_under_one_unique_key_are_refused_by_name() {
    let directory = TestDirectory::new("unique-duplicate");
    let store = WritableRepository::open(&directory.path).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let mut builder = UniqueBuilder::new(&mut writer);
    builder
        .push("Alpha", "/content/a")
        .expect("the first entry");
    let error = builder
        .push("Alpha", "/content/b")
        .expect_err("a second node under one unique key must be refused");
    assert!(
        matches!(
            &error,
            froe::Error::DuplicateUniqueKey { key, paths }
                if key == "Alpha" && paths == &["/content/a".to_owned(), "/content/b".to_owned()]
        ),
        "the refusal is typed and names both paths: {error}"
    );
}

/// The counters a rebuild writes have the shape Oak's algorithm produces.
///
/// The byte comparison cannot hold them — their name, presence and value
/// are each random — so what is pinned here is every invariant the
/// algorithm guarantees whatever the draws are
/// (`docs/analysis/index-property-storage.md` §11):
///
/// * every value is a **positive multiple of 100**, `COUNT_RESOLUTION`;
/// * no value reaches `COUNT_MAX`;
/// * counters appear on the `:index` node and on key nodes, and **never on
///   a path-element node** — the mirror strategy adjusts exactly two;
/// * `getCountSync` over them is not `-1`, which is the whole point: an
///   index Oak cannot price is an index Oak stops choosing.
///
/// The last is asserted over a build large enough that the 1-in-100 gate
/// is overwhelmingly likely to have fired; a rebuild of a handful of
/// entries legitimately writes none, exactly as a fresh Oak index has none.
#[test]
fn the_counters_have_the_shape_oaks_algorithm_produces() {
    let directory = TestDirectory::new("counter-shape");
    // Enough entries that both gates fire many times over.
    let mut entries: Vec<(String, String)> = (0..4000)
        .map(|serial| {
            (
                format!("key-{:04}", serial % 8),
                format!("/content/node-{serial:04}"),
            )
        })
        .collect();
    // The builder relies on `(key, path)` order and does not check it, so
    // the fixture has to establish it exactly as the sort does.
    entries.sort();
    let borrowed: Vec<(&str, &str)> = entries
        .iter()
        .map(|(key, path)| (key.as_str(), path.as_str()))
        .collect();
    write_through_the_builder(&directory.path, Strategy::Mirror, &borrowed);

    let repository = Repository::open(&directory.path).expect("open the repository");
    let mut rendered = Vec::new();
    digest_repository_excluding(&repository, &[], &[], &mut rendered).expect("digest");
    let digest = String::from_utf8(rendered).expect("the digest is UTF-8");

    let mut total_on_index = 0i64;
    let mut counted_nodes = 0usize;
    for line in digest.lines() {
        let Some((path, properties)) = line.split_once('\t') else {
            continue;
        };
        let counters: Vec<i64> = properties
            .split('\t')
            .filter_map(|field| field.strip_prefix(":count_"))
            .filter_map(|field| field.split_once('='))
            .filter_map(|(_, value)| value.strip_prefix("Long:"))
            .filter_map(|value| value.parse::<i64>().ok())
            .collect();
        if counters.is_empty() {
            continue;
        }
        counted_nodes += 1;

        // Two nodes and no more: the `:index` node, and a key node.
        let depth_below_index = path
            .strip_prefix("/index")
            .expect("under /index")
            .matches('/')
            .count();
        assert!(
            depth_below_index <= 1,
            "a counter landed on a path-element node at {path}: the mirror strategy \
             adjusts the :index node and the key node only"
        );

        for value in &counters {
            assert!(
                *value > 0,
                "{path}: a rebuild only adds, so {value} must be positive"
            );
            assert_eq!(
                value % 100,
                0,
                "{path}: {value} is not a multiple of COUNT_RESOLUTION"
            );
            assert!(*value < 10_000_000, "{path}: {value} reaches COUNT_MAX");
        }
        if depth_below_index == 0 {
            total_on_index = counters.iter().sum();
        }
    }

    assert!(
        counted_nodes > 0,
        "4000 entries produced no counter at all, so Oak would price this index at -1 \
         and stop choosing it"
    );
    assert!(
        total_on_index > 0,
        "the :index node itself carries no counter, which is the node Oak's cost model reads"
    );
}
