//! The export driver: one traversal feeding any sink.
//!
//! [`export_subtree`] owns the walk — document order, depth limiting,
//! and the core traversal's cycle and node-budget hardening — while an
//! [`ExportSink`] owns the output format. Sinks receive borrowed data
//! ([`ExportedNode`]), so the driver allocates nothing per node beyond
//! the property decode itself.

use froe::content::node::PropertyState;
use froe::content::traversal::DepthFirstTraversal;
use froe::store::Repository;

/// One node as handed to an export sink, valid for the duration of the
/// [`ExportSink::write_node`] call.
pub struct ExportedNode<'export> {
    /// The node's content path.
    pub path: &'export str,
    /// How many levels below the export root the node sits.
    pub depth: usize,
    /// The node's properties, in template order, with `jcr:primaryType`
    /// and `jcr:mixinTypes` synthesized first — the same view
    /// [`froe::content::node::NodeState::properties`] presents.
    pub properties: &'export [PropertyState],
}

/// A destination for exported nodes.
pub trait ExportSink {
    /// Writes one node. Nodes arrive in document order: each node before
    /// its children, children in storage order.
    fn write_node(&mut self, node: &ExportedNode<'_>) -> froe::Result<()>;

    /// Completes the export: flushes buffers and writes any trailer the
    /// format requires. Called exactly once, after the last node.
    fn finish(&mut self) -> froe::Result<()>;
}

/// Streams the subtree at `path` into `sink`. A depth of `Some(limit)`
/// exports nodes at most `limit` levels below the starting node; `None`
/// exports the whole subtree. Returns the number of nodes written, or
/// `Ok(None)` when the path does not exist (the sink is then untouched).
pub fn export_subtree(
    repository: &Repository,
    path: &str,
    depth: Option<usize>,
    sink: &mut dyn ExportSink,
) -> froe::Result<Option<u64>> {
    let Some(root) = repository.node_at_path(path)? else {
        return Ok(None);
    };
    let mut traversal = DepthFirstTraversal::new(root, path, depth);
    let mut written = 0u64;
    while let Some(visited) = traversal.next_node()? {
        let properties = visited.node.properties()?;
        sink.write_node(&ExportedNode {
            path: visited.path,
            depth: visited.depth,
            properties: &properties,
        })?;
        written += 1;
    }
    sink.finish()?;
    Ok(Some(written))
}
