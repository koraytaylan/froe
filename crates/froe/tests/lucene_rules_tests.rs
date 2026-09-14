//! Oak's indexing rules and property definitions, over synthetic
//! definitions written through froe's own writer.
//!
//! The shapes under test are node shapes rather than byte encodings, so
//! each case publishes a small store and reads the definition back the
//! way the reindex will. `docs/analysis/lucene-oak-documents.md` §1 and
//! §2 are the specification these confirm; the name-pattern half lives in
//! `lucene_name_pattern_tests.rs`, where Java's own engine is the oracle.

use std::path::{Path, PathBuf};

use froe::content::PropertyType;
use froe::index::lucene::documents::aggregate::Match;
use froe::index::lucene::documents::name_pattern::ALL_PROPERTIES;
use froe::index::lucene::documents::rules::{CodecVerdict, IndexingRules, PropertyDefinition};
use froe::index::{IndexError, IndexWarning};
use froe::segment::record::RecordIdentifier;
use froe::store::Repository;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use froe::writer::store_writer::WritableRepository;

/// A directory that removes itself, so a failing test leaves nothing
/// behind.
struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-lucene-rules-{name}-{}-{:?}",
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

/// A node to write: its properties, each a name, a type and one value, and
/// its children.
#[derive(Clone)]
struct Node {
    properties: Vec<(&'static str, PropertyType, String)>,
    multiple: Vec<(&'static str, PropertyType, Vec<String>)>,
    children: Vec<(String, Node)>,
}

impl Node {
    fn new() -> Self {
        Self {
            properties: Vec::new(),
            multiple: Vec::new(),
            children: Vec::new(),
        }
    }

    fn with(mut self, name: &'static str, property_type: PropertyType, value: &str) -> Self {
        self.properties
            .push((name, property_type, value.to_owned()));
        self
    }

    fn boolean(self, name: &'static str, value: bool) -> Self {
        self.with(
            name,
            PropertyType::Boolean,
            if value { "true" } else { "false" },
        )
    }

    fn string(self, name: &'static str, value: &str) -> Self {
        self.with(name, PropertyType::String, value)
    }

    fn long(self, name: &'static str, value: i64) -> Self {
        self.with(name, PropertyType::Long, &value.to_string())
    }

    /// A multi-valued `NAMES` property, which the node-type subtype lists
    /// are.
    fn names(mut self, name: &'static str, values: &[&str]) -> Self {
        self.multiple.push((
            name,
            PropertyType::Name,
            values.iter().map(|value| (*value).to_owned()).collect(),
        ));
        self
    }

    fn child(mut self, name: &str, node: Node) -> Self {
        self.children.push((name.to_owned(), node));
        self
    }

    /// The same, by value, for a caller that holds the node already.
    fn clone_with_child(&self, name: &str, node: Node) -> Self {
        self.clone().child(name, node)
    }
}

fn write_tree(writer: &mut RecordWriter<impl SegmentSink>, node: &Node) -> RecordIdentifier {
    let children: Vec<(String, RecordIdentifier)> = node
        .children
        .iter()
        .map(|(name, child)| (name.clone(), write_tree(writer, child)))
        .collect();
    let mut properties: Vec<PropertyToWrite> = node
        .properties
        .iter()
        .map(|(name, property_type, value)| PropertyToWrite {
            name: (*name).to_owned(),
            property_type: *property_type,
            values: PropertyValuesToWrite::Single(
                writer.write_string(value).expect("write a property value"),
            ),
        })
        .collect();
    for (name, property_type, values) in &node.multiple {
        let identifiers = values
            .iter()
            .map(|value| writer.write_string(value).expect("write a property value"))
            .collect();
        properties.push(PropertyToWrite {
            name: (*name).to_owned(),
            property_type: *property_type,
            values: PropertyValuesToWrite::Multiple(identifiers),
        });
    }
    let child_nodes = match children.len() {
        0 => ChildNodesToWrite::Zero,
        1 => {
            let (name, node) = children.into_iter().next().expect("one child");
            ChildNodesToWrite::One { name, node }
        }
        _ => ChildNodesToWrite::Many(children),
    };
    writer
        .write_node(None, &[], &child_nodes, &properties)
        .expect("write a node")
}

/// Publishes a store holding `/oak:index/test` and, when one is given,
/// `/jcr:system/jcr:nodeTypes`.
fn publish(directory: &Path, definition: &Node, node_types: Option<&Node>) {
    let store = WritableRepository::open(directory).expect("open the store directory");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);
    let written = write_tree(&mut writer, definition);
    let index = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "test".to_owned(),
                node: written,
            },
            &[],
        )
        .expect("write /oak:index");
    let mut root_children = vec![("oak:index".to_owned(), index)];
    if let Some(node_types) = node_types {
        let types = write_tree(&mut writer, node_types);
        let system = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "jcr:nodeTypes".to_owned(),
                    node: types,
                },
                &[],
            )
            .expect("write /jcr:system");
        root_children.push(("jcr:system".to_owned(), system));
    }
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
    store.close().expect("close the store");
}

