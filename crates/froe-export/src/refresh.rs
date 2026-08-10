//! Refreshing an existing Parquet export in place.
//!
//! A full export decodes the whole content tree; a refresh decodes only
//! what changed since the export was taken. The mechanism:
//!
//! 1. **Validate.** Both files of an existing export carry an
//!    [`ExportProvenance`] stamp in their footers. The files are a
//!    usable base when both stamps exist, agree with each other, and
//!    match the requested root path and depth limit. Anything else —
//!    missing files, foreign Parquet files, disagreeing stamps (the
//!    residue of an interrupted refresh) — makes the export
//!    [`ParquetRefresh::NotReusable`], and the caller falls back to a
//!    full export.
//! 2. **Diff.** The store's head is pinned once
//!    ([`Repository::head_record_identifier`]), so a live repository
//!    cannot tear the refresh, and [`diff_revisions`] between the
//!    stamped and the pinned revision yields the changed paths. The
//!    diff prunes unchanged subtrees by record identifier, so this
//!    walks only the divergent spine.
//! 3. **Delta.** Changed paths become *dirty ranges*: an added node
//!    re-exports its whole subtree, a removed node excises its subtree's
//!    rows, a property change re-exports just that node's rows. The
//!    replacements are exported — at the pinned revision — into
//!    temporary delta files.
//! 4. **Merge.** Old rows and delta rows merge into fresh files:
//!    old rows inside a dirty range are dropped, the range's
//!    replacement rows are written in their place. Rows stay nearly
//!    document-ordered, keeping path-column statistics selective.
//! 5. **Swap.** The merged files atomically replace the old ones
//!    ([`replace_export_output`]). A crash between the two renames
//!    leaves disagreeing stamps, which step 1 treats as not reusable —
//!    the next run simply rebuilds.
//!
//! The result is exactly the row set a full export of the pinned
//! revision would produce; a refresh never leaves stale rows behind.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use froe::content::node::NodeState;
use froe::segment::record::RecordIdentifier;
use froe::store::Repository;
use froe::tooling::diff::{NodeDifference, diff_revisions};

use crate::export::{ExportSink, ExportedNode, export_node};
use crate::output_file::{
    create_export_directory, create_export_output, replace_export_output, sweep_temporary_outputs,
    temporary_output_name,
};
use crate::parquet::{
    ExportProvenance, NodeRow, ParquetExportOptions, ParquetSink, PropertyRow,
    read_export_provenance,
};

/// The nodes table's file name within the export directory.
pub const NODES_FILE_NAME: &str = "nodes.parquet";

/// The properties table's file name within the export directory.
pub const PROPERTIES_FILE_NAME: &str = "properties.parquet";

/// The conceptual file names the delta temp files derive from; the
/// `.delta.` infix keeps their sweep pattern apart from the real
/// tables' temp files.
const NODES_DELTA_FILE_NAME: &str = "nodes.delta.parquet";
const PROPERTIES_DELTA_FILE_NAME: &str = "properties.delta.parquet";

/// The outcome of attempting to refresh an existing Parquet export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParquetRefresh {
    /// The existing export already reflects the repository head; no
    /// file was rewritten.
    Current {
        /// The head revision the export reflects.
        revision: String,
    },
    /// The export was refreshed to the repository head.
    Refreshed {
        /// The head revision the export now reflects.
        revision: String,
        /// How many dirty ranges the delta covered.
        ranges: u64,
        /// How many nodes the delta re-exported.
        nodes: u64,
    },
    /// The existing files cannot serve as a refresh base; the caller
    /// should run a full export, which replaces them.
    NotReusable {
        /// Why the files are unusable, phrased for the operator.
        reason: String,
    },
}

/// Refreshes the Parquet export in `output_directory` to the
/// repository's current head, decoding only what changed since the
/// export was taken. `root_path` and `depth` must match the original
/// export's, or the export is [`ParquetRefresh::NotReusable`].
/// `on_node` receives the running count of re-exported nodes as the
/// delta export progresses.
///
/// The repository is read at one pinned head revision throughout, so a
/// live writer cannot tear the refresh. Hard errors (an unreadable
/// store, unwritable output) abort; every content-level surprise —
/// missing, foreign, or disagreeing files, a compacted-away stamped
/// revision, rows that do not decode — resolves to
/// [`ParquetRefresh::NotReusable`].
pub fn refresh_parquet_export(
    repository: &Repository,
    root_path: &str,
    depth: Option<usize>,
    output_directory: &Path,
    options: &ParquetExportOptions,
    on_node: &mut dyn FnMut(u64),
) -> froe::Result<ParquetRefresh> {
    create_export_directory(repository.directory(), output_directory)?;
    let provenance = match validate(output_directory, root_path, depth) {
        Ok(provenance) => provenance,
        Err(reason) => return Ok(ParquetRefresh::NotReusable { reason }),
    };
    let revision = repository.head_record_identifier();
    let revision_text = revision.to_string();
    if provenance.revision() == revision_text {
        return Ok(ParquetRefresh::Current {
            revision: revision_text,
        });
    }
    let differences = match diff_revisions(
        repository.directory(),
        provenance.revision(),
        &revision_text,
        provenance.root_path(),
    ) {
        Ok(differences) => differences,
        Err(error) => {
            return Ok(ParquetRefresh::NotReusable {
                reason: format!(
                    "the stamped revision {} no longer resolves ({error}); \
                     the store was likely compacted since the export",
                    provenance.revision()
                ),
            });
        }
    };
    let ranges = dirty_ranges(&differences, provenance.root_path(), depth);
    if ranges.is_empty() {
        // The head moved without touching the exported subtree — a
        // commit elsewhere or a checkpoint change — so the exported
        // rows already match the pinned revision's.
        return Ok(ParquetRefresh::Current {
            revision: revision_text,
        });
    }

    for file_name in [
        NODES_FILE_NAME,
        PROPERTIES_FILE_NAME,
        NODES_DELTA_FILE_NAME,
        PROPERTIES_DELTA_FILE_NAME,
    ] {
        sweep_temporary_outputs(output_directory, file_name)?;
    }
    let mut temporaries = TemporaryFiles::default();
    let delta_nodes =
        temporaries.track(output_directory.join(temporary_output_name(NODES_DELTA_FILE_NAME)));
    let delta_properties =
        temporaries.track(output_directory.join(temporary_output_name(PROPERTIES_DELTA_FILE_NAME)));
    let merged_nodes =
        temporaries.track(output_directory.join(temporary_output_name(NODES_FILE_NAME)));
    let merged_properties =
        temporaries.track(output_directory.join(temporary_output_name(PROPERTIES_FILE_NAME)));

    let nodes = export_delta(
        repository,
        revision,
        provenance.root_path(),
        &ranges,
        &delta_nodes,
        &delta_properties,
        options,
        on_node,
    )?;
    let new_provenance =
        ExportProvenance::new(revision_text.clone(), provenance.root_path(), depth);
    match merge_tables(
        repository,
        &output_directory.join(NODES_FILE_NAME),
        &delta_nodes,
        &merged_nodes,
        &output_directory.join(PROPERTIES_FILE_NAME),
        &delta_properties,
        &merged_properties,
        &ranges,
        &new_provenance,
        options,
    )? {
        MergeVerdict::Done => {}
        MergeVerdict::Unusable(reason) => {
            return Ok(ParquetRefresh::NotReusable { reason });
        }
    }
    replace_export_output(&merged_nodes, &output_directory.join(NODES_FILE_NAME))?;
    replace_export_output(
        &merged_properties,
        &output_directory.join(PROPERTIES_FILE_NAME),
    )?;
    Ok(ParquetRefresh::Refreshed {
        revision: revision_text,
        ranges: ranges.len() as u64,
        nodes,
    })
}

