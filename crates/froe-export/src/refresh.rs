//! Refreshing an existing Parquet export in place.
//!
//! A full export decodes the whole content tree; a refresh decodes only
//! what changed since the export was taken. The mechanism:
//!
//! 1. **Lock.** The output directory's advisory lock
//!    ([`lock_export_directory`]) serializes concurrent froe exports,
//!    so no live writer's temporaries are swept and no rival's renames
//!    interleave the refresh.
//! 2. **Validate.** Both files of an existing export carry an
//!    [`ExportProvenance`] stamp in their footers. The files are a
//!    usable base when both stamps exist, agree with each other, and
//!    match the requested root path and depth limit. Anything else —
//!    missing files, foreign Parquet files, disagreeing stamps (the
//!    residue of an interrupted refresh) — makes the export
//!    [`ParquetRefresh::NotReusable`]; its `replaceable` flag records
//!    whether a full export may replace the files without being asked
//!    (they are absent or verifiably froe's own at the requested
//!    scope) or must wait for an explicit rebuild.
//! 3. **Diff.** The store's head is pinned once
//!    ([`Repository::head_record_identifier`]), so a live repository
//!    cannot tear the refresh, and [`diff_revisions_visiting`] between
//!    the stamped and the pinned revision yields the changed paths. The
//!    diff prunes unchanged subtrees by record identifier, so this
//!    walks only the divergent spine, and each difference is folded into
//!    a dirty range as it arrives rather than collected first.
//! 4. **Delta.** Changed paths become *dirty ranges*: an added node
//!    re-exports its whole subtree, a removed node excises its subtree's
//!    rows, a property change re-exports just that node's rows. The
//!    replacements are exported — at the pinned revision — into
//!    temporary delta files.
//! 5. **Merge.** Old rows and delta rows merge into fresh files: old
//!    rows inside a dirty range are dropped, the range's replacement
//!    rows are written in their place. The base files' stamps are
//!    validated on the very readers the merge consumes — an open
//!    handle keeps its bytes even if its pathname is replaced, so a
//!    base swapped by a writer outside the lock is caught, never
//!    merged under the new head. Rows stay nearly document-ordered,
//!    keeping path-column statistics selective.
//! 6. **Swap.** The merged files replace the old ones
//!    ([`replace_export_output`]), each rename atomic and durable.
//!    The *pair* is not one transaction — a crash or a concurrent
//!    reader between the two renames can observe new nodes with old
//!    properties — so the stamps exist in both files: a later froe run
//!    detects the disagreement and rebuilds, and query-side tooling
//!    can compare every `froe.*` stamp key across both footers for a
//!    consistent pair before trusting a result.
//!
//! The result is exactly the row set a full export of the pinned
//! revision would produce; a refresh never leaves stale rows behind.
//!
//! When the stamp already names the head, the export is
//! [`ParquetRefresh::Current`] on the strength of the footers alone —
//! validating row payloads would mean reading the whole dataset, the
//! very cost a refresh avoids. A footer-intact but row-corrupt file is
//! therefore reported current; the repair for that is a full export.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use froe::content::node::NodeState;
use froe::progress::DiscardedProgress;
use froe::segment::record::RecordIdentifier;
use froe::store::Repository;
use froe::tooling::diff::{NodeDifference, diff_revisions_visiting};

