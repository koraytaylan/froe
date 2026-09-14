//! The index inventory over a synthetic store holding one definition of every
//! type.
//!
//! What these pin is the aggregation rather than any one reader: that a
//! definition the model cannot build is *listed* rather than ending the
//! walk, that a store whose non-root definitions cannot be enumerated still
//! lists its root ones with a warning saying so, that each type's own
//! estimate is the one Oak's information provider computes, and that the
//! observed and unobserved spellings return identical inventories.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::PathBuf;

use froe::index::IndexType;
use froe::index::counter::NodeCountEstimate;
use froe::index::inventory::IndexInventory;
use froe::progress::{ProgressObserver, Step};
use froe::store::Repository;
use support::property_index_layout::{Node, Property, write_repository_with_tree};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-index-inventory-{name}-{}-{:?}",
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

/// An observer that records the steps it was told about, so the twin-equality
/// test can also assert that the observed spelling actually reported.
#[derive(Default)]
struct RecordingObserver {
    steps: Vec<(String, Option<u64>)>,
    advances: usize,
}

impl ProgressObserver for RecordingObserver {
    fn step_began(&mut self, step: &Step<'_>) {
        self.steps
            .push((step.description().to_owned(), step.total()));
    }

    fn step_advanced(&mut self, _completed: u64) {
        self.advances += 1;
    }

    fn step_ended(&mut self) {}
}

fn definition(index_type: &str) -> Node {
    Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text(index_type.to_owned()))
}

/// A store with one definition of every type froe models, plus a definition
/// whose `async` names two lanes — which is a refusal at the model level and
/// so must be *listed* rather than ending the walk.
fn every_type_store(name: &str) -> (TestDirectory, Repository) {
    let directory = TestDirectory::new(name);

    let property_index = definition("property")
        .with(
            "propertyNames",
            Property::Names(vec!["jcr:title".to_owned()]),
        )
        .with_child(
            ":index",
            Node::new()
                .with(":count_0daeb465", Property::Long(200))
                .with(":count_65f31f1f", Property::Long(400))
                .with_child("alpha", Node::new().with("match", Property::Boolean(true))),
        );

    let unique_index = definition("property")
        .with(
            "propertyNames",
            Property::Names(vec!["jcr:uuid".to_owned()]),
        )
        .with("unique", Property::Boolean(true))
        .with_child(
            ":index",
            Node::new()
                .with(":count_21b3108d", Property::Long(100))
                .with_child(
                    "an-identifier",
                    Node::new().with("entry", Property::Texts(vec!["/content".to_owned()])),
                ),
        );

    let reference_index = definition("reference");

    let counter_index = definition("counter")
        .with("async", Property::Text("async".to_owned()))
        .with("seed", Property::Long(7))
        .with_child(":index", Node::new().with(":cnt", Property::Long(10_240)));

    // A stored clone that differs from the definition by one visible
    // property, so the drift verdict is true and names where.
    let stored_clone = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("lucene".to_owned()))
        .with("async", Property::Text("async".to_owned()));
    let lucene_index = definition("lucene")
        .with("async", Property::Text("async".to_owned()))
        .with(":version", Property::Long(2))
        .with("queryPaths", Property::Texts(vec!["/content".to_owned()]))
        .with_child(":index-definition", stored_clone)
        .with_child(
            ":status",
            Node::new()
                .with("uid", Property::Text("1750000000000".to_owned()))
                .with(
                    "lastUpdated",
                    Property::Date("2026-09-14T06:24:27.689Z".to_owned()),
                )
                .with("indexedNodes", Property::Long(12)),
        )
        .with_child(
            ":data",
            Node::new()
                .with("dirListing", Property::Texts(vec!["_0.si".to_owned()]))
                .with_child(
                    "_0.si",
                    Node::new()
                        .with("uniqueKey", Property::Text("00".repeat(16)))
                        .with("blobSize", Property::Long(1_047_552))
                        .with("jcr:data", Property::Binary(vec![7u8; 241])),
                ),
        );

    // Two lane names is one of the refusals Oak throws on, so the model
    // cannot be built — and the listing must still show every other index.
    let broken_index = definition("lucene").with(
        "async",
        Property::Texts(vec!["async".to_owned(), "fulltext-async".to_owned()]),
    );

    let async_state = Node::new()
        .with(
            "async",
            Property::Text("95521dd3-c005-4b45-a901-0754c7315904".to_owned()),
        )
        .with(
            "async-LastIndexedTo",
            Property::Date("2026-09-14T06:24:27.689Z".to_owned()),
        );

    let root = Node::new()
        .with_child(
            "oak:index",
            Node::new()
                .with_child("title", property_index)
                .with_child("uuid", unique_index)
                .with_child("reference", reference_index)
                .with_child("counter", counter_index)
                .with_child("lucene", lucene_index)
                .with_child("broken", broken_index),
        )
        .with_child(":async", async_state)
        .with_child(
            "content",
            Node::new().with("jcr:title", Property::Text("alpha".to_owned())),
        );
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open the repository");
    (directory, repository)
}

