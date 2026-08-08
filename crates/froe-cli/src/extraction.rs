//! The extraction command: streaming node data as JSON lines.
//!
//! One JSON object per node, written depth-first:
//!
//! ```json
//! {"path":"/content","properties":{"jcr:primaryType":"nt:unstructured","title":"Hello"}}
//! ```
//!
//! Inline binaries appear as `{"binary_length":N}` and external binaries
//! as `{"binary_reference":"..."}` — binary *content* is never embedded,
//! which keeps extraction fast and output line-oriented.
//!
//! The traversal uses an explicit stack instead of recursion, so tree
//! depth is bounded by memory rather than stack size, and a depth limit
//! guards against node cycles in corrupt repositories.

use std::io::Write;
use std::path::Path;

use froe::content::node::NodeState;
use froe::store::Repository;

use crate::content_display::normalized_path;
use crate::output::{append_json_string, append_json_values};

/// Creates the extraction output file. Never an existing file — an
/// output path aimed at the repository itself (`journal.log`, an
/// archive, or an alias of one) must never be truncated, and unrelated
/// files must never be silently overwritten — and never a fresh file
/// *inside* the repository directory, where the next open would mistake
/// it for a damaged archive. On Unix the file is created
/// owner-accessible only, regardless of the ambient umask.
pub(crate) fn create_extraction_output(
    repository_path: &Path,
    output_path: &Path,
) -> froe::Result<std::fs::File> {
    // Canonical paths, so symlinks and relative forms cannot smuggle the
    // output into the repository directory. A nonexistent parent skips
    // the check and fails file creation below instead.
    let repository_directory = std::fs::canonicalize(repository_path)?;
    let output_parent = match output_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if let Ok(canonical_parent) = std::fs::canonicalize(output_parent)
        && canonical_parent.starts_with(&repository_directory)
    {
        return Err(froe::Error::InvalidFormat {
            details: format!(
                "output file {} is inside the repository directory; a stray file there \
                     could be mistaken for a damaged archive at the next open",
                output_path.display()
            ),
        });
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(output_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            froe::Error::InvalidFormat {
                details: format!(
                    "output file {} already exists; extraction never overwrites — choose a \
                     fresh path or remove the file first",
                    output_path.display()
                ),
            }
        } else {
            froe::Error::InputOutput(error)
        }
    })
}

/// Nodes deeper than this indicate a cycle in a corrupt repository;
/// real content trees are nowhere near this deep.
const MAXIMUM_TRAVERSAL_DEPTH: usize = 16_384;

/// The total nodes one extraction may emit. A depth bound alone cannot
/// stop corrupt records shaped as a wide DAG, whose distinct paths grow
/// exponentially while staying shallow; real repositories stay far below
/// this.
const MAXIMUM_EXTRACTED_NODES: u64 = 1_000_000_000;

/// One unit of traversal work.
enum WorkItem<'repository> {
    /// Emit this node and schedule its children.
    Visit {
        node: NodeState<'repository>,
        name: String,
        depth: usize,
    },
    /// Restore the path buffer after a subtree completes.
    RestorePathLength(usize),
}

/// Streams the subtree at `path` to `sink` as JSON lines. `depth` bounds
/// the traversal (`None` extracts the whole subtree). Returns the number
/// of nodes written, or `Ok(None)` when the path does not exist.
pub(crate) fn extract_json_lines(
    repository: &Repository,
    path: &str,
    depth: Option<usize>,
    sink: &mut dyn Write,
) -> froe::Result<Option<u64>> {
    let Some(root) = repository.node_at_path(path)? else {
        return Ok(None);
    };
    let mut path_buffer = normalized_path(path);
    if path_buffer == "/" {
        path_buffer.clear();
    }

    let mut line_buffer = String::with_capacity(1024);
    let mut written = 0u64;
    let mut stack = vec![WorkItem::Visit {
        node: root,
        name: String::new(),
        depth: 0,
    }];

    while let Some(item) = stack.pop() {
        let (node, name, node_depth) = match item {
            WorkItem::RestorePathLength(length) => {
                path_buffer.truncate(length);
                continue;
            }
            WorkItem::Visit { node, name, depth } => (node, name, depth),
        };
        if node_depth >= MAXIMUM_TRAVERSAL_DEPTH {
            return Err(froe::Error::InvalidFormat {
                details: format!(
                    "content tree exceeds depth {MAXIMUM_TRAVERSAL_DEPTH}; \
                     the node records probably form a cycle"
                ),
            });
        }

        if !name.is_empty() {
            stack.push(WorkItem::RestorePathLength(path_buffer.len()));
            path_buffer.push('/');
            path_buffer.push_str(&name);
        }

        write_node_line(&node, &path_buffer, sink, &mut line_buffer)?;
        written += 1;
        if written > MAXIMUM_EXTRACTED_NODES {
            return Err(froe::Error::InvalidFormat {
                details: format!(
                    "extraction exceeds {MAXIMUM_EXTRACTED_NODES} nodes; \
                     the node records probably form a pathological graph"
                ),
            });
        }

        let descend = match depth {
            Some(limit) => node_depth < limit,
            None => true,
        };
        if descend {
            // Push children in reverse so they pop in storage order.
            let children = node.child_node_entries()?;
            for (child_name, child) in children.into_iter().rev() {
                stack.push(WorkItem::Visit {
                    node: child,
                    name: child_name,
                    depth: node_depth + 1,
                });
            }
        }
    }
    Ok(Some(written))
}

/// Writes one node as a JSON line.
fn write_node_line(
    node: &NodeState<'_>,
    path_buffer: &str,
    sink: &mut dyn Write,
    line_buffer: &mut String,
) -> froe::Result<()> {
    line_buffer.clear();
    line_buffer.push_str("{\"path\":");
    let display_path = if path_buffer.is_empty() {
        "/"
    } else {
        path_buffer
    };
    append_json_string(line_buffer, display_path);
    line_buffer.push_str(",\"properties\":{");
    for (position, property) in node.properties()?.iter().enumerate() {
        if position > 0 {
            line_buffer.push(',');
        }
        append_json_string(line_buffer, &property.name);
        line_buffer.push(':');
        append_json_values(line_buffer, &property.values);
    }
    line_buffer.push_str("}}\n");
    sink.write_all(line_buffer.as_bytes())?;
    Ok(())
}
