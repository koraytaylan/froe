//! Building a plan from a repository directory: shape validation,
//! directory fingerprints, and the pass that collects every action.

use super::apply_identity::append_apply_identity_preview_warning;
use super::checkpoints::plan_checkpoints;
use super::journal::JournalAnalysis;
use super::journal::analyze_journal;
use super::manifest::upgrade_manifest_atomically;
use super::options::{CompactionOptions, MaintenanceTask};
use super::plan::{
    CompactionAction, CompactionPlan, HistoryProtection, JournalLineRemoval, RetainedReclaimable,
    StaleArchiveReason,
};
use super::reclamation::{
    active_index_generations, compaction_target_generation, extend_segment_closure,
    predict_shared_bulk_segments, prospective_retained_roots, retained_reclaimable_from,
    segments_ahead_of_the_head, validate_prospective_segment_plan,
    validate_reclaim_reference_invariant,
};
use super::recovery_backups::{plan_recovery_backups, recovery_backup_target};
use super::stale_archives::{
    generation_from_header, plan_stale_archives, planned_archive_repairs,
    reject_cross_number_duplicate_active_segments, reject_duplicate_active_segments,
    unrepairable_archive_names,
};
use super::temporaries::{plan_stale_temporaries, temporary_kind};
use crate::content::node::NodeState;
use crate::error::{Error, Result};
use crate::progress::{ProgressObserver, Step, WorkUnit};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::tooling::NodeTreeVerifier;
use crate::writer::compaction::CompactionKind;
use crate::writer::maintenance::journal::RawJournal;
use crate::writer::maintenance::journal::scan_raw_journal;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::StandaloneSegmentCompactionPlan;
use crate::writer::store_writer::{
    PlannedArchiveSweep, ReclaimRule, next_cleanup_archive_number, plan_standalone_segment_cleanup,
};
use std::collections::{BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod content_census;
mod listing;
mod segments;
mod shape;
mod version_storage;

pub(crate) use content_census::*;
pub(crate) use listing::*;
pub(crate) use segments::*;
pub(in crate::writer::maintenance) use shape::*;
pub(crate) use version_storage::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct JournalPlan {
    pub(super) retained_record_ids: Vec<RecordIdentifier>,
    pub(super) retained_raw_lines: Vec<Vec<u8>>,
    pub(super) removals: Vec<JournalLineRemoval>,
    pub(super) removed_lines: usize,
    pub(super) parser_ignored: usize,
    pub(super) missing_segments: usize,
    pub(super) unreadable_revisions: usize,
    pub(super) beyond_retention: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CheckpointPlan {
    pub(super) names: Vec<String>,
    pub(super) expired: usize,
    pub(super) unreferenced: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StaleArchive {
    pub(super) file_name: String,
    pub(super) reason: StaleArchiveReason,
    pub(super) bytes: u64,
    pub(super) fingerprint: FileFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PlannedFileRemoval {
    pub(super) file_name: String,
    pub(super) bytes: u64,
    pub(super) fingerprint: FileFingerprint,
}

/// Builds the plan, keeping the warnings it established even when it fails.
///
/// A refusal is exactly when an operator most needs the facts the same run
/// already worked out — a plan that dies at the segment gate has usually
/// finished the stale-archive scan, and that scan's findings are as true on
/// the failing path as on the succeeding one. `docs/cli-output.md` states
/// warnings are never suppressed; that promise has to survive an error.
pub(super) fn build_plan(
    directory: &Path,
    options: &CompactionOptions,
    now: SystemTime,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionPlan> {
    let mut warnings = Vec::new();
    build_plan_collecting(directory, options, now, observer, &mut warnings)
        .map_err(|error| attach_planning_warnings(error, &warnings))
}

/// Re-attaches durable repairs to a refusal raised after them.
///
/// Same obligation as [`attach_planning_warnings`], for the one mutation that
/// happens before there is a plan to record it in: a refusal that named only
/// the failure would leave the operator believing the store is as they left
/// it, when archives have been rewritten and originals moved aside.
pub(super) fn attach_completed_repairs(
    error: Error,
    repaired: &[crate::writer::store_writer::RepairedArchive],
) -> Error {
    if repaired.is_empty() {
        return error;
    }
    let names: Vec<&str> = repaired
        .iter()
        .map(|archive| archive.file_name.as_str())
        .collect();
    Error::InvalidFormat {
        details: format!(
            "{error} This refusal came after {} archive index rebuild(s), which are already \
             durable: {}. The originals are retained under `.bak` names, and those archives \
             need no second attempt.",
            repaired.len(),
            names.join(", ")
        ),
    }
}

pub(super) fn attach_planning_warnings(error: Error, warnings: &[String]) -> Error {
    const WARNINGS_SHOWN: usize = 3;
    match error {
        Error::InvalidFormat { details } if !warnings.is_empty() => {
            let mut attached = warnings
                .iter()
                .take(WARNINGS_SHOWN)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("; ");
            let remaining = warnings.len() - warnings.len().min(WARNINGS_SHOWN);
            if remaining == 1 {
                attached.push_str(", and 1 further warning");
            } else if remaining > 1 {
                let _ = write!(attached, ", and {remaining} further warnings");
            }
            Error::InvalidFormat {
                details: format!("{details} Also established before the refusal: {attached}."),
            }
        }
        other => other,
    }
}

/// Re-attaches established warnings to a format refusal. Other variants are
/// returned untouched: an `InputOutput` failure must stay matchable as one
/// for a library caller, and its cause is not a fact about the store.
///
/// The attachment is bounded, because a store with many damaged archives
/// produces one warning per archive and an unbounded tail would bury the
/// refusal itself. The count of omitted warnings is stated so the operator
/// knows the list was cut rather than exhausted, and the full set is
/// reachable by rerunning with only the tasks that do not refuse.
/// Raises `store.version` to 2 the first time a rebuilt archive is about to
/// be installed, and never otherwise.
///
/// The upgrade is one-way and the repair is not guaranteed: a rebuild can
/// fail per archive on a full disk, an unresolvable blob catalog, or a
/// staging residue, none of which a survey can predict. Paying the
/// transition for a run that then installs nothing would leave the store
/// still damaged and no longer openable by an Oak older than 1.8 — so the
/// price is charged at the exact instant the first version-2 trailer becomes
/// visible, which is also the only instant the invariant requires.
pub(super) struct ManifestUpgradeOnFirstInstall<'directory> {
    pub(super) directory: &'directory Path,
    pub(super) done: bool,
}

impl<'directory> ManifestUpgradeOnFirstInstall<'directory> {
    pub(super) fn new(directory: &'directory Path) -> Self {
        Self {
            directory,
            done: false,
        }
    }
}

impl crate::writer::store_writer::AuthorizeVersionTwoWrite for ManifestUpgradeOnFirstInstall<'_> {
    fn authorize(&mut self) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if crate::store::read_manifest_store_version(&self.directory.join("manifest"))? == 1 {
            ensure_numbered_name_available(self.directory, "manifest.cleaning")?;
            upgrade_manifest_atomically(self.directory)?;
        }
        self.done = true;
        Ok(())
    }
}

/// What a read-only survey of the store establishes before any action is
/// planned.
pub(crate) struct RepositoryState {
    pub(crate) raw_journal: RawJournal,
    pub(crate) journal_analysis: JournalAnalysis,
    pub(crate) pending_repairs: Vec<CompactionAction>,
    pub(crate) index_available: bool,
    pub(crate) checkpoints: CheckpointPlan,
    pub(crate) checkpoint_archive_number: Option<u32>,
}

/// Surveys the journal, the archives needing repair, and the checkpoints,
/// refusing the combinations no plan can safely express.
///
/// While any repair is pending the survey reports `index_available` as
/// false: the rebuild happens under the lock inside
/// `PreparedCompaction::prepare`, and until it has, nothing that reads an
/// index can be planned.
pub(crate) fn survey_repository_state(
    directory: &Path,
    repository: &Repository,
    options: &super::options::CompactionOptions,
    current_head: RecordIdentifier,
    now: SystemTime,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<RepositoryState> {
    let raw_journal = scan_raw_journal(directory)?;
    let journal_analysis = analyze_journal(
        repository,
        &raw_journal,
        current_head,
        options.journal_revision_retention,
        observer,
    )?;

    // Repairs the operator has authorized, named but not yet performed. This
    // is the read-only preview: the rebuild happens under the lock inside
    // `PreparedCompaction::prepare`, and until it has, nothing that reads an
    // index can be planned. So while any repair is pending this plan is
    // deliberately partial — it names the repairs and stops. `prepare`
    // repairs first and then plans in full, and the CLI's existing
    // authoritative-plan comparison shows the operator the difference before
    // a single further byte moves.
    let pending_repairs = if options.contains(MaintenanceTask::RepairArchives) {
        // Refuse here, in the read-only preview, where nothing has been
        // touched. An index-less number that scans to nothing dooms the run
        // however it is retried, and without this the filter below would
        // simply drop it: a mixed store would hide the offending archive
        // behind a plan naming only the repairable ones, and a store whose
        // only damage is unrepairable would report "no mutations needed" and
        // exit zero — for a store froe cannot open for writing at all.
        let unrepairable = unrepairable_archive_names(repository);
        if !unrepairable.is_empty() {
            return Err(crate::writer::store_writer::unrepairable_archives_refusal(
                &unrepairable,
            ));
        }
        planned_archive_repairs(repository)
    } else {
        Vec::new()
    };
    let index_available = pending_repairs.is_empty();

    let checkpoints = if index_available {
        plan_checkpoints(repository, options, now, warnings)?
    } else {
        // Checkpoint removal rewrites the head and consults active index
        // generations; both wait for the repair.
        CheckpointPlan::default()
    };
    // Checkpoint removal installs a new head and appends its journal line, so
    // by the time the journal is rewritten the newest revision is one this
    // plan never saw. A bound counted from there retires the head this plan
    // retained, and the apply aborts on its own retained-root proof — with
    // the checkpoint removal already committed. Refuse here instead, where
    // nothing has moved and the operator can simply run the two in sequence.
    if options.journal_revision_retention.is_some() && !checkpoints.names.is_empty() {
        return Err(Error::InvalidFormat {
            details: format!(
                "a journal revision retention bound cannot run beside the removal of {} checkpoint(s): \
                 checkpoint removal moves the head and appends a journal line, which would put the \
                 bound's newest revision beyond the one this plan retained — remove the checkpoints \
                 first, then bound the journal in a second run",
                checkpoints.names.len()
            ),
        });
    }
    // A run that moves the head needs a certified output archive number,
    // whether the head moves because checkpoints were removed from it or
    // because the whole tree was copied into a fresh generation.
    let checkpoint_archive_number =
        if checkpoints.names.is_empty() && options.compaction_kind.is_none() {
            None
        } else {
            Some(next_cleanup_archive_number(directory)?)
        };
    if options.contains(MaintenanceTask::Segments)
        || options.contains(MaintenanceTask::StaleArchives)
        || !checkpoints.names.is_empty()
    {
        // While repairs are pending only the cross-number half can be
        // trusted: the letters of one unindexed number share segments by
        // construction and the repair is about to collapse them. Everything
        // the preview *can* prove still gets proved there, so a store that is
        // unfit for cleanup says so before anything is authorized rather than
        // after the rewrite.
        if index_available {
            reject_duplicate_active_segments(repository)?;
        } else {
            reject_cross_number_duplicate_active_segments(repository)?;
        }
    }
    Ok(RepositoryState {
        raw_journal,
        journal_analysis,
        pending_repairs,
        index_available,
        checkpoints,
        checkpoint_archive_number,
    })
}

/// The staging temporaries and recovery backups a run would retire.
#[allow(
    clippy::too_many_arguments,
    reason = "both scans read the same store, options, clock and reporter"
)]
pub(crate) fn plan_leftover_files(
    directory: &Path,
    repository: &Repository,
    options: &super::options::CompactionOptions,
    index_available: bool,
    raw_journal: &RawJournal,
    now: SystemTime,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<(Vec<PlannedFileRemoval>, Vec<PlannedFileRemoval>)> {
    let temporaries = if index_available && options.contains(MaintenanceTask::StaleTemporaries) {
        crate::progress::observe(
            observer,
            &Step::new("scanning for stale temporary files", WorkUnit::Files),
            |observer| {
                plan_stale_temporaries(directory, repository, raw_journal, warnings, observer)
            },
        )?
    } else {
        Vec::new()
    };
    let recovery_backups = if options.contains(MaintenanceTask::RecoveryBackups) {
        plan_recovery_backups(
            directory,
            now,
            options
                .recovery_backup_policy
                .expect("validated recovery backup policy"),
        )?
    } else {
        Vec::new()
    };

    // A rebuilt archive carries a version-2 binary-references trailer, so a
    // repair writes v2 data exactly as a rewrite or a checkpoint removal
    // does, and a version-1 store must be raised first. Including it here is
    // what puts the upgrade in the plan the operator confirms, rather than
    // leaving `prepare` to perform it unannounced.
    Ok((temporaries, recovery_backups))
}

/// Whether this run must raise the store to manifest version 2 first.
///
/// A rebuilt archive carries a version-2 binary-references trailer, so a
/// repair writes v2 data exactly as a rewrite or a checkpoint removal
/// does, and a version-1 store must be raised first. Deciding it here is
/// what puts the upgrade in the plan the operator confirms, rather than
/// leaving `prepare` to perform it unannounced.
#[allow(
    clippy::too_many_arguments,
    reason = "every task that writes version-2 data has a say in this one decision"
)]
pub(crate) fn decide_manifest_upgrade(
    directory: &Path,
    _options: &super::options::CompactionOptions,
    pending_repairs: &[CompactionAction],
    checkpoints: &CheckpointPlan,
    segment_plan: Option<&StandaloneSegmentCompactionPlan>,
) -> Result<bool> {
    let writes_v2 = !pending_repairs.is_empty()
        || !checkpoints.names.is_empty()
        || segment_plan.as_ref().is_some_and(|plan| {
            plan.archives
                .iter()
                .any(|archive| matches!(archive, PlannedArchiveSweep::Rewrite { .. }))
        });
    let manifest_upgrade =
        writes_v2 && crate::store::read_manifest_store_version(&directory.join("manifest"))? < 2;
    if manifest_upgrade {
        ensure_numbered_name_available(directory, "manifest.cleaning")?;
    }
    Ok(manifest_upgrade)
}