use crate::export::{ExportSink, ExportedNode, export_node};
use crate::output_file::{
    create_export_directory, create_export_output, lock_export_directory, replace_export_output,
    sweep_temporary_outputs, temporary_output_name,
};
use crate::parquet::{
    ExportProvenance, NodeRow, ParquetExportOptions, ParquetSink, PropertyRow, provenance_of,
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
    /// The existing files cannot serve as a refresh base.
    NotReusable {
        /// Why the files are unusable, phrased for the operator.
        reason: String,
        /// Whether a full export may replace the files uninvited: they
        /// are absent, or they validated as this tool's own export of
        /// the requested subtree (stale, partial, or corrupt refresh
        /// residue). Foreign files and exports of a different scope
        /// stay untouched until the caller passes the explicit rebuild
        /// flag — replacing them uninvited could destroy data froe
        /// does not own.
        replaceable: bool,
    },
    /// The exported path does not exist at the head revision — the
    /// same verdict a full export reports for a missing path, with the
    /// existing export left untouched.
    Missing,
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
    let _lock = lock_export_directory(output_directory)?;
    let ValidatedBase { provenance, base } =
        match validate(repository, output_directory, root_path, depth) {
            Ok(base) => base,
            Err(rejection) => {
                return Ok(ParquetRefresh::NotReusable {
                    reason: rejection.reason,
                    replaceable: rejection.replaceable,
                });
            }
        };
    let revision = repository.head_record_identifier();
    let revision_text = revision.to_string();
    if provenance.revision() == revision_text {
        return Ok(ParquetRefresh::Current {
            revision: revision_text,
        });
    }
    // Folded as the diff produces them: the change set is reduced to dirty
    // ranges and then discarded, so collecting it first only meant holding
    // the larger of the two representations alongside the smaller.
    let mut ranges = Vec::new();
    let mut root_removed = false;
    match diff_revisions_visiting(
        repository.directory(),
        provenance.revision(),
        &revision_text,
        provenance.root_path(),
        &mut DiscardedProgress,
        &mut |difference| {
            // A removed export root is the full export's missing-path
            // verdict: definitive — no retry can change it — and the
            // existing tables stay untouched.
            if matches!(
                &difference,
                NodeDifference::NodeRemoved { path } if path == provenance.root_path()
            ) {
                root_removed = true;
            }
            if let Some(range) = dirty_range_for(&difference, provenance.root_path(), depth) {
                ranges.push(range);
            }
        },
    ) {
        Ok(()) => {}
        Err(error) => {
            // Validation resolved the stamped revision moments ago, so
            // this is a segment vanishing mid-refresh — a compaction
            // racing us. The base is unusable but undeniably ours.
            return Ok(ParquetRefresh::NotReusable {
                reason: format!(
                    "the stamped revision {} no longer resolves ({error}); \
                     the store was likely compacted since the export",
                    provenance.revision()
                ),
                replaceable: true,
            });
        }
    }
    if root_removed {
        return Ok(ParquetRefresh::Missing);
    }
    normalize_dirty_ranges(&mut ranges);
    if ranges.is_empty() {
        // The head moved without touching the exported subtree — a
        // commit elsewhere or a checkpoint change — so the exported
        // rows already match the pinned revision's.
        return Ok(ParquetRefresh::Current {
            revision: revision_text,
        });
    }
    apply_ranges(
        repository,
        &provenance,
        &base,
        revision,
        &revision_text,
        &ranges,
        depth,
        output_directory,
        options,
        on_node,
    )
}

/// The back half of a refresh: delta export, merge, and the atomic
/// swap, under the caller's export lock. The validated base readers
/// stay open from inspection through the merge.
#[allow(
    clippy::too_many_arguments,
    reason = "the phases thread their source, ranges, scope, and destination"
)]
fn apply_ranges(
    repository: &Repository,
    provenance: &ExportProvenance,
    base: &[::parquet::file::reader::SerializedFileReader<std::fs::File>; 2],
    revision: RecordIdentifier,
    revision_text: &str,
    ranges: &[DirtyRange],
    depth: Option<usize>,
    output_directory: &Path,
    options: &ParquetExportOptions,
    on_node: &mut dyn FnMut(u64),
) -> froe::Result<ParquetRefresh> {
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
        ranges,
        &delta_nodes,
        &delta_properties,
        options,
        on_node,
    )?;
    let [base_nodes, base_properties] = base;
    let new_provenance =
        ExportProvenance::new(revision_text.to_owned(), provenance.root_path(), depth);
    match merge_tables(
        repository,
        base_nodes,
        &delta_nodes,
        &merged_nodes,
        base_properties,
        &delta_properties,
        &merged_properties,
        ranges,
        &new_provenance,
        options,
    )? {
        MergeVerdict::Done => {}
        MergeVerdict::Unusable(reason) => {
            return Ok(ParquetRefresh::NotReusable {
                reason,
                replaceable: true,
            });
        }
    }
    replace_export_output(&merged_nodes, &output_directory.join(NODES_FILE_NAME))?;
    replace_export_output(
        &merged_properties,
        &output_directory.join(PROPERTIES_FILE_NAME),
    )?;
    Ok(ParquetRefresh::Refreshed {
        revision: revision_text.to_owned(),
        ranges: ranges.len() as u64,
        nodes,
    })
}

/// Why an existing export is not a refresh base, classified for the
/// caller's replace decision.
struct Rejection {
    /// The operator-facing reason.
    reason: String,
    /// Whether a full export may replace the files uninvited: they are
    /// absent, or verifiably froe's own export of the requested scope
    /// (possibly interrupted). Foreign files and other scopes require
    /// the explicit rebuild flag.
    replaceable: bool,
}