/// Reads one definition's rules, with the store kept alive by the caller.
fn read_rules(
    name: &str,
    definition: &Node,
    node_types: Option<&Node>,
) -> (TestDirectory, Result<IndexingRules, IndexError>) {
    let directory = TestDirectory::new(name);
    publish(&directory.path, definition, node_types);
    let repository = Repository::open(&directory.path).expect("open the repository");
    let root = repository.content_root().expect("the content root");
    let node = repository
        .node_at_path("/oak:index/test")
        .expect("resolve the definition")
        .expect("the definition exists");
    let mut warnings: Vec<IndexWarning> = Vec::new();
    let rules = IndexingRules::read(&node, "/oak:index/test", &root, &mut warnings);
    (directory, rules)
}

/// A definition shaped like the fixture's default: one `nt:base` rule with
/// a catch-all analyzed property definition, which is what makes it
/// fulltext-enabled and therefore an `oakCodec` index.
fn default_definition() -> Node {
    Node::new()
        .with(
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        )
        .string("type", "lucene")
        .boolean("evaluatePathRestrictions", true)
        .child(
            "indexRules",
            Node::new().child(
                "nt:base",
                Node::new().child(
                    "properties",
                    Node::new().child(
                        "all",
                        Node::new()
                            .string("name", ALL_PROPERTIES)
                            .boolean("isRegexp", true)
                            .boolean("analyzed", true)
                            .boolean("nodeScopeIndex", true),
                    ),
                ),
            ),
        )
}

#[test]
fn the_default_shape_reads_as_an_oak_codec_definition() {
    let (_directory, rules) = read_rules("default", &default_definition(), None);
    let rules = rules.expect("the definition reads");
    assert_eq!(rules.codec, CodecVerdict::OakCodec);
    assert!(rules.evaluate_path_restrictions);
    assert_eq!(rules.rules.len(), 1);
    let rule = &rules.rules[0];
    assert!(rule.fulltext_enabled);
    assert!(rule.inherited, "a rule's `inherited` defaults to true");
    // The catch-all is a pattern, not an exact name, and a hidden name
    // reaches it — the bug compatibility Oak's own comment records.
    assert!(rule.config_of("jcr:title").is_some());
    assert!(rule.config_of(":nodeName").is_some());
    assert!(rule.config_of("jcr:content/jcr:title").is_none());
}

#[test]
fn a_definition_that_is_not_fulltext_enabled_is_refused_by_name() {
    let mut definition = default_definition();
    definition.children[0].1.children[0].1.children[0]
        .1
        .children[0]
        .1 = Node::new()
        .string("name", "jcr:title")
        .boolean("propertyIndex", true);
    let (_directory, rules) = read_rules("not-fulltext", &definition, None);
    let Err(refusal) = rules else {
        panic!("a definition without a fulltext-enabled property is refused");
    };
    assert!(refusal.to_string().contains("Lucene46"), "{refusal}");
}

#[test]
fn an_explicit_codec_is_refused_by_name() {
    let definition = default_definition().string("codec", "Lucene46");
    let (_directory, rules) = read_rules("explicit-codec", &definition, None);
    let Err(refusal) = rules else {
        panic!("an explicit codec is refused");
    };
    assert!(refusal.to_string().contains("Lucene46"), "{refusal}");
}

/// A rule whose property definitions are written by a closure over one
/// `properties` node, so a case below states only what it changes.
fn definition_with_properties(properties: Node) -> Node {
    Node::new()
        .with(
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        )
        .string("type", "lucene")
        .child(
            "indexRules",
            Node::new().child("nt:base", Node::new().child("properties", properties)),
        )
}