/// Validates an existing export as a refresh base: both files present,
/// stamped, stamps agreeing, root path and depth limit matching the
/// request. Every failure is a reason string, not an error — nothing
/// about a reusable-or-not verdict is exceptional.
fn validate(
    output_directory: &Path,
    root_path: &str,
    depth: Option<usize>,
) -> Result<ExportProvenance, String> {
    let mut provenances = Vec::with_capacity(2);
    for file_name in [NODES_FILE_NAME, PROPERTIES_FILE_NAME] {
        let path = output_directory.join(file_name);
        let provenance = match read_export_provenance(&path) {
            Ok(Some(provenance)) => provenance,
            Ok(None) => {
                return Err(format!(
                    "{} is a Parquet file, but not a froe export — it carries no export stamp",
                    path.display()
                ));
            }
            Err(error) => {
                let missing = matches!(
                    &error,
                    froe::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::NotFound
                );
                return Err(if missing {
                    format!("{} does not exist", path.display())
                } else {
                    format!(
                        "{} is not readable as a Parquet export: {error}",
                        path.display()
                    )
                });
            }
        };
        provenances.push(provenance);
    }
    let [nodes_provenance, properties_provenance] = provenances.try_into().expect("two files");
    if nodes_provenance != properties_provenance {
        return Err(
            "the export's two files carry different stamps; an earlier refresh must \
             have been interrupted"
                .to_owned(),
        );
    }
    let provenance = nodes_provenance;
    let requested = ExportProvenance::new(String::new(), root_path, depth);
    if provenance.root_path() != requested.root_path() {
        return Err(format!(
            "the existing export covers {}, not {}",
            provenance.root_path(),
            requested.root_path()
        ));
    }
    if provenance.depth_limit() != depth {
        let describe = |limit: Option<usize>| {
            limit.map_or_else(|| "unlimited".to_owned(), |limit| format!("depth {limit}"))
        };
        return Err(format!(
            "the existing export was {}, this request is {}",
            describe(provenance.depth_limit()),
            describe(depth)
        ));
    }
    Ok(provenance)
}

/// One dirty path range: the old rows it replaces and the replacement
/// to re-export, if any.
struct DirtyRange {
    /// The range's root path.
    path: String,
    /// Whether the range covers the whole subtree below `path` (`true`,
    /// for added and removed nodes) or only the node's own rows
    /// (`false`, for property changes).
    subtree: bool,
    /// What replaces the range's old rows.
    replacement: Replacement,
}

/// What replaces a dirty range's old rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Replacement {
    /// Nothing: the rows are excised without replacement (a removed
    /// node).
    Excise,
    /// Re-export the range's root with this depth limit — `None` for
    /// the whole subtree, `Some(0)` for just the node's own rows.
    ReExport { depth: Option<usize> },
}