/// One table file's ownership state.
enum TableFile {
    /// No directory entry at the path at all.
    Missing,
    /// A readable, stamped froe export file. The reader is retained
    /// from inspection on, so the bytes a refresh validates are the
    /// bytes it merges — a pathname swap can never substitute them.
    Stamped {
        /// The file's stamped provenance.
        provenance: ExportProvenance,
        /// The open reader the merge consumes.
        reader: ::parquet::file::reader::SerializedFileReader<std::fs::File>,
    },
    /// Present but not a demonstrably froe-owned regular Parquet file:
    /// unreadable, unstamped, a symlink (dangling symlinks fail
    /// `File::open` with `NotFound` and would otherwise masquerade as
    /// [`TableFile::Missing`]), or another non-regular entry such as a
    /// FIFO — which must never reach a blocking `File::open`.
    Foreign(String),
}

/// Inspects one table file, retaining the reader of a stamped file.
fn inspect_table(path: &Path) -> TableFile {
    use ::parquet::file::reader::SerializedFileReader;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return TableFile::Missing;
        }
        Err(error) => {
            return TableFile::Foreign(format!("{} cannot be inspected: {error}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        return TableFile::Foreign(format!("{} is not a regular file", path.display()));
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return TableFile::Foreign(format!("{} cannot be read: {error}", path.display()));
        }
    };
    match SerializedFileReader::new(file) {
        Ok(reader) => match provenance_of(&reader) {
            Some(provenance) => TableFile::Stamped { provenance, reader },
            None => TableFile::Foreign(format!(
                "{} is a Parquet file, but not a froe export — it carries no export stamp",
                path.display()
            )),
        },
        Err(error) => TableFile::Foreign(format!(
            "{} is not readable as a Parquet export: {error}",
            path.display()
        )),
    }
}

/// The classification of an export directory against a requested
/// export, deciding both refreshability and replacement authorization.
enum Classification {
    /// Both files are present, stamped, in scope, and agreeing; the
    /// open readers ride along to the merge.
    Reusable {
        /// The agreed provenance.
        provenance: ExportProvenance,
        /// The nodes and properties readers, open since inspection.
        base: [::parquet::file::reader::SerializedFileReader<std::fs::File>; 2],
    },
    /// Nothing present, or only froe's own in-scope output (possibly
    /// interrupted mid-refresh): a full export may replace it uninvited.
    Replaceable(String),
    /// Foreign, other-repository, or out-of-scope files are present:
    /// replacing them takes the explicit rebuild flag.
    Guarded(String),
}

/// The scope-mismatch reason when `provenance` does not match the
/// requested root path and depth limit.
fn scope_mismatch(
    provenance: &ExportProvenance,
    root_path: &str,
    depth: Option<usize>,
) -> Option<String> {
    let requested = ExportProvenance::new(String::new(), root_path, depth);
    if provenance.root_path() != requested.root_path() {
        return Some(format!(
            "the existing export covers {}, not {}",
            provenance.root_path(),
            requested.root_path()
        ));
    }
    if provenance.depth_limit() != requested.depth_limit() {
        let describe = |limit: Option<usize>| {
            limit.map_or_else(|| "unlimited".to_owned(), |limit| format!("depth {limit}"))
        };
        return Some(format!(
            "the existing export was {}, this request is {}",
            describe(provenance.depth_limit()),
            describe(depth)
        ));
    }
    None
}

/// The ownership check a `TarMK` store supports. The store has no
/// repository UUID, and compaction rewrites the journal to one line, so
/// history cannot prove identity: a stamped revision is this
/// repository's own exactly when its segment still resolves. A foreign
/// repository's segments never collide (random UUIDs); a compacted-away
/// revision conservatively fails the check, so replacing such an export
/// takes the explicit rebuild flag.
fn resolves_here(repository: &Repository, provenance: &ExportProvenance) -> bool {
    froe::journal::parse_record_identifier_text(provenance.revision())
        .is_some_and(|identifier| repository.contains_segment(identifier.segment))
}

/// The unresolvable-stamp rejection reason.
fn unresolvable_reason(provenance: &ExportProvenance) -> String {
    format!(
        "the stamped revision {} does not resolve against this repository; the store was \
         likely compacted since the export, or the export belongs to a different repository",
        provenance.revision()
    )
}