/// One analyzed property definition, which is what keeps a definition
/// fulltext-enabled while a case exercises something else.
fn analyzed_property() -> Node {
    Node::new()
        .string("name", "jcr:title")
        .boolean("analyzed", true)
}

#[test]
fn a_name_resolves_case_insensitively_and_a_duplicate_leaves_one() {
    // One child is named for the property; the other names it through its
    // `name` property, in another case. Oak guards on the **child's** name
    // against a map keyed by each definition's `name`, and puts on the
    // definition's name — so whichever order the node state yields them
    // in, exactly one survives and it is the one named `other`: reached
    // first it is overwritten by nothing, reached second it overwrites.
    let definition = definition_with_properties(
        Node::new()
            .child("analyzed", analyzed_property())
            .child("sling:resourceType", Node::new().boolean("ordered", true))
            .child(
                "other",
                Node::new()
                    .string("name", "SLING:RESOURCETYPE")
                    .boolean("nodeScopeIndex", true),
            ),
    );
    let (_directory, rules) = read_rules("case", &definition, None);
    let rules = rules.expect("the definition reads");
    let rule = &rules.rules[0];
    let found = rule
        .config_of("sling:resourcetype")
        .expect("the case-insensitive hit");
    assert_eq!(found.node_name, "other");
    assert!(found.node_scope_index);
    assert_eq!(
        rule.properties.len(),
        2,
        "the duplicate left one definition"
    );
    // The lookup itself is case-insensitive in both directions, which is
    // what the fixture's lower-cased names depend on.
    assert!(rule.config_of("SLING:resourceType").is_some());
}

#[test]
fn an_unindexed_definition_reads_every_other_flag_as_false() {
    let definition = definition_with_properties(
        Node::new().child("analyzed", analyzed_property()).child(
            "off",
            Node::new()
                .string("name", "jcr:description")
                .boolean("index", false)
                .boolean("analyzed", true)
                .boolean("ordered", true)
                .boolean("nodeScopeIndex", true),
        ),
    );
    let (_directory, rules) = read_rules("unindexed", &definition, None);
    let rules = rules.expect("the definition reads");
    let found = rules.rules[0]
        .config_of("jcr:description")
        .expect("the definition");
    assert!(!found.index);
    assert!(
        !found.analyzed,
        "`index` false forces every other flag false"
    );
    assert!(!found.ordered);
    assert!(!found.node_scope_index);
    assert!(!found.fulltext_enabled());
}

#[test]
fn an_explicit_boost_forces_a_definition_analyzed() {
    let definition = definition_with_properties(
        Node::new().child(
            "boosted",
            Node::new()
                .string("name", "jcr:title")
                .with("boost", PropertyType::Double, "2.5"),
        ),
    );
    let (_directory, rules) = read_rules("boost", &definition, None);
    let rules = rules.expect("the definition reads");
    let found = rules.rules[0]
        .config_of("jcr:title")
        .expect("the definition");
    assert!(found.analyzed, "a boosted field MUST be analyzed");
    assert!((found.boost - 2.5).abs() < f32::EPSILON);
    assert!(rules.rules[0].fulltext_enabled);
}

#[test]
fn a_relative_name_carries_its_ancestors() {
    let definition =
        definition_with_properties(Node::new().child("analyzed", analyzed_property()).child(
            "relative",
            Node::new().string("name", "jcr:content/metadata/dc:title"),
        ));
    let (_directory, rules) = read_rules("relative", &definition, None);
    let rules = rules.expect("the definition reads");
    let found = rules.rules[0]
        .config_of("jcr:content/metadata/dc:title")
        .expect("the definition");
    assert!(found.relative);
    assert_eq!(found.ancestors, vec!["jcr:content", "metadata"]);
}

#[test]
fn the_definition_level_settings_are_read() {
    let definition = default_definition()
        .long("maxFieldLength", 500)
        .child("tika", Node::new().string("maxExtractLength", "1000"))
        .child("analyzers", Node::new().boolean("indexOriginalTerm", true))
        .child("suggestion", Node::new().boolean("suggestAnalyzed", true));
    let (_directory, rules) = read_rules("settings", &definition, None);
    let rules = rules.expect("the definition reads");
    assert_eq!(rules.maximum_field_length, Some(500));
    assert!(
        rules.has_tika_configuration,
        "a tika child is seen and reported"
    );
    assert!(rules.index_original_term);
    assert!(rules.suggest_analyzed);
}