/// How many path segments a normalized absolute path carries: `/` is 0,
/// `/a/b` is 2.
fn path_depth(path: &str) -> usize {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

/// Maps a diff to dirty ranges, sorted by path. Ranges beyond the
/// export's depth limit carry no rows — the old file has none there
/// and a full re-export would write none — so they are dropped; an
/// added subtree inside the limit re-exports with its remaining depth.
fn dirty_ranges(
    differences: &[NodeDifference],
    root_path: &str,
    depth_limit: Option<usize>,
) -> Vec<DirtyRange> {
    let root_depth = path_depth(root_path);
    let mut ranges = Vec::new();
    for difference in differences {
        let (path, subtree, replacement) = match difference {
            NodeDifference::NodeAdded { path } => {
                (path, true, Replacement::ReExport { depth: None })
            }
            NodeDifference::NodeRemoved { path } => (path, true, Replacement::Excise),
            NodeDifference::PropertyChanged { path, .. } => {
                (path, false, Replacement::ReExport { depth: Some(0) })
            }
        };
        let range_depth = path_depth(path).saturating_sub(root_depth);
        let replacement = match (replacement, depth_limit) {
            (Replacement::Excise, _) => Replacement::Excise,
            (Replacement::ReExport { .. }, Some(limit)) if range_depth > limit => continue,
            (Replacement::ReExport { depth: None }, Some(limit)) => Replacement::ReExport {
                depth: Some(limit - range_depth),
            },
            (reexport @ Replacement::ReExport { .. }, _) => reexport,
        };
        ranges.push(DirtyRange {
            path: path.clone(),
            subtree,
            replacement,
        });
    }
    ranges.sort_by(|first, second| first.path.cmp(&second.path));
    // The diff never reports nested or duplicated ranges, but the merge
    // relies on it: fold any duplicate defensively — the subtree shape
    // and a present replacement win. `dedup_by` passes the later
    // element first and removes it when the closure returns true, so
    // the retained earlier element accumulates the folded facts.
    ranges.dedup_by(|later, retained| {
        if later.path != retained.path {
            return false;
        }
        retained.subtree |= later.subtree;
        if retained.replacement == Replacement::Excise {
            retained.replacement = later.replacement;
        }
        true
    });
    ranges
}

/// Whether `path` lies inside `range`: the range root itself, or — for
/// subtree ranges — any descendant. The descendant test respects the
/// `/` boundary, so `/a/bc` is not under `/a/b`.
fn path_in_range(path: &str, range: &DirtyRange) -> bool {
    path == range.path || (range.subtree && path_under(path, &range.path))
}

/// Whether `path` is a proper descendant of `ancestor`.
fn path_under(path: &str, ancestor: &str) -> bool {
    if ancestor == "/" {
        return path.starts_with('/') && path != "/";
    }
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The dirty ranges indexed for containment queries: a path is dirty
/// when it is an exact range root or has a subtree range root among its
/// ancestors (itself included).
struct RangeIndex<'ranges> {
    exact: HashSet<&'ranges str>,
    subtree: HashSet<&'ranges str>,
}

impl<'ranges> RangeIndex<'ranges> {
    fn new(ranges: &'ranges [DirtyRange]) -> Self {
        let mut index = Self {
            exact: HashSet::new(),
            subtree: HashSet::new(),
        };
        for range in ranges {
            if range.subtree {
                index.subtree.insert(range.path.as_str());
            } else {
                index.exact.insert(range.path.as_str());
            }
        }
        index
    }

    /// Whether `path` falls inside any dirty range.
    fn contains(&self, path: &str) -> bool {
        if self.exact.contains(path) || self.subtree.contains(path) {
            return true;
        }
        let mut rest = path;
        while let Some((parent, _)) = rest.rsplit_once('/') {
            let parent = if parent.is_empty() { "/" } else { parent };
            if self.subtree.contains(parent) {
                return true;
            }
            if parent == "/" {
                return false;
            }
            rest = parent;
        }
        false
    }
}

/// Exports every dirty range's replacement — at the pinned revision —
/// into the delta files, returning the number of nodes written.
/// `on_node` reports the running count per node.
#[allow(
    clippy::too_many_arguments,
    reason = "the delta export threads its source, ranges, destinations, and progress"
)]
fn export_delta(
    repository: &Repository,
    revision: RecordIdentifier,
    root_path: &str,
    ranges: &[DirtyRange],
    delta_nodes: &Path,
    delta_properties: &Path,
    options: &ParquetExportOptions,
    on_node: &mut dyn FnMut(u64),
) -> froe::Result<u64> {
    let nodes_file = create_export_output(repository.directory(), delta_nodes)?;
    let properties_file = create_export_output(repository.directory(), delta_properties)?;
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
fn node_at_revision<'repository>(
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
struct DepthOffsetSink<'a, W: Write + Send> {
    inner: &'a mut ParquetSink<W>,
    offset: usize,
    written: &'a mut u64,
    on_node: &'a mut dyn FnMut(u64),
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

/// The verdict of merging: fresh merged files, or the discovery that
/// the existing files' rows do not decode as a froe export.
enum MergeVerdict {
    Done,
    Unusable(String),
}

/// Merges the old tables with the delta tables into fresh files at the
/// merged paths, stamped with `provenance`.
#[allow(
    clippy::too_many_arguments,
    reason = "two tables times three paths, plus ranges, provenance, and options"
)]
fn merge_tables(
    repository: &Repository,
    old_nodes: &Path,
    delta_nodes: &Path,
    merged_nodes: &Path,
    old_properties: &Path,
    delta_properties: &Path,
    merged_properties: &Path,
    ranges: &[DirtyRange],
    provenance: &ExportProvenance,
    options: &ParquetExportOptions,
) -> froe::Result<MergeVerdict> {
    use ::parquet::file::reader::SerializedFileReader;

    let open = |path: &Path| -> froe::Result<SerializedFileReader<std::fs::File>> {
        SerializedFileReader::new(std::fs::File::open(path)?).map_err(parquet_read_error)
    };
    let old_nodes_reader = open(old_nodes)?;
    let delta_nodes_reader = open(delta_nodes)?;
    let old_properties_reader = open(old_properties)?;
    let delta_properties_reader = open(delta_properties)?;

    // A decode failure does not abort the merge; it ends the affected
    // stream, and the flag turns the verdict into Unusable afterwards —
    // the partial merged files are then discarded and a full export
    // replaces the unparseable base.
    let failure = RefCell::new(None::<String>);

    let nodes_out = create_export_output(repository.directory(), merged_nodes)?;
    let properties_out = create_export_output(repository.directory(), merged_properties)?;
    let mut sink = ParquetSink::new_with_provenance(
        std::io::BufWriter::with_capacity(1 << 20, nodes_out),
        std::io::BufWriter::with_capacity(1 << 20, properties_out),
        options,
        provenance,
    )?;
    let index = RangeIndex::new(ranges);
    merge_row_streams(
        NodeRows::new(&old_nodes_reader, &failure)?,
        NodeRows::new(&delta_nodes_reader, &failure)?,
        ranges,
        &index,
        |row| {
            sink.append_node_row(
                &row.path,
                row.parent_path.as_deref(),
                &row.name,
                row.depth,
                row.primary_type.as_deref(),
            )
        },
    )?;
    merge_row_streams(
        PropertyRows::new(&old_properties_reader, &failure)?,
        PropertyRows::new(&delta_properties_reader, &failure)?,
        ranges,
        &index,
        |row| {
            sink.append_property_columns(
                &row.path,
                &row.name,
                &row.property_type,
                row.multiple,
                row.position,
                row.value.as_deref(),
                row.long_value,
                row.double_value,
                row.boolean_value,
                row.binary_length,
                row.binary_reference.as_deref(),
            )
        },
    )?;
    sink.finish()?;
    if let Some(reason) = failure.into_inner() {
        return Ok(MergeVerdict::Unusable(reason));
    }
    Ok(MergeVerdict::Done)
}

