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

mod assessment;
mod delta;
mod merge;
mod ranges;
#[cfg(test)]
mod test_support;

pub use assessment::*;
pub(crate) use delta::*;
pub(crate) use merge::*;
pub(crate) use ranges::*;
#[cfg(test)]
pub(crate) use test_support::*;

/// The nodes table's file name within the export directory.
pub const NODES_FILE_NAME: &str = "nodes.parquet";

/// The properties table's file name within the export directory.
pub const PROPERTIES_FILE_NAME: &str = "properties.parquet";

/// The conceptual file names the delta temp files derive from; the
/// `.delta.` infix keeps their sweep pattern apart from the real
/// tables' temp files.
pub(crate) const NODES_DELTA_FILE_NAME: &str = "nodes.delta.parquet";

pub(crate) const PROPERTIES_DELTA_FILE_NAME: &str = "properties.delta.parquet";

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
pub(crate) fn apply_ranges(
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
        TablePaths {
            nodes: &delta_nodes,
            properties: &delta_properties,
        },
        options,
        on_node,
    )?;
    let [base_nodes, base_properties] = base;
    let new_provenance =
        ExportProvenance::new(revision_text.to_owned(), provenance.root_path(), depth);
    match merge_tables(
        repository,
        TableMerge {
            previous: base_nodes,
            delta: &delta_nodes,
            merged: &merged_nodes,
        },
        TableMerge {
            previous: base_properties,
            delta: &delta_properties,
            merged: &merged_properties,
        },
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

/// Temporary files removed when the guard drops — after success the
/// renames have already moved the merged files out of the list's
/// paths, so only genuine leftovers (a failed delta, an interrupted
/// merge) are swept.
#[derive(Default)]
pub(crate) struct TemporaryFiles(Vec<PathBuf>);

impl TemporaryFiles {
    /// Registers `path` for removal and returns it.
    pub(crate) fn track(&mut self, path: PathBuf) -> PathBuf {
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
    use super::ParquetRefresh;
    use super::*;
    use crate::parquet::read_export_provenance;
    use froe::content::PropertyType;
    use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use froe::writer::store_writer::WritableRepository;
    use std::path::Path;

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
            let x_node = writer.leaf(&[]);
            let flag = writer.property("flag", PropertyType::Boolean, "true");
            let subtree = writer.child("x", x_node, &[flag]);
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
}