/// Proves the directory is byte-identical to the fingerprint planning
/// started from, returning the fresh fingerprint the plan will carry.
fn require_directory_unchanged(
    directory: &Path,
    fingerprint_before: &DirectoryFingerprint,
) -> Result<DirectoryFingerprint> {
    let fingerprint_after = directory_fingerprint(directory)?;
    if *fingerprint_before != fingerprint_after {
        return Err(Error::InvalidFormat {
            details:
                "the repository changed while cleanup was planning; retry against a quiescent store"
                    .to_owned(),
        });
    }
    Ok(fingerprint_after)
}

/// Decides the manifest upgrade and reserves the journal staging names a
/// journal-pruning run will need, in one pass over the survey's facts.
fn decide_manifest_upgrade_and_reserve_journal_names(
    directory: &Path,
    options: &CompactionOptions,
    state: &RepositoryState,
    segment_plan: Option<&StandaloneSegmentCompactionPlan>,
) -> Result<bool> {
    let manifest_upgrade = decide_manifest_upgrade(
        directory,
        options,
        &state.pending_repairs,
        &state.checkpoints,
        segment_plan,
    )?;
    if options.contains(MaintenanceTask::Journal) && state.journal_analysis.plan.removed_lines != 0
    {
        ensure_numbered_name_available(directory, "journal.log.cleaning")?;
        ensure_numbered_name_available(directory, "journal.log.bak")?;
    }
    Ok(manifest_upgrade)
}

