//! Building a plan from a repository directory: shape validation,
//! directory fingerprints, and the pass that collects every action.

use super::apply_identity::append_apply_identity_preview_warning;
use super::checkpoints::plan_checkpoints;
use super::journal_analysis::analyze_journal;
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
use crate::content::provider::SegmentProvider as _;
use crate::error::{Error, Result};
use crate::progress::{ProgressObserver, Step, WorkUnit};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::tooling::NodeTreeVerifier;
use crate::writer::compaction::CompactionKind;
use crate::writer::journal_maintenance::scan_raw_journal;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DirectoryFingerprint {
    pub(super) entries: Vec<FileFingerprint>,
    #[cfg(unix)]
    pub(super) device: u64,
    #[cfg(unix)]
    pub(super) inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileFingerprint {
    pub(super) name: OsString,
    pub(super) kind: u8,
    pub(super) length: u64,
    pub(super) modified: Option<SystemTime>,
    #[cfg(unix)]
    pub(super) device: u64,
    #[cfg(unix)]
    pub(super) inode: u64,
    #[cfg(unix)]
    pub(super) change_time_seconds: i64,
    #[cfg(unix)]
    pub(super) change_time_nanoseconds: i64,
}

pub(super) fn validate_options(options: &CompactionOptions) -> Result<()> {
    if options.contains(MaintenanceTask::RecoveryBackups)
        && options.recovery_backup_policy.is_none()
    {
        return Err(Error::InvalidFormat {
            details: "recovery-backups requires an explicit age/count retention policy".to_owned(),
        });
    }
    // Repair retires the original archive bytes to a `.bak` name, and it runs
    // before this run's plan is built — so its own backups are visible to the
    // backup policy that would retire them. A zero age with a zero keep-count
    // is reachable from the command line, and would delete the only copy of
    // whatever the recovery scan could not read, in the same breath that made
    // the copy. The two tasks are coherent in sequence and never together.
    if options.contains(MaintenanceTask::RepairArchives)
        && options.contains(MaintenanceTask::RecoveryBackups)
    {
        return Err(Error::InvalidFormat {
            details: "repair-archives and recovery-backups cannot run together: repair retires \
                      the original archive to a `.bak` name that the backup policy could then \
                      delete in the same run, discarding the only copy of any segment the \
                      rebuild could not read — repair first, verify the store, then retire the \
                      backups in a later run"
                .to_owned(),
        });
    }
    // The bound and the pruning are one operation. Un-rooting a line without
    // removing it leaves it in the journal, where the prospective-plan check
    // still verifies it as retained history and refuses the very plan the
    // bound was set to enable. The builder selects the task, so this only
    // fires for a caller that deselected it afterwards.
    if options.journal_revision_retention.is_some() && !options.contains(MaintenanceTask::Journal) {
        return Err(Error::InvalidFormat {
            details: "a journal revision retention bound requires the journal task: the bounded \
                      lines must leave the journal in the same run, or they remain retained \
                      history and the segments behind them stay protected"
                .to_owned(),
        });
    }
    Ok(())
}

pub(super) fn canonical_repository_directory(directory: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(directory).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::InvalidFormat {
                details: format!("{} is not a repository directory", directory.display()),
            }
        } else {
            Error::InputOutput(source)
        }
    })
}

pub(super) fn validate_repository_shape(directory: &Path) -> Result<()> {
    let root_metadata = std::fs::symlink_metadata(directory).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::InvalidFormat {
                details: format!("{} is not a repository directory", directory.display()),
            }
        } else {
            Error::InputOutput(source)
        }
    })?;
    if root_metadata.file_type().is_symlink() {
        return Err(Error::InvalidFormat {
            details: format!(
                "canonical repository target {} became a symbolic link after path resolution; refusing to continue",
                directory.display()
            ),
        });
    }
    if !directory.is_dir() {
        return Err(Error::InvalidFormat {
            details: format!("{} is not a repository directory", directory.display()),
        });
    }
    let manifest = directory.join("manifest");
    let journal = directory.join("journal.log");
    if !manifest.try_exists()? || !journal.try_exists()? {
        return Err(Error::InvalidFormat {
            details: format!(
                "{} is not an existing segment-tar repository (manifest and journal.log are required)",
                directory.display()
            ),
        });
    }
    validate_managed_file_types(directory)
}