/// Classifies the export directory. The authorization rule, in one
/// place: automatic replacement is safe only when both files are
/// absent, or when every present file is demonstrably froe-owned —
/// stamped, in scope, and resolving against this repository — with a
/// foreign, other-repository, or out-of-scope file anywhere guarding
/// the directory. Two stamps may disagree only in their revision for
/// the pair to count as interrupted-refresh residue; any other
/// disagreement is out of scope by construction and never reaches the
/// residue branch.
fn classify(
    repository: &Repository,
    output_directory: &Path,
    root_path: &str,
    depth: Option<usize>,
) -> Classification {
    let nodes_path = output_directory.join(NODES_FILE_NAME);
    let properties_path = output_directory.join(PROPERTIES_FILE_NAME);
    let nodes = inspect_table(&nodes_path);
    let properties = inspect_table(&properties_path);
    for file in [&nodes, &properties] {
        if let TableFile::Foreign(reason) = file {
            return Classification::Guarded(reason.clone());
        }
    }
    match (nodes, properties) {
        (TableFile::Missing, TableFile::Missing) => Classification::Replaceable(format!(
            "there is no export at {} yet",
            output_directory.display()
        )),
        (TableFile::Stamped { provenance, .. }, TableFile::Missing)
        | (TableFile::Missing, TableFile::Stamped { provenance, .. }) => {
            if let Some(reason) = scope_mismatch(&provenance, root_path, depth) {
                return Classification::Guarded(reason);
            }
            if !resolves_here(repository, &provenance) {
                return Classification::Guarded(unresolvable_reason(&provenance));
            }
            Classification::Replaceable("one of the export's two files is missing".to_owned())
        }
        (
            TableFile::Stamped {
                provenance: first,
                reader: first_reader,
            },
            TableFile::Stamped {
                provenance: second,
                reader: second_reader,
            },
        ) => {
            for provenance in [&first, &second] {
                if let Some(reason) = scope_mismatch(provenance, root_path, depth) {
                    return Classification::Guarded(reason);
                }
            }
            if !resolves_here(repository, &first) {
                return Classification::Guarded(unresolvable_reason(&first));
            }
            if !resolves_here(repository, &second) {
                return Classification::Guarded(unresolvable_reason(&second));
            }
            if first == second {
                Classification::Reusable {
                    provenance: first,
                    base: [first_reader, second_reader],
                }
            } else {
                Classification::Replaceable(
                    "the export's two files carry different revisions; an earlier refresh \
                     must have been interrupted"
                        .to_owned(),
                )
            }
        }
        (TableFile::Foreign(_), _) | (_, TableFile::Foreign(_)) => {
            unreachable!("foreign files returned above")
        }
    }
}

/// The assessment of an export directory's contents before a full
/// export replaces them; see [`assess_export`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportAssessment {
    /// A valid, own, in-scope export stands there — refresh it rather
    /// than replace it.
    Reusable,
    /// Nothing stands there, or only froe's own in-scope residue —
    /// replacement is safe; the string says why it is not Reusable.
    Replaceable(String),
    /// Foreign, other-repository, or out-of-scope files stand there —
    /// replacement needs the explicit rebuild flag.
    Guarded(String),
}

/// Assesses the export directory's contents ahead of a full export.
/// Callers replacing files should hold the export directory lock
/// ([`lock_export_directory`]) across the assessment and the
/// replacement, so the verdict cannot go stale.
#[must_use]
pub fn assess_export(
    repository: &Repository,
    output_directory: &Path,
    root_path: &str,
    depth: Option<usize>,
) -> ExportAssessment {
    match classify(repository, output_directory, root_path, depth) {
        Classification::Reusable { .. } => ExportAssessment::Reusable,
        Classification::Replaceable(reason) => ExportAssessment::Replaceable(reason),
        Classification::Guarded(reason) => ExportAssessment::Guarded(reason),
    }
}

/// Validates an existing export as a refresh base. Every failure is a
/// [`Rejection`], not an error — nothing about a reusable-or-not
/// verdict is exceptional.
fn validate(
    repository: &Repository,
    output_directory: &Path,
    root_path: &str,
    depth: Option<usize>,
) -> Result<ValidatedBase, Rejection> {
    match classify(repository, output_directory, root_path, depth) {
        Classification::Reusable { provenance, base } => Ok(ValidatedBase { provenance, base }),
        Classification::Replaceable(reason) => Err(Rejection {
            reason,
            replaceable: true,
        }),
        Classification::Guarded(reason) => Err(Rejection {
            reason,
            replaceable: false,
        }),
    }
}