/// A definition-level `valueRegex` gates the per-property fulltext loop
/// with a regular expression froe does not evaluate.
#[test]
fn a_definition_level_value_regex_is_refused_by_name() {
    let definition = default_definition().string("valueRegex", "^abc.*$");
    let (_directory, rules) = read_rules("value-regex", &definition, None);
    let Err(refusal) = rules else {
        panic!("a definition-level valueRegex is refused");
    };
    assert!(refusal.to_string().contains("^abc.*$"), "{refusal}");
}

/// `suggestAnalyzed` on the definition root is where older definitions
/// carry it, and the `suggestion` child takes precedence.
#[test]
fn suggest_analyzed_falls_back_to_the_definition_root() {
    let definition = default_definition().boolean("suggestAnalyzed", true);
    let (_directory, rules) = read_rules("suggest-root", &definition, None);
    assert!(rules.expect("the definition reads").suggest_analyzed);

    let definition = default_definition()
        .boolean("suggestAnalyzed", true)
        .child("suggestion", Node::new().boolean("suggestAnalyzed", false));
    let (_directory, rules) = read_rules("suggest-child", &definition, None);
    assert!(
        !rules.expect("the definition reads").suggest_analyzed,
        "the suggestion child wins where it states the flag"
    );
}

/// Each of these writes a field this plan does not produce.
#[test]
fn every_unported_construct_is_refused_by_name() {
    let cases: [(&str, Node, &str); 6] = [
        (
            "function",
            definition_with_properties(Node::new().child(
                "functional",
                analyzed_property().string("function", "lower([jcr:title])"),
            )),
            "function",
        ),
        (
            "dynamic-boost",
            definition_with_properties(
                Node::new().child("boosted", analyzed_property().boolean("dynamicBoost", true)),
            ),
            "dynamicBoost",
        ),
        (
            "similarity",
            definition_with_properties(Node::new().child(
                "similar",
                analyzed_property().boolean("useInSimilarity", true),
            )),
            "useInSimilarity",
        ),
        (
            "similarity-tags",
            definition_with_properties(Node::new().child(
                "tagged",
                analyzed_property().boolean("similarityTags", true),
            )),
            "similarityTags",
        ),
        (
            "unique",
            definition_with_properties(
                Node::new().child("analyzed", analyzed_property()).child(
                    "identifier",
                    Node::new()
                        .string("name", "jcr:uuid")
                        .boolean("unique", true),
                ),
            ),
            "unique",
        ),
        (
            "sync",
            definition_with_properties(
                Node::new().child("analyzed", analyzed_property()).child(
                    "synchronous",
                    Node::new()
                        .string("name", "jcr:title")
                        .boolean("sync", true),
                ),
            ),
            "sync",
        ),
    ];
    for (name, definition, expected) in cases {
        let (_directory, rules) = read_rules(name, &definition, None);
        let Err(refusal) = rules else {
            panic!("{name} is refused");
        };
        assert!(refusal.to_string().contains(expected), "{name}: {refusal}");
        assert!(
            matches!(refusal, IndexError::UnsupportedDefinition { .. }),
            "{name}: {refusal}"
        );
    }
}

#[test]
fn a_version_one_definition_is_refused_by_name() {
    let definition = default_definition().long("compatVersion", 1);
    let (_directory, rules) = read_rules("compat-one", &definition, None);
    let Err(refusal) = rules else {
        panic!("a version-one definition is refused");
    };
    assert!(refusal.to_string().contains("compatVersion 1"), "{refusal}");

    // And so is one that carries no rules at all, which Oak reads as
    // version one however `compatVersion` is written.
    let flat = Node::new()
        .with(
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        )
        .string("type", "lucene")
        .string("includePropertyNames", "jcr:title");
    let (_directory, rules) = read_rules("no-rules", &flat, None);
    let Err(refusal) = rules else {
        panic!("a definition without indexRules is refused");
    };
    assert!(refusal.to_string().contains("indexRules"), "{refusal}");
}

#[test]
fn a_zero_maximum_field_length_is_refused_by_name() {
    let definition = default_definition().long("maxFieldLength", 0);
    let (_directory, rules) = read_rules("zero-length", &definition, None);
    let Err(refusal) = rules else {
        panic!("a zero maxFieldLength is refused");
    };
    assert!(refusal.to_string().contains("maxFieldLength"), "{refusal}");
}