/// Selects the version-history purge when one is requested: the orphans,
/// minus configurations, minus anything younger than the age bound, minus
/// the advisory reference demotions — the last computed by its own pass,
/// which only runs when there are candidates to check.
fn select_version_history_purge(
    repository: &Repository,
    options: &CompactionOptions,
    current_head: RecordIdentifier,
    census: &PlanningContentCensus,
    now: SystemTime,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<Option<PurgeSelection>> {
    if !options.purges_orphaned_version_histories() {
        return Ok(None);
    }
    let now_epoch_seconds = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let preliminary = select_purge(
        &census.version_storage,
        options.purged_history_minimum_age,
        now_epoch_seconds,
        &HashSet::new(),
    );
    let demoted = if preliminary.histories == 0 {
        HashSet::new()
    } else {
        let content_root = repository
            .node(current_head)
            .child_node("root")?
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("journal root {current_head} has no content \"root\" child node"),
            })?;
        let version_storage_record = content_root
            .child_node("jcr:system")?
            .map(|system| system.child_node("jcr:versionStorage"))
            .transpose()?
            .flatten()
            .map(|storage| storage.record_identifier());
        let candidates = candidate_internal_identifiers(
            &census.version_storage,
            &preliminary.selected_identifiers,
        );
        demoted_by_inbound_references(
            repository,
            content_root.record_identifier(),
            version_storage_record,
            &candidates,
            observer,
        )?
    };
    let selection = select_purge(
        &census.version_storage,
        options.purged_history_minimum_age,
        now_epoch_seconds,
        &demoted,
    );
    if selection.kept_configurations != 0 {
        warnings.push(format!(
            "{} orphaned version histories kept: they freeze nt:configuration versionables, which this purge does not touch",
            selection.kept_configurations
        ));
    }
    if selection.kept_by_age != 0 {
        warnings.push(format!(
            "{} orphaned version histories kept by the age bound (younger than the minimum, or without a parsable version creation date)",
            selection.kept_by_age
        ));
    }
    if selection.kept_by_references != 0 {
        warnings.push(format!(
            "{} orphaned version histories kept: REFERENCE or WEAKREFERENCE values outside version storage still name records inside them",
            selection.kept_by_references
        ));
    }
    if census.version_storage.malformed_identifiers != 0 {
        warnings.push(format!(
            "{} version histories carry versionable identifiers that do not parse and were not classified",
            census.version_storage.malformed_identifiers
        ));
    }
    Ok(Some(selection))
}