/// A refresh base that passed validation: the agreed provenance and the
/// two table readers, held open from inspection through the merge.
struct ValidatedBase {
    /// The agreed provenance of the two files.
    provenance: ExportProvenance,
    /// The nodes and properties readers the merge consumes.
    base: [::parquet::file::reader::SerializedFileReader<std::fs::File>; 2],
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
#[cfg(test)]
fn dirty_ranges(
    differences: &[NodeDifference],
    root_path: &str,
    depth_limit: Option<usize>,
) -> Vec<DirtyRange> {
    let mut ranges: Vec<DirtyRange> = differences
        .iter()
        .filter_map(|difference| dirty_range_for(difference, root_path, depth_limit))
        .collect();
    normalize_dirty_ranges(&mut ranges);
    ranges
}

/// The dirty range one difference implies, or `None` when the change falls
/// outside the export's depth limit.
///
/// Split out from [`dirty_ranges`] so a refresh can fold each difference as
/// the diff produces it. Holding the whole change set only to reduce it to
/// ranges meant both collections were live at once, and the change set is
/// far the larger of the two — every entry carries full before and after
/// property state.
fn dirty_range_for(
    difference: &NodeDifference,
    root_path: &str,
    depth_limit: Option<usize>,
) -> Option<DirtyRange> {
    let root_depth = path_depth(root_path);
    {
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
        // Rows beyond the export's depth limit exist in neither the old
        // file nor a full re-export, whatever the change — dropping the
        // range here keeps a deep removal from rewriting both tables
        // for zero effect.
        if depth_limit.is_some_and(|limit| range_depth > limit) {
            return None;
        }
        let replacement = match (replacement, depth_limit) {
            (Replacement::Excise, _) => Replacement::Excise,
            (Replacement::ReExport { depth: None }, Some(limit)) => Replacement::ReExport {
                depth: Some(limit - range_depth),
            },
            (reexport @ Replacement::ReExport { .. }, _) => reexport,
        };
        Some(DirtyRange {
            path: path.clone(),
            subtree,
            replacement,
        })
    }
}

/// Sorts and folds dirty ranges into the canonical set the refresh applies.
fn normalize_dirty_ranges(ranges: &mut Vec<DirtyRange>) {
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
#[derive(Debug)]
enum MergeVerdict {
    Done,
    Unusable(String),
}

/// Merges the old tables with the delta tables into fresh files at the
/// merged paths, stamped with `provenance`. The old tables arrive as
/// the readers validation opened and the refresh held ever since — an
/// open handle keeps its bytes even if its pathname is replaced, so a
/// base swapped by a writer outside the lock can never be merged and
/// stamped under the new head.
#[allow(
    clippy::too_many_arguments,
    reason = "two tables times three paths, plus ranges, provenance, and options"
)]
fn merge_tables(
    repository: &Repository,
    old_nodes_reader: &::parquet::file::reader::SerializedFileReader<std::fs::File>,
    delta_nodes: &Path,
    merged_nodes: &Path,
    old_properties_reader: &::parquet::file::reader::SerializedFileReader<std::fs::File>,
    delta_properties: &Path,
    merged_properties: &Path,
    ranges: &[DirtyRange],
    provenance: &ExportProvenance,
    options: &ParquetExportOptions,
) -> froe::Result<MergeVerdict> {
    use ::parquet::file::reader::SerializedFileReader;

    let open_delta = |path: &Path| -> froe::Result<SerializedFileReader<std::fs::File>> {
        SerializedFileReader::new(std::fs::File::open(path)?).map_err(parquet_read_error)
    };
    let delta_nodes_reader = open_delta(delta_nodes)?;
    let delta_properties_reader = open_delta(delta_properties)?;

    // A decode or read failure inside an old stream does not abort the
    // merge; it ends the affected stream, and the flag turns the
    // verdict into Unusable afterwards — the partial merged files are
    // then discarded and a full export replaces the unparseable base.
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
        NodeRows::new(old_nodes_reader, &failure, RowSource::PreviousExport)?,
        NodeRows::new(&delta_nodes_reader, &failure, RowSource::FreshDelta)?,
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
        PropertyRows::new(old_properties_reader, &failure, RowSource::PreviousExport)?,
        PropertyRows::new(&delta_properties_reader, &failure, RowSource::FreshDelta)?,
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

/// Where a row stream came from, which decides whether a read error is
/// fatal or merely recorded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowSource {
    /// The export already on disk, which may legitimately be corrupt: a
    /// read error is recorded and ends the stream.
    PreviousExport,
    /// A delta this run just wrote, where a read error is a hard failure.
    FreshDelta,
}

/// An iterator decoding nodes-table rows. A decode failure records in
/// `failure` and ends the stream. A read error is hard, except on the
/// previous export's stream, where it records instead.
struct NodeRows<'a> {
    inner: ::parquet::record::reader::RowIter<'a>,
    failure: &'a RefCell<Option<String>>,
    source: RowSource,
}