fn collect(repository: &Repository) -> IndexInventory {
    IndexInventory::collect(repository, &repository.head()).expect("collect the inventory")
}

#[test]
fn the_inventory_lists_every_definition_the_store_holds() {
    let (_directory, repository) = every_type_store("every-type");
    let inventory = collect(&repository);
    let mut paths: Vec<&str> = inventory
        .indexes
        .iter()
        .map(|info| info.path.as_str())
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "/oak:index/broken",
            "/oak:index/counter",
            "/oak:index/lucene",
            "/oak:index/reference",
            "/oak:index/title",
            "/oak:index/uuid",
        ]
    );
}

#[test]
fn a_property_indexs_estimate_is_the_sum_of_its_approximate_counters() {
    let (_directory, repository) = every_type_store("property-estimate");
    let inventory = collect(&repository);
    let title = inventory
        .index_at("/oak:index/title")
        .expect("the property index");
    assert_eq!(title.index_type(), Some(&IndexType::Property));
    assert_eq!(
        title.estimated_entry_count,
        Some(600),
        "max(added / 2, added - removed) with 600 added and nothing removed"
    );
    assert_eq!(title.approximate_counters, 2);
}

#[test]
fn a_unique_indexs_counters_are_the_ones_on_its_index_node_alone() {
    let (_directory, repository) = every_type_store("unique-counters");
    let inventory = collect(&repository);
    let uuid = inventory
        .index_at("/oak:index/uuid")
        .expect("the unique index");
    assert_eq!(
        uuid.approximate_counters, 1,
        "the unique strategy writes its counters on :index alone"
    );
}

#[test]
fn a_reference_definition_has_no_entry_estimate() {
    let (_directory, repository) = every_type_store("reference");
    let inventory = collect(&repository);
    let reference = inventory
        .index_at("/oak:index/reference")
        .expect("the reference index");
    assert_eq!(reference.index_type(), Some(&IndexType::Reference));
    assert_eq!(
        reference.estimated_entry_count, None,
        "Oak's index printer reports nothing but the type for a reference index"
    );
}

#[test]
fn a_counter_definitions_estimate_comes_from_its_cnt_map() {
    let (_directory, repository) = every_type_store("counter-estimate");
    let inventory = collect(&repository);
    let counter = inventory
        .index_at("/oak:index/counter")
        .expect("the counter");
    assert_eq!(
        counter.estimated_node_count,
        Some(NodeCountEstimate::Count(10_240))
    );
}

#[test]
fn a_lucene_definitions_size_and_files_come_from_its_data_directory() {
    let (_directory, repository) = every_type_store("lucene-size");
    let inventory = collect(&repository);
    let lucene = inventory
        .index_at("/oak:index/lucene")
        .expect("the Lucene index");
    assert_eq!(lucene.index_type(), Some(&IndexType::Lucene));
    assert_eq!(
        lucene.size_in_bytes,
        Some(225),
        "241 stored bytes less the 16 key bytes"
    );
    assert_eq!(lucene.lucene_files, [("_0.si".to_owned(), 225)]);
    assert_eq!(
        lucene.document_count, None,
        "this fixture's :data holds a lone .si and no commit file, so there is no count"
    );
    assert_eq!(
        lucene.estimated_entry_count, None,
        "and no count means no estimated entry count either"
    );
    assert!(lucene.suggest_size_in_bytes.is_none());
}