/// The plan's two version-history parts: the always-on report, and the
/// purge when a non-empty one is selected.
fn version_history_plan_parts(
    repository: &Repository,
    current_head: RecordIdentifier,
    census: &PlanningContentCensus,
    head_nodes: u64,
    closure_indexed_bytes: Option<(u64, u64)>,
    retired_checkpoints: u64,
    selection: Option<PurgeSelection>,
) -> Result<(
    crate::writer::maintenance::plan::OrphanedVersionHistoryReport,
    Option<crate::writer::maintenance::plan::VersionHistoryPurge>,
)> {
    let mut report = orphaned_version_history_report(
        repository,
        census,
        selection.as_ref(),
        head_nodes,
        closure_indexed_bytes,
    );
    report.retained_checkpoints =
        crate::progress::count(repository.checkpoints()?.len()).saturating_sub(retired_checkpoints);
    let Some(selection) = selection.filter(|selection| selection.histories != 0) else {
        return Ok((report, None));
    };
    // The ancestors whose rewritten form depends on the scope: the chain
    // from the content root down to version storage, plus every partially
    // emptied intermediate the selection identified.
    let mut context_dependent_records = selection.context_dependent_intermediates;
    let mut chain = repository.node(current_head).child_node("root")?;
    context_dependent_records.extend(chain.as_ref().map(NodeState::record_identifier));
    for name in ["jcr:system", "jcr:versionStorage"] {
        chain = match &chain {
            Some(node) => node.child_node(name)?,
            None => None,
        };
        context_dependent_records.extend(chain.as_ref().map(NodeState::record_identifier));
    }
    Ok((
        report,
        Some(crate::writer::maintenance::plan::VersionHistoryPurge {
            omitted_records: selection.omitted_records,
            context_dependent_records,
            histories: selection.histories,
            nodes: selection.nodes,
            retained_checkpoints: report.retained_checkpoints,
        }),
    ))
}

