//! The index definition model, against real stores on disk.
//!
//! The unit tests beside the implementation cover the two pure pieces — the
//! path filter's construction and the value pattern's composed rule. These
//! cover the reading: that each property is read at the strictness of the Oak
//! consumer whose verdict it reproduces, that every disagreement between two
//! of Oak's own consumers surfaces as a warning rather than a silent choice,
//! that the two refusals Oak throws on are refusals here, and that
//! `index_paths` takes the branch Oak's path service takes.
//!
//! Every store is written through froe's own writer, because the shapes under
//! test are node shapes rather than byte encodings; the independent-encoder
//! layer belongs to the readers of the storage subtrees, which land with the
//! tasks that read them.

use std::path::{Path, PathBuf};

use froe::content::PropertyType;
use froe::index::definition::{DEFAULT_LUCENE_BLOB_SIZE, IndexType};
use froe::index::status::{DifferenceKind, StatusNode, StoredDefinition, definition_drift};
use froe::index::{AsyncLanes, IndexDefinition, IndexError, IndexWarning, index_paths};
use froe::segment::record::RecordIdentifier;
use froe::store::Repository;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use froe::writer::store_writer::{StoreSink, WritableRepository};

/// A directory that removes itself, so a failing test leaves nothing behind.
struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-index-definition-{name}-{}-{:?}",
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

/// One property to write, described the way a test reads best: a name, a
/// type, and either one value or several.
struct WrittenProperty<'text> {
    name: &'text str,
    property_type: PropertyType,
    values: Vec<&'text str>,
    multi_valued: bool,
}

fn single<'text>(
    name: &'text str,
    property_type: PropertyType,
    value: &'text str,
) -> WrittenProperty<'text> {
    WrittenProperty {
        name,
        property_type,
        values: vec![value],
        multi_valued: false,
    }
}

fn multiple<'text>(
    name: &'text str,
    property_type: PropertyType,
    values: &[&'text str],
) -> WrittenProperty<'text> {
    WrittenProperty {
        name,
        property_type,
        values: values.to_vec(),
        multi_valued: true,
    }
}