#[test]
fn a_consumer_registered_analyzer_is_refused_by_name() {
    let definition = default_definition().child(
        "analyzers",
        Node::new().child(
            "default",
            Node::new().child("tokenizer", Node::new().string("name", "Standard")),
        ),
    );
    let (_directory, rules) = read_rules("analyzer-child", &definition, None);
    let Err(refusal) = rules else {
        panic!("a consumer-registered analyzer is refused");
    };
    assert!(refusal.to_string().contains("analyzers"), "{refusal}");
}

#[test]
fn an_nt_base_rule_with_a_null_check_is_refused_by_name() {
    let definition = definition_with_properties(
        Node::new().child("analyzed", analyzed_property()).child(
            "absent",
            Node::new()
                .string("name", "jcr:description")
                .boolean("nullCheckEnabled", true),
        ),
    );
    let (_directory, rules) = read_rules("nt-base-null", &definition, None);
    let Err(refusal) = rules else {
        panic!("an nt:base rule with nullCheckEnabled is refused");
    };
    assert!(refusal.to_string().contains("nt:base"), "{refusal}");
}

#[test]
fn two_rules_typing_one_ordered_property_differently_are_refused() {
    let definition = Node::new()
        .with(
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        )
        .string("type", "lucene")
        .child(
            "indexRules",
            Node::new()
                .child(
                    "nt:file",
                    Node::new().child(
                        "properties",
                        Node::new().child(
                            "ordered",
                            Node::new()
                                .string("name", "jcr:created")
                                .boolean("analyzed", true)
                                .boolean("ordered", true)
                                .string("type", "Date"),
                        ),
                    ),
                )
                .child(
                    "nt:folder",
                    Node::new().child(
                        "properties",
                        Node::new().child(
                            "ordered",
                            Node::new()
                                .string("name", "jcr:created")
                                .boolean("analyzed", true)
                                .boolean("ordered", true)
                                .string("type", "String"),
                        ),
                    ),
                ),
        );
    let (_directory, rules) = read_rules("two-types", &definition, None);
    let Err(refusal) = rules else {
        panic!("one doc-value field with two types is refused");
    };
    assert!(refusal.to_string().contains("jcr:created"), "{refusal}");
}

/// `/jcr:system/jcr:nodeTypes` with one type hierarchy: `nt:file` and
/// `nt:folder` below `nt:base`, and the mixin `mix:title` below
/// `mix:referenceable`.
fn node_types() -> Node {
    Node::new()
        .child(
            "nt:base",
            Node::new().names("rep:primarySubtypes", &["nt:file", "nt:folder"]),
        )
        .child("nt:file", Node::new())
        .child(
            "mix:referenceable",
            Node::new()
                .boolean("jcr:isMixin", true)
                .names("rep:mixinSubtypes", &["mix:title"]),
        )
}

/// A rule over one node type, analyzed so the definition stays an
/// `oakCodec` one.
fn rule_over(node_type: &str, inherited: Option<bool>) -> (String, Node) {
    let mut rule = Node::new().child(
        "properties",
        Node::new().child("analyzed", analyzed_property()),
    );
    if let Some(inherited) = inherited {
        rule = rule.boolean("inherited", inherited);
    }
    (node_type.to_owned(), rule)
}

fn definition_with_rules(rules: Vec<(String, Node)>) -> Node {
    let mut rule_node = Node::new();
    for (name, rule) in rules {
        rule_node = rule_node.child(&name, rule);
    }
    Node::new()
        .with(
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        )
        .string("type", "lucene")
        .child("indexRules", rule_node)
}

/// A node to resolve a rule for, written under `/oak:index/test` so the
/// same store carries it.
fn node_of_type(primary: &str, mixins: &[&str]) -> Node {
    let mut node = Node::new().with("jcr:primaryType", PropertyType::Name, primary);
    if !mixins.is_empty() {
        node = node.names("jcr:mixinTypes", mixins);
    }
    node
}