/// Assembles the always-on orphan report from what the walks established.
fn orphaned_version_history_report(
    repository: &Repository,
    census: &PlanningContentCensus,
    selection: Option<&PurgeSelection>,
    head_nodes: u64,
    closure_indexed_bytes: Option<(u64, u64)>,
) -> crate::writer::maintenance::plan::OrphanedVersionHistoryReport {
    let mut report = crate::writer::maintenance::plan::OrphanedVersionHistoryReport {
        malformed_identifiers: census.version_storage.malformed_identifiers,
        ..Default::default()
    };
    let mut orphan_bulk: HashSet<SegmentIdentifier> = HashSet::new();
    let mut kept_orphan_bulk: HashSet<SegmentIdentifier> = HashSet::new();
    let mut released_nodes = 0_u64;
    for (identifier, facts) in census.version_storage.orphans() {
        report.orphaned_histories += 1;
        report.orphaned_nodes += facts.nodes;
        report.inline_binary_bytes = report
            .inline_binary_bytes
            .saturating_add(facts.inline_binary_bytes);
        report.external_references += facts.external_references;
        // Released bulk describes what the *selected* purge frees; without
        // a selection it describes a purge of every orphan. A block a kept
        // orphan still references is not released.
        let released_by_this_run = selection.is_none_or(|selection| {
            selection.histories == 0 || selection.selected_identifiers.contains(identifier)
        });
        if released_by_this_run {
            released_nodes += facts.nodes;
            orphan_bulk.extend(facts.bulk_segments.iter().copied());
        } else {
            kept_orphan_bulk.extend(facts.bulk_segments.iter().copied());
        }
    }
    // Bulk a purge releases: what only the purged histories reference. A
    // block shared with the head, a checkpoint's own content, a live
    // history, or a kept orphan stays, so this is a subtraction, not a
    // sum. Blocks a retained checkpoint's *shared* snapshot pins cannot be
    // seen here, which is why the printed figure carries a checkpoint
    // caveat whenever checkpoints are retained.
    for live in census
        .live_bulk
        .iter()
        .chain(census.version_storage.live_history_bulk.iter())
        .chain(kept_orphan_bulk.iter())
    {
        orphan_bulk.remove(live);
    }
    report.released_bulk_segments = crate::progress::count(orphan_bulk.len());
    let (_, bulk_bytes) =
        crate::writer::maintenance::reclamation::indexed_bytes_by_kind(repository, &orphan_bulk);
    report.released_bulk_bytes = bulk_bytes;
    // The released histories' node-record share, scaled from the head's
    // average bytes per node: an estimate the copy realizes. Scoped like
    // the bulk figure — a kept orphan's nodes are not a saving this run
    // delivers.
    if let (Some((data_bytes, _)), true) = (closure_indexed_bytes, head_nodes != 0) {
        report.node_record_bytes_estimate = data_bytes / head_nodes * released_nodes
            + (data_bytes % head_nodes) * released_nodes / head_nodes;
    }
    report
}