impl<'a> NodeRows<'a> {
    /// Decodes nodes-table rows from `reader`.
    fn new(
        reader: &'a ::parquet::file::reader::SerializedFileReader<std::fs::File>,
        failure: &'a RefCell<Option<String>>,
        source: RowSource,
    ) -> froe::Result<Self> {
        use ::parquet::file::reader::FileReader;
        Ok(Self {
            inner: reader.get_row_iter(None).map_err(parquet_read_error)?,
            failure,
            source,
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
                    record_read_failure(
                        self.failure,
                        "an export file's rows do not match the export schema; \
                         the files were not written by this export"
                            .to_owned(),
                    );
                    None
                }
            }
            Err(error) => {
                if self.source == RowSource::PreviousExport {
                    record_read_failure(
                        self.failure,
                        format!("the existing export's rows are not readable: {error}"),
                    );
                    None
                } else {
                    Some(Err(parquet_read_error(error)))
                }
            }
        }
    }
}

/// An iterator decoding properties-table rows; behaves like
/// [`NodeRows`].
struct PropertyRows<'a> {
    inner: ::parquet::record::reader::RowIter<'a>,
    failure: &'a RefCell<Option<String>>,
    source: RowSource,
}

impl<'a> PropertyRows<'a> {
    /// Decodes properties-table rows from `reader`.
    fn new(
        reader: &'a ::parquet::file::reader::SerializedFileReader<std::fs::File>,
        failure: &'a RefCell<Option<String>>,
        source: RowSource,
    ) -> froe::Result<Self> {
        use ::parquet::file::reader::FileReader;
        Ok(Self {
            inner: reader.get_row_iter(None).map_err(parquet_read_error)?,
            failure,
            source,
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
                    record_read_failure(
                        self.failure,
                        "an export file's rows do not match the export schema; \
                         the files were not written by this export"
                            .to_owned(),
                    );
                    None
                }
            }
            Err(error) => {
                if self.source == RowSource::PreviousExport {
                    record_read_failure(
                        self.failure,
                        format!("the existing export's rows are not readable: {error}"),
                    );
                    None
                } else {
                    Some(Err(parquet_read_error(error)))
                }
            }
        }
    }
}