/// Reads the rules and resolves one for a node written beside them.
fn resolve_rule(name: &str, definition: &Node, subject: &Node) -> (TestDirectory, Option<String>) {
    let definition = definition.clone_with_child("subject", subject.clone());
    let (directory, rules) = read_rules(name, &definition, Some(&node_types()));
    let rules = rules.expect("the definition reads");
    let repository = Repository::open(&directory.path).expect("open the repository");
    let node = repository
        .node_at_path("/oak:index/test/subject")
        .expect("resolve the subject")
        .expect("the subject exists");
    let rule = rules
        .applicable_rule(&node)
        .expect("resolve a rule")
        .map(|rule| rule.node_type_name.clone());
    (directory, rule)
}

#[test]
fn an_inherited_rule_covers_every_subtype_and_a_plain_one_does_not() {
    let inherited = definition_with_rules(vec![rule_over("nt:base", None)]);
    let (_directory, rule) = resolve_rule("inherited", &inherited, &node_of_type("nt:file", &[]));
    assert_eq!(
        rule.as_deref(),
        Some("nt:base"),
        "`inherited` defaults to true"
    );

    let plain = definition_with_rules(vec![rule_over("nt:base", Some(false))]);
    let (_directory, rule) = resolve_rule("plain", &plain, &node_of_type("nt:file", &[]));
    assert_eq!(
        rule, None,
        "a rule that is not inherited covers its own type alone"
    );

    let (_directory, rule) = resolve_rule("plain-exact", &plain, &node_of_type("nt:base", &[]));
    assert_eq!(rule.as_deref(), Some("nt:base"));
}

#[test]
fn the_primary_type_is_tried_before_the_mixins() {
    let definition = definition_with_rules(vec![
        rule_over("nt:file", Some(false)),
        rule_over("mix:title", Some(false)),
    ]);
    let (_directory, rule) = resolve_rule(
        "primary-first",
        &definition,
        &node_of_type("nt:file", &["mix:title"]),
    );
    assert_eq!(
        rule.as_deref(),
        Some("nt:file"),
        "the primary type wins however the rules are ordered"
    );
    // With no rule for the primary type, the mixin's rule applies.
    let mixin_only = definition_with_rules(vec![rule_over("mix:title", Some(false))]);
    let (_directory, rule) = resolve_rule(
        "mixin",
        &mixin_only,
        &node_of_type("nt:folder", &["mix:title"]),
    );
    assert_eq!(rule.as_deref(), Some("mix:title"));
}

#[test]
fn a_node_no_rule_covers_resolves_to_nothing() {
    let definition = definition_with_rules(vec![rule_over("nt:file", Some(false))]);
    let (_directory, rule) = resolve_rule("no-rule", &definition, &node_of_type("nt:folder", &[]));
    assert_eq!(
        rule, None,
        "a node no rule applies to contributes no document"
    );
}

#[test]
fn a_synchronous_node_type_rule_is_refused_by_name() {
    let definition = definition_with_rules(vec![
        rule_over("nt:base", None),
        (
            "nt:file".to_owned(),
            Node::new()
                .boolean("nodeTypeIndex", true)
                .boolean("sync", true),
        ),
    ]);
    let (_directory, rules) = read_rules("sync-node-type", &definition, None);
    let Err(refusal) = rules else {
        panic!("a synchronous nodeTypeIndex rule is refused");
    };
    assert!(refusal.to_string().contains("nodeTypeIndex"), "{refusal}");
}