pub(super) fn validate_managed_file_types(directory: &Path) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if !is_managed_name(&name) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "managed repository path {} is not a regular file",
                    entry.path().display()
                ),
            });
        }
    }
    Ok(())
}

pub(super) fn is_managed_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(name, "manifest" | "journal.log" | "gc.log" | "repo.lock")
        || ArchiveFileName::parse(name).is_some()
        || temporary_kind(name).is_some()
        || recovery_backup_target(name).is_some()
}

pub(super) fn directory_fingerprint(directory: &Path) -> Result<DirectoryFingerprint> {
    let directory_metadata = std::fs::symlink_metadata(directory)?;
    if !directory_metadata.file_type().is_dir() {
        return Err(Error::InvalidFormat {
            details: format!(
                "{} ceased to be a repository directory",
                directory.display()
            ),
        });
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == OsStr::new("repo.lock") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        entries.push(file_fingerprint(name, &metadata));
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(DirectoryFingerprint {
        entries,
        #[cfg(unix)]
        device: directory_metadata.dev(),
        #[cfg(unix)]
        inode: directory_metadata.ino(),
    })
}

pub(super) fn file_fingerprint(name: OsString, metadata: &Metadata) -> FileFingerprint {
    let file_type = metadata.file_type();
    let kind = if file_type.is_file() {
        1
    } else if file_type.is_dir() {
        2
    } else if file_type.is_symlink() {
        3
    } else {
        4
    };
    FileFingerprint {
        name,
        kind,
        length: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        change_time_seconds: metadata.ctime(),
        #[cfg(unix)]
        change_time_nanoseconds: metadata.ctime_nsec(),
    }
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

#[allow(
    clippy::too_many_lines,
    reason = "the plan builder is one safety-ordered inventory transaction; splitting it would obscure ordering and duplicate state"
)]
pub(super) fn build_plan_collecting(
    directory: &Path,
    options: &CompactionOptions,
    now: SystemTime,
    observer: &mut dyn ProgressObserver,
    warnings: &mut Vec<String>,
) -> Result<CompactionPlan> {
    let fingerprint_before = directory_fingerprint(directory)?;
    let repository = Repository::open_with_progress(directory, observer)?;
    let current_head = repository.head_record_identifier();

    // Repository::open deliberately binds by segment existence, matching
    // Oak. Cleanup's gate is stronger: the exact selected record and every
    // descendant (including binary blocks and checkpoints) must traverse.
    let head_nodes = verify_exact_super_root_counting_nodes(&repository, current_head, observer)?;

    let raw_journal = scan_raw_journal(directory)?;
    let journal_analysis = analyze_journal(
        &repository,
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
        let unrepairable = unrepairable_archive_names(&repository);
        if !unrepairable.is_empty() {
            return Err(crate::writer::store_writer::unrepairable_archives_refusal(
                &unrepairable,
            ));
        }
        planned_archive_repairs(&repository)
    } else {
        Vec::new()
    };
    let index_available = pending_repairs.is_empty();

    let checkpoints = if index_available {
        plan_checkpoints(&repository, options, now, warnings)?
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
            reject_duplicate_active_segments(&repository)?;
        } else {
            reject_cross_number_duplicate_active_segments(&repository)?;
        }
    }
    let stale_archives = if index_available && options.contains(MaintenanceTask::StaleArchives) {
        crate::progress::observe(
            observer,
            &Step::new("scanning for stale archives", WorkUnit::Archives)
                .with_total(crate::progress::count(repository.archives().len())),
            |observer| plan_stale_archives(directory, &repository, warnings, observer),
        )?
    } else {
        Vec::new()
    };

    let reference_generation = generation_from_header(&repository, current_head.segment)?;
    // One rule per run, read by the head-safety guard and by the mark phase
    // below. Binding it here rather than rebuilding it at each use is what
    // makes "the guard proved exactly what the sweep will apply" a property of
    // the code instead of a coincidence between two constants.
    let reclaim_rule = ReclaimRule {
        reference: reference_generation,
        kind: CompactionKind::Full,
        retained_generations: crate::writer::store_writer::RETAINED_GENERATIONS,
    };
    let mut current_closure = HashSet::new();
    let mut residue_segments = 0usize;
    if index_available
        && (options.contains(MaintenanceTask::Segments) || !checkpoints.names.is_empty())
    {
        let active_index_generations = active_index_generations(&repository)?;
        residue_segments =
            segments_ahead_of_the_head(&active_index_generations, reference_generation);
        crate::progress::observe(
            observer,
            &Step::new(
                "tracing segments reachable from the head",
                WorkUnit::Segments,
            ),
            |observer| {
                extend_segment_closure(
                    &repository,
                    [current_head.segment],
                    &mut current_closure,
                    observer,
                )
            },
        )?;
        validate_reclaim_reference_invariant(
            &repository,
            &current_closure,
            &active_index_generations,
            reclaim_rule,
        )?;
    }
    // The residue sweep, planned with the generation predicate disabled so
    // nothing is reclaimed by age: `i32::MAX` retained generations makes
    // `is_reclaimable` false for every realistic triple, leaving only the
    // dangling-future rule — which reclaims exactly the compacted entries
    // positioned after the head in global reverse order — and the rule that
    // frees a bulk segment nothing points at. Planned before the copy, so the
    // next run heals what a killed run left rather than accumulating it.
    let residue_sweep = if index_available && residue_segments != 0 {
        Some(crate::progress::observe(
            observer,
            &Step::new("planning residue retirement", WorkUnit::Archives)
                .with_total(crate::progress::count(repository.archives().len())),
            |observer| {
                plan_standalone_segment_cleanup(
                    directory,
                    &repository,
                    ReclaimRule {
                        reference: reference_generation,
                        kind: CompactionKind::Full,
                        retained_generations: i32::MAX,
                    },
                    current_head.segment,
                    &HashSet::new(),
                    options.archive_rewrite_policy,
                    observer,
                )
            },
        )?)
    } else {
        None
    };
    // What the copy's own reclaim pass will do, planned read-only so the
    // operator sees which archives go before authorizing the run. Exact rather
    // than estimated: the same mark and the same per-archive planner the run
    // itself uses, seeded with the bulk segments the copy will share in place.
    let predicted_sweep = match options.compaction_kind {
        Some(kind) if index_available => {
            let omitted: BTreeSet<String> = checkpoints.names.iter().cloned().collect();
            let shared =
                predict_shared_bulk_segments(&repository, current_head, &omitted, observer)?;
            let target = compaction_target_generation(reference_generation, kind);
            let absent: HashSet<String> = stale_archives
                .iter()
                .map(|stale| stale.file_name.clone())
                .collect();
            Some(crate::progress::observe(
                observer,
                &Step::new("predicting the reclamation", WorkUnit::Archives)
                    .with_total(crate::progress::count(repository.archives().len())),
                |_observer| {
                    crate::writer::store_writer::predict_post_compaction_reclamation(
                        directory,
                        &repository,
                        ReclaimRule {
                            reference: target,
                            kind,
                            retained_generations: crate::writer::store_writer::RETAINED_GENERATIONS,
                        },
                        &shared,
                        options.archive_rewrite_policy,
                        &absent,
                    )
                },
            )?)
        }
        _ => None,
    };
    let mut protected_history_segments = HashSet::new();
    let mut history_protection = HistoryProtection::default();
    // A sweep planned here judges every segment against the *current* head
    // generation. When this run copies the head into a fresh generation, that
    // reference is superseded before a single archive is touched and the plan
    // would authorize the wrong set. The copy's own reclaim pass sweeps
    // instead, against the generation it just created.
    let segment_plan = if index_available
        && options.contains(MaintenanceTask::Segments)
        && options.compaction_kind.is_none()
    {
        // Captured before the head closure is consumed as the history seed:
        // afterwards the two are indistinguishable, and the difference is
        // exactly what the journal history costs.
        let head_data_segments: HashSet<SegmentIdentifier> = current_closure
            .iter()
            .copied()
            .filter(|identifier| identifier.is_data_segment())
            .collect();
        let mut retained_closure = current_closure;
        crate::progress::observe(
            observer,
            &Step::new(
                "tracing segments reachable from history",
                WorkUnit::Segments,
            ),
            |observer| {
                extend_segment_closure(
                    &repository,
                    journal_analysis
                        .retained_record_ids
                        .iter()
                        .map(|record| record.segment),
                    &mut retained_closure,
                    observer,
                )
            },
        )?;
        protected_history_segments.extend(
            retained_closure
                .into_iter()
                .filter(|identifier| identifier.is_data_segment()),
        );
        history_protection.history_only_segments = protected_history_segments
            .iter()
            .filter(|identifier| !head_data_segments.contains(identifier))
            .count();

        let plan = crate::progress::observe(
            observer,
            &Step::new("planning segment reclamation", WorkUnit::Archives)
                .with_total(crate::progress::count(repository.archives().len())),
            |observer| {
                plan_standalone_segment_cleanup(
                    directory,
                    &repository,
                    reclaim_rule,
                    current_head.segment,
                    &protected_history_segments,
                    options.archive_rewrite_policy,
                    observer,
                )
            },
        )?;
        // What the veto costs, priced by the sweep itself rather than
        // estimated beside it. Skipped when the veto protects nothing the
        // head does not already reach, because then there is nothing to
        // price and the second pass would be pure cost.
        if history_protection.history_only_segments != 0 {
            let (unvetoed_segments, unvetoed_bytes) = crate::progress::observe(
                observer,
                &Step::new("pricing the journal-history protection", WorkUnit::Archives)
                    .with_total(crate::progress::count(repository.archives().len())),
                |observer| {
                    crate::writer::store_writer::measure_unvetoed_reclamation(
                        directory,
                        &repository,
                        reclaim_rule,
                        current_head.segment,
                        options.archive_rewrite_policy,
                        observer,
                    )
                },
            )?;
            let (vetoed_segments, vetoed_bytes) =
                crate::writer::store_writer::plan_reclaimed_totals(&plan);
            history_protection.would_be_reclaimable_segments =
                unvetoed_segments.saturating_sub(vetoed_segments);
            history_protection.would_be_reclaimable_bytes =
                unvetoed_bytes.saturating_sub(vetoed_bytes);
        }
        let retained_roots = prospective_retained_roots(
            directory,
            &repository,
            &plan,
            &journal_analysis.retained_record_ids,
        );
        crate::progress::observe(
            observer,
            &Step::new("validating the prospective plan", WorkUnit::Nodes),
            |observer| {
                validate_prospective_segment_plan(
                    directory,
                    &repository,
                    &plan,
                    &retained_roots,
                    observer,
                )
            },
        )?;
        Some(plan)
    } else {
        None
    };

    let temporaries = if index_available && options.contains(MaintenanceTask::StaleTemporaries) {
        crate::progress::observe(
            observer,
            &Step::new("scanning for stale temporary files", WorkUnit::Files),
            |observer| {
                plan_stale_temporaries(directory, &repository, &raw_journal, warnings, observer)
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
    if options.contains(MaintenanceTask::Journal) && journal_analysis.plan.removed_lines != 0 {
        ensure_numbered_name_available(directory, "journal.log.cleaning")?;
        ensure_numbered_name_available(directory, "journal.log.bak")?;
    }

    let mut actions = Vec::new();
    let mut estimated_reclaimable_bytes = 0u64;
    let mut estimated_archive_rewrite_source_bytes = 0u64;
    let mut retained_reclaimable = RetainedReclaimable::default();
    // First, because everything else in a repairing run is downstream of it —
    // and because it is the one action here that *adds* bytes rather than
    // reclaiming them, so it stays out of the reclaimable estimate.
    actions.extend(pending_repairs);
    if manifest_upgrade {
        actions.push(CompactionAction::UpgradeManifest);
    }
    if residue_sweep.is_some() {
        actions.push(CompactionAction::RetireInterruptedCompactionResidue {
            segments: residue_segments,
        });
    }
    if let Some(kind) = options.compaction_kind {
        actions.push(CompactionAction::CopyHeadIntoFreshGeneration {
            head_nodes,
            target_generation: compaction_target_generation(reference_generation, kind),
            kind,
        });
    }
    if let Some(predicted) = &predicted_sweep {
        for archive in &predicted.archives {
            match archive {
                PlannedArchiveSweep::Remove {
                    file_name,
                    segment_count,
                    file_bytes,
                } => {
                    actions.push(CompactionAction::RemoveReclaimableArchive {
                        file_name: file_name.clone(),
                        segments: *segment_count,
                        bytes: *file_bytes,
                    });
                    add_estimate(&mut estimated_reclaimable_bytes, *file_bytes)?;
                }
                PlannedArchiveSweep::Rewrite {
                    file_name,
                    replacement_name,
                    segment_count,
                    eligible_entry_bytes,
                } => {
                    actions.push(CompactionAction::RewriteArchive {
                        file_name: file_name.clone(),
                        replacement_name: replacement_name.clone(),
                        segments: *segment_count,
                        eligible_bytes: *eligible_entry_bytes,
                    });
                    add_estimate(&mut estimated_reclaimable_bytes, *eligible_entry_bytes)?;
                }
                other => retained_reclaimable_from(other, &mut retained_reclaimable, warnings)?,
            }
        }
    }
    if let Some(plan) = &segment_plan {
        for archive in &plan.archives {
            match archive {
                PlannedArchiveSweep::Remove {
                    file_name,
                    segment_count,
                    file_bytes,
                } => {
                    actions.push(CompactionAction::RemoveReclaimableArchive {
                        file_name: file_name.clone(),
                        segments: *segment_count,
                        bytes: *file_bytes,
                    });
                    add_estimate(&mut estimated_reclaimable_bytes, *file_bytes)?;
                }
                PlannedArchiveSweep::Rewrite {
                    file_name,
                    replacement_name,
                    segment_count,
                    eligible_entry_bytes,
                } => {
                    add_estimate(
                        &mut estimated_archive_rewrite_source_bytes,
                        std::fs::symlink_metadata(directory.join(file_name))?.len(),
                    )?;
                    actions.push(CompactionAction::RewriteArchive {
                        file_name: file_name.clone(),
                        replacement_name: replacement_name.clone(),
                        segments: *segment_count,
                        eligible_bytes: *eligible_entry_bytes,
                    });
                    add_estimate(&mut estimated_reclaimable_bytes, *eligible_entry_bytes)?;
                }
                PlannedArchiveSweep::DeferredBySavings {
                    file_name,
                    segment_count,
                    eligible_entry_bytes,
                } => {
                    retained_reclaimable.below_savings_gate += segment_count;
                    add_estimate(&mut retained_reclaimable.bytes, *eligible_entry_bytes)?;
                    warnings.push(format!(
                        "{file_name}: {segment_count} reclaimable segments ({}) retained because savings do not exceed Oak's 25% rewrite gate",
                        crate::units::format_byte_size(*eligible_entry_bytes)
                    ));
                }
                PlannedArchiveSweep::DeferredAtLastGeneration {
                    file_name,
                    segment_count,
                    eligible_entry_bytes,
                } => {
                    retained_reclaimable.at_last_generation += segment_count;
                    add_estimate(&mut retained_reclaimable.bytes, *eligible_entry_bytes)?;
                    warnings.push(format!(
                        "{file_name}: {segment_count} reclaimable segments ({}) retained because archive generation z cannot be rewritten",
                        crate::units::format_byte_size(*eligible_entry_bytes)
                    ));
                }
                PlannedArchiveSweep::BlockedByOccupiedGeneration {
                    file_name,
                    occupied_name,
                    segment_count,
                    eligible_entry_bytes,
                } => {
                    retained_reclaimable.blocked_by_occupied_generation += segment_count;
                    add_estimate(&mut retained_reclaimable.bytes, *eligible_entry_bytes)?;
                    warnings.push(format!(
                        "{file_name}: {segment_count} reclaimable segments ({}) retained because {occupied_name} already exists",
                        crate::units::format_byte_size(*eligible_entry_bytes)
                    ));
                }
            }
        }
    }
    if estimated_archive_rewrite_source_bytes != 0
        && available_filesystem_bytes(directory)
            .is_some_and(|available| available < estimated_archive_rewrite_source_bytes)
    {
        warnings.push(
            "available filesystem space is below the cumulative source size of planned archive rewrites; cleanup remains prefix-safe on ENOSPC, but may need a rerun after space is freed"
                .to_owned(),
        );
    }
    if estimated_archive_rewrite_source_bytes != 0 {
        warnings.push(
            "archive rewrite publication requires same-directory hard-link support, which a read-only plan cannot preflight; an unsupported filesystem fails safely with the source archive intact"
                .to_owned(),
        );
    }
    for stale in &stale_archives {
        actions.push(CompactionAction::RemoveStaleArchive {
            file_name: stale.file_name.clone(),
            reason: stale.reason,
            bytes: stale.bytes,
        });
        add_estimate(&mut estimated_reclaimable_bytes, stale.bytes)?;
    }
    if !checkpoints.names.is_empty() {
        actions.push(CompactionAction::RemoveCheckpoints {
            names: checkpoints.names.clone(),
            expired: checkpoints.expired,
            unreferenced: checkpoints.unreferenced,
        });
    }
    if options.compaction_kind.is_some() {
        // Every revision goes, whether or not it still resolves: the run keeps
        // only the line naming the head it is about to write. This is the
        // irreversible half of a maintenance run, so the plan states it plainly
        // rather than leaving the operator to infer it from a prune count that
        // describes a different rule.
        actions.push(CompactionAction::RetireJournalHistory {
            revisions: raw_journal.lines().len(),
        });
    } else if options.contains(MaintenanceTask::Journal) && journal_analysis.plan.removed_lines != 0
    {
        actions.push(CompactionAction::PruneJournal {
            lines: journal_analysis.plan.removed_lines,
            parser_ignored: journal_analysis.plan.parser_ignored,
            missing_segments: journal_analysis.plan.missing_segments,
            unreadable_revisions: journal_analysis.plan.unreadable_revisions,
            beyond_retention: journal_analysis.plan.beyond_retention,
        });
    }
    for temporary in &temporaries {
        actions.push(CompactionAction::RemoveTemporary {
            file_name: temporary.file_name.clone(),
            bytes: temporary.bytes,
        });
        add_estimate(&mut estimated_reclaimable_bytes, temporary.bytes)?;
    }
    for backup in &recovery_backups {
        actions.push(CompactionAction::RemoveRecoveryBackup {
            file_name: backup.file_name.clone(),
            bytes: backup.bytes,
        });
        add_estimate(&mut estimated_reclaimable_bytes, backup.bytes)?;
    }
    let fingerprint_after = directory_fingerprint(directory)?;
    if fingerprint_before != fingerprint_after {
        return Err(Error::InvalidFormat {
            details:
                "the repository changed while cleanup was planning; retry against a quiescent store"
                    .to_owned(),
        });
    }

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
        history_protection,
        fingerprint: fingerprint_after,
        journal: journal_analysis.plan,
        checkpoints,
        checkpoint_archive_number,
        stale_archives,
        temporaries,
        recovery_backups,
        segment_plan,
        residue_sweep,
        reference_generation,
        protected_history_segments,
        manifest_upgrade,
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

/// Verifies the exact head tree, reporting it as one step of its own.
/// A function that reports owns its step: wrapping this call in a step
/// belonging to a caller would put two different counters — nodes here,
/// whatever the caller counts — inside one report.
pub(super) fn verify_exact_super_root(
    repository: &Repository,
    head: RecordIdentifier,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    verify_exact_super_root_counting_nodes(repository, head, observer).map(|_| ())
}

/// Verifies exactly like [`verify_exact_super_root`], returning how many
/// distinct node records the head reaches. The walk happens either way, so
/// the count is free; it is what the plan reports as the size of the tree a
/// copy would rewrite.
pub(super) fn verify_exact_super_root_counting_nodes(
    repository: &Repository,
    head: RecordIdentifier,
    observer: &mut dyn ProgressObserver,
) -> Result<u64> {
    crate::progress::observe(
        observer,
        &Step::new("verifying the current head", WorkUnit::Nodes),
        |observer| {
            let mut verifier = NodeTreeVerifier::new(repository);
            verify_exact_super_root_with_verifier(repository, head, &mut verifier, observer)?;
            Ok(verifier.verified_nodes())
        },
    )
}

pub(super) fn verify_exact_super_root_with_verifier(
    repository: &Repository,
    head: RecordIdentifier,
    verifier: &mut NodeTreeVerifier<'_>,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let view = repository.segment(head.segment)?;
    if view.structure.record_type(head.record_number) != Some(RecordType::Node) {
        return Err(Error::InvalidFormat {
            details: format!("current journal head {head} is not a node record"),
        });
    }
    verifier.verify_with_progress(head, observer)?;
    let super_root = repository.node(head);
    super_root
        .child_node("root")?
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("journal root {head} has no content \"root\" child node"),
        })?;
    if let Some(checkpoints) = super_root.child_node("checkpoints")? {
        for (name, checkpoint) in checkpoints.child_node_entries()? {
            checkpoint
                .child_node("root")?
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "checkpoint {name} under journal root {head} has no snapshot \"root\" child node"
                    ),
                })?;
        }
    }
    Ok(())
}