fn write_properties(
    writer: &mut RecordWriter<impl SegmentSink>,
    properties: &[WrittenProperty<'_>],
) -> Vec<PropertyToWrite> {
    properties
        .iter()
        .map(|property| {
            let identifiers: Vec<RecordIdentifier> = property
                .values
                .iter()
                .map(|value| writer.write_string(value).expect("write a property value"))
                .collect();
            PropertyToWrite {
                name: property.name.to_owned(),
                property_type: property.property_type,
                values: if property.multi_valued {
                    PropertyValuesToWrite::Multiple(identifiers)
                } else {
                    PropertyValuesToWrite::Single(identifiers[0])
                },
            }
        })
        .collect()
}

/// Writes a node with the given properties and children.
fn write_node(
    writer: &mut RecordWriter<impl SegmentSink>,
    properties: &[WrittenProperty<'_>],
    children: Vec<(String, RecordIdentifier)>,
) -> RecordIdentifier {
    let written = write_properties(writer, properties);
    let child_nodes = match children.len() {
        0 => ChildNodesToWrite::Zero,
        1 => {
            let (name, node) = children.into_iter().next().expect("one child");
            ChildNodesToWrite::One { name, node }
        }
        _ => ChildNodesToWrite::Many(children),
    };
    writer
        .write_node(None, &[], &child_nodes, &written)
        .expect("write a node")
}

/// Writes the content root and super-root over `root_children`, publishes
/// the head, and closes the store. Every fixture below ends this way.
fn publish_root(
    store: &WritableRepository,
    mut writer: RecordWriter<StoreSink<'_>>,
    root_children: Vec<(String, RecordIdentifier)>,
) {
    let root = writer
        .write_node(
            Some("rep:root"),
            &[],
            &ChildNodesToWrite::Many(root_children),
            &[],
        )
        .expect("write the content root");
    let super_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
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

/// Publishes a store whose `/oak:index` holds whatever `build` writes.
fn publish(
    directory: &Path,
    build: impl FnOnce(&mut RecordWriter<StoreSink<'_>>) -> Vec<(String, RecordIdentifier)>,
) {
    let store = WritableRepository::open(directory).expect("open the store directory");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);
    let definitions = build(&mut writer);
    let oak_index = write_node(&mut writer, &[], definitions);
    publish_root(&store, writer, vec![("oak:index".to_owned(), oak_index)]);
    store.close().expect("close the store");
}

/// Publishes a store that also carries the `/jcr:system/jcr:nodeTypes` entry
/// the printer's second branch reads its type sets from.
///
/// `oak:QueryIndexDefinition` has no subtypes in a stock store, which is why
/// `rep:primarySubtypes` and `rep:mixinSubtypes` are absent here: the selector
/// then looks up exactly one key, its own name.
fn publish_with_node_types(
    directory: &Path,
    build: impl FnOnce(&mut RecordWriter<StoreSink<'_>>) -> Vec<(String, RecordIdentifier)>,
) {
    let store = WritableRepository::open(directory).expect("open the store directory");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);
    let definitions = build(&mut writer);
    let oak_index = write_node(&mut writer, &[], definitions);
    let definition_type = write_node(
        &mut writer,
        &[
            single(
                "jcr:nodeTypeName",
                PropertyType::Name,
                "oak:QueryIndexDefinition",
            ),
            multiple("rep:supertypes", PropertyType::Name, &["nt:base"]),
            single("jcr:isMixin", PropertyType::Boolean, "false"),
        ],
        Vec::new(),
    );
    let node_types = write_node(
        &mut writer,
        &[],
        vec![("oak:QueryIndexDefinition".to_owned(), definition_type)],
    );
    let system = write_node(
        &mut writer,
        &[],
        vec![("jcr:nodeTypes".to_owned(), node_types)],
    );
    publish_root(
        &store,
        writer,
        vec![
            ("oak:index".to_owned(), oak_index),
            ("jcr:system".to_owned(), system),
        ],
    );
    store.close().expect("close the store");
}

/// The common case: one definition under `/oak:index`, read back.
fn read_one_definition(
    name: &str,
    properties: &[WrittenProperty<'_>],
    children: Vec<(&str, Vec<WrittenProperty<'_>>)>,
) -> (TestDirectory, Result<IndexDefinition, IndexError>) {
    let directory = TestDirectory::new(name);
    let definition_name = name.to_owned();
    publish(&directory.path, |writer| {
        let child_nodes: Vec<(String, RecordIdentifier)> = children
            .into_iter()
            .map(|(child_name, child_properties)| {
                (
                    child_name.to_owned(),
                    write_node(writer, &child_properties, Vec::new()),
                )
            })
            .collect();
        let definition = write_node(writer, properties, child_nodes);
        vec![(definition_name, definition)]
    });
    let repository = Repository::open(&directory.path).expect("open the repository");
    let node = repository
        .node_at_path(&format!("/oak:index/{name}"))
        .expect("resolve the definition")
        .expect("the definition exists");
    let definition = IndexDefinition::read(&node, &format!("/oak:index/{name}"));
    (directory, definition)
}

/// The properties Oak's own initial content writes on a definition, which is
/// the baseline every malformed case below deviates from by one property.
fn well_formed(index_type: &'static str) -> Vec<WrittenProperty<'static>> {
    vec![
        single(
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        ),
        single("type", PropertyType::String, index_type),
    ]
}

#[test]
fn a_well_formed_property_definition_reads_as_one() {
    let (_directory, definition) = read_one_definition(
        "well-formed",
        &[
            single(
                "jcr:primaryType",
                PropertyType::Name,
                "oak:QueryIndexDefinition",
            ),
            single("type", PropertyType::String, "property"),
            multiple("propertyNames", PropertyType::Name, &["jcr:uuid"]),
            single("unique", PropertyType::Boolean, "true"),
            single("reindex", PropertyType::Boolean, "false"),
            single("reindexCount", PropertyType::Long, "1"),
        ],
        Vec::new(),
    );
    let definition = definition.expect("a well-formed definition reads");
    assert_eq!(definition.index_type, Some(IndexType::Property));
    assert_eq!(definition.property.property_names, ["jcr:uuid"]);
    assert!(definition.property.unique);
    assert_eq!(definition.reindex.count, 1);
    assert!(!definition.reindex.flagged);
    assert!(definition.warnings.is_empty(), "{:?}", definition.warnings);
    assert!(definition.is_maintained_by_oak());
}

#[test]
fn a_multi_valued_type_is_reported_as_ignored_by_oak() {
    let (_directory, definition) = read_one_definition(
        "array-type",
        &[
            single(
                "jcr:primaryType",
                PropertyType::Name,
                "oak:QueryIndexDefinition",
            ),
            multiple("type", PropertyType::String, &["property"]),
        ],
        Vec::new(),
    );
    let definition = definition.expect("a listing still succeeds");
    assert_eq!(definition.index_type, None);
    assert!(!definition.is_maintained_by_oak());
    assert!(
        definition
            .warnings
            .iter()
            .any(|warning| matches!(warning, IndexWarning::IgnoredByOak { .. })),
        "{:?}",
        definition.warnings
    );
}

#[test]
fn a_name_typed_type_is_reported_as_ignored_by_oak() {
    let (_directory, definition) = read_one_definition(
        "name-type",
        &[
            single(
                "jcr:primaryType",
                PropertyType::Name,
                "oak:QueryIndexDefinition",
            ),
            single("type", PropertyType::Name, "property"),
        ],
        Vec::new(),
    );
    let definition = definition.expect("a listing still succeeds");
    assert_eq!(definition.index_type, None);
    assert!(!definition.is_maintained_by_oak());
}

#[test]
fn an_async_property_naming_no_lane_is_refused() {
    let mut properties = well_formed("lucene");
    properties.push(multiple("async", PropertyType::String, &["sync", "nrt"]));
    let (_directory, definition) = read_one_definition("no-lane", &properties, Vec::new());
    let error = definition.expect_err("Oak throws here, so froe refuses");
    assert!(
        matches!(&error, IndexError::NoLaneName { definition_path } if definition_path == "/oak:index/no-lane"),
        "{error}"
    );
}

#[test]
fn an_async_property_naming_two_lanes_is_refused_by_name() {
    let mut properties = well_formed("lucene");
    properties.push(multiple(
        "async",
        PropertyType::String,
        &["async", "fulltext-async"],
    ));
    let (_directory, definition) = read_one_definition("two-lanes", &properties, Vec::new());
    let error = definition.expect_err("Oak throws here, so froe refuses");
    assert!(
        matches!(
            &error,
            IndexError::SeveralLaneNames { lane_names, .. }
                if lane_names == &["async".to_owned(), "fulltext-async".to_owned()]
        ),
        "{error}"
    );
}

#[test]
fn a_lane_name_beside_the_two_synonyms_resolves_and_records_both_modes() {
    let mut properties = well_formed("lucene");
    properties.push(multiple(
        "async",
        PropertyType::String,
        &["async", "sync", "nrt"],
    ));
    let (_directory, definition) = read_one_definition("hybrid", &properties, Vec::new());
    let definition = definition.expect("one lane name resolves");
    assert_eq!(definition.lane.as_deref(), Some("async"));
    assert!(!definition.indexing_mode.synchronous);
    assert!(definition.indexing_mode.synchronous_synonym);
    assert!(definition.indexing_mode.near_real_time);
}

#[test]
fn a_definition_without_an_async_property_is_synchronous() {
    let (_directory, definition) =
        read_one_definition("synchronous", &well_formed("property"), Vec::new());
    let definition = definition.expect("a synchronous definition reads");
    assert_eq!(definition.lane, None);
    assert!(definition.indexing_mode.synchronous);
}

#[test]
fn a_string_property_names_is_indexed_but_never_selected() {
    let mut properties = well_formed("property");
    properties.push(multiple("propertyNames", PropertyType::String, &["foo"]));
    let (_directory, definition) = read_one_definition("string-names", &properties, Vec::new());
    let definition = definition.expect("the editor converts it, so froe reads it");
    assert_eq!(definition.property.property_names, ["foo"]);
    assert!(
        definition.warnings.iter().any(|warning| matches!(
            warning,
            IndexWarning::IndexedButNeverSelected { property_name, .. }
                if property_name == "propertyNames"
        )),
        "{:?}",
        definition.warnings
    );
}

#[test]
fn a_single_name_property_names_is_indexed_but_never_selected() {
    let mut properties = well_formed("property");
    properties.push(single("propertyNames", PropertyType::Name, "foo"));
    let (_directory, definition) = read_one_definition("single-name", &properties, Vec::new());
    let definition = definition.expect("the editor converts a single NAME too");
    assert_eq!(definition.property.property_names, ["foo"]);
    assert!(
        definition.warnings.iter().any(|warning| matches!(
            warning,
            IndexWarning::IndexedButNeverSelected { property_name, .. }
                if property_name == "propertyNames"
        )),
        "a single NAME reads as empty at the planner, which reads NAMES"
    );
}

#[test]
fn a_non_names_declaring_node_types_matches_nothing_and_says_so() {
    let mut properties = well_formed("property");
    properties.push(multiple(
        "declaringNodeTypes",
        PropertyType::String,
        &["nt:file"],
    ));
    let (_directory, definition) = read_one_definition("string-types", &properties, Vec::new());
    let definition = definition.expect("a listing still succeeds");
    assert!(
        definition.property.declaring_node_types.is_empty(),
        "a non-NAMES declaration yields a predicate matching nothing"
    );
    assert!(
        definition
            .warnings
            .iter()
            .any(|warning| matches!(warning, IndexWarning::DeclaringNodeTypesMatchNothing { .. })),
        "{:?}",
        definition.warnings
    );
}

#[test]
fn a_string_unique_reads_as_a_mirror_index() {
    let mut properties = well_formed("property");
    properties.push(single("unique", PropertyType::String, "true"));
    let (_directory, definition) = read_one_definition("string-unique", &properties, Vec::new());
    let definition = definition.expect("a listing still succeeds");
    assert!(
        !definition.property.unique,
        "Oak reads unique strictly, so a String \"true\" is a mirror index"
    );
    assert!(
        definition
            .warnings
            .iter()
            .any(|warning| matches!(warning, IndexWarning::UniqueIsNotBoolean { .. })),
        "{:?}",
        definition.warnings
    );
}

#[test]
fn a_single_non_string_value_prefix_is_refused() {
    let mut properties = well_formed("property");
    properties.push(single("valueIncludedPrefixes", PropertyType::Name, "abc"));
    let (_directory, definition) = read_one_definition("bad-prefix", &properties, Vec::new());
    let error = definition.expect_err("Oak cannot index this definition at all");
    assert!(
        matches!(
            &error,
            IndexError::NonStringPrefixValue { property_name, .. }
                if property_name == "valueIncludedPrefixes"
        ),
        "{error}"
    );
}

#[test]
fn a_relative_included_path_is_refused() {
    let mut properties = well_formed("property");
    properties.push(multiple(
        "includedPaths",
        PropertyType::String,
        &["content"],
    ));
    let (_directory, definition) = read_one_definition("relative-path", &properties, Vec::new());
    let error = definition.expect_err("Oak's path filter refuses a relative path");
    assert!(
        matches!(&error, IndexError::RelativeFilterPath { value, .. } if value == "content"),
        "{error}"
    );
}

#[test]
fn the_counter_definitions_seed_and_resolution_read_converting() {
    let mut properties = well_formed("counter");
    properties.push(single("seed", PropertyType::Long, "-7610761686379641542"));
    properties.push(single("resolution", PropertyType::String, "500"));
    let (_directory, definition) = read_one_definition("counter", &properties, Vec::new());
    let definition = definition.expect("a counter definition reads");
    assert_eq!(definition.index_type, Some(IndexType::Counter));
    assert_eq!(definition.seed, Some(-7_610_761_686_379_641_542));
    assert_eq!(
        definition.resolution,
        Some(500),
        "resolution is read converting, so a String counts"
    );
}

#[test]
fn a_lucene_definition_reads_its_version_status_and_stored_definition() {
    let directory = TestDirectory::new("lucene");
    publish(&directory.path, |writer| {
        let status = write_node(
            writer,
            &[
                single("uid", PropertyType::String, "1750000000000"),
                single(
                    "lastUpdated",
                    PropertyType::Date,
                    "2026-09-14T06:24:27.689Z",
                ),
                single("indexedNodes", PropertyType::Long, "12"),
            ],
            Vec::new(),
        );
        let stored = write_node(
            writer,
            &[single("type", PropertyType::String, "lucene")],
            Vec::new(),
        );
        let data = write_node(writer, &[], Vec::new());
        let definition = write_node(
            writer,
            &[
                single(
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single("type", PropertyType::String, "lucene"),
                single("async", PropertyType::String, "async"),
                single(":version", PropertyType::Long, "2"),
            ],
            vec![
                (":status".to_owned(), status),
                (":index-definition".to_owned(), stored),
                (":data".to_owned(), data),
            ],
        );
        vec![("lucene".to_owned(), definition)]
    });
    let repository = Repository::open(&directory.path).expect("open the repository");
    let node = repository
        .node_at_path("/oak:index/lucene")
        .expect("resolve")
        .expect("exists");
    let definition =
        IndexDefinition::read(&node, "/oak:index/lucene").expect("a Lucene definition reads");
    assert_eq!(definition.index_type, Some(IndexType::Lucene));
    assert_eq!(definition.lane.as_deref(), Some("async"));
    assert_eq!(definition.lucene.format_version, Some(2));
    assert_eq!(
        definition.lucene.blob_size, DEFAULT_LUCENE_BLOB_SIZE,
        "an absent blobSize takes the definition default, not the reader's"
    );
    assert!(definition.lucene.save_directory_listing);
    assert!(definition.has_hidden_child(":data"));
    assert!(!definition.has_visible_child(":data"));

    let status = StatusNode::read(&node)
        .expect("read the status node")
        .expect("the status node exists");
    assert_eq!(status.unique_identifier.as_deref(), Some("1750000000000"));
    assert_eq!(status.indexed_nodes, Some(12));
    assert_eq!(status.reindex_completion_timestamp, None);

    let stored = StoredDefinition::read(&node)
        .expect("read the stored definition")
        .expect("the stored definition exists");
    assert_eq!(
        stored.creation_timestamp, None,
        "creationTimestamp is absent right after a reindex"
    );
}

#[test]
fn definition_drift_ignores_the_reindex_bookkeeping_and_hidden_properties() {
    let directory = TestDirectory::new("drift");
    publish(&directory.path, |writer| {
        // The stored clone: the definition as it was, without the later
        // `queryPaths`, and with the pre-reindex reindex bookkeeping.
        let stored_child = write_node(
            writer,
            &[
                single("propertyIndex", PropertyType::Boolean, "true"),
                single(":childOrder", PropertyType::Name, "ignored"),
            ],
            Vec::new(),
        );
        // The clone Oak writes is of the builder's *base* state, so it
        // carries the pre-reindex `reindex = true` and the old count, and
        // every visible property the definition had at that moment.
        let stored = write_node(
            writer,
            &[
                single(
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single("type", PropertyType::String, "lucene"),
                single("async", PropertyType::String, "async"),
                single("reindex", PropertyType::Boolean, "true"),
                single("reindexCount", PropertyType::Long, "0"),
            ],
            vec![("rules".to_owned(), stored_child)],
        );
        let current_child = write_node(
            writer,
            &[
                single("propertyIndex", PropertyType::Boolean, "true"),
                single(":childOrder", PropertyType::Name, "different"),
            ],
            Vec::new(),
        );
        let definition = write_node(
            writer,
            &[
                single(
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single("type", PropertyType::String, "lucene"),
                single("async", PropertyType::String, "async"),
                single("reindex", PropertyType::Boolean, "false"),
                single("reindexCount", PropertyType::Long, "1"),
                multiple("queryPaths", PropertyType::String, &["/content"]),
                single(":version", PropertyType::Long, "2"),
            ],
            vec![
                (":index-definition".to_owned(), stored),
                ("rules".to_owned(), current_child),
            ],
        );
        vec![("drifted".to_owned(), definition)]
    });
    let repository = Repository::open(&directory.path).expect("open the repository");
    let node = repository
        .node_at_path("/oak:index/drifted")
        .expect("resolve")
        .expect("exists");
    let stored = node
        .child_node(":index-definition")
        .expect("read the stored clone")
        .expect("the stored clone exists");
    let differences = definition_drift(&node, &stored, &[]).expect("compare");
    assert_eq!(
        differences.len(),
        1,
        "only queryPaths differs; reindex, reindexCount, :version and the child's \
         :childOrder are all ignored — {differences:?}"
    );
    assert_eq!(differences[0].path, "/queryPaths");
    assert_eq!(differences[0].kind, DifferenceKind::Added);
}

#[test]
fn definition_drift_reports_a_visible_child_added_or_removed() {
    let directory = TestDirectory::new("drift-children");
    publish(&directory.path, |writer| {
        let stored_only = write_node(writer, &[], Vec::new());
        let stored = write_node(
            writer,
            &[
                single(
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single("type", PropertyType::String, "lucene"),
                single("async", PropertyType::String, "async"),
            ],
            vec![("gone".to_owned(), stored_only)],
        );
        let added = write_node(writer, &[], Vec::new());
        let definition = write_node(
            writer,
            &[
                single(
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single("type", PropertyType::String, "lucene"),
                single("async", PropertyType::String, "async"),
            ],
            vec![
                (":index-definition".to_owned(), stored),
                ("fresh".to_owned(), added),
            ],
        );
        vec![("children".to_owned(), definition)]
    });
    let repository = Repository::open(&directory.path).expect("open the repository");
    let node = repository
        .node_at_path("/oak:index/children")
        .expect("resolve")
        .expect("exists");
    let stored = node
        .child_node(":index-definition")
        .expect("read")
        .expect("exists");
    let differences = definition_drift(&node, &stored, &[]).expect("compare");
    let rendered: Vec<(String, DifferenceKind)> = differences
        .iter()
        .map(|difference| (difference.path.clone(), difference.kind))
        .collect();
    assert_eq!(
        rendered,
        [
            ("/fresh".to_owned(), DifferenceKind::Added),
            ("/gone".to_owned(), DifferenceKind::Removed),
        ]
    );
}

#[test]
fn the_lanes_of_the_fixtures_async_node_read_back_field_by_field() {
    let directory = TestDirectory::new("lanes");
    // A second publish would be simpler, but the helper writes `/oak:index`
    // only; write the whole store here instead so `/:async` is a sibling.
    let store = WritableRepository::open(&directory.path).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let async_state = write_node(
        &mut writer,
        &[
            single(
                "async",
                PropertyType::String,
                "95521dd3-c005-4b45-a901-0754c7315904",
            ),
            multiple(
                "async-temp",
                PropertyType::String,
                &[
                    "f369be42-c8ac-44d3-a964-a9820e53e2b7",
                    "95521dd3-c005-4b45-a901-0754c7315904",
                ],
            ),
            single(
                "async-LastIndexedTo",
                PropertyType::Date,
                "2026-09-14T06:24:27.689Z",
            ),
            single("fulltext-async-lease", PropertyType::Long, "1750000000000"),
        ],
        Vec::new(),
    );
    let root = writer
        .write_node(
            Some("rep:root"),
            &[],
            &ChildNodesToWrite::One {
                name: ":async".to_owned(),
                node: async_state,
            },
            &[],
        )
        .expect("write the content root");
    let super_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &[],
        )
        .expect("write the super-root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, super_root), "advance");
    store.close().expect("close the store");

    let repository = Repository::open(&directory.path).expect("open the repository");
    let content_root = repository.content_root().expect("content root");
    let lanes = AsyncLanes::read(&content_root).expect("read /:async");
    assert!(lanes.node_present());

    let default_lane = lanes.lane("async").expect("the async lane");
    assert_eq!(
        default_lane.checkpoint.as_deref(),
        Some("95521dd3-c005-4b45-a901-0754c7315904")
    );
    assert_eq!(default_lane.pending_release.len(), 2);
    assert_eq!(
        default_lane.last_indexed_to.as_deref(),
        Some("2026-09-14T06:24:27.689Z")
    );
    assert_eq!(default_lane.lease_expiry, None);

    let fulltext = lanes
        .lane("fulltext-async")
        .expect("a hyphenated lane name is not split on its first hyphen");
    assert_eq!(fulltext.lease_expiry, Some(1_750_000_000_000));
    assert_eq!(fulltext.checkpoint, None);

    let dangling = AsyncLanes::dangling_checkpoints(&content_root, &repository.head())
        .expect("resolve the lane checkpoints");
    assert_eq!(
        dangling,
        ["95521dd3-c005-4b45-a901-0754c7315904"],
        "the store has no checkpoints, and the -temp list is excused"
    );
}

#[test]
fn index_paths_takes_the_root_branch_when_the_nodetype_index_declares_nothing() {
    let directory = TestDirectory::new("paths-root-branch");
    publish(&directory.path, |writer| {
        let entries = write_node(writer, &[], Vec::new());
        let node_type = write_node(
            writer,
            &[
                single(
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single("type", PropertyType::String, "property"),
                multiple(
                    "propertyNames",
                    PropertyType::Name,
                    &["jcr:primaryType", "jcr:mixinTypes"],
                ),
            ],
            vec![(":index".to_owned(), entries)],
        );
        let uuid = write_node(writer, &well_formed("property"), Vec::new());
        let not_a_definition = write_node(
            writer,
            &[single(
                "jcr:primaryType",
                PropertyType::Name,
                "nt:unstructured",
            )],
            Vec::new(),
        );
        vec![
            ("uuid".to_owned(), uuid),
            ("nodetype".to_owned(), node_type),
            ("stray".to_owned(), not_a_definition),
        ]
    });
    let repository = Repository::open(&directory.path).expect("open the repository");
    let content_root = repository.content_root().expect("content root");
    let mut paths = index_paths(&content_root).expect("enumerate index paths");
    paths.sort();
    assert_eq!(
        paths,
        ["/oak:index/nodetype", "/oak:index/uuid"],
        "the stray node, whose jcr:primaryType is not the definition type, is filtered out"
    );
}

#[test]
fn index_paths_refuses_when_the_nodetype_index_is_disabled() {
    let directory = TestDirectory::new("paths-disabled");
    publish(&directory.path, |writer| {
        let node_type = write_node(
            writer,
            &[
                single(
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single("type", PropertyType::String, "disabled"),
            ],
            Vec::new(),
        );
        vec![("nodetype".to_owned(), node_type)]
    });
    let repository = Repository::open(&directory.path).expect("open the repository");
    let content_root = repository.content_root().expect("content root");
    let error = index_paths(&content_root).expect_err("Oak's path service throws here");
    assert!(
        matches!(error, IndexError::NodeTypeIndexUnusable { .. }),
        "{error}"
    );
}

#[test]
fn index_paths_refuses_when_the_nodetype_index_is_absent() {
    let directory = TestDirectory::new("paths-absent");
    publish(&directory.path, |writer| {
        let uuid = write_node(writer, &well_formed("property"), Vec::new());
        vec![("uuid".to_owned(), uuid)]
    });
    let repository = Repository::open(&directory.path).expect("open the repository");
    let content_root = repository.content_root().expect("content root");
    let error = index_paths(&content_root).expect_err("an absent nodetype index refuses too");
    assert!(
        matches!(error, IndexError::NodeTypeIndexUnusable { .. }),
        "{error}"
    );
}

#[test]
fn index_paths_takes_the_mirror_branch_when_the_nodetype_index_declares_definitions() {
    let directory = TestDirectory::new("paths-mirror-branch");
    publish_with_node_types(&directory.path, |writer| {
        // The mirror entry for `/content/oak:index/custom`, under the key the
        // URL encoding of `oak:QueryIndexDefinition` produces.
        let leaf = write_node(
            writer,
            &[single("match", PropertyType::Boolean, "true")],
            Vec::new(),
        );
        let oak_index_level = write_node(writer, &[], vec![("custom".to_owned(), leaf)]);
        let content_level = write_node(
            writer,
            &[],
            vec![("oak%3Aindex".to_owned(), oak_index_level)],
        );
        let key = write_node(writer, &[], vec![("content".to_owned(), content_level)]);
        let entries = write_node(
            writer,
            &[],
            vec![("oak%3AQueryIndexDefinition".to_owned(), key)],
        );
        let node_type = write_node(
            writer,
            &[
                single(
                    "jcr:primaryType",
                    PropertyType::Name,
                    "oak:QueryIndexDefinition",
                ),
                single("type", PropertyType::String, "property"),
                multiple(
                    "propertyNames",
                    PropertyType::Name,
                    &["jcr:primaryType", "jcr:mixinTypes"],
                ),
                multiple(
                    "declaringNodeTypes",
                    PropertyType::Name,
                    &["oak:QueryIndexDefinition"],
                ),
            ],
            vec![(":index".to_owned(), entries)],
        );
        vec![("nodetype".to_owned(), node_type)]
    });
    let repository = Repository::open(&directory.path).expect("open the repository");
    let content_root = repository.content_root().expect("content root");
    let paths = index_paths(&content_root).expect("enumerate index paths");
    assert_eq!(
        paths,
        ["/content/oak%3Aindex/custom"],
        "the mirror walk yields the paths the index recorded, de-duplicated across \
         the two property queries"
    );
}