/// The row shape the merge needs: every table's rows carry their node
/// path.
trait MergeRow {
    /// The row's node path.
    fn path(&self) -> &str;
}

impl MergeRow for NodeRow {
    fn path(&self) -> &str {
        &self.path
    }
}

impl MergeRow for PropertyRow {
    fn path(&self) -> &str {
        &self.path
    }
}

/// Merges one table's old and delta rows into `write`, streaming.
///
/// Old rows inside a dirty range are dropped wherever they appear —
/// the containment test, not row order, decides — and each range's
/// replacement rows (contiguous in the delta, ranges exported in path
/// order) are written where the merge walk passes the range. Rows
/// therefore stay ordered exactly as far as the old file and document
/// order agree, which keeps path-column statistics selective.
fn merge_row_streams<R: MergeRow>(
    old_rows: impl Iterator<Item = froe::Result<R>>,
    delta_rows: impl Iterator<Item = froe::Result<R>>,
    ranges: &[DirtyRange],
    index: &RangeIndex<'_>,
    mut write: impl FnMut(R) -> froe::Result<()>,
) -> froe::Result<()> {
    let mut old_rows = old_rows.peekable();
    let mut delta_rows = delta_rows.peekable();
    for range in ranges {
        loop {
            match old_rows.peek() {
                Some(Ok(row)) if index.contains(row.path()) => {
                    old_rows.next();
                }
                Some(Ok(row)) if row.path() < range.path.as_str() => {
                    write(old_rows.next().expect("the peeked row")?)?;
                }
                Some(Ok(_)) | None => break,
                Some(Err(_)) => return Err(take_error(&mut old_rows)),
            }
        }
        loop {
            match delta_rows.peek() {
                Some(Ok(row)) if path_in_range(row.path(), range) => {
                    write(delta_rows.next().expect("the peeked row")?)?;
                }
                Some(Ok(_)) | None => break,
                Some(Err(_)) => return Err(take_error(&mut delta_rows)),
            }
        }
    }
    for row in old_rows {
        let row = row?;
        if !index.contains(row.path()) {
            write(row)?;
        }
    }
    if delta_rows.next().is_some() {
        return Err(froe::Error::InvalidFormat {
            details: "the refresh delta holds rows outside the changed ranges".to_owned(),
        });
    }
    Ok(())
}

/// Extracts the error a peek showed from a row stream.
fn take_error<R, I: Iterator<Item = froe::Result<R>>>(
    rows: &mut std::iter::Peekable<I>,
) -> froe::Error {
    if let Some(Err(error)) = rows.next() {
        return error;
    }
    froe::Error::InvalidFormat {
        details: "a row stream changed underneath the merge".to_owned(),
    }
}

/// An iterator decoding nodes-table rows, recording the first decode
/// failure in `failure` and ending the stream there.
struct NodeRows<'a> {
    inner: ::parquet::record::reader::RowIter<'a>,
    failure: &'a RefCell<Option<String>>,
}

impl<'a> NodeRows<'a> {
    /// Decodes nodes-table rows from `reader`.
    fn new(
        reader: &'a ::parquet::file::reader::SerializedFileReader<std::fs::File>,
        failure: &'a RefCell<Option<String>>,
    ) -> froe::Result<Self> {
        use ::parquet::file::reader::FileReader;
        Ok(Self {
            inner: reader.get_row_iter(None).map_err(parquet_read_error)?,
            failure,
        })
    }
}

impl Iterator for NodeRows<'_> {
    type Item = froe::Result<NodeRow>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next()? {
            Ok(row) => {
                if let Some(decoded) = NodeRow::decode(&row) {
                    Some(Ok(decoded))
                } else {
                    record_decode_failure(self.failure);
                    None
                }
            }
            Err(error) => Some(Err(parquet_read_error(error))),
        }
    }
}

/// An iterator decoding properties-table rows, recording the first
/// decode failure in `failure` and ending the stream there.
struct PropertyRows<'a> {
    inner: ::parquet::record::reader::RowIter<'a>,
    failure: &'a RefCell<Option<String>>,
}

impl<'a> PropertyRows<'a> {
    /// Decodes properties-table rows from `reader`.
    fn new(
        reader: &'a ::parquet::file::reader::SerializedFileReader<std::fs::File>,
        failure: &'a RefCell<Option<String>>,
    ) -> froe::Result<Self> {
        use ::parquet::file::reader::FileReader;
        Ok(Self {
            inner: reader.get_row_iter(None).map_err(parquet_read_error)?,
            failure,
        })
    }
}

impl Iterator for PropertyRows<'_> {
    type Item = froe::Result<PropertyRow>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next()? {
            Ok(row) => {
                if let Some(decoded) = PropertyRow::decode(&row) {
                    Some(Ok(decoded))
                } else {
                    record_decode_failure(self.failure);
                    None
                }
            }
            Err(error) => Some(Err(parquet_read_error(error))),
        }
    }
}

/// Records the first decode failure; later failures keep the first's
/// message.
fn record_decode_failure(failure: &RefCell<Option<String>>) {
    let mut failure = failure.borrow_mut();
    if failure.is_none() {
        *failure = Some(
            "an export file's rows do not match the export schema; \
             the files were not written by this export"
                .to_owned(),
        );
    }
}

/// Wraps a Parquet read error as an output error.
fn parquet_read_error(error: ::parquet::errors::ParquetError) -> froe::Error {
    froe::Error::InputOutput(std::io::Error::other(error))
}

/// Temporary files removed when the guard drops — after success the
/// renames have already moved the merged files out of the list's
/// paths, so only genuine leftovers (a failed delta, an interrupted
/// merge) are swept.
#[derive(Default)]
struct TemporaryFiles(Vec<PathBuf>);

impl TemporaryFiles {
    /// Registers `path` for removal and returns it.
    fn track(&mut self, path: PathBuf) -> PathBuf {
        self.0.push(path.clone());
        path
    }
}

impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use froe::content::{PropertyState, PropertyType, PropertyValue, PropertyValues};
    use froe::store::Repository;
    use froe::tooling::diff::{NodeDifference, PropertyChange};
    use froe::writer::StoreSink;
    use froe::writer::record_writer::{
        ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter,
    };
    use froe::writer::store_writer::WritableRepository;

    use super::{
        DirtyRange, MergeRow, ParquetRefresh, RangeIndex, Replacement, dirty_ranges,
        merge_row_streams, path_in_range, path_under, refresh_parquet_export,
    };
    use crate::export::export_subtree;
    use crate::parquet::{
        ExportProvenance, NodeRow, ParquetExportOptions, ParquetSink, PropertyRow,
        read_export_provenance,
    };

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-refresh-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create");
            Self { path }
        }

        fn store(&self) -> PathBuf {
            self.path.join("segmentstore")
        }

        fn export(&self) -> PathBuf {
            self.path.join("export")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A record writer with terse helpers for the fixture trees.
    struct RevisionWriter<'store> {
        writer: RecordWriter<StoreSink<'store>>,
    }

    impl RevisionWriter<'_> {
        fn property(
            &mut self,
            name: &str,
            property_type: PropertyType,
            text: &str,
        ) -> PropertyToWrite {
            let value = self.writer.write_string(text).expect("write string");
            PropertyToWrite {
                name: name.to_owned(),
                property_type,
                values: PropertyValuesToWrite::Single(value),
            }
        }

        fn binary(&mut self, name: &str, content: &[u8]) -> PropertyToWrite {
            let value = self
                .writer
                .write_binary_content(content)
                .expect("write binary");
            PropertyToWrite {
                name: name.to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(value),
            }
        }

        fn node(
            &mut self,
            properties: &[PropertyToWrite],
            children: &ChildNodesToWrite,
        ) -> froe::RecordIdentifier {
            self.writer
                .write_node(Some("nt:unstructured"), &[], children, properties)
                .expect("write node")
        }

        fn leaf(&mut self, properties: &[PropertyToWrite]) -> froe::RecordIdentifier {
            self.node(properties, &ChildNodesToWrite::Zero)
        }

        fn child(
            &mut self,
            name: &str,
            node: froe::RecordIdentifier,
            properties: &[PropertyToWrite],
        ) -> froe::RecordIdentifier {
            self.node(
                properties,
                &ChildNodesToWrite::One {
                    name: name.to_owned(),
                    node,
                },
            )
        }
    }

    /// Commits one revision: `build` produces the content root record,
    /// and the helper wraps it in root and super-root nodes and advances
    /// the head.
    fn revise(directory: &Path, build: impl FnOnce(&mut RevisionWriter) -> froe::RecordIdentifier) {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = RevisionWriter {
            writer: store.record_writer(generation),
        };
        let root = build(&mut writer);
        let head = writer.child("root", root, &[]);
        writer.writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        store.close().expect("close");
    }

    /// The first fixture revision: `/content` with typed properties, a
    /// `jcr:content` child, a `kept` subtree, and a `subtree` subtree.
    /// `build` returns the content root, so the last node wraps
    /// `/content` into it.
    fn populate_first(directory: &Path) {
        revise(directory, |writer| {
            let ratio = writer.property("ratio", PropertyType::Double, "2.5");
            let jcr_content = writer.leaf(&[ratio]);
            let name = writer.property("name", PropertyType::String, "leaf");
            let leaf = writer.leaf(&[name]);
            let kept = writer.child("leaf", leaf, &[]);
            let x = writer.leaf(&[]);
            let flag = writer.property("flag", PropertyType::Boolean, "true");
            let subtree = writer.child("x", x, &[flag]);
            let title = writer.property("title", PropertyType::String, "Hello");
            let count = writer.property("count", PropertyType::Long, "42");
            let data = writer.binary("data", &[1, 2, 3]);
            let content = writer.node(
                &[title, count, data],
                &ChildNodesToWrite::Many(vec![
                    ("jcr:content".to_owned(), jcr_content),
                    ("kept".to_owned(), kept),
                    ("subtree".to_owned(), subtree),
                ]),
            );
            writer.child("content", content, &[])
        });
    }

    /// The second fixture revision: `title` changed on `/content`,
    /// `extra` added on `/content/jcr:content`, `/content/subtree`
    /// removed, `/content/added/deep/x` added, `/content/kept`
    /// byte-identical under a fresh record.
    fn populate_second(directory: &Path) {
        revise(directory, |writer| {
            let x = writer.leaf(&[]);
            let deep = writer.child("x", x, &[]);
            let added = writer.child("deep", deep, &[]);
            let ratio = writer.property("ratio", PropertyType::Double, "2.5");
            let extra = writer.property("extra", PropertyType::String, "new");
            let jcr_content = writer.leaf(&[ratio, extra]);
            let name = writer.property("name", PropertyType::String, "leaf");
            let leaf = writer.leaf(&[name]);
            let kept = writer.child("leaf", leaf, &[]);
            let title = writer.property("title", PropertyType::String, "Goodbye");
            let count = writer.property("count", PropertyType::Long, "42");
            let data = writer.binary("data", &[1, 2, 3]);
            let content = writer.node(
                &[title, count, data],
                &ChildNodesToWrite::Many(vec![
                    ("added".to_owned(), added),
                    ("jcr:content".to_owned(), jcr_content),
                    ("kept".to_owned(), kept),
                ]),
            );
            writer.child("content", content, &[])
        });
    }

    /// The head revision of the store in text form.
    fn head_revision(directory: &Path) -> String {
        Repository::open(directory)
            .expect("open")
            .head_record_identifier()
            .to_string()
    }

    /// Runs a full export of the store into `output`, returning the
    /// stamped revision. `stamped_revision` overrides the stamp, for
    /// provenance-fixture tests.
    fn full_export(
        store: &Path,
        root_path: &str,
        depth: Option<usize>,
        output: &Path,
        stamped_revision: Option<String>,
    ) -> String {
        std::fs::create_dir_all(output).expect("create export directory");
        let repository = Repository::open(store).expect("open");
        let revision = repository.head_record_identifier().to_string();
        let provenance = ExportProvenance::new(
            stamped_revision.unwrap_or_else(|| revision.clone()),
            root_path,
            depth,
        );
        let nodes = std::fs::File::create(output.join("nodes.parquet")).expect("nodes file");
        let properties =
            std::fs::File::create(output.join("properties.parquet")).expect("properties file");
        let mut sink = ParquetSink::new_with_provenance(
            nodes,
            properties,
            &ParquetExportOptions::default(),
            &provenance,
        )
        .expect("sink");
        export_subtree(&repository, root_path, depth, &mut sink).expect("export");
        revision
    }

    /// A full export without the provenance stamp — the shape a plain
    /// `ParquetSink` produces.
    fn full_export_without_stamp(store: &Path, root_path: &str, output: &Path) {
        std::fs::create_dir_all(output).expect("create export directory");
        let repository = Repository::open(store).expect("open");
        let nodes = std::fs::File::create(output.join("nodes.parquet")).expect("nodes file");
        let properties =
            std::fs::File::create(output.join("properties.parquet")).expect("properties file");
        let mut sink =
            ParquetSink::new(nodes, properties, &ParquetExportOptions::default()).expect("sink");
        export_subtree(&repository, root_path, None, &mut sink).expect("export");
    }

    fn refresh(
        store: &Path,
        root_path: &str,
        depth: Option<usize>,
        output: &Path,
    ) -> ParquetRefresh {
        let repository = Repository::open(store).expect("open");
        refresh_parquet_export(
            &repository,
            root_path,
            depth,
            output,
            &ParquetExportOptions::default(),
            &mut |_| {},
        )
        .expect("refresh")
    }

    /// Reads back a table's rows, sorted so order plays no part.
    fn node_rows(output: &Path) -> Vec<NodeRow> {
        use ::parquet::file::reader::{FileReader, SerializedFileReader};
        let reader = SerializedFileReader::new(
            std::fs::File::open(output.join("nodes.parquet")).expect("open"),
        )
        .expect("reader");
        let mut rows: Vec<NodeRow> = reader
            .get_row_iter(None)
            .expect("rows")
            .map(|row| NodeRow::decode(&row.expect("row")).expect("decode"))
            .collect();
        rows.sort_by(|first, second| first.path.cmp(&second.path));
        rows
    }

    /// Reads back the properties table's rows, sorted so order plays no
    /// part.
    fn property_rows(output: &Path) -> Vec<PropertyRow> {
        use ::parquet::file::reader::{FileReader, SerializedFileReader};
        let reader = SerializedFileReader::new(
            std::fs::File::open(output.join("properties.parquet")).expect("open"),
        )
        .expect("reader");
        let mut rows: Vec<PropertyRow> = reader
            .get_row_iter(None)
            .expect("rows")
            .map(|row| PropertyRow::decode(&row.expect("row")).expect("decode"))
            .collect();
        rows.sort_by(|first, second| {
            (&first.path, &first.name, first.position).cmp(&(
                &second.path,
                &second.name,
                second.position,
            ))
        });
        rows
    }

    #[test]
    fn a_refresh_reproduces_a_full_export() {
        let directory = TestDirectory::new("round-trip");
        populate_first(&directory.store());
        let first_revision = full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        assert_eq!(
            read_export_provenance(&directory.export().join("nodes.parquet"))
                .expect("read")
                .expect("stamped")
                .revision(),
            first_revision,
            "the full export stamps its revision"
        );

        populate_second(&directory.store());
        let second_revision = head_revision(&directory.store());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        assert_eq!(
            outcome,
            ParquetRefresh::Refreshed {
                revision: second_revision.clone(),
                ranges: 4,
                nodes: 5,
            },
            "two property changes, one addition, one removal; five re-exported nodes"
        );

        let reference = directory.path.join("reference");
        full_export(&directory.store(), "/content", None, &reference, None);
        assert_eq!(
            node_rows(&directory.export()),
            node_rows(&reference),
            "the refreshed nodes table equals a full export's"
        );
        assert_eq!(
            property_rows(&directory.export()),
            property_rows(&reference),
            "the refreshed properties table equals a full export's"
        );
        assert_eq!(
            read_export_provenance(&directory.export().join("properties.parquet"))
                .expect("read")
                .expect("stamped")
                .revision(),
            second_revision,
            "the refreshed export stamps the new revision"
        );
        // No refresh residue may linger in the export directory.
        let leftovers: Vec<_> = std::fs::read_dir(directory.export())
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            2,
            "only the two tables remain: {leftovers:?}"
        );
    }

    #[test]
    fn an_unchanged_head_reports_current() {
        let directory = TestDirectory::new("unchanged");
        populate_first(&directory.store());
        let revision = full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        let before = std::fs::read(directory.export().join("nodes.parquet")).expect("read");

        assert_eq!(
            refresh(&directory.store(), "/content", None, &directory.export()),
            ParquetRefresh::Current { revision },
        );
        assert_eq!(
            std::fs::read(directory.export().join("nodes.parquet")).expect("read"),
            before,
            "a current export is not rewritten"
        );
    }

    #[test]
    fn a_checkpoint_only_head_move_reports_current() {
        let directory = TestDirectory::new("checkpoint-only");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );

        let store = WritableRepository::open(&directory.store()).expect("open");
        froe::writer::create_checkpoint(&store, 3_600_000, &[]).expect("checkpoint");
        store.close().expect("close");
        let revision = head_revision(&directory.store());
        assert_eq!(
            refresh(&directory.store(), "/content", None, &directory.export()),
            ParquetRefresh::Current { revision },
            "a checkpoint moves the head without touching the content tree"
        );
    }

    #[test]
    fn a_removed_export_root_refreshes_to_empty_tables() {
        let directory = TestDirectory::new("root-removed");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );

        revise(&directory.store(), |writer| writer.leaf(&[]));
        assert_eq!(
            refresh(&directory.store(), "/content", None, &directory.export()),
            ParquetRefresh::Refreshed {
                revision: head_revision(&directory.store()),
                ranges: 1,
                nodes: 0,
            },
        );
        assert!(node_rows(&directory.export()).is_empty());
        assert!(property_rows(&directory.export()).is_empty());
    }

    #[test]
    fn a_depth_limited_export_refreshes_within_the_limit() {
        let directory = TestDirectory::new("depth-limited");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            Some(1),
            &directory.export(),
            None,
        );
        let nodes_before = node_rows(&directory.export());
        assert_eq!(nodes_before.len(), 4, "depth 1 keeps the children only");

        populate_second(&directory.store());
        let outcome = refresh(&directory.store(), "/content", Some(1), &directory.export());
        assert!(
            matches!(outcome, ParquetRefresh::Refreshed { .. }),
            "the limited export refreshes: {outcome:?}"
        );

        let reference = directory.path.join("reference");
        full_export(&directory.store(), "/content", Some(1), &reference, None);
        assert_eq!(node_rows(&directory.export()), node_rows(&reference));
        assert_eq!(
            property_rows(&directory.export()),
            property_rows(&reference)
        );
        assert!(
            node_rows(&directory.export())
                .iter()
                .all(|row| row.depth <= 1),
            "nothing below the limit appears"
        );
    }

    #[test]
    fn a_missing_export_is_not_reusable() {
        let directory = TestDirectory::new("missing");
        populate_first(&directory.store());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable { reason } = outcome else {
            panic!("no files, no refresh: {outcome:?}");
        };
        assert!(reason.contains("does not exist"), "the reason: {reason}");
    }

    #[test]
    fn a_stampless_export_is_not_reusable() {
        let directory = TestDirectory::new("stampless");
        populate_first(&directory.store());
        full_export_without_stamp(&directory.store(), "/content", &directory.export());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable { reason } = outcome else {
            panic!("an unstamped file is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("no export stamp"), "the reason: {reason}");
    }

    #[test]
    fn disagreeing_stamps_are_not_reusable() {
        let directory = TestDirectory::new("disagreeing");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        // Rewrite the properties table alone with a different stamp, the
        // residue an interrupted refresh would leave.
        let other = directory.path.join("other");
        full_export(
            &directory.store(),
            "/content",
            None,
            &other,
            Some("00000000-0000-0000-0000-000000000000.00000001".to_owned()),
        );
        std::fs::copy(
            other.join("properties.parquet"),
            directory.export().join("properties.parquet"),
        )
        .expect("copy");

        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable { reason } = outcome else {
            panic!("disagreeing stamps are no refresh base: {outcome:?}");
        };
        assert!(reason.contains("different stamps"), "the reason: {reason}");
    }

    #[test]
    fn a_compacted_away_revision_is_not_reusable() {
        let directory = TestDirectory::new("stale-revision");
        populate_first(&directory.store());
        // A well-formed revision naming a segment the store never held
        // stands in for a compacted-away one: the diff cannot resolve it.
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            Some("00000000-0000-0000-0000-000000000000.00000001".to_owned()),
        );
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable { reason } = outcome else {
            panic!("an unresolvable base revision is no refresh base: {outcome:?}");
        };
        assert!(
            reason.contains("no longer resolves"),
            "the reason: {reason}"
        );
    }

    #[test]
    fn a_different_root_path_is_not_reusable() {
        let directory = TestDirectory::new("other-root");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        let outcome = refresh(&directory.store(), "/", None, &directory.export());
        let ParquetRefresh::NotReusable { reason } = outcome else {
            panic!("a different subtree is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("covers /content"), "the reason: {reason}");
    }

    #[test]
    fn a_different_depth_limit_is_not_reusable() {
        let directory = TestDirectory::new("other-depth");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        let outcome = refresh(&directory.store(), "/content", Some(2), &directory.export());
        let ParquetRefresh::NotReusable { reason } = outcome else {
            panic!("a different depth limit is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("unlimited"), "the reason: {reason}");
    }

    // ---- pure-logic unit tests --------------------------------------

    fn property_change(path: &str) -> NodeDifference {
        NodeDifference::PropertyChanged {
            path: path.to_owned(),
            change: PropertyChange::Added(PropertyState {
                name: "p".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValues::Single(PropertyValue::String("v".to_owned())),
            }),
        }
    }

    fn range_paths(ranges: &[DirtyRange]) -> Vec<(&str, bool, Replacement)> {
        ranges
            .iter()
            .map(|range| (range.path.as_str(), range.subtree, range.replacement))
            .collect()
    }

    #[test]
    fn dirty_ranges_map_the_difference_kinds() {
        let differences = vec![
            property_change("/root/b"),
            NodeDifference::NodeRemoved {
                path: "/root/a".to_owned(),
            },
            NodeDifference::NodeAdded {
                path: "/root/c".to_owned(),
            },
        ];
        let ranges = dirty_ranges(&differences, "/root", None);
        assert_eq!(
            range_paths(&ranges),
            vec![
                ("/root/a", true, Replacement::Excise),
                ("/root/b", false, Replacement::ReExport { depth: Some(0) }),
                ("/root/c", true, Replacement::ReExport { depth: None }),
            ],
            "sorted by path; removals excise, changes re-export the node, additions the subtree"
        );
    }

    #[test]
    fn dirty_ranges_apply_the_depth_limit() {
        let differences = vec![
            NodeDifference::NodeAdded {
                path: "/root/a".to_owned(),
            },
            NodeDifference::NodeAdded {
                path: "/root/a/b".to_owned(),
            },
            property_change("/root/a/b/c"),
        ];
        let ranges = dirty_ranges(&differences, "/root", Some(1));
        assert_eq!(
            range_paths(&ranges),
            vec![("/root/a", true, Replacement::ReExport { depth: Some(0) })],
            "an addition at the limit keeps its root only; deeper ranges carry no rows"
        );
        let wider = dirty_ranges(&differences, "/root", Some(2));
        assert_eq!(
            range_paths(&wider),
            vec![
                ("/root/a", true, Replacement::ReExport { depth: Some(1) }),
                ("/root/a/b", true, Replacement::ReExport { depth: Some(0) }),
            ],
            "additions inside the limit re-export with their remaining depth"
        );
    }

    #[test]
    fn dirty_ranges_fold_duplicates_defensively() {
        let differences = vec![
            NodeDifference::NodeRemoved {
                path: "/root/a".to_owned(),
            },
            property_change("/root/a"),
        ];
        let ranges = dirty_ranges(&differences, "/root", None);
        assert_eq!(
            range_paths(&ranges),
            vec![("/root/a", true, Replacement::ReExport { depth: Some(0) })],
        );
    }

    #[test]
    fn range_containment_respects_the_slash_boundary() {
        let subtree = DirtyRange {
            path: "/a/b".to_owned(),
            subtree: true,
            replacement: Replacement::Excise,
        };
        let exact = DirtyRange {
            path: "/a/b".to_owned(),
            subtree: false,
            replacement: Replacement::Excise,
        };
        assert!(path_in_range("/a/b", &exact));
        assert!(path_in_range("/a/b/c", &subtree));
        assert!(!path_in_range("/a/b/c", &exact));
        assert!(!path_in_range("/a/bc", &subtree), "/a/bc is not under /a/b");
        assert!(!path_in_range("/a", &subtree));
        assert!(path_under("/anything", "/"));
        assert!(!path_under("/", "/"));
    }

    #[test]
    fn the_range_index_finds_containing_ranges() {
        let ranges = [
            DirtyRange {
                path: "/a".to_owned(),
                subtree: true,
                replacement: Replacement::Excise,
            },
            DirtyRange {
                path: "/b/c".to_owned(),
                subtree: false,
                replacement: Replacement::Excise,
            },
            DirtyRange {
                path: "/".to_owned(),
                subtree: true,
                replacement: Replacement::Excise,
            },
        ];
        let index = RangeIndex::new(&ranges[..2]);
        assert!(index.contains("/a/deep/path"));
        assert!(index.contains("/b/c"));
        assert!(!index.contains("/b/c/d"));
        assert!(!index.contains("/b"));
        let root_index = RangeIndex::new(&ranges[2..]);
        assert!(root_index.contains("/anything/at/all"));
        assert!(root_index.contains("/"));
    }

    /// A merge test row: just a path.
    struct TestRow(&'static str);

    impl MergeRow for TestRow {
        fn path(&self) -> &str {
            self.0
        }
    }

    fn merged(
        old: Vec<&'static str>,
        delta: Vec<&'static str>,
        ranges: &[DirtyRange],
    ) -> Vec<&'static str> {
        let index = RangeIndex::new(ranges);
        let mut out = Vec::new();
        merge_row_streams(
            old.into_iter().map(|path| Ok(TestRow(path))),
            delta.into_iter().map(|path| Ok(TestRow(path))),
            ranges,
            &index,
            |row| {
                out.push(row.0);
                Ok(())
            },
        )
        .expect("merge");
        out
    }

    fn excise(path: &str) -> DirtyRange {
        DirtyRange {
            path: path.to_owned(),
            subtree: true,
            replacement: Replacement::Excise,
        }
    }

    #[test]
    fn the_merge_without_ranges_passes_the_old_rows_through() {
        assert_eq!(
            merged(vec!["/a", "/a/b"], Vec::new(), &[]),
            vec!["/a", "/a/b"]
        );
    }

    #[test]
    fn the_merge_excises_a_subtree() {
        assert_eq!(
            merged(
                vec!["/a", "/a/b", "/a/b/x", "/a/c"],
                Vec::new(),
                &[excise("/a/b")],
            ),
            vec!["/a", "/a/c"]
        );
    }

    #[test]
    fn the_merge_filters_dirty_rows_wherever_they_sort() {
        // Non-alphabetical storage order: the dirty subtree's rows sit
        // after a row that sorts past the range.
        assert_eq!(
            merged(
                vec!["/a/z", "/a/m/1", "/a/m/2", "/a/a"],
                Vec::new(),
                &[excise("/a/m")],
            ),
            vec!["/a/z", "/a/a"],
            "containment, not position, decides — dirty rows never leak"
        );
    }

    #[test]
    fn the_merge_injects_an_added_subtree() {
        assert_eq!(
            merged(
                vec!["/a", "/a/c"],
                vec!["/a/b", "/a/b/x"],
                &[DirtyRange {
                    path: "/a/b".to_owned(),
                    subtree: true,
                    replacement: Replacement::ReExport { depth: None },
                }],
            ),
            vec!["/a", "/a/b", "/a/b/x", "/a/c"]
        );
    }

    #[test]
    fn the_merge_replaces_a_changed_nodes_own_rows_only() {
        assert_eq!(
            merged(
                vec!["/a", "/a/b", "/a/b/c"],
                vec!["/a/b"],
                &[DirtyRange {
                    path: "/a/b".to_owned(),
                    subtree: false,
                    replacement: Replacement::ReExport { depth: Some(0) },
                }],
            ),
            vec!["/a", "/a/b", "/a/b/c"],
            "the descendant row survives the exact replacement"
        );
    }

    #[test]
    fn the_merge_handles_multiple_ranges() {
        assert_eq!(
            merged(
                vec!["/r", "/r/a", "/r/a/x", "/r/b", "/r/c"],
                vec!["/r/a", "/r/d", "/r/d/y"],
                &[
                    DirtyRange {
                        path: "/r/a".to_owned(),
                        subtree: true,
                        replacement: Replacement::ReExport { depth: Some(0) },
                    },
                    excise("/r/c"),
                    DirtyRange {
                        path: "/r/d".to_owned(),
                        subtree: true,
                        replacement: Replacement::ReExport { depth: None },
                    },
                ],
            ),
            vec!["/r", "/r/a", "/r/b", "/r/d", "/r/d/y"],
            "/r/a's subtree collapses to its new root row, /r/c is excised, /r/d injected"
        );
    }

    #[test]
    fn the_merge_refuses_leftover_delta_rows() {
        let index = RangeIndex::new(&[]);
        let result = merge_row_streams(
            Vec::new().into_iter().map(|path| Ok(TestRow(path))),
            vec!["/stray"].into_iter().map(|path| Ok(TestRow(path))),
            &[],
            &index,
            |_| Ok(()),
        );
        assert!(
            result.is_err(),
            "delta rows outside every range are an error"
        );
    }

    #[test]
    fn the_merge_propagates_stream_errors() {
        let failing: Vec<froe::Result<TestRow>> = vec![Err(froe::Error::InvalidFormat {
            details: "corrupt".to_owned(),
        })];
        let index = RangeIndex::new(&[]);
        let result = merge_row_streams(
            failing.into_iter(),
            Vec::new().into_iter(),
            &[],
            &index,
            |_| Ok(()),
        );
        assert!(result.is_err());
    }
}
