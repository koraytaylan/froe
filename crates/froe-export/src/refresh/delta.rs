//! Exporting the replacement rows a dirty range needs, at the revision
//! the refresh pinned.

use super::{
    DirtyRange, ExportSink, ExportedNode, NodeState, ParquetExportOptions, ParquetSink, Path,
    RecordIdentifier, Replacement, Repository, Write, create_export_output, export_node,
    path_depth,
};

/// Exports every dirty range's replacement — at the pinned revision —
/// into the delta files, returning the number of nodes written.
/// `on_node` reports the running count per node.
/// The pair of files one export writes, kept together so the two
/// same-typed paths cannot be transposed.
#[derive(Clone, Copy)]
pub(crate) struct TablePaths<'paths> {
    pub(crate) nodes: &'paths Path,
    pub(crate) properties: &'paths Path,
}

pub(crate) fn export_delta(
    repository: &Repository,
    revision: RecordIdentifier,
    root_path: &str,
    ranges: &[DirtyRange],
    delta: TablePaths<'_>,
    options: &ParquetExportOptions,
    on_node: &mut dyn FnMut(u64),
) -> froe::Result<u64> {
    let nodes_file = create_export_output(repository.directory(), delta.nodes)?;
    let properties_file = create_export_output(repository.directory(), delta.properties)?;
    let mut sink = ParquetSink::new(
        std::io::BufWriter::with_capacity(1 << 20, nodes_file),
        std::io::BufWriter::with_capacity(1 << 20, properties_file),
        options,
    )?;
    let mut written = 0u64;
    let root_depth = path_depth(root_path);
    for range in ranges {
        let Replacement::ReExport {
            depth: replacement_depth,
        } = range.replacement
        else {
            continue;
        };
        // The diff reported the path at the pinned revision, so the
        // node resolves; a missing node would mean the store violates
        // the file protocol, and skipping it degrades the range to a
        // removal rather than corrupting the merge.
        let Some(node) = node_at_revision(repository, revision, &range.path)? else {
            continue;
        };
        let mut offset_sink = DepthOffsetSink {
            inner: &mut sink,
            offset: path_depth(&range.path).saturating_sub(root_depth),
            written: &mut written,
            on_node: &mut *on_node,
        };
        export_node(node, &range.path, replacement_depth, &mut offset_sink)?;
    }
    sink.finish()?;
    Ok(written)
}

/// Resolves a content path at a specific head revision — the pinned
/// counterpart of [`Repository::node_at_path`].
pub(crate) fn node_at_revision<'repository>(
    repository: &'repository Repository,
    revision: RecordIdentifier,
    path: &str,
) -> froe::Result<Option<NodeState<'repository>>> {
    let super_root = repository.node(revision);
    let Some(mut current) = super_root.child_node("root")? else {
        return Ok(None);
    };
    for name in path.split('/').filter(|segment| !segment.is_empty()) {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// A sink forwarding nodes into the delta sink with their `depth`
/// shifted by the range's depth below the export root — a re-exported
/// subtree starts its traversal at depth 0, but its rows must carry
/// the depth they hold in the full export.
pub(crate) struct DepthOffsetSink<'a, W: Write + Send> {
    pub(crate) inner: &'a mut ParquetSink<W>,
    pub(crate) offset: usize,
    pub(crate) written: &'a mut u64,
    pub(crate) on_node: &'a mut dyn FnMut(u64),
}

impl<W: Write + Send> ExportSink for DepthOffsetSink<'_, W> {
    fn write_node(&mut self, node: &ExportedNode<'_>) -> froe::Result<()> {
        self.inner.write_node(&ExportedNode {
            path: node.path,
            depth: node.depth + self.offset,
            properties: node.properties,
        })?;
        *self.written += 1;
        (self.on_node)(*self.written);
        Ok(())
    }

    fn finish(&mut self) -> froe::Result<()> {
        // The inner sink finishes once, after the last range.
        Ok(())
    }
}
