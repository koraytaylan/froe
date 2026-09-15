//! Synthetic index definitions, published and read back.
//!
//! The rules suite and anything else that needs a definition in a real
//! store share this: a node builder, a writer that publishes one store
//! holding `/oak:index/test` and optionally a node-type registry, and the
//! read that hands back `IndexingRules`. Split out of
//! `lucene_rules_tests.rs` when the thousand-line gate found the seam.

#![allow(dead_code, reason = "each binary uses a different part of this module")]

use std::path::{Path, PathBuf};

use froe::content::PropertyType;
use froe::index::lucene::documents::rules::IndexingRules;
use froe::index::{IndexError, IndexWarning};
use froe::segment::record::RecordIdentifier;
use froe::store::Repository;
use froe::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use froe::writer::store_writer::WritableRepository;

/// A directory that removes itself, so a failing test leaves nothing
/// behind.
pub(crate) struct TestDirectory {
    pub(crate) path: PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
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
pub(crate) struct Node {
    pub(crate) properties: Vec<(&'static str, PropertyType, String)>,
    pub(crate) multiple: Vec<(&'static str, PropertyType, Vec<String>)>,
    pub(crate) children: Vec<(String, Node)>,
}

impl Node {
    pub(crate) fn new() -> Self {
        Self {
            properties: Vec::new(),
            multiple: Vec::new(),
            children: Vec::new(),
        }
    }

    pub(crate) fn with(
        mut self,
        name: &'static str,
        property_type: PropertyType,
        value: &str,
    ) -> Self {
        self.properties
            .push((name, property_type, value.to_owned()));
        self
    }

    pub(crate) fn boolean(self, name: &'static str, value: bool) -> Self {
        self.with(
            name,
            PropertyType::Boolean,
            if value { "true" } else { "false" },
        )
    }

    pub(crate) fn string(self, name: &'static str, value: &str) -> Self {
        self.with(name, PropertyType::String, value)
    }

    pub(crate) fn long(self, name: &'static str, value: i64) -> Self {
        self.with(name, PropertyType::Long, &value.to_string())
    }

    /// A multi-valued `NAMES` property, which the node-type subtype lists
    /// are.
    pub(crate) fn names(mut self, name: &'static str, values: &[&str]) -> Self {
        self.multiple.push((
            name,
            PropertyType::Name,
            values.iter().map(|value| (*value).to_owned()).collect(),
        ));
        self
    }

    pub(crate) fn child(mut self, name: &str, node: Node) -> Self {
        self.children.push((name.to_owned(), node));
        self
    }

    /// The same, by value, for a caller that holds the node already.
    pub(crate) fn clone_with_child(&self, name: &str, node: Node) -> Self {
        self.clone().child(name, node)
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
pub(crate) fn publish(directory: &Path, definition: &Node, node_types: Option<&Node>) {
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
pub(crate) fn read_rules(
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