/// Records the first read or decode failure; later failures keep the
/// first's message.
fn record_read_failure(failure: &RefCell<Option<String>>, message: String) {
    let mut failure = failure.borrow_mut();
    if failure.is_none() {
        *failure = Some(message);
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
        let mut leftovers: Vec<String> = std::fs::read_dir(directory.export())
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        leftovers.sort();
        assert_eq!(
            leftovers,
            vec![
                ".froe-export.lock".to_owned(),
                "nodes.parquet".to_owned(),
                "properties.parquet".to_owned(),
            ],
            "only the two tables and the lock file remain"
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
    fn a_removed_export_root_reports_missing() {
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
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        assert_eq!(
            outcome,
            ParquetRefresh::Missing,
            "a vanished export root follows the missing-path contract"
        );
        assert_eq!(
            node_rows(&directory.export()).len(),
            6,
            "the existing export stays untouched"
        );
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
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("no files, no refresh: {outcome:?}");
        };
        assert!(reason.contains("no export"), "the reason: {reason}");
        assert!(replaceable, "there is nothing to destroy");
    }

    #[test]
    fn a_stampless_export_is_not_reusable() {
        let directory = TestDirectory::new("stampless");
        populate_first(&directory.store());
        full_export_without_stamp(&directory.store(), "/content", &directory.export());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("an unstamped file is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("no export stamp"), "the reason: {reason}");
        assert!(
            !replaceable,
            "foreign files wait for the explicit rebuild flag"
        );
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
        // The residue an interrupted refresh leaves: one file stamped
        // with the next real revision, one with the previous — both
        // this repository's own.
        populate_second(&directory.store());
        let other = directory.path.join("other");
        full_export(&directory.store(), "/content", None, &other, None);
        std::fs::copy(
            other.join("nodes.parquet"),
            directory.export().join("nodes.parquet"),
        )
        .expect("copy");

        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("disagreeing stamps are no refresh base: {outcome:?}");
        };
        assert!(
            reason.contains("different revisions"),
            "the reason: {reason}"
        );
        assert!(replaceable, "interrupted-refresh residue is froe's own");
    }

    #[test]
    fn an_unresolvable_stamped_revision_is_guarded() {
        let directory = TestDirectory::new("stale-revision");
        populate_first(&directory.store());
        // A well-formed revision naming a segment the store never held
        // stands in for both a compacted-away revision and a foreign
        // repository's: unresolvable is unresolvable, and replacing the
        // files takes the explicit flag.
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            Some("00000000-0000-0000-0000-000000000000.00000001".to_owned()),
        );
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("an unresolvable stamp is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("does not resolve"), "the reason: {reason}");
        assert!(
            !replaceable,
            "compaction cannot be told from a foreign repository; --full decides"
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
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("a different subtree is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("covers /content"), "the reason: {reason}");
        assert!(
            !replaceable,
            "a different scope waits for the explicit rebuild flag"
        );
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
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("a different depth limit is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("unlimited"), "the reason: {reason}");
        assert!(
            !replaceable,
            "a different scope waits for the explicit rebuild flag"
        );
    }

    #[test]
    fn an_out_of_depth_removal_reports_current() {
        let directory = TestDirectory::new("deep-removal");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            Some(1),
            &directory.export(),
            None,
        );
        let before = std::fs::read(directory.export().join("nodes.parquet")).expect("read");

        // Remove only /content/kept/leaf — depth 2, outside the export.
        revise(&directory.store(), |writer| {
            let ratio = writer.property("ratio", PropertyType::Double, "2.5");
            let jcr_content = writer.leaf(&[ratio]);
            let kept = writer.leaf(&[]);
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
        let outcome = refresh(&directory.store(), "/content", Some(1), &directory.export());
        assert!(
            matches!(outcome, ParquetRefresh::Current { .. }),
            "a removal outside the depth limit is a no-op: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(directory.export().join("nodes.parquet")).expect("read"),
            before,
            "and the tables are not rewritten"
        );
    }

    #[test]
    fn a_type_only_property_change_refreshes() {
        let directory = TestDirectory::new("type-only");
        let populate = |directory: &Path, property_type: PropertyType| {
            revise(directory, |writer| {
                let tags = PropertyToWrite {
                    name: "tags".to_owned(),
                    property_type,
                    values: PropertyValuesToWrite::Multiple(Vec::new()),
                };
                let content = writer.leaf(&[tags]);
                writer.child("content", content, &[])
            });
        };
        populate(&directory.store(), PropertyType::String);
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );

        populate(&directory.store(), PropertyType::Long);
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        assert!(
            matches!(outcome, ParquetRefresh::Refreshed { .. }),
            "an empty String[] to Long[] retype is a change: {outcome:?}"
        );
        let rows = property_rows(&directory.export());
        let tags: Vec<_> = rows.iter().filter(|row| row.name == "tags").collect();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].property_type, "Long", "the retyped property row");
    }

    #[test]
    fn a_same_stamp_swap_mid_refresh_is_never_merged() {
        let directory = TestDirectory::new("swap-mid-refresh");
        populate_first(&directory.store());
        let first_revision = full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );

        // The rogue pair: a different repository's export carrying the
        // same stamp — footer equality is not content identity.
        let rogue = TestDirectory::new("swap-rogue");
        revise(&rogue.store(), |writer| {
            let evil = writer.leaf(&[]);
            let content = writer.child("evil", evil, &[]);
            writer.child("content", content, &[])
        });
        full_export(
            &rogue.store(),
            "/content",
            None,
            &rogue.export(),
            Some(first_revision),
        );

        populate_second(&directory.store());

        // Rename the rogue pair over the base from the delta-export
        // progress callback: after validation, before the merge.
        let mut swapped = false;
        let mut on_node = |_: u64| {
            if !swapped {
                swapped = true;
                for name in ["nodes.parquet", "properties.parquet"] {
                    std::fs::rename(rogue.export().join(name), directory.export().join(name))
                        .expect("swap");
                }
            }
        };
        let repository = Repository::open(&directory.store()).expect("open");
        let outcome = refresh_parquet_export(
            &repository,
            "/content",
            None,
            &directory.export(),
            &ParquetExportOptions::default(),
            &mut on_node,
        )
        .expect("refresh");
        assert!(swapped, "the swap really happened mid-refresh: {outcome:?}");

        // The merge consumed the readers validation opened — the honest
        // base — so the result is exactly a full export of the new head.
        let reference = directory.path.join("reference");
        full_export(&directory.store(), "/content", None, &reference, None);
        assert_eq!(
            node_rows(&directory.export()),
            node_rows(&reference),
            "the swapped-in rogue rows never entered the merge"
        );
        assert_eq!(
            property_rows(&directory.export()),
            property_rows(&reference)
        );
        assert!(
            node_rows(&directory.export())
                .iter()
                .all(|row| !row.path.contains("evil")),
            "no rogue row survives under the new stamp"
        );
    }

    #[test]
    fn a_row_corrupt_base_is_not_reusable() {
        let directory = TestDirectory::new("corrupt-base");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        // Corrupt the first data page of the nodes table: the footer —
        // and with it the stamp — stays readable, but the rows do not
        // decode.
        {
            use ::parquet::file::reader::{FileReader, SerializedFileReader};
            let nodes_path = directory.export().join("nodes.parquet");
            let offset = {
                let reader =
                    SerializedFileReader::new(std::fs::File::open(&nodes_path).expect("open"))
                        .expect("reader");
                reader.metadata().row_group(0).column(0).file_offset() as usize
            };
            let mut bytes = std::fs::read(&nodes_path).expect("read");
            for byte in &mut bytes[offset..offset + 16] {
                *byte ^= 0xFF;
            }
            std::fs::write(&nodes_path, bytes).expect("write");
        }

        populate_second(&directory.store());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("a base whose rows do not decode rebuilds: {outcome:?}");
        };
        assert!(reason.contains("not readable"), "the reason: {reason}");
        assert!(replaceable, "a corrupt froe-owned base rebuilds uninvited");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_are_foreign_never_missing() {
        let directory = TestDirectory::new("dangling-symlinks");
        populate_first(&directory.store());
        std::fs::create_dir_all(directory.export()).expect("create export directory");
        for name in ["nodes.parquet", "properties.parquet"] {
            std::os::unix::fs::symlink(
                directory.path.join("no-such-target"),
                directory.export().join(name),
            )
            .expect("symlink");
        }
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("a dangling symlink is not an absent file: {outcome:?}");
        };
        assert!(
            reason.contains("not a regular file"),
            "the reason: {reason}"
        );
        assert!(
            !replaceable,
            "foreign filesystem objects are never replaced uninvited"
        );
        let repository = Repository::open(&directory.store()).expect("open");
        assert!(
            matches!(
                super::assess_export(&repository, &directory.export(), "/content", None),
                super::ExportAssessment::Guarded(_)
            ),
            "the assessment agrees"
        );
    }

    #[test]
    fn an_export_from_another_repository_is_guarded() {
        let directory = TestDirectory::new("cross-repository");
        // A complete, valid, in-scope export — of a *different* store.
        let foreign = TestDirectory::new("cross-repository-foreign");
        populate_first(&foreign.store());
        full_export(
            &foreign.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );

        populate_first(&directory.store());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("another repository's export is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("does not resolve"), "the reason: {reason}");
        assert!(
            !replaceable,
            "a foreign repository's export is indistinguishable from a compacted base"
        );
    }

    #[test]
    fn a_missing_table_with_a_foreign_peer_is_not_replaceable() {
        let directory = TestDirectory::new("foreign-peer");
        populate_first(&directory.store());
        // No nodes.parquet; a foreign properties.parquet.
        std::fs::create_dir_all(directory.export()).expect("create export directory");
        std::fs::write(directory.export().join("properties.parquet"), b"foreign").expect("seed");
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason: _,
            replaceable,
        } = outcome
        else {
            panic!("a foreign peer must guard the directory: {outcome:?}");
        };
        assert!(
            !replaceable,
            "the missing table must not authorize replacing the surviving one"
        );
    }

    #[test]
    fn a_missing_table_with_an_out_of_scope_peer_is_not_replaceable() {
        let directory = TestDirectory::new("out-of-scope-peer");
        populate_first(&directory.store());
        // A stamped /content export with its properties table removed,
        // queried as a / export.
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        std::fs::remove_file(directory.export().join("properties.parquet")).expect("remove");
        let outcome = refresh(&directory.store(), "/", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("an out-of-scope peer must guard the directory: {outcome:?}");
        };
        assert!(reason.contains("covers /content"), "the reason: {reason}");
        assert!(
            !replaceable,
            "the missing peer must not launder the surviving file's scope"
        );
    }

    #[test]
    fn mixed_scope_stamps_are_not_replaceable() {
        let directory = TestDirectory::new("mixed-scope");
        populate_first(&directory.store());
        // nodes.parquet from a / export, properties.parquet from a
        // /content export of the same revision: disagreement, but not
        // interrupted-refresh residue.
        full_export(&directory.store(), "/", None, &directory.export(), None);
        let other = directory.path.join("other");
        full_export(&directory.store(), "/content", None, &other, None);
        std::fs::copy(
            other.join("properties.parquet"),
            directory.export().join("properties.parquet"),
        )
        .expect("copy");

        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("mixed scopes are not residue: {outcome:?}");
        };
        assert!(reason.contains("covers /"), "the reason: {reason}");
        assert!(
            !replaceable,
            "only the revision may differ for the residue classification"
        );
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
    fn dirty_ranges_drop_out_of_depth_removals() {
        let differences = vec![NodeDifference::NodeRemoved {
            path: "/root/a/b".to_owned(),
        }];
        assert!(
            dirty_ranges(&differences, "/root", Some(1)).is_empty(),
            "a removal beyond the limit touches no exported row"
        );
        assert_eq!(
            range_paths(&dirty_ranges(&differences, "/root", Some(2))),
            vec![("/root/a/b", true, Replacement::Excise)],
            "at the limit it excises"
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
