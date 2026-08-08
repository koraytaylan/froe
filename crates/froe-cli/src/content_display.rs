//! Content commands: showing single nodes and trees.

use froe::content::PropertyValues;
use froe::content::node::NodeState;
use froe::content::property::PropertyValue;
use froe::store::Repository;

use crate::output::{append_json_values, sanitize_terminal_text};

/// `froe node`: one node's properties and child names.
pub(crate) fn print_node(repository: &Repository, path: &str) -> froe::Result<bool> {
    let Some(node) = repository.node_at_path(path)? else {
        return Ok(false);
    };
    println!(
        "path              {}",
        sanitize_terminal_text(&normalized_path(path))
    );
    println!("record            {}", node.record_identifier());
    println!("stable identifier {}", node.stable_identifier()?);
    for property in node.properties()? {
        println!(
            "property          {} <{}{}> = {}",
            sanitize_terminal_text(&property.name),
            property.property_type.jcr_name(),
            if matches!(property.values, PropertyValues::Multiple(_)) {
                "[]"
            } else {
                ""
            },
            render_values(&property.values),
        );
    }
    for (name, child) in node.child_node_entries()? {
        println!(
            "child             {}  {}",
            sanitize_terminal_text(&name),
            child.record_identifier()
        );
    }
    Ok(true)
}

/// `froe tree`: the tree under a path, one indented line per node.
/// Iterative traversal, so a large `--depth` over a deep (or, in a
/// corrupt store, cyclic) tree cannot overflow the stack.
pub(crate) fn print_tree(repository: &Repository, path: &str, depth: usize) -> froe::Result<bool> {
    let Some(root) = repository.node_at_path(path)? else {
        return Ok(false);
    };
    let mut stack: Vec<(NodeState<'_>, String, usize)> = vec![(root, normalized_path(path), 0)];
    while let Some((node, name, level)) = stack.pop() {
        let primary_type =
            node.property("jcr:primaryType")?
                .and_then(|property| match property.values {
                    PropertyValues::Single(PropertyValue::Name(name)) => Some(name),
                    _ => None,
                });
        let indentation = level * 2;
        let name = sanitize_terminal_text(&name);
        match primary_type {
            Some(primary_type) => println!(
                "{:indentation$}{name}  [{}]",
                "",
                sanitize_terminal_text(&primary_type)
            ),
            None => println!("{:indentation$}{name}", ""),
        }
        if level < depth {
            for (child_name, child) in node.child_node_entries()?.into_iter().rev() {
                stack.push((child, child_name, level + 1));
            }
        }
    }
    Ok(true)
}

/// Renders property values for terminal display, JSON-style.
fn render_values(values: &PropertyValues) -> String {
    let mut buffer = String::new();
    append_json_values(&mut buffer, values);
    buffer
}

/// Displays `/` for the root and strips duplicate slashes elsewhere.
pub(crate) fn normalized_path(path: &str) -> String {
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", segments.join("/"))
    }
}
