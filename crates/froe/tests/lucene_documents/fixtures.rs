//! The stores these cases read, and the shape of a case's expectation.

use super::*;

/// A directory that removes itself.
pub(crate) struct TestDirectory {
    pub(crate) path: PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-lucene-documents-{name}-{}-{:?}",
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

/// A node to write.
#[derive(Clone)]
pub(crate) struct Node {
    properties: Vec<(String, PropertyType, Vec<String>, bool)>,
    pub(crate) children: Vec<(String, Node)>,
}

impl Node {
    pub(crate) fn new() -> Self {
        Self {
            properties: Vec::new(),
            children: Vec::new(),
        }
    }

    pub(crate) fn single(mut self, name: &str, property_type: PropertyType, value: &str) -> Self {
        self.properties.push((
            name.to_owned(),
            property_type,
            vec![value.to_owned()],
            false,
        ));
        self
    }

    pub(crate) fn multiple(
        mut self,
        name: &str,
        property_type: PropertyType,
        values: &[&str],
    ) -> Self {
        self.properties.push((
            name.to_owned(),
            property_type,
            values.iter().map(|value| (*value).to_owned()).collect(),
            true,
        ));
        self
    }

    pub(crate) fn string(self, name: &str, value: &str) -> Self {
        self.single(name, PropertyType::String, value)
    }

    pub(crate) fn boolean(self, name: &str, value: bool) -> Self {
        self.single(
            name,
            PropertyType::Boolean,
            if value { "true" } else { "false" },
        )
    }

    pub(crate) fn child(mut self, name: &str, node: Node) -> Self {
        self.children.push((name.to_owned(), node));
        self
    }
}

pub(crate) fn write_tree(
    writer: &mut RecordWriter<impl SegmentSink>,
    node: &Node,
) -> RecordIdentifier {
    let children: Vec<(String, RecordIdentifier)> = node
        .children
        .iter()
        .map(|(name, child)| (name.clone(), write_tree(writer, child)))
        .collect();
    let properties: Vec<PropertyToWrite> = node
        .properties
        .iter()
        .map(|(name, property_type, values, multi_valued)| {
            let identifiers: Vec<RecordIdentifier> = values
                .iter()
                .map(|value| writer.write_string(value).expect("write a property value"))
                .collect();
            PropertyToWrite {
                name: name.clone(),
                property_type: *property_type,
                values: if *multi_valued {
                    PropertyValuesToWrite::Multiple(identifiers)
                } else {
                    PropertyValuesToWrite::Single(identifiers[0])
                },
            }
        })
        .collect();
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

/// Publishes `/oak:index/test` and `/content/page`.
pub(crate) fn publish(directory: &Path, definition: &Node, subject: &Node) {
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
    let page = write_tree(&mut writer, subject);
    let content = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "page".to_owned(),
                node: page,
            },
            &[],
        )
        .expect("write /content");
    // The node types the rules resolve through: every case's rule is over
    // `nt:base`, and a subject of any other type reaches it only because
    // the registry records the subtype.
    let node_types = write_tree(
        &mut writer,
        &Node::new()
            .child(
                "nt:base",
                Node::new().multiple(
                    "rep:primarySubtypes",
                    PropertyType::Name,
                    &["nt:unstructured", "nt:file", "nt:resource", "nt:folder"],
                ),
            )
            .child("nt:unstructured", Node::new()),
    );
    let system = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:nodeTypes".to_owned(),
                node: node_types,
            },
            &[],
        )
        .expect("write /jcr:system");
    let root = writer
        .write_node(
            Some("rep:root"),
            &[],
            &ChildNodesToWrite::Many(vec![
                ("oak:index".to_owned(), index),
                ("content".to_owned(), content),
                ("jcr:system".to_owned(), system),
            ]),
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

/// Makes the document for `/content/page` under `definition`.
pub(crate) fn make(
    name: &str,
    definition: &Node,
    subject: &Node,
    policy: BinaryTextPolicy,
) -> (TestDirectory, Option<MadeDocument>) {
    let directory = TestDirectory::new(name);
    publish(&directory.path, definition, subject);
    let repository = Repository::open(&directory.path).expect("open the repository");
    let root = repository.content_root().expect("the content root");
    let node = repository
        .node_at_path("/oak:index/test")
        .expect("resolve the definition")
        .expect("the definition exists");
    let mut warnings: Vec<IndexWarning> = Vec::new();
    let rules = IndexingRules::read(&node, "/oak:index/test", &root, &mut warnings)
        .expect("the definition reads");
    let page = repository
        .node_at_path("/content/page")
        .expect("resolve the node")
        .expect("the node exists");
    let rule = rules
        .applicable_rule(&page)
        .expect("resolve a rule")
        .expect("a rule applies")
        .clone();
    let maker = DocumentMaker::new("/oak:index/test", &rules, policy);
    let made = maker
        .make(&page, "/content/page", &rule)
        .expect("make the document");
    (directory, made)
}

/// One field, as a case states it: name, options, stored, norms, and the
/// terms it holds.
pub(crate) fn describe(field: &Field) -> (String, IndexOptions, bool, bool, Vec<String>) {
    (
        field.name.clone(),
        field.options,
        field.stored.is_some(),
        !field.omit_norms,
        field
            .tokens
            .iter()
            .map(|token| String::from_utf8_lossy(&token.bytes).into_owned())
            .collect(),
    )
}

pub(crate) fn names(made: &MadeDocument) -> Vec<&str> {
    made.document
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect()
}

/// The fixture's default shape: one `nt:base` rule whose catch-all
/// pattern is analyzed and `nodeScopeIndex`, with path restrictions on.
pub(crate) fn default_definition() -> Node {
    Node::new()
        .single(
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

pub(crate) fn marker_policy() -> BinaryTextPolicy {
    BinaryTextPolicy::new(BinaryTextFallback::Marker)
}

/// A definition with one rule over `nt:base` and the property
/// definitions a case states.
pub(crate) fn definition_with(properties: Vec<(&str, Node)>) -> Node {
    definition_over("nt:base", properties)
}

/// The same over a named node type, for the cases `nt:base` refuses.
pub(crate) fn definition_over(node_type: &str, properties: Vec<(&str, Node)>) -> Node {
    let mut property_node = Node::new();
    for (name, definition) in properties {
        property_node = property_node.child(name, definition);
    }
    Node::new()
        .single(
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        )
        .string("type", "lucene")
        .child(
            "indexRules",
            Node::new().child(node_type, Node::new().child("properties", property_node)),
        )
}

/// One analyzed definition, which keeps the definition fulltext-enabled.
pub(crate) fn analyzed(name: &str) -> Node {
    Node::new().string("name", name).boolean("analyzed", true)
}

pub(crate) fn unstructured() -> Node {
    Node::new().single("jcr:primaryType", PropertyType::Name, "nt:unstructured")
}