#[test]
fn an_aggregate_matcher_walks_its_includes() {
    let definition = default_definition().child(
        "aggregates",
        Node::new().child(
            "nt:file",
            Node::new()
                .long("reaggregateLimit", 3)
                .child("include0", Node::new().string("path", "jcr:content"))
                .child(
                    "include1",
                    Node::new()
                        .string("path", "*/metadata")
                        .boolean("relativeNode", true),
                ),
        ),
    );
    let (directory, rules) = read_rules("aggregate", &definition, None);
    let rules = rules.expect("the definition reads");
    let aggregate = &rules.rules[0].aggregate;
    assert!(
        !aggregate.has_node_aggregates(),
        "the aggregate is declared for nt:file, and this rule is nt:base"
    );

    // The rule's own type carries it.
    let definition = definition_with_rules(vec![rule_over("nt:file", None)]).child(
        "aggregates",
        Node::new().child(
            "nt:file",
            Node::new()
                .child("include0", Node::new().string("path", "jcr:content"))
                .child("include1", Node::new().string("path", "*/metadata")),
        ),
    );
    let (_directory, rules) = read_rules("aggregate-own", &definition, None);
    let rules = rules.expect("the definition reads");
    let aggregate = &rules.rules[0].aggregate;
    assert!(aggregate.has_node_aggregates());
    assert!(
        rules.rules[0].fulltext_enabled,
        "node aggregates alone make a rule fulltext-enabled"
    );

    let repository = Repository::open(&directory.path).expect("open the repository");
    let node = repository
        .node_at_path("/oak:index/test")
        .expect("resolve a node")
        .expect("a node to step over");
    // `jcr:content` ends the first include, so it aggregates; `renditions`
    // matches the `*` of the second and continues; `metadata` under it
    // ends that one.
    let matcher = aggregate.matcher();
    let (after_content, outcome) = matcher.step("jcr:content", &node).expect("step");
    assert!(matches!(outcome, Match::Aggregate(_)), "{outcome:?}");
    let _ = after_content;
    let (after_star, outcome) = matcher.step("renditions", &node).expect("step");
    assert_eq!(outcome, Match::Continue);
    let (_, outcome) = after_star.step("metadata", &node).expect("step");
    assert!(matches!(outcome, Match::Aggregate(_)), "{outcome:?}");
    let (_, outcome) = after_star.step("other", &node).expect("step");
    assert_eq!(outcome, Match::Stop, "no include can match deeper");
}

/// `unique` forces `sync`, and `sync` forces `propertyIndex`. A rule
/// carrying either is refused, so the forcing is only visible on the
/// property definition itself.
#[test]
fn unique_forces_sync_and_sync_forces_the_property_index() {
    let definition = definition_with_properties(
        Node::new().child("analyzed", analyzed_property()).child(
            "identifier",
            Node::new()
                .string("name", "jcr:uuid")
                .boolean("unique", true),
        ),
    );
    let directory = TestDirectory::new("unique-forces");
    publish(&directory.path, &definition, None);
    let repository = Repository::open(&directory.path).expect("open the repository");
    let node = repository
        .node_at_path("/oak:index/test/indexRules/nt:base/properties/identifier")
        .expect("resolve the property definition")
        .expect("the property definition exists");
    let mut warnings: Vec<IndexWarning> = Vec::new();
    let read = PropertyDefinition::read("/oak:index/test", "identifier", &node, &mut warnings)
        .expect("the property definition reads");
    assert!(read.unique);
    assert!(read.sync, "`unique` forces `sync`");
    assert!(read.property_index, "`sync` forces `propertyIndex`");
}

/// `primaryType` is enforced on the **last** step of an include and on no
/// other, which is what Jackrabbit 2 did before it.
#[test]
fn an_includes_primary_type_is_enforced_on_the_last_step_alone() {
    let definition = definition_with_rules(vec![rule_over("nt:file", None)])
        .child(
            "aggregates",
            Node::new().child(
                "nt:file",
                Node::new().child(
                    "include0",
                    Node::new()
                        .string("path", "jcr:content")
                        .string("primaryType", "nt:resource"),
                ),
            ),
        )
        .child("resource", node_of_type("nt:resource", &[]))
        .child("folder", node_of_type("nt:folder", &[]));
    let directory = TestDirectory::new("aggregate-type");
    publish(&directory.path, &definition, None);
    let repository = Repository::open(&directory.path).expect("open the repository");
    let root = repository.content_root().expect("the content root");
    let node = repository
        .node_at_path("/oak:index/test")
        .expect("resolve the definition")
        .expect("the definition exists");
    let mut warnings: Vec<IndexWarning> = Vec::new();
    let rules = IndexingRules::read(&node, "/oak:index/test", &root, &mut warnings)
        .expect("the definition reads");
    let matcher = rules.rules[0].aggregate.matcher();
    let resource = repository
        .node_at_path("/oak:index/test/resource")
        .expect("resolve the node")
        .expect("the node exists");
    let folder = repository
        .node_at_path("/oak:index/test/folder")
        .expect("resolve the node")
        .expect("the node exists");
    let (_, outcome) = matcher.step("jcr:content", &resource).expect("step");
    assert!(matches!(outcome, Match::Aggregate(_)), "{outcome:?}");
    let (_, outcome) = matcher.step("jcr:content", &folder).expect("step");
    assert_eq!(
        outcome,
        Match::Stop,
        "the primary type is enforced on the last step"
    );
}