#[test]
fn a_lucene_definitions_timestamps_come_from_its_status_and_its_lane() {
    let (_directory, repository) = every_type_store("lucene-status");
    let inventory = collect(&repository);
    let lucene = inventory
        .index_at("/oak:index/lucene")
        .expect("the Lucene index");
    assert_eq!(
        lucene.last_updated.as_deref(),
        Some("2026-09-14T06:24:27.689Z"),
        "from :status/lastUpdated"
    );
    assert_eq!(
        lucene.indexed_up_to.as_deref(),
        Some("2026-09-14T06:24:27.689Z"),
        "from the lane's own LastIndexedTo, not from the status node"
    );
    assert_eq!(
        lucene.creation_timestamp, None,
        "absent right after a reindex, which is not a defect"
    );
}

#[test]
fn a_drifted_definition_names_where_it_drifted() {
    let (_directory, repository) = every_type_store("drift");
    let inventory = collect(&repository);
    let lucene = inventory
        .index_at("/oak:index/lucene")
        .expect("the Lucene index");
    assert!(
        lucene.definition_changed,
        "queryPaths is on the definition and not in its stored clone"
    );
    assert_eq!(lucene.definition_diff.len(), 1);
    assert_eq!(lucene.definition_diff[0].path, "/queryPaths");
}

#[test]
fn a_definition_the_model_refuses_is_listed_rather_than_ending_the_walk() {
    let (_directory, repository) = every_type_store("broken");
    let inventory = collect(&repository);
    let broken = inventory.index_at("/oak:index/broken").expect("listed");
    assert!(broken.definition.is_none());
    let reason = broken.model_error.as_deref().expect("a reason");
    assert!(
        reason.contains("several asynchronous lanes"),
        "the reason names the condition — {reason}"
    );
    assert!(broken.needs_attention());
    assert_eq!(
        inventory.indexes.len(),
        6,
        "one broken definition must not cost the other five"
    );
}

#[test]
fn a_store_whose_nodetype_index_is_missing_still_lists_its_root_definitions() {
    // The synthetic store has no `/oak:index/nodetype`, so Oak's path
    // service would refuse the whole enumeration. The inventory warns.
    let (_directory, repository) = every_type_store("no-nodetype");
    let inventory = collect(&repository);
    assert_eq!(inventory.indexes.len(), 6);
    assert!(
        inventory.warnings.iter().any(|warning| matches!(
            warning,
            froe::index::IndexWarning::NonRootDefinitionsNotEnumerated { .. }
        )),
        "{:?}",
        inventory.warnings
    );
}

#[test]
fn a_dangling_lane_checkpoint_is_reported_on_every_definition_on_that_lane() {
    let (_directory, repository) = every_type_store("dangling");
    let inventory = collect(&repository);
    assert_eq!(
        inventory.dangling_checkpoints,
        ["95521dd3-c005-4b45-a901-0754c7315904"],
        "the store has no checkpoints, so the lane's resume point is gone"
    );
    for path in ["/oak:index/counter", "/oak:index/lucene"] {
        let info = inventory.index_at(path).expect("listed");
        assert!(
            info.lane_checkpoint_dangling,
            "{path} is on the async lane and must say so"
        );
        assert!(info.needs_attention());
    }
    let title = inventory.index_at("/oak:index/title").expect("listed");
    assert!(
        !title.lane_checkpoint_dangling,
        "a synchronous definition has no lane and cannot have a dangling one"
    );
}

#[test]
fn an_observed_inventory_equals_an_unobserved_one() {
    let (_directory, repository) = every_type_store("observed-twin");
    let without_observer =
        IndexInventory::collect(&repository, &repository.head()).expect("collect");
    let mut observer = RecordingObserver::default();
    let with_observer =
        IndexInventory::collect_with_progress(&repository, &repository.head(), &mut observer)
            .expect("collect with progress");
    assert_eq!(with_observer, without_observer);
    assert_eq!(
        observer.steps,
        [("inventorying indexes".to_owned(), Some(6))],
        "one step, counting the definition nodes it reads"
    );
    assert!(observer.advances >= 6, "the step advanced per definition");
}