/// The journal's contribution to the convergence gate, read from the raw
/// journal the survey already scanned: a completed compaction leaves
/// exactly one line naming its head, so anything else means history this
/// run still has to retire.
fn journal_convergence_of(
    raw_journal: &RawJournal,
    current_head: RecordIdentifier,
) -> JournalConvergence {
    JournalConvergence {
        single_line_naming_head: raw_journal.lines().len() == 1
            && matches!(
                raw_journal.lines()[0].classification(),
                crate::writer::maintenance::journal::RawJournalLineClassification::Record(record)
                    if record.record_identifier == current_head
            ),
    }
}

/// Verifies the exact head while the content census rides the same walk,
/// so the plan's content facts cost no second pass.
fn verify_head_and_census(
    repository: &Repository,
    current_head: RecordIdentifier,
    options: &CompactionOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<(u64, PlanningContentCensus)> {
    let mut content_census = PlanningContentCensus::default();
    let head_nodes = verify_head_for_planning(
        repository,
        current_head,
        &mut content_census,
        options.purges_orphaned_version_histories(),
        observer,
    )?;
    Ok((head_nodes, content_census))
}

/// Everything a plan is assembled from, gathered by the walks and surveys
/// in their one required order.
struct GatheredPlanFacts {
    repository: Repository,
    current_head: RecordIdentifier,
    head_nodes: u64,
    content_census: PlanningContentCensus,
    state: RepositoryState,
    version_history_purge_selection: Option<PurgeSelection>,
    segment_work: SegmentWork,
    temporaries: Vec<PlannedFileRemoval>,
    recovery_backups: Vec<PlannedFileRemoval>,
    manifest_upgrade: bool,
}

/// Opens, verifies, surveys, selects, and predicts — every read the plan
/// needs, none of the assembly.
fn gather_plan_facts(
    directory: &Path,
    options: &CompactionOptions,
    now: SystemTime,
    observer: &mut dyn ProgressObserver,
    warnings: &mut Vec<String>,
) -> Result<GatheredPlanFacts> {
    let repository = Repository::open_with_progress(directory, observer)?;
    let current_head = repository.head_record_identifier();

    // Repository::open deliberately binds by segment existence, matching
    // Oak. Cleanup's gate is stronger: the exact selected record and every
    // descendant (including binary blocks and checkpoints) must traverse.
    let (head_nodes, content_census) =
        verify_head_and_census(&repository, current_head, options, observer)?;

    let state = survey_repository_state(
        directory,
        &repository,
        options,
        current_head,
        now,
        warnings,
        observer,
    )?;
    let version_history_purge_selection = select_version_history_purge(
        &repository,
        options,
        current_head,
        &content_census,
        now,
        warnings,
        observer,
    )?;
    let journal_convergence = journal_convergence_of(&state.raw_journal, current_head);
    let segment_work = plan_segment_work(
        &SegmentWorkInputs {
            directory,
            repository: &repository,
            options,
            current_head,
            index_available: state.index_available,
            checkpoints: &state.checkpoints,
            journal_analysis: &state.journal_analysis,
            journal_convergence: &journal_convergence,
            purge_selected: version_history_purge_selection
                .as_ref()
                .is_some_and(|selection| selection.histories != 0),
        },
        warnings,
        observer,
    )?;
    let (temporaries, recovery_backups) = plan_leftover_files(
        directory,
        &repository,
        options,
        state.index_available,
        &state.raw_journal,
        now,
        warnings,
        observer,
    )?;
    let manifest_upgrade = decide_manifest_upgrade_and_reserve_journal_names(
        directory,
        options,
        &state,
        segment_work.segment_plan.as_ref(),
    )?;
    Ok(GatheredPlanFacts {
        repository,
        current_head,
        head_nodes,
        content_census,
        state,
        version_history_purge_selection,
        segment_work,
        temporaries,
        recovery_backups,
        manifest_upgrade,
    })
}

pub(super) fn build_plan_collecting(
    directory: &Path,
    options: &CompactionOptions,
    now: SystemTime,
    observer: &mut dyn ProgressObserver,
    warnings: &mut Vec<String>,
) -> Result<CompactionPlan> {
    let fingerprint_before = directory_fingerprint(directory)?;
    let GatheredPlanFacts {
        repository,
        current_head,
        head_nodes,
        content_census,
        state,
        version_history_purge_selection,
        segment_work,
        temporaries,
        recovery_backups,
        manifest_upgrade,
    } = gather_plan_facts(directory, options, now, observer, warnings)?;
    let (orphaned_version_histories, version_history_purge) = version_history_plan_parts(
        &repository,
        current_head,
        &content_census,
        head_nodes,
        segment_work.closure_indexed_bytes,
        crate::progress::count(state.checkpoints.names.len()),
        version_history_purge_selection,
    )?;
    let PlanListing {
        actions,
        estimated_reclaimable_bytes,
        estimated_archive_rewrite_source_bytes,
        retained_reclaimable,
    } = list_planned_actions(
        &PlanFindings {
            directory,
            options,
            state: &state,
            segment_work: &segment_work,
            temporaries: &temporaries,
            recovery_backups: &recovery_backups,
            manifest_upgrade,
            head_nodes,
            version_history_purge: version_history_purge
                .as_ref()
                .map(|purge| (purge.histories, purge.nodes, purge.retained_checkpoints)),
        },
        warnings,
    )?;
    let fingerprint_after = require_directory_unchanged(directory, &fingerprint_before)?;

    let mut plan = CompactionPlan {
        directory: directory.to_owned(),
        tasks: options.tasks().collect(),
        current_head,
        actions,
        // Cloned rather than moved: the caller keeps its copy so a failure
        // added below this point would still carry them out.
        warnings: warnings.clone(),
        estimated_reclaimable_bytes,
        estimated_archive_rewrite_source_bytes,
        retained_reclaimable,
        history_protection: segment_work.history_protection,
        fingerprint: fingerprint_after,
        journal: state.journal_analysis.plan,
        checkpoints: state.checkpoints,
        checkpoint_archive_number: state.checkpoint_archive_number,
        stale_archives: segment_work.stale_archives,
        temporaries,
        recovery_backups,
        segment_plan: segment_work.segment_plan,
        residue_sweep: segment_work.residue_sweep,
        reference_generation: segment_work.reference_generation,
        protected_history_segments: segment_work.protected_history_segments,
        manifest_upgrade,
        // Known exactly when the trace ran and a copy is selected: the copy
        // rewrites the closure's data bytes and shares its bulk bytes in
        // place, so the data figure bounds what the fresh generation costs.
        // A selected purge omits its histories from that rewrite, so its
        // node-record share comes off the prediction.
        predicted_copy_output_bytes: match (
            segment_work.effective_compaction_kind,
            segment_work.closure_indexed_bytes,
        ) {
            (Some(_), Some((data_bytes, _))) => Some(data_bytes.saturating_sub(
                if version_history_purge.is_some() {
                    orphaned_version_histories.node_record_bytes_estimate
                } else {
                    0
                },
            )),
            _ => None,
        },
        external_binary_footprint: content_census.external_binaries.footprint(),
        effective_compaction_kind: segment_work.effective_compaction_kind,
        already_fully_compacted: segment_work.already_fully_compacted,
        orphaned_version_histories,
        version_history_purge,
    };
    append_apply_identity_preview_warning(directory, &mut plan);
    plan.warnings.sort();
    plan.warnings.dedup();
    Ok(plan)
}

pub(super) fn add_estimate(total: &mut u64, amount: u64) -> Result<()> {
    *total = total
        .checked_add(amount)
        .ok_or_else(|| Error::InvalidFormat {
            details: "cleanup byte estimate overflow".to_owned(),
        })?;
    Ok(())
}

pub(super) fn ensure_numbered_name_available(directory: &Path, stem: &str) -> Result<()> {
    for counter in 0..1000u16 {
        let path = directory.join(format!("{stem}.{counter:03}"));
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::InvalidFormat {
        details: format!("all numbered names for {stem} (000-999) are occupied"),
    })
}

pub(super) fn available_filesystem_bytes(directory: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let path = std::ffi::CString::new(directory.as_os_str().as_bytes()).ok()?;
        let mut statistics = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `path` is NUL-terminated and live for the call;
        // `statistics` points to writable storage which is read only after a
        // successful return.
        if unsafe { libc::statvfs(path.as_ptr(), statistics.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: statvfs returned success and initialized the structure.
        let statistics = unsafe { statistics.assume_init() };
        let fragment_size = if statistics.f_frsize == 0 {
            statistics.f_bsize
        } else {
            statistics.f_frsize
        };
        let bytes = u128::from(statistics.f_bavail).checked_mul(u128::from(fragment_size))?;
        u64::try_from(bytes).ok()
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning_warnings_survive_a_refusal_and_stay_bounded() {
        let untouched = attach_planning_warnings(
            crate::error::Error::InvalidFormat {
                details: "refused".to_owned(),
            },
            &[],
        );
        let crate::error::Error::InvalidFormat { details } = untouched else {
            panic!("variant must be preserved");
        };
        assert_eq!(details, "refused", "no warnings means no tail");

        let warnings: Vec<String> = (0..5).map(|index| format!("warning {index}")).collect();
        let attached = attach_planning_warnings(
            crate::error::Error::InvalidFormat {
                details: "refused.".to_owned(),
            },
            &warnings,
        );
        let crate::error::Error::InvalidFormat { details } = attached else {
            panic!("variant must be preserved");
        };
        assert!(
            details.contains("warning 0") && details.contains("warning 2"),
            "established warnings reach the operator: {details}"
        );
        assert!(
            !details.contains("warning 3"),
            "the tail is bounded so it cannot bury the refusal: {details}"
        );
        assert!(
            details.contains("and 2 further warnings"),
            "the omitted warnings are counted: {details}"
        );

        // A non-format error keeps its variant: a caller matching on it for
        // an input/output failure must still be able to.
        let input_output = attach_planning_warnings(
            crate::error::Error::InputOutput(std::io::Error::other("disk")),
            &warnings,
        );
        assert!(
            matches!(input_output, crate::error::Error::InputOutput(_)),
            "only format refusals carry the tail"
        );
    }
}
