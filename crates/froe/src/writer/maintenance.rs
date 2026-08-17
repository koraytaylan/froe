//! Conservative offline maintenance for an existing segment-tar repository.
//!
//! Cleanup is deliberately split into a read-only plan and a prepared apply
//! session. Planning never acquires `repo.lock` and never opens the ordinary
//! writable repository (whose startup lifecycle repairs archives and rewrites
//! the manifest). A prepared session takes the repository lock, rebuilds the
//! plan from disk, fingerprints every directory entry, and holds the lock
//! until application and fresh post-operation verification complete.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[cfg(unix)]
// `PermissionsExt::mode` is always `u32`, while libc's `mode_t` (and thus
// `libc::S_ISGID`) is `u16` on Apple targets.
const SETGID_MODE: u32 = 0o2000;

use crate::content::node::PropertyValues;
use crate::content::property::PropertyValue;
use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::segment::view::SegmentView;
use crate::store::Repository;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::{ArchiveFileName, group_file_generations_newest_first};
use crate::tooling::NodeTreeVerifier;
use crate::writer::commit::remove_checkpoints;
use crate::writer::compaction::CompactionKind;
use crate::writer::journal_maintenance::{
    JournalRewriteOutcome, RawJournal, RawJournalLine, RawJournalLineClassification,
    rewrite_journal_atomically, scan_raw_journal, scan_raw_journal_file,
};
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::{
    ArchiveRewritePolicy, PlannedArchiveSweep, ReclaimRule, StandaloneSegmentCompactionOutcome,
    StandaloneSegmentCompactionPlan, WritableRepository, apply_standalone_segment_cleanup,
    certify_active_archive, certify_active_archives_with_progress, is_reclaimable,
    next_cleanup_archive_number, plan_standalone_segment_cleanup, planned_unavailable_segments,
    preserve_file_metadata, sync_directory_strict,
};

/// How many segments a closure trace visits between progress reports.
/// The trace resolves one segment per step, so reporting every segment
/// would call the observer millions of times on a large store.
const SEGMENT_TRACE_REPORT_STRIDE: u64 = 256;

/// One independently selectable cleanup category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub(crate) enum MaintenanceTask {
    /// Remove parser-ignored, missing-segment, and unreadable historical
    /// journal lines while retaining every readable revision byte-for-byte.
    Journal,
    /// Run Oak's standalone FULL/two-retained-generation segment sweep.
    Segments,
    /// Remove superseded archive letters and empty incomplete archives.
    StaleArchives,
    /// Remove checkpoints whose valid timestamp is strictly before `now`.
    ExpiredCheckpoints,
    /// Remove provably redundant interrupted-operation staging files.
    StaleTemporaries,
    /// Remove checkpoints not referenced by string values under `/:async`.
    UnreferencedCheckpoints,
    /// Apply the explicitly configured age/count policy to recovery backups.
    RecoveryBackups,
    /// Rebuild the index of an active archive that has none, retaining the
    /// original bytes under a `.bak` name.
    ///
    /// An archive whose trailers were never written is what a killed Oak
    /// writer leaves behind, and every other cleanup category is blocked by
    /// it: generation decisions may not rest on a recovery scan. Opting into
    /// this makes cleanup repair that state instead of refusing it. It is
    /// deliberately not a default, because it rewrites an archive, and
    /// because a store damaged in the middle rather than at the tail is a
    /// case the operator should look at before authorizing.
    RepairArchives,
}

impl std::fmt::Display for MaintenanceTask {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Journal => "journal",
            Self::Segments => "segments",
            Self::StaleArchives => "stale-archives",
            Self::ExpiredCheckpoints => "expired-checkpoints",
            Self::StaleTemporaries => "stale-temporaries",
            Self::UnreferencedCheckpoints => "unreferenced-checkpoints",
            Self::RecoveryBackups => "recovery-backups",
            Self::RepairArchives => "repair-archives",
        };
        formatter.write_str(name)
    }
}

/// Explicit retention policy required before cleanup may remove recovery
/// backups. Both conditions apply: a backup must be at least `minimum_age` old
/// and fall outside the newest `keep_latest_per_target` files for its target.
/// If modification times tie at the count boundary, every file in the tie is
/// retained because no safe newest-first order is provable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryBackupPolicy {
    /// Minimum age of a removable backup.
    pub minimum_age: Duration,
    /// Minimum number of newest backups retained for each original target;
    /// an mtime tie at the boundary can retain more.
    pub keep_latest_per_target: usize,
}

impl RecoveryBackupPolicy {
    /// Creates the mandatory age-and-count policy for opt-in backup cleanup.
    #[must_use]
    pub fn new(minimum_age: Duration, keep_latest_per_target: usize) -> Self {
        Self {
            minimum_age,
            keep_latest_per_target,
        }
    }

    /// Minimum age of a removable backup.
    #[must_use]
    pub fn minimum_age(&self) -> Duration {
        self.minimum_age
    }

    /// Minimum number of newest backups retained for each original target;
    /// an mtime tie at the boundary can retain more.
    #[must_use]
    pub fn keep_latest_per_target(&self) -> usize {
        self.keep_latest_per_target
    }
}

/// Cleanup selection and opt-in retention settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionOptions {
    tasks: BTreeSet<MaintenanceTask>,
    recovery_backup_policy: Option<RecoveryBackupPolicy>,
    journal_revision_retention: Option<NonZeroUsize>,
    archive_rewrite_policy: ArchiveRewritePolicy,
    compaction_kind: Option<CompactionKind>,
}

impl Default for CompactionOptions {
    fn default() -> Self {
        Self {
            tasks: BTreeSet::from([
                MaintenanceTask::Journal,
                MaintenanceTask::Segments,
                MaintenanceTask::StaleArchives,
                MaintenanceTask::ExpiredCheckpoints,
                MaintenanceTask::StaleTemporaries,
            ]),
            recovery_backup_policy: None,
            // Unbounded: every readable revision stays a tracing root, which
            // is the conservative default froe has always applied.
            journal_revision_retention: None,
            // Reclaim everything the mark phase proves dead. An offline,
            // operator-invoked run that identifies garbage and then declines to
            // move it is the defect this default fixes.
            archive_rewrite_policy: ArchiveRewritePolicy::EveryReclaimableArchive,
            compaction_kind: None,
        }
    }
}

impl CompactionOptions {
    /// Starts with the conservative default task set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the selected task set. Supplying an empty iterator performs a
    /// health-only plan/apply.
    #[must_use]
    /// Replaces the internal task set. Test-only: the command's own surface
    /// is the flags above, and a run cannot be restricted to one stage.
    #[cfg(test)]
    pub(crate) fn with_tasks(mut self, tasks: impl IntoIterator<Item = MaintenanceTask>) -> Self {
        self.tasks = tasks.into_iter().collect();
        self
    }

    /// Enables one task in addition to the current selection.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn with_task(mut self, task: MaintenanceTask) -> Self {
        self.tasks.insert(task);
        self
    }

    /// Enables backup cleanup with its mandatory two-part retention policy.
    #[must_use]
    pub fn with_recovery_backup_policy(mut self, policy: RecoveryBackupPolicy) -> Self {
        self.tasks.insert(MaintenanceTask::RecoveryBackups);
        self.recovery_backup_policy = Some(policy);
        self
    }

    /// Keeps only the newest `revisions` resolvable journal revisions,
    /// removing the older lines and — decisively — releasing their segment
    /// closure from the history keep-veto.
    ///
    /// Without a bound, froe treats every readable revision as a tracing
    /// root, which is stricter than Oak: Oak judges data segments by their
    /// index generation triple alone and leaves `journal.log` untouched. On a
    /// long-lived store that veto is normally why cleanup reclaims nothing.
    /// A bound of one leaves the current head as the only root, which is the
    /// closest standalone cleanup comes to Oak's own retention.
    ///
    /// This implies the journal task: the bounded lines must actually
    /// leave the journal in the same run. A line that stopped being a root
    /// while remaining in the file would still be verified as retained
    /// history when the plan is validated, and the run would refuse itself.
    #[must_use]
    pub fn with_journal_revision_retention(mut self, revisions: NonZeroUsize) -> Self {
        self.tasks.insert(MaintenanceTask::Journal);
        self.journal_revision_retention = Some(revisions);
        self
    }

    /// Applies Oak's 25% savings heuristic instead of rewriting every archive
    /// that holds reclaimable segments.
    ///
    /// froe's default reclaims all identified garbage, which is what an
    /// offline, operator-invoked cleanup is for. Oak's gate keeps an archive
    /// untouched unless the rewrite would shrink it by at least a quarter,
    /// which on a store whose archives hold live binary content alongside
    /// dead node segments means the garbage is never removed by any number of
    /// runs. Selecting the gate makes this run leave behind exactly what
    /// `oak-run compact` would, at the cost of retaining garbage the run has
    /// already proved removable.
    #[must_use]
    pub fn with_oak_savings_gate(mut self) -> Self {
        self.archive_rewrite_policy = ArchiveRewritePolicy::OakSavingsGate;
        self
    }

    /// Which archives a sweep is willing to rewrite.
    #[must_use]
    pub fn archive_rewrite_policy(&self) -> ArchiveRewritePolicy {
        self.archive_rewrite_policy
    }

    /// Deep-copies the head into a fresh garbage-collection generation as part
    /// of this run, and reclaims every generation the copy supersedes.
    ///
    /// The copy is what makes reclamation complete rather than incidental: a
    /// sweep alone works at segment granularity, so a segment holding one live
    /// record is wholly live however much dead content sits beside it. Only a
    /// rewrite recovers that, and only a rewrite lets the run retain a single
    /// generation.
    ///
    /// Checkpoints this run retires are omitted from the copy rather than
    /// removed from the live head first, so the head moves exactly once and
    /// no record is written at a generation the same run then reclaims.
    #[must_use]
    pub fn with_compaction(mut self, kind: CompactionKind) -> Self {
        self.compaction_kind = Some(kind);
        self
    }

    /// The compaction this run performs, if any.
    #[must_use]
    pub fn compaction_kind(&self) -> Option<CompactionKind> {
        self.compaction_kind
    }

    /// Carries checkpoints whose valid timestamp has passed into the fresh
    /// generation instead of dropping them.
    ///
    /// Dropping is the default: an expired checkpoint's content is otherwise
    /// copied into the new generation, where one retained generation can never
    /// reclaim it.
    #[must_use]
    pub fn keeping_expired_checkpoints(mut self) -> Self {
        self.tasks.remove(&MaintenanceTask::ExpiredCheckpoints);
        self
    }

    /// Also drops checkpoints that no string value under `/:async`
    /// references. Not a default: an operator-created checkpoint held for a
    /// backup is unreferenced by that rule.
    #[must_use]
    pub fn with_unreferenced_checkpoint_removal(mut self) -> Self {
        self.tasks.insert(MaintenanceTask::UnreferencedCheckpoints);
        self
    }

    /// Rebuilds the index of an active archive that has none — what a killed
    /// Oak writer leaves behind — keeping the original under a `.bak` name.
    ///
    /// Not a default: it rewrites an archive, and a store damaged in the
    /// middle rather than at the tail is a case to look at before authorizing.
    #[must_use]
    pub fn with_archive_index_repair(mut self) -> Self {
        self.tasks.insert(MaintenanceTask::RepairArchives);
        self
    }

    /// Selected tasks in deterministic order.
    pub(crate) fn tasks(&self) -> impl Iterator<Item = MaintenanceTask> + '_ {
        self.tasks.iter().copied()
    }

    /// Whether a category is selected.
    #[must_use]
    pub(crate) fn contains(&self, task: MaintenanceTask) -> bool {
        self.tasks.contains(&task)
    }
}

/// Why an archive file can be removed without losing an active segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StaleArchiveReason {
    /// A different letter of the same archive number has the newest valid
    /// index and is the active reader winner.
    Superseded,
    /// The file is empty and therefore contains no recoverable segment.
    EmptyIncomplete,
}

impl std::fmt::Display for StaleArchiveReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Superseded => "superseded by the active archive generation",
            Self::EmptyIncomplete => "empty incomplete archive",
        })
    }
}

/// Why one physical journal line is removable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum JournalRemovalReason {
    /// The tolerant journal reader skips a line that contains no ASCII space.
    ParserSkippedNoSpace,
    /// The first space-delimited field is not a valid record identifier.
    InvalidRecordIdentifier,
    /// The record identifier names a segment that is not present.
    MissingSegment,
    /// The non-current historical node revision does not fully traverse.
    UnreadableRevision,
    /// The revision resolves, but an explicit retention bound keeps only
    /// newer revisions. Removing the line is what releases its closure from
    /// the history keep-veto; without it the line stays a tracing root and
    /// the segments behind it stay protected.
    BeyondRetention,
}

impl std::fmt::Display for JournalRemovalReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ParserSkippedNoSpace => "parser-skipped (no ASCII space)",
            Self::InvalidRecordIdentifier => "invalid record identifier",
            Self::MissingSegment => "missing segment",
            Self::UnreadableRevision => "unreadable historical revision",
            Self::BeyondRetention => "beyond the journal retention bound",
        })
    }
}

/// One physical journal line selected for removal.
///
/// The preview is an exact, bounded prefix of the line excluding its line
/// terminator. It is bytes rather than text so invalid UTF-8 remains auditable;
/// terminal applications must escape it before display.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct JournalLineRemoval {
    line_number: usize,
    record_identifier: Option<RecordIdentifier>,
    reason: JournalRemovalReason,
    preview: Vec<u8>,
    preview_truncated: bool,
}

impl JournalLineRemoval {
    /// One-based physical line number in the journal snapshot.
    #[must_use]
    pub fn line_number(&self) -> usize {
        self.line_number
    }

    /// Parsed record identifier, when the line contained one.
    #[must_use]
    pub fn record_identifier(&self) -> Option<RecordIdentifier> {
        self.record_identifier
    }

    /// Structured reason for removing this line.
    #[must_use]
    pub fn reason(&self) -> JournalRemovalReason {
        self.reason
    }

    /// Exact bounded prefix of the line, excluding its terminator.
    #[must_use]
    pub fn preview_bytes(&self) -> &[u8] {
        &self.preview
    }

    /// Whether bytes after [`Self::preview_bytes`] were omitted.
    #[must_use]
    pub fn preview_truncated(&self) -> bool {
        self.preview_truncated
    }
}

const ALREADY_ABSENT_DELETION_DETAIL: &str = "file was already absent when deletion was attempted";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileDeletionFailureKind {
    Retained,
    AlreadyAbsent,
}

/// A planned deletion that this cleanup could not perform or confirm itself.
///
/// The target usually remains for a later retry. It can instead have already
/// been absent when the guarded unlink was reached; use
/// [`Self::target_was_already_absent`] to distinguish that auditable race.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileDeletionFailure {
    file_name: String,
    error: String,
    kind: FileDeletionFailureKind,
}

impl FileDeletionFailure {
    fn retained(file_name: String, error: impl Into<String>) -> Self {
        Self {
            file_name,
            error: error.into(),
            kind: FileDeletionFailureKind::Retained,
        }
    }

    fn already_absent(file_name: String, error: impl Into<String>) -> Self {
        Self {
            file_name,
            error: error.into(),
            kind: FileDeletionFailureKind::AlreadyAbsent,
        }
    }

    /// Exact managed file name involved in the partial deletion result.
    ///
    /// The path need not remain when [`Self::target_was_already_absent`]
    /// returns `true`.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Operating-system or consistency detail from the incomplete or
    /// externally satisfied deletion.
    #[must_use]
    pub fn error(&self) -> &str {
        &self.error
    }

    /// Whether another actor had already removed the exact planned pathname
    /// when cleanup reached its guarded deletion.
    #[must_use]
    pub fn target_was_already_absent(&self) -> bool {
        self.kind == FileDeletionFailureKind::AlreadyAbsent
    }
}

/// One concrete, deterministically ordered cleanup action.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompactionAction {
    /// Rebuild the index of an active archive that has none.
    ///
    /// Reported by the read-only preview, which cannot do more than name the
    /// work: the repair itself happens under the repository lock, and every
    /// index-dependent decision — the segment sweep, checkpoint removal —
    /// can only be planned once it has. The authoritative plan the CLI
    /// re-confirms is therefore always larger than the preview that named
    /// this.
    RepairArchiveIndex {
        /// Archive file name the rebuilt archive is installed under: the
        /// lowest non-empty generation letter of its number.
        file_name: String,
        /// Other generation letters of the same number, whose contents are
        /// merged into the rebuild and which are then retired to `.bak`
        /// names. Named because confirmation is scoped to the files a plan
        /// printed, and these leave the archive namespace.
        retired_file_names: Vec<String>,
        /// Why the existing index was rejected.
        reason: String,
        /// Whole-file bytes across every letter that will be read, which is
        /// what ends up retained under `.bak` names.
        bytes: u64,
    },
    /// Rewrite the journal while retaining readable record lines verbatim.
    PruneJournal {
        /// Total physical lines removed.
        lines: usize,
        /// Lines the tolerant reader already skips.
        parser_ignored: usize,
        /// Syntactic record lines whose head segment is absent.
        missing_segments: usize,
        /// Non-current historical node roots that do not fully traverse.
        unreadable_revisions: usize,
        /// Resolvable revisions older than an explicit retention bound.
        beyond_retention: usize,
    },
    /// Atomically raise `store.version` from 1 to 2 before writing v2 data.
    UpgradeManifest,
    /// Remove checkpoints in one head update.
    RemoveCheckpoints {
        /// Exact checkpoint names, sorted and deduplicated.
        names: Vec<String>,
        /// Names selected because their valid timestamp has expired.
        expired: usize,
        /// Additional names selected by the opt-in `/:async` rule.
        unreferenced: usize,
    },
    /// Unlink a fully reclaimable active archive.
    RemoveReclaimableArchive {
        /// Current archive file name.
        file_name: String,
        /// Segments made unavailable by the unlink.
        segments: usize,
        /// Current whole-file bytes.
        bytes: u64,
    },
    /// Rewrite an active archive to its next letter with only survivors.
    RewriteArchive {
        /// Source archive file name.
        file_name: String,
        /// Exclusively created replacement name.
        replacement_name: String,
        /// Segments omitted from the replacement.
        segments: usize,
        /// TAR-entry bytes eligible for reclamation.
        eligible_bytes: u64,
    },
    /// Remove an inactive archive generation or empty incomplete archive.
    RemoveStaleArchive {
        /// Exact archive file name.
        file_name: String,
        /// Proof supporting removal.
        reason: StaleArchiveReason,
        /// Current whole-file bytes.
        bytes: u64,
    },
    /// Remove a provably redundant interrupted-operation staging file.
    RemoveTemporary {
        /// Exact file name.
        file_name: String,
        /// Current whole-file bytes.
        bytes: u64,
    },
    /// Retire every journal revision but the one the copy publishes.
    ///
    /// Named separately from `PruneJournal`, which describes removing lines
    /// that cannot resolve. This removes lines that resolve perfectly well,
    /// by policy, because the segments behind them are what the run reclaims.
    RetireJournalHistory {
        /// Physical journal lines present before the run, all of which are
        /// replaced by the single line naming the compacted head.
        revisions: usize,
    },
    /// Retire the output an interrupted earlier compaction left behind.
    ///
    /// Segments stamped ahead of the head are, by construction, the copy of a
    /// run that died before it committed. No ordinary rule removes them, so a
    /// killed run would otherwise leave residue that every later run steps
    /// around while it holds bulk segments alive.
    RetireInterruptedCompactionResidue {
        /// Data segments found ahead of the head.
        segments: usize,
    },
    /// Deep-copy the head, and every checkpoint this run retains, into a
    /// fresh garbage-collection generation.
    CopyHeadIntoFreshGeneration {
        /// Distinct node records the current head reaches.
        ///
        /// What the copy rewrites is this minus whatever only a retired
        /// checkpoint reaches, so it is an exact statement about the store
        /// rather than a prediction about the copy.
        head_nodes: u64,
        /// The generation the copy writes into.
        target_generation: GarbageCollectionGeneration,
        /// Whether this is a full or a tail compaction.
        kind: CompactionKind,
    },
    /// Remove an explicitly authorized old recovery backup.
    RemoveRecoveryBackup {
        /// Exact file name.
        file_name: String,
        /// Current whole-file bytes.
        bytes: u64,
    },
}

/// Segments this run identified as reclaimable and then declined to remove.
///
/// Every count here is garbage the mark phase proved removable; the archive
/// sweep kept it anyway, because rewriting the archive that holds it would
/// not repay the rewrite. Reporting it is what separates "this store holds no
/// garbage" from "this store holds garbage that is not worth moving".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RetainedReclaimable {
    /// Segments kept by Oak's 25% savings gate.
    below_savings_gate: usize,
    /// Segments kept because the archive exhausted the `a`–`z` namespace.
    at_last_generation: usize,
    /// Segments kept because another generation pathname is occupied.
    blocked_by_occupied_generation: usize,
    /// TAR entry bytes those segments occupy, summed across every reason.
    bytes: u64,
}

impl RetainedReclaimable {
    /// Segments identified as reclaimable and left in place, all reasons.
    fn segments(self) -> usize {
        self.below_savings_gate
            .saturating_add(self.at_last_generation)
            .saturating_add(self.blocked_by_occupied_generation)
    }
}

/// What the journal-history keep-veto protects, and what it costs.
///
/// froe retains every readable journal revision as a tracing root, which Oak
/// does not do: Oak judges data segments by their index generation triple
/// alone. The veto is strictly conservative, so it can never delete anything
/// Oak would keep — but on a long-lived store it is normally the single
/// largest reason a cleanup reclaims nothing, and nothing in the run used to
/// say so.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HistoryProtection {
    /// Data segments reachable only from a historical journal revision, and
    /// not from the current head.
    history_only_segments: usize,
    /// Segments this same sweep would physically free with the veto lifted
    /// and nothing else changed. Measured by replanning rather than reasoned
    /// about: the veto holds bulk segments only through the data segments
    /// that reference them, and releasing more of an archive can carry it
    /// over the 25% rewrite gate. Counting protected data segments alone
    /// understates this by orders of magnitude on a store whose history
    /// holds inline binaries.
    would_be_reclaimable_segments: usize,
    /// Bytes those segments occupy, whole archive files included where the
    /// unvetoed sweep would unlink one outright.
    would_be_reclaimable_bytes: u64,
}

/// A strictly read-only cleanup analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionPlan {
    directory: PathBuf,
    tasks: Vec<MaintenanceTask>,
    current_head: RecordIdentifier,
    actions: Vec<CompactionAction>,
    warnings: Vec<String>,
    estimated_reclaimable_bytes: u64,
    estimated_archive_rewrite_source_bytes: u64,
    retained_reclaimable: RetainedReclaimable,
    history_protection: HistoryProtection,
    fingerprint: DirectoryFingerprint,
    journal: JournalPlan,
    checkpoints: CheckpointPlan,
    checkpoint_archive_number: Option<u32>,
    stale_archives: Vec<StaleArchive>,
    temporaries: Vec<PlannedFileRemoval>,
    recovery_backups: Vec<PlannedFileRemoval>,
    segment_plan: Option<StandaloneSegmentCompactionPlan>,
    /// The sweep that retires an interrupted earlier compaction's output,
    /// applied before this run's own copy.
    residue_sweep: Option<StandaloneSegmentCompactionPlan>,
    reference_generation: GarbageCollectionGeneration,
    protected_history_segments: HashSet<SegmentIdentifier>,
    manifest_upgrade: bool,
}

impl CompactionPlan {
    /// Canonical absolute repository directory this plan describes.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Selected cleanup categories in deterministic order.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn tasks(&self) -> &[MaintenanceTask] {
        &self.tasks
    }

    /// Exact current head verified while planning.
    #[must_use]
    pub fn current_head(&self) -> RecordIdentifier {
        self.current_head
    }

    /// Concrete mutations in deterministic display order.
    #[must_use]
    pub fn actions(&self) -> &[CompactionAction] {
        &self.actions
    }

    /// Exact physical journal lines selected for removal. This is empty unless
    /// the journal task was selected; internal journal analysis still
    /// runs for the safety of other tasks.
    #[must_use]
    pub fn journal_line_removals(&self) -> &[JournalLineRemoval] {
        if self.tasks.contains(&MaintenanceTask::Journal) {
            &self.journal.removals
        } else {
            &[]
        }
    }

    /// Non-fatal deferrals and malformed metadata retained for safety.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Conservative sum of whole files and TAR entry bytes selected for
    /// removal. Archive overhead and deletion failures can make the actual
    /// result differ.
    #[must_use]
    pub fn estimated_reclaimable_bytes(&self) -> u64 {
        self.estimated_reclaimable_bytes
    }

    /// Sum of the current source-file sizes for archives that will be
    /// rewritten. Source mappings stay open through the sweep, so the
    /// filesystem may need cumulative additional space of this order. This is
    /// an operational proxy for archive rewriting, not a bound on other cleanup
    /// files or filesystem allocation overhead.
    #[must_use]
    pub fn estimated_archive_rewrite_source_bytes(&self) -> u64 {
        self.estimated_archive_rewrite_source_bytes
    }

    /// Segments proved reclaimable that this run will nevertheless leave in
    /// place, because rewriting the archives holding them is not worthwhile
    /// or not possible. Nonzero alongside a zero reclaimable estimate means
    /// the store holds garbage this cleanup declined, not that it holds none.
    #[must_use]
    pub fn retained_reclaimable_segments(&self) -> usize {
        self.retained_reclaimable.segments()
    }

    /// TAR entry bytes occupied by [`Self::retained_reclaimable_segments`].
    #[must_use]
    pub fn retained_reclaimable_bytes(&self) -> u64 {
        self.retained_reclaimable.bytes
    }

    /// Data segments kept alive only because a historical journal revision
    /// still reaches them. Zero unless the segment task ran.
    #[must_use]
    pub fn history_protected_segments(&self) -> usize {
        self.history_protection.history_only_segments
    }

    /// Those of [`Self::history_protected_segments`] that Oak's generation
    /// predicate would have reclaimed, and the bytes they occupy. This is
    /// what retiring the journal history — a full compaction — would make
    /// eligible; standalone cleanup never will.
    #[must_use]
    pub fn history_protected_reclaimable(&self) -> (usize, u64) {
        (
            self.history_protection.would_be_reclaimable_segments,
            self.history_protection.would_be_reclaimable_bytes,
        )
    }

    /// Whether application would request any mutation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// What a run's deep copy produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompactedGeneration {
    /// Distinct node records the copy rewrote.
    pub nodes: u64,
    /// The garbage-collection generation the copy wrote into.
    pub generation: GarbageCollectionGeneration,
}

/// Result of a prepared maintenance application and its final fresh
/// verification.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompactionOutcome {
    /// Head before cleanup.
    pub head_before: RecordIdentifier,
    /// Freshly reopened and verified head after cleanup.
    pub head_after: RecordIdentifier,
    /// Number of checkpoints removed in one logical commit.
    pub removed_checkpoints: u64,
    /// Journal physical lines removed.
    pub removed_journal_lines: usize,
    /// Active archives rewritten.
    pub rewritten_archives: usize,
    /// Active fully reclaimable archives unlinked.
    pub removed_reclaimable_archives: usize,
    /// Superseded/empty archive files unlinked.
    pub removed_stale_archives: usize,
    /// Proven staging files removed.
    pub removed_temporaries: usize,
    /// Opt-in recovery backups removed.
    pub removed_recovery_backups: usize,
    /// Archive indexes rebuilt before planning, under the repository lock.
    pub repaired_archives: usize,
    /// Recognized deletion targets this cleanup did not unlink itself.
    ///
    /// Most entries remain for retry; entries reported as already absent need
    /// no further deletion attempt.
    pub files_not_deleted: Vec<String>,
    /// Bytes in recognized archive files before application.
    pub archive_bytes_before: u64,
    /// Bytes in recognized archive files after application.
    pub archive_bytes_after: u64,
    /// Bytes still held by retained recovery backups after application.
    ///
    /// These sit outside [`Self::archive_bytes_after`], which counts only
    /// active archive names. A run that rebuilds an index retires the
    /// original under a `.bak` name, so the directory grows by this much
    /// while the archive figures report no change at all.
    pub retained_recovery_backup_bytes: u64,
    /// Distinct node records copied into the fresh generation, and the
    /// generation they were copied into. `None` when the run did not compact.
    pub compacted: Option<CompactedGeneration>,
    removed_segments: usize,
    journal_backup_path: Option<PathBuf>,
    deletion_failures: Vec<FileDeletionFailure>,
}

impl CompactionOutcome {
    /// Orphan segments removed from the active archive set.
    #[must_use]
    pub fn removed_segments(&self) -> usize {
        self.removed_segments
    }

    /// Durable byte-exact journal backup created by this cleanup, if any.
    #[must_use]
    pub fn journal_backup_path(&self) -> Option<&Path> {
        self.journal_backup_path.as_deref()
    }

    /// Planned deletions this cleanup did not perform itself, making the
    /// result partial even when another actor already removed a target.
    #[must_use]
    pub fn deletion_failures(&self) -> &[FileDeletionFailure] {
        &self.deletion_failures
    }

    /// Whether this cleanup itself completed every planned deletion without
    /// an auditable partial result.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.deletion_failures.is_empty()
    }
}

/// An authoritative cleanup plan protected by the held repository lock.
pub struct PreparedCompaction {
    directory: PathBuf,
    options: CompactionOptions,
    plan: CompactionPlan,
    /// Archives whose index this preparation already rebuilt, before the
    /// plan was built. Carried so the outcome can report work that, by
    /// construction, happened before there was a plan to record it in.
    repaired: Vec<crate::writer::store_writer::RepairedArchive>,
    repository_lock: Arc<RepositoryLock>,
}

impl PreparedCompaction {
    /// Resolves the repository path once to its canonical absolute target,
    /// validates it without mutation, acquires `repo.lock`, and rebuilds an
    /// authoritative plan while holding that lock.
    pub fn prepare(directory: &Path, options: CompactionOptions) -> Result<Self> {
        Self::prepare_with_progress(directory, options, &mut DiscardedProgress)
    }

    /// Prepares exactly like [`PreparedCompaction::prepare`], reporting the
    /// authoritative replan — the slow part, repeated under the lock — to
    /// `observer`.
    pub fn prepare_with_progress(
        directory: &Path,
        options: CompactionOptions,
        observer: &mut dyn ProgressObserver,
    ) -> Result<Self> {
        validate_options(&options)?;
        let directory = canonical_repository_directory(directory)?;
        validate_repository_shape(&directory)?;
        // The store version, before anything is written. `build_plan` reaches
        // this through `Repository::open`, but the repair below runs first, so
        // without it here froe would rewrite the archives of a store it then
        // declares itself unable to read — and a caller using the library API
        // directly never gets the lockless preview that would have refused.
        crate::store::check_manifest(&directory, true)?;
        validate_apply_environment(&directory)?;
        validate_apply_identity(&directory)?;
        let repository_lock = Arc::new(RepositoryLock::acquire(&directory)?);
        // The path may have changed between the lockless shape check and lock
        // acquisition. Revalidate every managed type while the cooperative
        // repository lock is held before reading the authoritative plan.
        validate_repository_shape(&directory)?;
        crate::store::check_manifest(&directory, true)?;
        validate_apply_environment(&directory)?;
        validate_apply_identity(&directory)?;
        repository_lock.validate_path_identity(&directory)?;
        let repaired = Self::repair_before_planning(&directory, &options, observer)?;
        let now = SystemTime::now();
        let plan = build_plan(&directory, &options, now, observer).map_err(|error| {
            // The repair already happened and is durable. Every gate below it
            // — the duplicate-segment check, the generation invariant, the
            // segment plan — is evaluating this store for the first time in
            // its history, precisely because the index-less state suppressed
            // them before. So this path is ordinary, not exotic, and a
            // refusal that did not mention the rewrite would leave the
            // operator believing nothing moved.
            attach_completed_repairs(error, &repaired)
        })?;
        // Inside the same guard: this is the one identity gate that can only
        // fire after the repair, because the archives it inspects are the
        // ones the repair just wrote.
        validate_plan_apply_identity(&directory, &plan)
            .map_err(|error| attach_completed_repairs(error, &repaired))?;
        Ok(Self {
            directory,
            options,
            plan,
            repaired,
            repository_lock,
        })
    }

    /// Rebuilds index-less archives, when the task is selected and there is
    /// something to rebuild.
    ///
    /// This is the only mutation that precedes planning, because every
    /// index-dependent decision is impossible until it has run — the preview
    /// could name it and nothing more. It is gated on the *scan*, never on
    /// task selection alone: a store with nothing to repair must come out of
    /// a repair run byte-identical, including its manifest.
    fn repair_before_planning(
        directory: &Path,
        options: &CompactionOptions,
        observer: &mut dyn ProgressObserver,
    ) -> Result<Vec<crate::writer::store_writer::RepairedArchive>> {
        if !options.contains(MaintenanceTask::RepairArchives) {
            return Ok(Vec::new());
        }
        // One predicate for the whole decision. `repairable` — not merely
        // "index-less" — is what gates the irreversible steps below, because
        // a number that cannot be rebuilt makes the run fail however it is
        // retried, and paying a manifest upgrade or a durable rewrite to
        // discover that is exactly the trade this ordering exists to avoid.
        let survey = crate::writer::store_writer::survey_indexless_archive_numbers(directory)?;
        if !survey.unrepairable.is_empty() {
            return Err(crate::writer::store_writer::unrepairable_archives_refusal(
                &survey.unrepairable,
            ));
        }
        if survey.repairable == 0 {
            return Ok(Vec::new());
        }
        // Duplicate `(number, letter)` pairs before the upgrade, not after:
        // the repair refuses them, and a refusal must not cost a one-way
        // manifest transition. `Repository::open` rejects such a store, so
        // only a library caller skipping the preview reaches this.
        crate::writer::store_writer::reject_duplicate_archive_generations(directory)?;
        // Ownership of the archives about to be rewritten, from stat(2),
        // before anything is touched. A rebuild ends in
        // `preserve_file_metadata`, whose `fchown` fails EPERM when the
        // target belongs to another uid — and the newest archive of a store
        // whose Oak ran as root is exactly the killed-writer artifact this
        // task exists to repair. Ownership does not change on retry, so
        // discovering it after the rewrite means no rerun ever converges.
        // Nothing else covers these files: `journal_service_user_issue`
        // stats only `journal.log`, and `planned_metadata_sources` is
        // consulted after the repair has already run.
        Self::validate_repair_target_identity(directory)?;
        // A rebuilt archive carries a version-2 trailer, so a version-1 store
        // is raised first — but only at the instant one is about to become
        // visible, never merely because a rebuild was predicted. A repair can
        // still fail per archive for reasons no survey models (a full disk, a
        // blob catalog that will not resolve), and paying an irreversible
        // format transition for a run that then rebuilds nothing would leave
        // the store damaged *and* unopenable by an older Oak.
        let manifest_upgrade = &mut ManifestUpgradeOnFirstInstall::new(directory);
        crate::writer::store_writer::repair_indexless_archive_numbers(
            directory,
            observer,
            manifest_upgrade,
        )
    }

    /// Refuses before the first rewrite when an archive the repair would
    /// replace cannot have its metadata preserved by this process.
    fn validate_repair_target_identity(directory: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

            let credentials = current_apply_credentials()?;
            let directory_metadata = std::fs::symlink_metadata(directory)?;
            let possible_created_gids = possible_created_group_ids(
                directory_metadata.gid(),
                directory_metadata.permissions().mode(),
                &credentials,
            );
            for name in crate::writer::store_writer::repair_target_names(directory)? {
                let path = directory.join(&name);
                let metadata = std::fs::symlink_metadata(&path)?;
                if let Some(issue) = metadata_source_apply_identity_issue(
                    &path,
                    metadata.uid(),
                    metadata.gid(),
                    metadata.permissions().mode(),
                    &possible_created_gids,
                    &credentials,
                ) {
                    return Err(Error::InvalidFormat { details: issue });
                }
            }
        }
        #[cfg(not(unix))]
        let _ = directory;
        Ok(())
    }

    /// The lock-protected plan callers should display and confirm.
    #[must_use]
    pub fn plan(&self) -> &CompactionPlan {
        &self.plan
    }

    /// Archive indexes this preparation already rebuilt.
    ///
    /// Non-zero only for a `repair-archives` run, and non-zero *before*
    /// anything is applied: the rebuild is what made the plan computable, so
    /// it is durable by the time a caller sees this. A caller that declines
    /// the plan has still had these archives rewritten, and should say so.
    #[must_use]
    pub fn repaired_archives(&self) -> usize {
        self.repaired.len()
    }

    /// Applies exactly this authoritative plan, failing before the first
    /// mutation if any directory entry changed after planning.
    pub fn apply(self) -> Result<CompactionOutcome> {
        apply_prepared(self, &mut DiscardedProgress)
    }

    /// Applies exactly like [`PreparedCompaction::apply`], reporting the
    /// archive rewrites and file removals to `observer`. Reporting cannot
    /// alter the mutation sequence: the observer is told what has already
    /// been done and never decides anything.
    pub fn apply_with_progress(
        self,
        observer: &mut dyn ProgressObserver,
    ) -> Result<CompactionOutcome> {
        apply_prepared(self, observer)
    }
}

fn validate_apply_environment(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // Exercise the exact durability primitive before taking the lock or
        // performing the first mutation. Some filesystems reject directory
        // fsync; discovering that after an unlink would be too late.
        sync_directory_strict(directory)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Err(Error::InvalidFormat {
            details: "cleanup apply is supported only on Unix; dry-run planning remains available on this platform"
                .to_owned(),
        })
    }
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn validate_apply_identity(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not access memory.
        let effective_uid = unsafe { libc::geteuid() };
        validate_apply_identity_for_uid(directory, effective_uid)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
}

#[cfg(unix)]
fn validate_apply_identity_for_uid(directory: &Path, effective_uid: u32) -> Result<()> {
    if let Some(issue) = journal_service_user_issue(directory, effective_uid)? {
        return Err(Error::InvalidFormat {
            details: format!(
                "{issue}; refusing before repo.lock or replacement files can be created with the wrong owner"
            ),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn journal_service_user_issue(directory: &Path, effective_uid: u32) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt as _;

    let journal = directory.join("journal.log");
    let owner = std::fs::symlink_metadata(&journal)?.uid();
    Ok((owner != effective_uid).then(|| {
        format!(
            "cleanup must run as the repository service user: {} is owned by uid {owner}, but the effective uid is {effective_uid}",
            journal.display()
        )
    }))
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn validate_plan_apply_identity(directory: &Path, plan: &CompactionPlan) -> Result<()> {
    #[cfg(unix)]
    {
        let credentials = current_apply_credentials()?;
        validate_plan_apply_identity_for_credentials(directory, plan, &credentials)
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, plan);
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ApplyCredentials {
    effective_uid: u32,
    effective_gid: u32,
    group_ids: BTreeSet<u32>,
}

#[cfg(unix)]
fn current_apply_credentials() -> Result<ApplyCredentials> {
    // SAFETY: these credential queries have no memory preconditions. The
    // null first `getgroups` call requests only the required element count.
    let caller_uid = unsafe { libc::geteuid() };
    // SAFETY: `getegid` has no preconditions and does not access memory.
    let primary_group = unsafe { libc::getegid() };
    // SAFETY: a zero-sized group query permits a null output pointer.
    let group_count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if group_count < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut raw_groups = vec![0 as libc::gid_t; group_count as usize];
    if group_count != 0 {
        // SAFETY: `raw_groups` has exactly `group_count` writable elements.
        let returned = unsafe { libc::getgroups(group_count, raw_groups.as_mut_ptr()) };
        if returned < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        raw_groups.truncate(returned as usize);
    }
    let mut group_ids: BTreeSet<u32> = raw_groups.into_iter().collect();
    group_ids.insert(primary_group);
    Ok(ApplyCredentials {
        effective_uid: caller_uid,
        effective_gid: primary_group,
        group_ids,
    })
}

#[cfg(unix)]
fn planned_metadata_sources(
    directory: &Path,
    manifest_upgrade: bool,
    segment_plan: Option<&StandaloneSegmentCompactionPlan>,
    moves_checkpoint_head: bool,
    rewrites_journal: bool,
) -> Result<BTreeSet<String>> {
    let mut metadata_sources = BTreeSet::new();
    if manifest_upgrade {
        metadata_sources.insert("manifest".to_owned());
    }
    if rewrites_journal {
        metadata_sources.insert("journal.log".to_owned());
    }
    if let Some(segment_plan) = segment_plan {
        metadata_sources.extend(segment_plan.archives.iter().filter_map(|archive| {
            if let PlannedArchiveSweep::Rewrite { file_name, .. } = archive {
                Some(file_name.clone())
            } else {
                None
            }
        }));
    }
    if moves_checkpoint_head {
        let dispositions: HashMap<&str, &PlannedArchiveSweep> = segment_plan
            .into_iter()
            .flat_map(|plan| plan.archives.iter())
            .map(|archive| (archive.file_name(), archive))
            .collect();
        // `open_prepared` takes metadata from the first active archive, in
        // newest-number-first order. A planned whole removal can either leave
        // that source in place after a safe unlink failure or make the next
        // archive the template. Consequently only the leading run of Remove
        // sources plus the first non-Remove source can become the template.
        // A rewrite stays at the same archive number and copies its source
        // metadata onto the replacement, so its source is the exact preflight
        // representative and terminates the candidate prefix.
        for archive in crate::store::open_all_archives(directory)? {
            let file_name = archive.file_name().to_owned();
            metadata_sources.insert(file_name.clone());
            if !matches!(
                dispositions.get(file_name.as_str()),
                Some(PlannedArchiveSweep::Remove { .. })
            ) {
                break;
            }
        }
    }
    Ok(metadata_sources)
}

#[cfg(unix)]
fn planned_apply_identity_issue(
    directory: &Path,
    plan: &CompactionPlan,
    credentials: &ApplyCredentials,
) -> Result<Option<String>> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let directory_metadata = std::fs::symlink_metadata(directory)?;
    let possible_created_gids = possible_created_group_ids(
        directory_metadata.gid(),
        directory_metadata.permissions().mode(),
        credentials,
    );

    for name in planned_metadata_sources(
        directory,
        plan.manifest_upgrade,
        plan.segment_plan.as_ref(),
        !plan.checkpoints.names.is_empty(),
        plan.tasks.contains(&MaintenanceTask::Journal) && plan.journal.removed_lines != 0,
    )? {
        let path = directory.join(&name);
        let metadata = std::fs::symlink_metadata(&path)?;
        if let Some(issue) = metadata_source_apply_identity_issue(
            &path,
            metadata.uid(),
            metadata.gid(),
            metadata.permissions().mode(),
            &possible_created_gids,
            credentials,
        ) {
            return Ok(Some(issue));
        }
    }
    Ok(None)
}

#[cfg(all(test, unix))]
std::thread_local! {
    static POSSIBLE_CREATED_GROUP_IDS_INPUT: std::cell::Cell<Option<(u32, u32)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, unix))]
fn take_possible_created_group_ids_input() -> Option<(u32, u32)> {
    POSSIBLE_CREATED_GROUP_IDS_INPUT.with(std::cell::Cell::take)
}

#[cfg(unix)]
fn possible_created_group_ids(
    directory_gid: u32,
    directory_mode: u32,
    credentials: &ApplyCredentials,
) -> BTreeSet<u32> {
    #[cfg(test)]
    POSSIBLE_CREATED_GROUP_IDS_INPUT.with(|input| input.set(Some((directory_gid, directory_mode))));

    if directory_mode & SETGID_MODE != 0 {
        BTreeSet::from([directory_gid])
    } else {
        // POSIX permits either System V inheritance (the process effective
        // gid) or BSD inheritance (the parent-directory gid). Linux normally
        // selects with S_ISGID, but some filesystems also honor bsdgroups/grpid
        // mount policy. A read-only preview cannot distinguish those cases,
        // so model both outcomes rather than assuming the host default.
        BTreeSet::from([credentials.effective_gid, directory_gid])
    }
}

#[cfg(unix)]
fn metadata_source_apply_identity_issue(
    path: &Path,
    owner: u32,
    group: u32,
    mode: u32,
    possible_created_gids: &BTreeSet<u32>,
    credentials: &ApplyCredentials,
) -> Option<String> {
    if owner != credentials.effective_uid {
        return Some(format!(
            "cleanup cannot safely replace {} while preserving its metadata: it is owned by uid {owner}, but the effective uid is {}",
            path.display(),
            credentials.effective_uid
        ));
    }
    let might_need_to_change_group = possible_created_gids
        .iter()
        .any(|&created_gid| created_gid != group);
    let must_install_setgid = mode & SETGID_MODE != 0;
    if credentials.effective_uid != 0
        && !credentials.group_ids.contains(&group)
        && (might_need_to_change_group || must_install_setgid)
    {
        return Some(format!(
            "cleanup cannot safely replace {} while preserving its metadata: gid {group} is not the effective or a supplementary group of uid {}, while a new staging file may have gid {possible_created_gids:?} and the source mode is {:#06o}; group ownership and setgid-mode preservation cannot be guaranteed read-only",
            path.display(),
            credentials.effective_uid,
            mode & 0o7777
        ));
    }
    None
}

#[cfg(unix)]
fn validate_plan_apply_identity_for_credentials(
    directory: &Path,
    plan: &CompactionPlan,
    credentials: &ApplyCredentials,
) -> Result<()> {
    if let Some(details) = planned_apply_identity_issue(directory, plan, credentials)? {
        return Err(Error::InvalidFormat {
            details: format!(
                "{details}; conservatively refusing before planned repository mutations"
            ),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn preview_apply_identity_issue(
    directory: &Path,
    plan: &CompactionPlan,
    credentials: &ApplyCredentials,
) -> Result<Option<String>> {
    if let Some(issue) = journal_service_user_issue(directory, credentials.effective_uid)? {
        return Ok(Some(issue));
    }
    planned_apply_identity_issue(directory, plan, credentials)
}

fn append_apply_identity_preview_warning(directory: &Path, plan: &mut CompactionPlan) {
    #[cfg(unix)]
    append_apply_identity_preview_warning_for_credentials(
        directory,
        plan,
        current_apply_credentials(),
    );
    #[cfg(not(unix))]
    let _ = (directory, plan);
}

#[cfg(unix)]
fn append_apply_identity_preview_warning_for_credentials(
    directory: &Path,
    plan: &mut CompactionPlan,
    credentials: Result<ApplyCredentials>,
) {
    match credentials
        .and_then(|credentials| preview_apply_identity_issue(directory, plan, &credentials))
    {
        Ok(Some(issue)) => plan.warnings.push(format!(
            "apply ownership preflight warning: {issue}; authoritative apply will conservatively refuse before planned repository mutations"
        )),
        Ok(None) => {}
        Err(error) => plan.warnings.push(format!(
            "apply ownership could not be proved during this read-only preview ({error}); authoritative apply will retry the check under the repository lock"
        )),
    }
}

/// Resolves `directory` once to its canonical absolute target, then builds a
/// cleanup plan without acquiring a lock or changing any byte. Interactive
/// callers should pass [`CompactionPlan::directory`] to
/// [`PreparedCompaction::prepare`] so an alias cannot redirect lock acquisition
/// after the preview.
pub fn plan_compaction(directory: &Path, options: &CompactionOptions) -> Result<CompactionPlan> {
    plan_compaction_with_progress(directory, options, &mut DiscardedProgress)
}

/// Plans exactly like [`plan_compaction`] — still strictly read-only, still
/// without acquiring the lock — reporting each planning step to
/// `observer`. Planning a large store is the phase that takes minutes:
/// it verifies the whole head tree, replays the journal, and traces the
/// reachable segment closure before it can say anything at all.
pub fn plan_compaction_with_progress(
    directory: &Path,
    options: &CompactionOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionPlan> {
    validate_options(options)?;
    let directory = canonical_repository_directory(directory)?;
    validate_repository_shape(&directory)?;
    build_plan(&directory, options, SystemTime::now(), observer)
}

/// Convenience non-interactive API: prepares under lock and immediately
/// applies the authoritative plan. Interactive callers should use
/// [`plan_compaction`] and [`PreparedCompaction`] so they can display/reconfirm.
pub fn compact(directory: &Path, options: CompactionOptions) -> Result<CompactionOutcome> {
    compact_with_progress(directory, options, &mut DiscardedProgress)
}

/// Prepares and applies exactly like [`compact`], reporting both phases
/// to `observer`.
pub fn compact_with_progress(
    directory: &Path,
    options: CompactionOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionOutcome> {
    PreparedCompaction::prepare_with_progress(directory, options, observer)?
        .apply_with_progress(observer)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct JournalPlan {
    retained_record_ids: Vec<RecordIdentifier>,
    retained_raw_lines: Vec<Vec<u8>>,
    removals: Vec<JournalLineRemoval>,
    removed_lines: usize,
    parser_ignored: usize,
    missing_segments: usize,
    unreadable_revisions: usize,
    beyond_retention: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CheckpointPlan {
    names: Vec<String>,
    expired: usize,
    unreferenced: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StaleArchive {
    file_name: String,
    reason: StaleArchiveReason,
    bytes: u64,
    fingerprint: FileFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedFileRemoval {
    file_name: String,
    bytes: u64,
    fingerprint: FileFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryFingerprint {
    entries: Vec<FileFingerprint>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileFingerprint {
    name: OsString,
    kind: u8,
    length: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_time_seconds: i64,
    #[cfg(unix)]
    change_time_nanoseconds: i64,
}

fn validate_options(options: &CompactionOptions) -> Result<()> {
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

fn canonical_repository_directory(directory: &Path) -> Result<PathBuf> {
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

fn validate_repository_shape(directory: &Path) -> Result<()> {
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

fn validate_managed_file_types(directory: &Path) -> Result<()> {
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

fn is_managed_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(name, "manifest" | "journal.log" | "gc.log" | "repo.lock")
        || ArchiveFileName::parse(name).is_some()
        || temporary_kind(name).is_some()
        || recovery_backup_target(name).is_some()
}

fn directory_fingerprint(directory: &Path) -> Result<DirectoryFingerprint> {
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

fn file_fingerprint(name: OsString, metadata: &Metadata) -> FileFingerprint {
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
fn build_plan(
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
struct ManifestUpgradeOnFirstInstall<'directory> {
    directory: &'directory Path,
    done: bool,
}

impl<'directory> ManifestUpgradeOnFirstInstall<'directory> {
    fn new(directory: &'directory Path) -> Self {
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
fn attach_completed_repairs(
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

fn attach_planning_warnings(error: Error, warnings: &[String]) -> Error {
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
fn build_plan_collecting(
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
        full: crate::writer::store_writer::FULL_GARBAGE_COLLECTION,
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
                        full: crate::writer::store_writer::FULL_GARBAGE_COLLECTION,
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
                            full: kind == CompactionKind::Full,
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

fn add_estimate(total: &mut u64, amount: u64) -> Result<()> {
    *total = total
        .checked_add(amount)
        .ok_or_else(|| Error::InvalidFormat {
            details: "cleanup byte estimate overflow".to_owned(),
        })?;
    Ok(())
}

fn ensure_numbered_name_available(directory: &Path, stem: &str) -> Result<()> {
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

fn available_filesystem_bytes(directory: &Path) -> Option<u64> {
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
fn verify_exact_super_root(
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
fn verify_exact_super_root_counting_nodes(
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

fn verify_exact_super_root_with_verifier(
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

struct JournalAnalysis {
    plan: JournalPlan,
    retained_indexes: Vec<usize>,
    retained_record_ids: Vec<RecordIdentifier>,
}

const JOURNAL_LINE_PREVIEW_LIMIT: usize = 160;

fn journal_line_removal(
    index: usize,
    line: &RawJournalLine,
    record_identifier: Option<RecordIdentifier>,
    reason: JournalRemovalReason,
) -> JournalLineRemoval {
    let content = line.content_bytes();
    let preview_length = content.len().min(JOURNAL_LINE_PREVIEW_LIMIT);
    JournalLineRemoval {
        line_number: index + 1,
        record_identifier,
        reason,
        preview: content[..preview_length].to_vec(),
        preview_truncated: preview_length != content.len(),
    }
}

fn analyze_journal(
    repository: &Repository,
    raw: &RawJournal,
    current_head: RecordIdentifier,
    retention: Option<NonZeroUsize>,
    observer: &mut dyn ProgressObserver,
) -> Result<JournalAnalysis> {
    crate::progress::observe(
        observer,
        &Step::new("analyzing journal revisions", WorkUnit::JournalLines)
            .with_total(crate::progress::count(raw.lines().len())),
        |observer| analyze_journal_lines(repository, raw, current_head, retention, observer),
    )
}

/// The first line index within the newest `retention` resolvable revisions.
///
/// Resolvability here is only "parses and its head segment exists" — the
/// cheap half of the classification. That is deliberate: a line older than
/// the bound is removed whether or not its tree verifies, so bounding before
/// the walk skips the expensive per-revision verification for exactly the
/// lines being discarded. On a store with tens of thousands of journal lines
/// that is the difference between minutes and seconds.
fn journal_retention_boundary(
    repository: &Repository,
    raw: &RawJournal,
    retention: NonZeroUsize,
    validity: &mut HashMap<RecordIdentifier, bool>,
    verifier: &mut NodeTreeVerifier<'_>,
) -> Option<usize> {
    let mut remaining = retention.get();
    for (index, line) in raw.lines().iter().enumerate().rev() {
        let RawJournalLineClassification::Record(record) = line.classification() else {
            continue;
        };
        let identifier = record.record_identifier;
        if !repository.contains_segment(identifier.segment) {
            continue;
        }
        // Only revisions that actually resolve fill a slot. Counting on
        // segment existence alone let a line that fails its walk consume one
        // and then be removed as unreadable anyway, so a bound of N kept
        // fewer than N revisions and irreversibly retired a readable one to
        // make room for it. Verdicts are memoised into the same map the
        // classification pass uses, so nothing is walked twice and the lines
        // beyond the bound are still never walked at all.
        let readable = if let Some(readable) = validity.get(&identifier) {
            *readable
        } else {
            let readable = verify_exact_super_root_with_verifier(
                repository,
                identifier,
                verifier,
                &mut DiscardedProgress,
            )
            .is_ok();
            validity.insert(identifier, readable);
            readable
        };
        if !readable {
            continue;
        }
        remaining -= 1;
        if remaining == 0 {
            return Some(index);
        }
    }
    // Fewer resolvable revisions than the bound allows: nothing is beyond it.
    None
}

/// The journal pass itself; [`analyze_journal`] owns the step around it.
#[allow(
    clippy::too_many_lines,
    reason = "classification, exact retained-line evidence, and removal diagnostics form one auditable journal pass"
)]
fn analyze_journal_lines(
    repository: &Repository,
    raw: &RawJournal,
    current_head: RecordIdentifier,
    retention: Option<NonZeroUsize>,
    observer: &mut dyn ProgressObserver,
) -> Result<JournalAnalysis> {
    let selected = raw
        .lines()
        .iter()
        .rev()
        .find_map(|line| match line.classification() {
            RawJournalLineClassification::Record(record)
                if repository.contains_segment(record.record_identifier.segment) =>
            {
                Some(record.record_identifier)
            }
            _ => None,
        })
        .ok_or_else(|| Error::InvalidFormat {
            details: "no raw journal record references an existing segment".to_owned(),
        })?;
    if selected != current_head {
        return Err(Error::InvalidFormat {
            details: format!(
                "raw journal selected {selected}, but the repository reader selected {current_head}"
            ),
        });
    }

    let mut parser_ignored = 0usize;
    let mut missing_segments = 0usize;
    let mut unreadable_revisions = 0usize;
    let mut beyond_retention = 0usize;
    let mut removals = Vec::new();
    let mut retained_indexes = Vec::new();
    let mut retained_record_ids = Vec::new();
    let mut validity: HashMap<RecordIdentifier, bool> = HashMap::new();
    validity.insert(current_head, true);
    let mut verifier = NodeTreeVerifier::new(repository);
    let retention_boundary = retention.and_then(|bound| {
        journal_retention_boundary(repository, raw, bound, &mut validity, &mut verifier)
    });

    for (index, line) in raw.lines().iter().enumerate() {
        observer.step_advanced(crate::progress::count(index));
        match line.classification() {
            RawJournalLineClassification::ParserSkippedNoSpace => {
                parser_ignored += 1;
                removals.push(journal_line_removal(
                    index,
                    line,
                    None,
                    JournalRemovalReason::ParserSkippedNoSpace,
                ));
            }
            RawJournalLineClassification::InvalidRecordIdentifier { .. } => {
                parser_ignored += 1;
                removals.push(journal_line_removal(
                    index,
                    line,
                    None,
                    JournalRemovalReason::InvalidRecordIdentifier,
                ));
            }
            RawJournalLineClassification::Record(record) => {
                let identifier = record.record_identifier;
                if !repository.contains_segment(identifier.segment) {
                    missing_segments += 1;
                    removals.push(journal_line_removal(
                        index,
                        line,
                        Some(identifier),
                        JournalRemovalReason::MissingSegment,
                    ));
                    continue;
                }
                // Before the verification walk, not after: a line the bound
                // discards is discarded whether or not its tree reads, so
                // proving readability first would be work spent on a line
                // already destined for removal.
                if retention_boundary.is_some_and(|boundary| index < boundary) {
                    beyond_retention += 1;
                    removals.push(journal_line_removal(
                        index,
                        line,
                        Some(identifier),
                        JournalRemovalReason::BeyondRetention,
                    ));
                    continue;
                }
                let readable = if let Some(readable) = validity.get(&identifier) {
                    *readable
                } else {
                    // The historical revision's own node walk shares this
                    // step's journal-line counter rather than reporting
                    // nodes: a step counts one unit, and the line index is
                    // the one the reader can act on.
                    let readable = verify_exact_super_root_with_verifier(
                        repository,
                        identifier,
                        &mut verifier,
                        &mut DiscardedProgress,
                    )
                    .is_ok();
                    validity.insert(identifier, readable);
                    readable
                };
                if readable {
                    retained_indexes.push(index);
                    retained_record_ids.push(identifier);
                } else {
                    unreadable_revisions += 1;
                    removals.push(journal_line_removal(
                        index,
                        line,
                        Some(identifier),
                        JournalRemovalReason::UnreadableRevision,
                    ));
                }
            }
        }
    }
    observer.step_advanced(crate::progress::count(raw.lines().len()));
    if !retained_record_ids.contains(&current_head) {
        return Err(Error::InvalidFormat {
            details: "journal analysis would not retain the exact current head".to_owned(),
        });
    }
    let removed_lines = parser_ignored
        .checked_add(missing_segments)
        .and_then(|count| count.checked_add(unreadable_revisions))
        .and_then(|count| count.checked_add(beyond_retention))
        .ok_or_else(|| Error::InvalidFormat {
            details: "journal line accounting overflow".to_owned(),
        })?;
    Ok(JournalAnalysis {
        plan: JournalPlan {
            retained_record_ids: retained_record_ids.clone(),
            retained_raw_lines: retained_indexes
                .iter()
                .map(|&index| raw.lines()[index].raw_bytes().to_vec())
                .collect(),
            removals,
            removed_lines,
            parser_ignored,
            missing_segments,
            unreadable_revisions,
            beyond_retention,
        },
        retained_indexes,
        retained_record_ids,
    })
}

fn plan_checkpoints(
    repository: &Repository,
    options: &CompactionOptions,
    now: SystemTime,
    warnings: &mut Vec<String>,
) -> Result<CheckpointPlan> {
    if !options.contains(MaintenanceTask::ExpiredCheckpoints)
        && !options.contains(MaintenanceTask::UnreferencedCheckpoints)
    {
        return Ok(CheckpointPlan::default());
    }
    let now_milliseconds = now.duration_since(UNIX_EPOCH).map_or(0, |duration| {
        duration.as_millis().min(i64::MAX as u128) as i64
    });
    let checkpoints = repository.checkpoints()?;
    let referenced = if options.contains(MaintenanceTask::UnreferencedCheckpoints) {
        async_checkpoint_references(repository)?
    } else {
        HashSet::new()
    };
    let mut expired_names = BTreeSet::new();
    let mut unreferenced_names = BTreeSet::new();
    for (name, checkpoint) in checkpoints {
        if options.contains(MaintenanceTask::ExpiredCheckpoints) {
            match checkpoint.property("timestamp")? {
                Some(property) => match property.values {
                    PropertyValues::Single(PropertyValue::Long(timestamp)) => {
                        if now_milliseconds > timestamp {
                            expired_names.insert(name.clone());
                        }
                    }
                    _ => warnings.push(format!(
                        "checkpoint {name} has a malformed timestamp and was not selected by expiry"
                    )),
                },
                None => warnings.push(format!(
                    "checkpoint {name} has no timestamp and was not selected by expiry"
                )),
            }
        }
        if options.contains(MaintenanceTask::UnreferencedCheckpoints)
            && !referenced.contains(&name)
            && !expired_names.contains(&name)
        {
            unreferenced_names.insert(name);
        }
    }
    let expired = expired_names.len();
    let unreferenced = unreferenced_names.len();
    expired_names.extend(unreferenced_names);
    Ok(CheckpointPlan {
        names: expired_names.into_iter().collect(),
        expired,
        unreferenced,
    })
}

fn async_checkpoint_references(repository: &Repository) -> Result<HashSet<String>> {
    let mut referenced = HashSet::new();
    if let Some(async_state) = repository.content_root()?.child_node(":async")? {
        for property in async_state.properties()? {
            match property.values {
                PropertyValues::Single(PropertyValue::String(value)) => {
                    referenced.insert(value);
                }
                PropertyValues::Multiple(values) => {
                    referenced.extend(values.into_iter().filter_map(|value| match value {
                        PropertyValue::String(value) => Some(value),
                        _ => None,
                    }));
                }
                PropertyValues::Single(_) => {}
            }
        }
    }
    Ok(referenced)
}

fn plan_stale_archives(
    directory: &Path,
    repository: &Repository,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<StaleArchive>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if ArchiveFileName::parse(&name).is_some() {
            names.push(name);
        }
    }
    let groups = group_file_generations_newest_first(&names)?;
    let group_count = groups.len();
    observer.step_total_resolved(crate::progress::count(group_count));
    let mut stale = Vec::new();
    for (examined, group) in groups.into_iter().enumerate() {
        observer.step_advanced(crate::progress::count(examined));
        let mut winner = None;
        let mut indexed_but_incomplete = None;
        for candidate in &group {
            let path = directory.join(&candidate.file_name);
            if std::fs::symlink_metadata(&path)?.len() == 0 {
                continue;
            }
            if let Ok(reader) = TarArchiveReader::open(&path)
                && !reader.is_recovered()
            {
                // This is the exact generation normal repository discovery
                // will select. Never skip past it to promote an older letter:
                // doing so could roll the active archive back. Its graph and
                // BRF are recovery-critical even when content reads happen to
                // succeed, so alternate letters are removable only when both
                // trailers validate as well as the index.
                match certify_active_archive(repository, &reader) {
                    Ok(()) => winner = Some(candidate.file_name.as_str()),
                    Err(error) => {
                        indexed_but_incomplete = Some(format!(
                            "active archive {} has incomplete recovery metadata ({error})",
                            candidate.file_name
                        ));
                    }
                }
                break;
            }
        }
        if let Some(winner) = winner {
            for candidate in &group {
                if candidate.file_name != winner {
                    let metadata = std::fs::symlink_metadata(directory.join(&candidate.file_name))?;
                    let bytes = metadata.len();
                    stale.push(StaleArchive {
                        file_name: candidate.file_name.clone(),
                        reason: if bytes == 0 {
                            StaleArchiveReason::EmptyIncomplete
                        } else {
                            StaleArchiveReason::Superseded
                        },
                        bytes,
                        fingerprint: file_fingerprint(
                            OsString::from(candidate.file_name.as_str()),
                            &metadata,
                        ),
                    });
                }
            }
        } else {
            let mut nonempty = Vec::new();
            for candidate in &group {
                let metadata = std::fs::symlink_metadata(directory.join(&candidate.file_name))?;
                let bytes = metadata.len();
                if bytes == 0 {
                    stale.push(StaleArchive {
                        file_name: candidate.file_name.clone(),
                        reason: StaleArchiveReason::EmptyIncomplete,
                        bytes,
                        fingerprint: file_fingerprint(
                            OsString::from(candidate.file_name.as_str()),
                            &metadata,
                        ),
                    });
                } else {
                    nonempty.push(candidate.file_name.clone());
                }
            }
            if !nonempty.is_empty() {
                if let Some(reason) = indexed_but_incomplete {
                    warnings.push(format!(
                        "{reason}; preserving every non-empty letter of archive number {} as recovery evidence",
                        group[0].archive_number,
                    ));
                } else {
                    warnings.push(format!(
                        "archive number {} has no valid indexed generation; preserving recoverable files {}",
                        group[0].archive_number,
                        nonempty.join(", ")
                    ));
                }
            }
        }
    }
    observer.step_advanced(crate::progress::count(group_count));
    stale.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    Ok(stale)
}

fn generation_from_header(
    repository: &Repository,
    identifier: SegmentIdentifier,
) -> Result<GarbageCollectionGeneration> {
    let view = repository.segment(identifier)?;
    if !identifier.is_data_segment() {
        return Err(Error::InvalidFormat {
            details: format!("journal head segment {identifier} is not a data segment"),
        });
    }
    Ok(GarbageCollectionGeneration {
        generation: view.structure.generation,
        full_generation: view.structure.full_generation,
        is_compacted: view.structure.is_compacted,
    })
}

fn reject_duplicate_active_segments(repository: &Repository) -> Result<()> {
    reject_duplicate_active_segments_across(repository, false)
}

/// Refuses a segment served by two archives of *different* numbers.
///
/// Split out because the whole check cannot run while repairs are pending: an
/// archive number with no valid index is served through every non-empty
/// letter it has, and those letters share segments by construction, so the
/// full check would refuse a store the repair is about to collapse back to
/// one letter. Two different *numbers* sharing a segment is not by
/// construction — it is a real, unrepairable defect, and suppressing it would
/// mean the preview cannot tell the operator the store is unfit and the guard
/// fires only after an irreversible rewrite.
fn reject_cross_number_duplicate_active_segments(repository: &Repository) -> Result<()> {
    reject_duplicate_active_segments_across(repository, true)
}

fn reject_duplicate_active_segments_across(
    repository: &Repository,
    ignore_within_one_number: bool,
) -> Result<()> {
    // An unparsable archive name counts as its own number rather than being
    // excused: failing to classify a file is not a reason to stop checking it.
    let number_of = |file_name: &str| -> Option<u32> {
        ArchiveFileName::parse(file_name).map(|parsed| parsed.archive_number)
    };
    let mut locations: HashMap<SegmentIdentifier, (&str, Option<u32>)> = HashMap::new();
    for archive in repository.archives() {
        let here = (archive.file_name(), number_of(archive.file_name()));
        for identifier in archive.segment_identifiers() {
            if let Some(previous) = locations.insert(identifier, here) {
                let same_number = previous.1.is_some() && previous.1 == here.1;
                if ignore_within_one_number && same_number {
                    continue;
                }
                return Err(Error::InvalidFormat {
                    details: format!(
                        "segment {identifier} occurs in active archives {} and {}; refusing cleanup",
                        previous.0, here.0
                    ),
                });
            }
        }
    }
    Ok(())
}

/// The refusal an index-less active archive earns.
///
/// Naming only the first offender cannot separate a writer killed before it
/// closed its newest archive — benign, and repaired by any froe write
/// command — from a store damaged throughout, and that is exactly the call
/// the operator has to make before touching the repository. So every
/// archive is counted, and whether the newest one is affected is stated
/// rather than left to be inferred from a file name.
///
/// Counting is by archive *number*, not by open reader. When no letter of a
/// number carries a valid index the reader serves every non-empty letter of
/// it, so counting readers would report one damaged number as two or three
/// damaged archives — and the warning carried alongside this refusal already
/// speaks in archive numbers.
fn indexless_archive_refusal(
    total_numbers: usize,
    indexless_numbers: usize,
    indexless: &[(&str, &str)],
    newest_is_indexless: bool,
    any_scan_is_empty: bool,
) -> String {
    const NAMES_SHOWN: usize = 5;
    let mut names = indexless
        .iter()
        .take(NAMES_SHOWN)
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    if indexless.len() > NAMES_SHOWN {
        let _ = write!(names, ", and {} more", indexless.len() - NAMES_SHOWN);
    }
    // Reasons are deduplicated rather than listed per archive: a store
    // damaged one way is damaged that way throughout far more often than
    // not, and the shape of the failure is what decides the response. Where
    // they genuinely differ, saying so is itself the finding.
    let distinct_reasons: BTreeSet<&str> = indexless.iter().map(|(_, reason)| *reason).collect();
    let reasons = distinct_reasons
        .iter()
        .copied()
        .collect::<Vec<_>>()
        .join("; ");
    let subject = if indexless_numbers == 1 {
        format!("1 of {total_numbers} active archive numbers has no index metadata")
    } else {
        format!(
            "{indexless_numbers} of {total_numbers} active archive numbers have no index metadata"
        )
    };
    // The remedy is stated conditionally, because the two shapes deserve
    // different advice. A writer killed before it closed its newest archive
    // left a complete archive missing only its trailer, and rebuilding the
    // index from a scan of it loses nothing. An index-less archive in the
    // middle of the store was closed once and stopped validating since, so
    // a scan may legitimately fail to read segments that were there —
    // repairing before looking would make that permanent in everything but
    // the `.bak`.
    let ordinality = if newest_is_indexless {
        "the newest active archive is among them, which is what a writer killed before it \
         closed its archive leaves behind"
    } else {
        "the newest active archive is not among them, so this is not simply a writer killed \
         before it closed its archive"
    };
    // Whether a repair would even succeed is knowable here, so it is not
    // guessed: a recovery-scanned archive reports the segments the scan
    // read, and one that read none is residue the write open refuses rather
    // than rebuilds. Advising a write command for it would send the operator
    // to a refusal, which is the circularity this branch exists to avoid.
    let remedy = if any_scan_is_empty {
        "at least one of them holds no segment the recovery scan can read, so nothing can rebuild \
         it — move that file aside to proceed, and keep it, it is the only copy of whatever it \
         holds"
    } else if newest_is_indexless {
        "rerun with `--repair-archive-indexes` to rebuild the missing index from the \
         archive's own entries, retaining the original bytes under a `.bak` name"
    } else {
        // A closed archive that stopped validating is not a missing trailer;
        // a scan of it may read fewer segments than it holds, and repairing
        // makes that the served truth in everything but the `.bak`.
        "inspect before repairing: `--repair-archive-indexes` would rebuild the missing \
         indexes from a recovery scan, which retains the original bytes under a `.bak` name but \
         cannot recover a segment the scan cannot read"
    };
    format!(
        "{subject} ({names}); the index was rejected because {reasons}; {ordinality}. Refusing \
         this cleanup run; no archive, journal, or checkpoint has been changed. Run \
         `froe archives` on this repository to see every archive's index state; {remedy}."
    )
}

/// The repairs an index-less store needs, one per archive *number*.
///
/// Named from the same census the refusal uses, so an operator who opts in
/// sees exactly the archives they would otherwise have been refused over.
///
/// Grouped by number rather than by open reader, because a number with no
/// valid index is served through every one of its non-empty letters while
/// the repair rebuilds it once — and installs the result under the *lowest*
/// letter, which is the name reported here so the plan names the file the
/// repair will actually write. Reporting per reader would promise two or
/// three repairs for one, under names that never appear again.
/// Index-less archive numbers whose letters together scan to nothing, named
/// by the file a rebuild would have installed under.
///
/// Read off the open readers rather than rescanning: `Repository::open`
/// already performed the recovery scan, and `segment_count()` on a recovered
/// archive is exactly what it read. Summed per number, because a rebuild
/// merges every letter — one empty letter beside a readable one is still
/// repairable.
fn unrepairable_archive_names(repository: &Repository) -> Vec<String> {
    let mut by_number: BTreeMap<u32, (String, usize)> = BTreeMap::new();
    for archive in repository.archives() {
        if archive.index().is_some() {
            continue;
        }
        let Some(parsed) = ArchiveFileName::parse(archive.file_name()) else {
            continue;
        };
        let name = archive.file_name().to_owned();
        let entry = by_number
            .entry(parsed.archive_number)
            .or_insert_with(|| (name.clone(), 0));
        if name < entry.0 {
            entry.0 = name;
        }
        entry.1 += archive.segment_count();
    }
    by_number
        .into_values()
        .filter(|(_, segments)| *segments == 0)
        .map(|(name, _)| name)
        .collect()
}

fn planned_archive_repairs(repository: &Repository) -> Vec<CompactionAction> {
    struct Group {
        target: String,
        retired: Vec<String>,
        reason: String,
        bytes: u64,
        scanned_segments: usize,
    }
    let mut by_number: BTreeMap<u32, Group> = BTreeMap::new();
    for archive in repository.archives() {
        let Some(reason) = archive.recovery_reason() else {
            continue;
        };
        let Some(parsed) = ArchiveFileName::parse(archive.file_name()) else {
            continue;
        };
        let name = archive.file_name().to_owned();
        let group = by_number
            .entry(parsed.archive_number)
            .or_insert_with(|| Group {
                target: name.clone(),
                retired: Vec::new(),
                reason: reason.to_owned(),
                bytes: 0,
                scanned_segments: 0,
            });
        // `Repository::archives()` already skips zero-length letters, so the
        // lowest name here is the lowest non-empty letter — the same target
        // `recover_archive_number` installs under. Every other letter is
        // retired to a `.bak`, so the plan names them too: confirmation is
        // scoped to the files it printed.
        if name < group.target {
            group
                .retired
                .push(std::mem::replace(&mut group.target, name));
            reason.clone_into(&mut group.reason);
        } else if name != group.target {
            group.retired.push(name);
        }
        group.bytes = group.bytes.saturating_add(archive.file_size());
        group.scanned_segments += archive.segment_count();
    }
    by_number
        .into_values()
        // A number whose letters together scan to nothing cannot be rebuilt —
        // `recover_archive_number` refuses it. Planning it anyway would have
        // the operator confirm a run cleanup can already prove will fail,
        // after paying a durable rewrite of every repairable archive for it,
        // with no rerun ever converging. Dropped here, the store falls through
        // to the refusal that states the impossibility up front. Summed per
        // number, never per letter: one empty letter beside a readable one is
        // still repairable, because the scan merges every letter.
        .filter(|group| group.scanned_segments > 0)
        .map(|mut group| {
            group.retired.sort();
            CompactionAction::RepairArchiveIndex {
                file_name: group.target,
                retired_file_names: group.retired,
                reason: group.reason,
                bytes: group.bytes,
            }
        })
        .collect()
}

/// Pre-existing bulk segments the compacted generation will reference where
/// they lie rather than re-encoding.
///
/// This is a second implementation of the sharing rule in
/// `RecordWriter::copy_binary_value`, and it must agree with it exactly or the
/// predicted sweep names the wrong archives. The rule, in the order that
/// function applies it: only `PropertyType::Binary`; only
/// `BinaryValue::Inline` (an external binary carries a blob identifier and no
/// blocks); only at or above `MEDIUM_VALUE_LIMIT`, because a shorter value is
/// materialized and re-encoded whole; and then only those blocks whose own
/// segment `is_bulk_segment()`, because a block in a *data* segment is copied.
///
/// Checkpoints this run retires are not walked: their content is not copied,
/// so it seeds nothing.
/// The generation a compaction of this kind writes into.
///
/// Named once, because the plan states it and the apply writes it: two
/// spellings of the same arithmetic is how a plan comes to describe a
/// generation the run does not produce.
fn compaction_target_generation(
    base: GarbageCollectionGeneration,
    kind: CompactionKind,
) -> GarbageCollectionGeneration {
    match kind {
        CompactionKind::Full => GarbageCollectionGeneration {
            generation: base.generation.wrapping_add(1),
            full_generation: base.full_generation.wrapping_add(1),
            is_compacted: true,
        },
        CompactionKind::Tail => GarbageCollectionGeneration {
            generation: base.generation.wrapping_add(1),
            full_generation: base.full_generation,
            is_compacted: true,
        },
    }
}

/// Records one non-actionable disposition: counted, and named in a warning.
fn retained_reclaimable_from(
    planned: &PlannedArchiveSweep,
    retained: &mut RetainedReclaimable,
    warnings: &mut Vec<String>,
) -> Result<()> {
    match planned {
        PlannedArchiveSweep::DeferredBySavings {
            file_name,
            segment_count,
            eligible_entry_bytes,
        } => {
            retained.below_savings_gate += segment_count;
            add_estimate(&mut retained.bytes, *eligible_entry_bytes)?;
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
            retained.at_last_generation += segment_count;
            add_estimate(&mut retained.bytes, *eligible_entry_bytes)?;
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
            retained.blocked_by_occupied_generation += segment_count;
            add_estimate(&mut retained.bytes, *eligible_entry_bytes)?;
            warnings.push(format!(
                "{file_name}: {segment_count} reclaimable segments ({}) retained because {occupied_name} already exists",
                crate::units::format_byte_size(*eligible_entry_bytes)
            ));
        }
        PlannedArchiveSweep::Remove { .. } | PlannedArchiveSweep::Rewrite { .. } => {}
    }
    Ok(())
}

fn predict_shared_bulk_segments(
    repository: &Repository,
    head: RecordIdentifier,
    omitted_checkpoints: &BTreeSet<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<HashSet<SegmentIdentifier>> {
    crate::progress::observe(
        observer,
        &Step::new("predicting the shared binary content", WorkUnit::Nodes),
        |observer| {
            let mut shared = HashSet::new();
            let mut visited: HashSet<RecordIdentifier> = HashSet::new();
            let mut pending = vec![head];
            let mut traced = crate::progress::StrideCounter::new(SEGMENT_TRACE_REPORT_STRIDE);
            while let Some(record) = pending.pop() {
                if !visited.insert(record) {
                    continue;
                }
                traced.advance(observer);
                let node = repository.node(record);
                for property in node.properties()? {
                    if property.property_type != crate::content::property::PropertyType::Binary {
                        continue;
                    }
                    let values = match &property.values {
                        crate::content::node::PropertyValues::Single(value) => {
                            std::slice::from_ref(value)
                        }
                        crate::content::node::PropertyValues::Multiple(values) => values.as_slice(),
                    };
                    for value in values {
                        let crate::content::property::PropertyValue::Binary(
                            crate::content::value::BinaryValue::Inline {
                                length,
                                record_identifier,
                            },
                        ) = value
                        else {
                            continue;
                        };
                        if *length < crate::writer::record_writer::MEDIUM_VALUE_LIMIT as u64 {
                            continue;
                        }
                        collect_shared_bulk_blocks(
                            repository,
                            *record_identifier,
                            *length,
                            &mut shared,
                        )?;
                    }
                }
                let is_checkpoint_container = record == head;
                for (name, child) in node.child_node_entries()? {
                    // The one place a name matters, and the same one the copy
                    // reads: a checkpoint this run retires is not descended
                    // into, exactly as `deep_copy_super_root_with_progress`
                    // declines to enter it.
                    if !is_checkpoint_container
                        && !omitted_checkpoints.is_empty()
                        && omitted_checkpoints.contains(&name)
                    {
                        continue;
                    }
                    pending.push(child.record_identifier());
                }
            }
            traced.finish(observer);
            Ok(shared)
        },
    )
}

/// The bulk segments holding one long inline binary's blocks.
fn collect_shared_bulk_blocks(
    repository: &Repository,
    value: RecordIdentifier,
    length: u64,
    shared: &mut HashSet<SegmentIdentifier>,
) -> Result<()> {
    let block_count = length.div_ceil(crate::content::value::BLOCK_SIZE);
    let view = repository.segment(value.segment)?;
    let list_identifier = view.read_record_identifier(value.record_number, 8, 0)?;
    for block in
        crate::content::list::uncounted_list_entries(repository, list_identifier, block_count)?
    {
        if block.segment.is_bulk_segment() {
            shared.insert(block.segment);
        }
    }
    Ok(())
}

/// Active data segments stamped ahead of the head's own generation.
///
/// Nothing legitimate sits ahead of the head: the writer stamps what it writes
/// at the head's generation, and a compaction publishes its head before
/// anything else observes the generation it wrote. So every one of these is
/// output from an earlier run that died between finishing its copy and
/// committing the head.
///
/// They matter because no ordinary rule removes them. Compaction's own mark
/// disables the dangling-future rule, and the generation predicate spares a
/// segment *newer* than its reference on both clauses — so residue would
/// survive every future run while continuing to hold bulk segments alive
/// through its references. Left alone it accumulates once per killed run.
fn segments_ahead_of_the_head(
    active_index_generations: &HashMap<SegmentIdentifier, GarbageCollectionGeneration>,
    reference: GarbageCollectionGeneration,
) -> usize {
    active_index_generations
        .iter()
        .filter(|(identifier, generation)| {
            identifier.is_data_segment() && generation.generation > reference.generation
        })
        .count()
}

fn active_index_generations(
    repository: &Repository,
) -> Result<HashMap<SegmentIdentifier, GarbageCollectionGeneration>> {
    // Census before refusal. `Repository::archives()` is ordered newest
    // archive number first, so this preserves that order and the newest
    // served archive is affected exactly when it is the first element.
    let indexless: Vec<(&str, &str)> = repository
        .archives()
        .iter()
        .filter_map(|archive| {
            archive
                .recovery_reason()
                .map(|reason| (archive.file_name(), reason))
        })
        .collect();
    if !indexless.is_empty() {
        // By number, not by reader: an unindexed number is served through
        // every one of its non-empty letters, and reporting those letters as
        // separate damaged archives would overstate the damage.
        let number_of =
            |file_name: &str| ArchiveFileName::parse(file_name).map(|n| n.archive_number);
        let indexless_numbers: BTreeSet<u32> = indexless
            .iter()
            .filter_map(|(name, _)| number_of(name))
            .collect();
        let total_numbers: BTreeSet<u32> = repository
            .archives()
            .iter()
            .filter_map(|archive| number_of(archive.file_name()))
            .collect();
        let newest_is_indexless = repository
            .archives()
            .first()
            .is_some_and(|newest| newest.index().is_none());
        // `segment_count()` on a recovery-scanned archive is what the scan
        // actually read, which is exactly what a rebuild would work from —
        // summed per archive *number*, because the rebuild merges every
        // letter of a number. A letter that scans empty beside one that does
        // not is still repairable, and reporting it as unrepairable withholds
        // the task that would fix the store and sends the operator to
        // hand-edit a damaged production directory instead.
        let mut scanned_segments: BTreeMap<u32, usize> = BTreeMap::new();
        for archive in repository.archives() {
            if archive.index().is_some() {
                continue;
            }
            if let Some(number) = number_of(archive.file_name()) {
                *scanned_segments.entry(number).or_default() += archive.segment_count();
            }
        }
        let any_scan_is_empty = scanned_segments.values().any(|count| *count == 0);
        return Err(Error::InvalidFormat {
            details: indexless_archive_refusal(
                total_numbers.len(),
                indexless_numbers.len(),
                &indexless,
                newest_is_indexless,
                any_scan_is_empty,
            ),
        });
    }
    let mut generations = HashMap::new();
    for archive in repository.archives() {
        for identifier in archive.segment_identifiers() {
            // Unreachable once the census above passes: an indexed archive
            // enumerates its segments out of the very index this looks them
            // up in. Kept so a change to either side fails closed.
            let entry = archive
                .index_entry(identifier)
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "active archive {} has no index metadata for its own segment {identifier}; refusing generation cleanup",
                        archive.file_name()
                    ),
                })?;
            generations.insert(
                identifier,
                GarbageCollectionGeneration {
                    generation: entry.generation,
                    full_generation: entry.full_generation,
                    is_compacted: entry.is_compacted,
                },
            );
        }
    }
    Ok(generations)
}

fn extend_segment_closure(
    provider: &dyn SegmentProvider,
    roots: impl IntoIterator<Item = SegmentIdentifier>,
    seen: &mut HashSet<SegmentIdentifier>,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let mut pending: VecDeque<_> = roots.into_iter().collect();
    let mut traced = crate::progress::StrideCounter::new(SEGMENT_TRACE_REPORT_STRIDE);
    while let Some(identifier) = pending.pop_front() {
        if !seen.insert(identifier) {
            continue;
        }
        let segment = provider.segment(identifier)?;
        traced.advance(observer);
        pending.extend(segment.structure.referenced_segments.iter().copied());
    }
    traced.finish(observer);
    Ok(())
}

/// Proves the current head reaches nothing the run's own reclaim rule would
/// discard.
///
/// The predicate alone does not make head-reachable data safe — it only makes
/// *older-generation* data reclaimable, and whether the head reaches any is a
/// property of the store, not of the arithmetic. So this re-evaluates the
/// identical rule the mark phase will consume, over the head's transitive
/// segment closure, and refuses before any mutation. Bulk segments are skipped
/// deliberately: they carry the null generation triple by format mandate and
/// are reclaimed by reachability, which the mark phase's reference set decides.
fn validate_reclaim_reference_invariant(
    repository: &Repository,
    current_closure: &HashSet<SegmentIdentifier>,
    active_index_generations: &HashMap<SegmentIdentifier, GarbageCollectionGeneration>,
    rule: ReclaimRule,
) -> Result<()> {
    for &identifier in current_closure {
        if !identifier.is_data_segment() {
            continue;
        }
        let header = generation_from_header(repository, identifier)?;
        let indexed = active_index_generations.get(&identifier).ok_or_else(|| {
            Error::InvalidFormat {
                details: format!(
                    "current head reaches data segment {identifier}, but no active archive index describes it"
                ),
            }
        })?;
        if *indexed != header {
            return Err(Error::InvalidFormat {
                details: format!(
                    "segment {identifier} has index generation {indexed:?}, but its header says {header:?}"
                ),
            });
        }
        if is_reclaimable(rule.reference, header, rule.full, rule.retained_generations) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "current head reaches data segment {identifier} in reclaimable generation {header:?}; refusing to trust generation cleanup"
                ),
            });
        }
    }
    Ok(())
}

struct ExcludingProvider<'repository> {
    repository: &'repository Repository,
    unavailable: &'repository HashSet<SegmentIdentifier>,
}

impl SegmentProvider for ExcludingProvider<'_> {
    fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        if self.unavailable.contains(&identifier) {
            return Err(Error::SegmentNotFound {
                segment_identifier: identifier,
            });
        }
        self.repository.segment(identifier)
    }

    fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, identifier).map(Arc::from)
    }

    fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, identifier).map(Arc::new)
    }
}

fn validate_prospective_segment_plan(
    directory: &Path,
    repository: &Repository,
    plan: &StandaloneSegmentCompactionPlan,
    retained_roots: &[RecordIdentifier],
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let unavailable = planned_unavailable_segments(directory, plan)?;
    if unavailable.is_empty() {
        return Ok(());
    }
    let provider = ExcludingProvider {
        repository,
        unavailable: &unavailable,
    };
    let mut verifier = NodeTreeVerifier::new(&provider);
    for &root in retained_roots {
        verifier
            .verify_with_progress(root, observer)
            .map_err(|error| Error::InvalidFormat {
                details: format!(
                    "segment cleanup would make retained journal root {root} unreadable: {error}"
                ),
            })?;
    }

    // Live survivors only. A segment the mark phase already proved
    // reclaimable is garbage that merely outlived the sweep, because the
    // archive holding it did not repay a rewrite; nothing reachable reads it,
    // and a dangling reference out of garbage is the ordinary state Oak
    // leaves behind every partial sweep. Refusing on those made the whole
    // plan fail on exactly the stores this reclamation exists for — ones
    // with dead segments scattered through archives the 25% gate defers.
    // What must not dangle is anything still reachable, and that is proved
    // above against every retained root.
    let reclaimable = plan.reclaimable_segments();
    for identifier in repository.distinct_segment_identifiers() {
        if !identifier.is_data_segment()
            || unavailable.contains(&identifier)
            || reclaimable.contains(&identifier)
        {
            continue;
        }
        let segment = repository.segment(identifier)?;
        if let Some(target) = segment
            .structure
            .referenced_segments
            .iter()
            .find(|target| unavailable.contains(target))
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "surviving data segment {identifier} references segment {target}, which the cleanup plan would remove"
                ),
            });
        }
    }
    Ok(())
}

fn prospective_retained_roots<'roots>(
    directory: &Path,
    repository: &Repository,
    plan: &StandaloneSegmentCompactionPlan,
    retained_roots: &'roots [RecordIdentifier],
) -> Cow<'roots, [RecordIdentifier]> {
    #[cfg(test)]
    if crate::writer::maintenance_fault_injection::is_substitution_armed(
        "cleanup.before-prospective-retained-root-verification",
    ) {
        let unavailable = planned_unavailable_segments(directory, plan)
            .expect("armed prospective-root fixture must have a valid physical plan");
        for identifier in unavailable {
            let segment = repository
                .segment(identifier)
                .expect("armed prospective-root fixture segment must be readable");
            if let Some(entry) = segment
                .structure
                .record_table()
                .iter()
                .find(|entry| entry.record_type() == Some(RecordType::Node))
            {
                let mut injected = retained_roots.to_vec();
                injected.push(RecordIdentifier::new(identifier, entry.record_number));
                return Cow::Owned(injected);
            }
        }
        panic!("prospective retained-root fault fixture has no removable node record");
    }
    let _ = (directory, repository, plan);
    Cow::Borrowed(retained_roots)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TemporaryKind {
    Journal,
    RecoveringArchive,
    Manifest,
}

fn temporary_kind(name: &str) -> Option<TemporaryKind> {
    if matches!(name, "journal.log.compacting" | "journal.log.recovered") {
        return Some(TemporaryKind::Journal);
    }
    if let Some(counter) = name.strip_prefix("journal.log.cleaning.")
        && counter.len() == 3
        && counter.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some(TemporaryKind::Journal);
    }
    if let Some(counter) = name.strip_prefix("manifest.cleaning.")
        && counter.len() == 3
        && counter.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some(TemporaryKind::Manifest);
    }
    if let Some((archive, counter)) = name.rsplit_once(".cleaning.")
        && counter.len() == 3
        && counter.bytes().all(|byte| byte.is_ascii_digit())
        && ArchiveFileName::parse(archive).is_some()
    {
        return Some(TemporaryKind::RecoveringArchive);
    }
    name.strip_suffix(".recovering")
        .and_then(ArchiveFileName::parse)
        .map(|_| TemporaryKind::RecoveringArchive)
}

fn plan_stale_temporaries(
    directory: &Path,
    repository: &Repository,
    canonical_journal: &RawJournal,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<PlannedFileRemoval>> {
    let canonical_records: HashSet<Vec<u8>> = canonical_journal
        .lines()
        .iter()
        .filter_map(|line| match line.classification() {
            RawJournalLineClassification::Record(_) => Some(line.content_bytes().to_vec()),
            _ => None,
        })
        .collect();
    let manifest_path = directory.join("manifest");
    let canonical_manifest = std::fs::read(&manifest_path)?;
    let upgraded_manifest = (crate::store::read_manifest_store_version(&manifest_path)? < 2)
        .then(|| manifest_upgrade_bytes(&canonical_manifest));
    let mut planned = Vec::new();
    // Every directory entry is one step, so nothing is worth batching.
    let mut examined = crate::progress::StrideCounter::new(1);
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(kind) = temporary_kind(&name) else {
            continue;
        };
        let metadata = std::fs::symlink_metadata(entry.path())?;
        let redundant = match kind {
            TemporaryKind::Journal => {
                if metadata.len() == 0 {
                    true
                } else {
                    let staging = scan_raw_journal_file(&entry.path())?;
                    !staging.lines().is_empty()
                        && staging.lines().iter().all(|line| {
                            matches!(
                                line.classification(),
                                RawJournalLineClassification::Record(_)
                            ) && canonical_records.contains(line.content_bytes())
                        })
                }
            }
            TemporaryKind::RecoveringArchive => {
                if metadata.len() == 0 {
                    true
                } else {
                    let mut identical_to_active = false;
                    for archive in repository.archives() {
                        if files_are_identical(&entry.path(), &directory.join(archive.file_name()))?
                        {
                            identical_to_active = true;
                            break;
                        }
                    }
                    identical_to_active
                }
            }
            TemporaryKind::Manifest => {
                if metadata.len() == 0 {
                    true
                } else {
                    let staging = match std::fs::read(entry.path()) {
                        Ok(staging) => staging,
                        Err(error) => {
                            warnings.push(format!(
                                "temporary {name} could not be read ({error}) and was retained"
                            ));
                            continue;
                        }
                    };
                    staging == canonical_manifest
                        || upgraded_manifest
                            .as_deref()
                            .is_some_and(|upgrade| staging == upgrade)
                }
            }
        };
        if redundant {
            planned.push(PlannedFileRemoval {
                fingerprint: file_fingerprint(OsString::from(name.as_str()), &metadata),
                file_name: name,
                bytes: metadata.len(),
            });
        } else {
            warnings.push(format!(
                "temporary {name} is not provably redundant and was retained"
            ));
        }
        // Counted once the entry has been classified, not on the way in.
        examined.advance(observer);
    }
    examined.finish(observer);
    planned.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    Ok(planned)
}

fn files_are_identical(left: &Path, right: &Path) -> Result<bool> {
    if std::fs::symlink_metadata(left)?.len() != std::fs::symlink_metadata(right)?.len() {
        return Ok(false);
    }
    let mut left = std::io::BufReader::new(File::open(left)?);
    let mut right = std::io::BufReader::new(File::open(right)?);
    let mut left_buffer = vec![0u8; 64 * 1024];
    let mut right_buffer = vec![0u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn recovery_backup_target(name: &str) -> Option<String> {
    if let Some(counter) = name.strip_prefix("journal.log.bak.")
        && counter.len() == 3
        && counter.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some("journal.log".to_owned());
    }
    if let Some(base) = name.strip_suffix(".ro.bak") {
        if ArchiveFileName::parse(base).is_some() {
            return Some(base.to_owned());
        }
        if let Some((archive, counter)) = base.rsplit_once('.')
            && is_oak_archive_backup_counter(counter)
            && ArchiveFileName::parse(archive).is_some()
        {
            return Some(archive.to_owned());
        }
    }
    let base = name.strip_suffix(".bak")?;
    if ArchiveFileName::parse(base).is_some() {
        return Some(base.to_owned());
    }
    let (archive, counter) = base.rsplit_once('.')?;
    if is_oak_archive_backup_counter(counter) && ArchiveFileName::parse(archive).is_some() {
        return Some(archive.to_owned());
    }
    None
}

fn is_oak_archive_backup_counter(counter: &str) -> bool {
    counter
        .parse::<i32>()
        .is_ok_and(|parsed| parsed >= 2 && parsed.to_string().as_bytes() == counter.as_bytes())
}

fn plan_recovery_backups(
    directory: &Path,
    now: SystemTime,
    policy: RecoveryBackupPolicy,
) -> Result<Vec<PlannedFileRemoval>> {
    let mut by_target: BTreeMap<String, Vec<(String, Metadata, SystemTime)>> = BTreeMap::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(target) = recovery_backup_target(&name) else {
            continue;
        };
        let metadata = std::fs::symlink_metadata(entry.path())?;
        let modified = metadata.modified()?;
        by_target
            .entry(target)
            .or_default()
            .push((name, metadata, modified));
    }
    let mut planned = Vec::new();
    for backups in by_target.values_mut() {
        backups.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
        let retained_mtime_tie = policy
            .keep_latest_per_target
            .checked_sub(1)
            .and_then(|position| backups.get(position))
            .map(|backup| backup.2);
        for (position, (name, metadata, modified)) in backups.iter().enumerate() {
            let old_enough = now
                .duration_since(*modified)
                .is_ok_and(|age| age >= policy.minimum_age);
            // Filesystem timestamps do not establish an order within a tie,
            // and numbered backup suffixes can be reused after holes form.
            // Preserve the whole equivalence class crossing the count cutoff.
            let retained_by_count = position < policy.keep_latest_per_target
                || retained_mtime_tie.is_some_and(|cutoff| *modified == cutoff);
            if !retained_by_count && old_enough {
                planned.push(PlannedFileRemoval {
                    file_name: name.clone(),
                    bytes: metadata.len(),
                    fingerprint: file_fingerprint(OsString::from(name.as_str()), metadata),
                });
            }
        }
    }
    planned.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    Ok(planned)
}

#[allow(
    clippy::too_many_lines,
    reason = "the apply path intentionally presents its crash-safe mutation order as one linear transaction"
)]
/// What the compaction phase of a merged run did.
#[derive(Clone, Debug)]
struct CompactionPhaseOutcome {
    /// The head the copy published. Equals the plan's current head only when
    /// no compaction ran.
    head_after: RecordIdentifier,
    /// The generation the copy wrote into.
    target_generation: GarbageCollectionGeneration,
    /// Distinct node records the copy rewrote.
    copied_nodes: u64,
    /// What the reclaim pass that follows the copy did.
    sweep: crate::writer::store_writer::SegmentSweepOutcome,
    /// The exact `gc.log` line this cycle appended.
    garbage_collection_entry: String,
}

/// Deep-copies the head into a fresh generation and reclaims what the copy
/// supersedes, in one writable session under the held lock.
///
/// The ordering inside is the whole safety argument. The copy is purely
/// additive — it appends fresh archives and touches no existing byte — so a
/// failure or a refusal anywhere before the head moves leaves the store
/// exactly as it was, minus some orphan archives a later run retires. Only
/// once the copy is committed and the head published does anything get
/// unlinked, and by then every segment the new head reaches lives in the
/// generation the sweep is about to retain.
#[allow(
    clippy::too_many_lines,
    reason = "the copy, the head commit, the reclaim pass and the cycle record are one ordered sequence whose adjacency is the safety argument"
)]
fn apply_compaction_phase(
    directory: &Path,
    plan: &CompactionPlan,
    repository_lock: &Arc<RepositoryLock>,
    kind: CompactionKind,
    rewrite_policy: ArchiveRewritePolicy,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionPhaseOutcome> {
    repository_lock.validate_path_identity(directory)?;
    let certified_archive_number =
        plan.checkpoint_archive_number
            .ok_or_else(|| Error::InvalidFormat {
                details: "the compaction phase has no certified output archive number".to_owned(),
            })?;
    let mut store = WritableRepository::open_prepared(
        directory,
        Arc::clone(repository_lock),
        certified_archive_number,
    )?;
    if store.head() != plan.current_head {
        return Err(Error::InvalidFormat {
            details: format!(
                "the compaction phase expected head {}, but strict writable open selected {}",
                plan.current_head,
                store.head()
            ),
        });
    }

    let base_generation = store
        .segment_generation(store.head().segment)
        .ok_or_else(|| Error::InvalidFormat {
            details: format!(
                "the head segment {} carries no generation triple",
                store.head().segment
            ),
        })?;
    let target_generation = compaction_target_generation(base_generation, kind);

    // Refuse a damaged source before the copy appends anything, so a retry
    // against a pre-existing defect does not durably append another full copy
    // before failing. The proof travels to the reclaim pass below.
    let certified_sources = store.preflight_reclaim_sources_with_progress(observer)?;

    let archive_bytes_before = archive_file_bytes(directory)?;
    let omitted_checkpoints: std::collections::BTreeSet<String> =
        plan.checkpoints.names.iter().cloned().collect();
    let mut writer = store.record_writer_with_identifier(target_generation, "c");
    let (new_head, copied_nodes) = crate::progress::observe(
        observer,
        &Step::new("copying nodes into a fresh generation", WorkUnit::Nodes),
        |observer| {
            crate::writer::compaction::deep_copy_super_root_with_progress(
                &store,
                &mut writer,
                plan.current_head,
                &omitted_checkpoints,
                observer,
            )
        },
    )?;
    writer.finish()?;

    if !store.set_head(plan.current_head, new_head) {
        return Err(Error::InvalidFormat {
            details: "the head moved during the compaction phase".to_owned(),
        });
    }
    store.flush()?;

    let sweep = crate::progress::observe(
        observer,
        &Step::new("reclaiming old generations", WorkUnit::Archives),
        |_observer| {
            store.reclaim_old_generations_with(
                crate::writer::store_writer::GenerationReclaimRequest {
                    rule: ReclaimRule {
                        reference: target_generation,
                        full: kind == CompactionKind::Full,
                        retained_generations: crate::writer::store_writer::RETAINED_GENERATIONS,
                    },
                    rewrite_policy,
                    certified_sources: Some(&certified_sources),
                    expected: None,
                },
            )
        },
    )?;

    store.close()?;
    sync_directory_strict(directory)?;

    // Oak reads `gc.log` to find the previous cycle when it tail-compacts, so
    // a completed cycle must record itself. The exact bytes are kept, because
    // the line carries a timestamp and the final verification proves the file
    // grew by *these* bytes rather than by something that merely looks alike.
    let archive_bytes_after = archive_file_bytes(directory)?;
    let garbage_collection_entry = crate::writer::compaction::garbage_collection_log_entry(
        archive_bytes_after,
        archive_bytes_before.saturating_sub(archive_bytes_after),
        target_generation,
        copied_nodes,
        new_head,
    );
    crate::writer::compaction::append_garbage_collection_log_entry(
        directory,
        &garbage_collection_entry,
    )?;
    sync_directory_strict(directory)?;

    Ok(CompactionPhaseOutcome {
        head_after: new_head,
        target_generation,
        copied_nodes,
        sweep,
        garbage_collection_entry,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "the ordered mutation sequence and the barriers between its phases are one safety argument that reads worse split apart"
)]
fn apply_prepared(
    prepared: PreparedCompaction,
    observer: &mut dyn ProgressObserver,
) -> Result<CompactionOutcome> {
    let PreparedCompaction {
        directory,
        options,
        plan,
        repaired,
        repository_lock,
    } = prepared;
    let current_fingerprint = directory_fingerprint(&directory)?;
    if current_fingerprint != plan.fingerprint {
        return Err(Error::InvalidFormat {
            details: "the repository changed after the authoritative cleanup plan was built; refusing to apply a stale plan"
                .to_owned(),
        });
    }

    repository_lock.validate_path_identity(&directory)?;

    let gc_log_before = read_optional_regular_file(&directory.join("gc.log"))?;
    let archive_bytes_before = archive_file_bytes(&directory)?;
    if plan.segment_plan.is_some() && plan.manifest_upgrade {
        // This is the final all-source gate before even the manifest may be
        // replaced. Version-two stores do this once inside the segment apply;
        // only the one-time manifest transition needs an additional pass so a
        // bad source cannot leave even a compatible metadata upgrade behind.
        let repository = Repository::open_with_progress(&directory, observer)?;
        reject_duplicate_active_segments(&repository)?;
        certify_active_archives_with_progress(&repository, repository.archives(), observer)?;
    }
    if plan.manifest_upgrade {
        repository_lock.validate_path_identity(&directory)?;
        upgrade_manifest_atomically(&directory)?;
    }

    let mut segment_outcome = StandaloneSegmentCompactionOutcome::default();
    if let Some(expected) = &plan.segment_plan {
        repository_lock.validate_path_identity(&directory)?;
        let (_, outcome) = apply_standalone_segment_cleanup(
            &directory,
            ReclaimRule {
                reference: plan.reference_generation,
                full: crate::writer::store_writer::FULL_GARBAGE_COLLECTION,
                retained_generations: crate::writer::store_writer::RETAINED_GENERATIONS,
            },
            plan.current_head.segment,
            &plan.protected_history_segments,
            options.archive_rewrite_policy,
            Some(expected),
            observer,
        )?;
        segment_outcome = outcome;
    }

    repository_lock.validate_path_identity(&directory)?;
    let (removed_stale_archives, mut stale_not_deleted) =
        if options.contains(MaintenanceTask::StaleArchives) {
            crate::progress::observe(
                observer,
                &Step::new("removing stale archives", WorkUnit::Files)
                    .with_total(crate::progress::count(plan.stale_archives.len())),
                |observer| {
                    remove_planned_files(
                        &directory,
                        plan.stale_archives
                            .iter()
                            .map(|archive| PlannedFileRemoval {
                                file_name: archive.file_name.clone(),
                                bytes: archive.bytes,
                                fingerprint: archive.fingerprint.clone(),
                            }),
                        PlannedFileRemovalFailureMode::RequireCertifiedTarget,
                        observer,
                    )
                },
            )?
        } else {
            (0, Vec::new())
        };

    // Before the copy, because a killed run's output holds bulk segments alive
    // through its references, and because retiring it first means a retry
    // converges instead of accumulating one more orphan generation.
    if let Some(expected) = &plan.residue_sweep {
        repository_lock.validate_path_identity(&directory)?;
        crate::progress::observe(
            observer,
            &Step::new(
                "retiring interrupted-compaction residue",
                WorkUnit::Archives,
            ),
            |observer| -> Result<()> {
                apply_standalone_segment_cleanup(
                    &directory,
                    ReclaimRule {
                        reference: plan.reference_generation,
                        full: crate::writer::store_writer::FULL_GARBAGE_COLLECTION,
                        retained_generations: i32::MAX,
                    },
                    plan.current_head.segment,
                    &HashSet::new(),
                    options.archive_rewrite_policy,
                    Some(expected),
                    observer,
                )
                .map(|_| ())
            },
        )?;
    }

    let mut expected_head_after = plan.current_head;
    let mut compaction_outcome: Option<CompactionPhaseOutcome> = None;
    let removed_checkpoints = if let Some(kind) = options.compaction_kind {
        // The head moves exactly once, and the checkpoints this run retires
        // are simply never carried into the fresh generation. Removing them
        // from the live head first would move the head twice, append a second
        // journal line, and strand records at a generation this same run then
        // reclaims — inside a session archive the reclaim pass never sweeps.
        let outcome = apply_compaction_phase(
            &directory,
            &plan,
            &repository_lock,
            kind,
            options.archive_rewrite_policy,
            observer,
        )?;
        expected_head_after = outcome.head_after;
        let omitted = plan.checkpoints.names.len() as u64;
        compaction_outcome = Some(outcome);
        omitted
    } else if plan.checkpoints.names.is_empty() {
        0
    } else {
        let (removed, head_after_checkpoints) = crate::progress::observe(
            observer,
            // No total: the removal is one indivisible commit, so there
            // is nothing to count up to and a declared total would stand
            // at zero for the whole phase.
            &Step::new("removing checkpoints", WorkUnit::Checkpoints),
            |_observer| -> Result<(u64, RecordIdentifier)> {
                repository_lock.validate_path_identity(&directory)?;
                let checkpoint_archive_number =
                    plan.checkpoint_archive_number
                        .ok_or_else(|| Error::InvalidFormat {
                            details: "checkpoint cleanup has no certified output archive number"
                                .to_owned(),
                        })?;
                let store = WritableRepository::open_prepared(
                    &directory,
                    Arc::clone(&repository_lock),
                    checkpoint_archive_number,
                )?;
                if store.head() != plan.current_head {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "cleanup expected checkpoint base head {}, but strict writable open selected {}",
                            plan.current_head,
                            store.head()
                        ),
                    });
                }
                let removed = remove_checkpoints(&store, &plan.checkpoints.names)?;
                if removed != plan.checkpoints.names.len() as u64 {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "cleanup planned to remove {} checkpoints, but the locked head contained {removed}",
                            plan.checkpoints.names.len()
                        ),
                    });
                }
                let head_after_checkpoints = store.head();
                store.close()?;
                sync_directory_strict(&directory)?;
                Ok((removed, head_after_checkpoints))
            },
        )?;
        expected_head_after = head_after_checkpoints;
        removed
    };

    // No step wraps this phase: the head verification and the journal
    // analysis inside it report steps of their own, and a step around
    // them would mix nodes and journal lines into one count.
    let journal_outcome = if let Some(compaction) = &compaction_outcome {
        // A compacting run retires history by identity, not by re-analysis.
        // The plan's retained roots named revisions of a head this run has
        // since replaced, and the segments behind them are already reclaimed,
        // so comparing a fresh analysis against them would refuse every run.
        // What is provable instead is exact: the journal keeps the single
        // physical line naming the head the copy just published, and nothing
        // else. That is the same shape Oak's own offline compact tool leaves.
        repository_lock.validate_path_identity(&directory)?;
        let repository = Repository::open_with_progress(&directory, observer)?;
        verify_exact_super_root(&repository, compaction.head_after, observer)?;
        let raw = scan_raw_journal(&directory)?;
        let retained = retained_compacted_head_line(&raw, compaction.head_after)?;
        verify_retained_journal_lines(&raw, &[raw.lines()[retained].content_bytes().to_vec()])?;
        if raw.lines().len() == 1 {
            JournalRewriteOutcome {
                changed: false,
                backup_path: None,
                retained_record_count: 1,
                removed_line_count: 0,
                bytes_written: raw.source_bytes().len(),
            }
        } else {
            rewrite_journal_atomically(&raw, &[retained])?
        }
    } else if options.contains(MaintenanceTask::Journal) {
        repository_lock.validate_path_identity(&directory)?;
        let repository = Repository::open_with_progress(&directory, observer)?;
        let head = repository.head_record_identifier();
        verify_exact_super_root(&repository, head, observer)?;
        let raw = scan_raw_journal(&directory)?;
        // The same bound the plan was built with: a fresh analysis that
        // retained every readable revision would disagree with the plan's
        // retained roots and abort the run the bound was set to enable.
        let analysis = analyze_journal(
            &repository,
            &raw,
            head,
            options.journal_revision_retention,
            observer,
        )?;
        // Earlier archive/checkpoint work ran after the operator confirmed the
        // plan. Do not let a fresh analysis turn an unexpected loss into an
        // unconfirmed journal deletion. The final reopen repeats both proofs,
        // but that would be too late to protect the canonical journal.
        verify_retained_journal_roots(
            &plan.journal.retained_record_ids,
            &analysis.retained_record_ids,
        )?;
        verify_retained_journal_lines(&raw, &plan.journal.retained_raw_lines)?;
        if analysis.plan.removed_lines == 0 {
            JournalRewriteOutcome {
                changed: false,
                backup_path: None,
                retained_record_count: analysis.retained_indexes.len(),
                removed_line_count: 0,
                bytes_written: raw.source_bytes().len(),
            }
        } else {
            rewrite_journal_atomically(&raw, &analysis.retained_indexes)?
        }
    } else {
        JournalRewriteOutcome {
            changed: false,
            backup_path: None,
            retained_record_count: 0,
            removed_line_count: 0,
            bytes_written: 0,
        }
    };

    sync_directory_strict(&directory)?;

    // `gc.log` records completed compaction cycles and nothing else. A run
    // that did not compact must leave it byte-identical; a run that did must
    // have appended exactly the one line describing the cycle it completed,
    // and changed nothing already in the file.
    let gc_log_after = read_optional_regular_file(&directory.join("gc.log"))?;
    match &compaction_outcome {
        None => {
            if gc_log_before != gc_log_after {
                return Err(Error::InvalidFormat {
                    details: "a run that did not compact changed gc.log, which is reserved for completed compaction cycles"
                        .to_owned(),
                });
            }
        }
        Some(compaction) => {
            let before = gc_log_before.as_deref().unwrap_or_default();
            let after = gc_log_after.as_deref().unwrap_or_default();
            let expected = compaction.garbage_collection_entry.as_bytes();
            if after.len() != before.len() + expected.len()
                || !after.starts_with(before)
                || &after[before.len()..] != expected
            {
                return Err(Error::InvalidFormat {
                    details: "the completed compaction did not append exactly its own gc.log entry"
                        .to_owned(),
                });
            }
        }
    }

    // All old archive mappings and writable caches are out of scope here.
    // Reopen from disk and prove the exact newly selected head and every
    // readable retained journal root through fresh mappings.
    // As above, the reopen, the head verification, and the journal
    // analysis each report a step of their own.
    let final_repository = Repository::open_with_progress(&directory, observer)?;
    let head_after = final_repository.head_record_identifier();
    if head_after != expected_head_after {
        return Err(Error::InvalidFormat {
            details: format!(
                "cleanup expected final head {expected_head_after}, but fresh reopen selected {head_after}"
            ),
        });
    }
    verify_exact_super_root(&final_repository, head_after, observer)?;
    let final_raw_journal = scan_raw_journal(&directory)?;
    // After the rewrite the journal holds at most the bound's worth of
    // resolvable revisions, so the boundary finds nothing beyond it and the
    // "no removable lines remain" assertion below stays exact.
    let mut final_journal_analysis = analyze_journal(
        &final_repository,
        &final_raw_journal,
        head_after,
        options.journal_revision_retention,
        observer,
    )?;
    inject_final_retained_root_fault(&mut final_journal_analysis.retained_record_ids);
    if let Some(compaction) = &compaction_outcome {
        // Exactly one line, naming exactly the head the copy published.
        // Stronger than the root comparison it replaces: that proved a set of
        // revisions still resolved, this proves the file holds one line and
        // which one.
        if final_raw_journal.lines().len() != 1 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a completed compaction left {} journal lines instead of one",
                    final_raw_journal.lines().len()
                ),
            });
        }
        retained_compacted_head_line(&final_raw_journal, compaction.head_after)?;
    } else {
        verify_retained_journal_roots(
            &plan.journal.retained_record_ids,
            &final_journal_analysis.retained_record_ids,
        )?;
        if options.contains(MaintenanceTask::Journal)
            && final_journal_analysis.plan.removed_lines != 0
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "journal cleanup left {} removable physical lines after its atomic rewrite",
                    final_journal_analysis.plan.removed_lines
                ),
            });
        }
        let expected_retained_lines =
            final_expected_retained_lines(&plan.journal.retained_raw_lines);
        verify_retained_journal_lines(&final_raw_journal, &expected_retained_lines)?;
    }

    // Recovery/staging material is the final, independent mutation. Never
    // discard it until every repository mutation has passed a fresh exact-head
    // and retained-history verification. These names are outside active
    // archive discovery, so their removal cannot invalidate the verified
    // state.
    repository_lock.validate_path_identity(&directory)?;
    // Each step is reported only when its task was selected. An unselected
    // task has nothing to remove, and announcing the work anyway told the
    // operator froe had considered backups it was never asked to touch.
    let (removed_temporaries, mut temporary_not_deleted) =
        if options.contains(MaintenanceTask::StaleTemporaries) {
            crate::progress::observe(
                observer,
                &Step::new("removing stale temporary files", WorkUnit::Files)
                    .with_total(crate::progress::count(plan.temporaries.len())),
                |observer| {
                    remove_planned_files(
                        &directory,
                        plan.temporaries.iter().cloned(),
                        PlannedFileRemovalFailureMode::Partial,
                        observer,
                    )
                },
            )?
        } else {
            (0, Vec::new())
        };
    let (removed_recovery_backups, mut backup_not_deleted) =
        if options.contains(MaintenanceTask::RecoveryBackups) {
            crate::progress::observe(
                observer,
                &Step::new("removing old recovery backups", WorkUnit::Files)
                    .with_total(crate::progress::count(plan.recovery_backups.len())),
                |observer| {
                    remove_planned_files(
                        &directory,
                        plan.recovery_backups.iter().cloned(),
                        PlannedFileRemovalFailureMode::Partial,
                        observer,
                    )
                },
            )?
        } else {
            (0, Vec::new())
        };
    sync_directory_strict(&directory)?;

    let archive_bytes_after = archive_file_bytes(&directory)?;
    let mut deletion_failures: Vec<_> = segment_outcome
        .deletion_failures
        .into_iter()
        .map(|failure| {
            if failure.target_was_already_absent {
                FileDeletionFailure::already_absent(failure.file_name, failure.error)
            } else {
                FileDeletionFailure::retained(failure.file_name, failure.error)
            }
        })
        .collect();
    deletion_failures.append(&mut stale_not_deleted);
    deletion_failures.append(&mut temporary_not_deleted);
    deletion_failures.append(&mut backup_not_deleted);
    deletion_failures.sort_by(|left, right| {
        left.file_name
            .cmp(&right.file_name)
            .then_with(|| left.error.cmp(&right.error))
    });
    deletion_failures.dedup();
    let mut files_not_deleted: Vec<_> = deletion_failures
        .iter()
        .map(|failure| failure.file_name.clone())
        .collect();
    files_not_deleted.sort();
    files_not_deleted.dedup();
    let removed_journal_lines = journal_outcome.removed_line_count;
    let journal_backup_path = journal_outcome.backup_path;

    // A run either sweeps through the directory-level engine or through the
    // compaction phase's own reclaim pass; never both, because a pre-copy plan
    // describes a store the copy replaces. Whichever ran carries the counts.
    let (rewritten_archives, removed_reclaimable_archives, removed_segments) =
        match &compaction_outcome {
            Some(compaction) => (
                compaction.sweep.rewritten_archives,
                compaction.sweep.removed_archives,
                compaction.sweep.removed_segments,
            ),
            None => (
                segment_outcome.rewritten_archives,
                segment_outcome.removed_archives,
                segment_outcome.removed_segments,
            ),
        };
    Ok(CompactionOutcome {
        head_before: plan.current_head,
        head_after,
        removed_checkpoints,
        removed_journal_lines,
        rewritten_archives,
        removed_reclaimable_archives,
        removed_stale_archives,
        removed_temporaries,
        removed_recovery_backups,
        repaired_archives: repaired.len(),
        files_not_deleted,
        archive_bytes_before,
        archive_bytes_after,
        retained_recovery_backup_bytes: recovery_backup_file_bytes(&directory)?,
        compacted: compaction_outcome
            .as_ref()
            .map(|compaction| CompactedGeneration {
                nodes: compaction.copied_nodes,
                generation: compaction.target_generation,
            }),
        removed_segments,
        journal_backup_path,
        deletion_failures,
    })
}

/// The index of the single physical journal line naming `head`.
///
/// Located by identity rather than by position: the copy appended its line to
/// whatever the journal already held, and a corrupt or duplicated file must be
/// refused rather than guessed at.
fn retained_compacted_head_line(raw: &RawJournal, head: RecordIdentifier) -> Result<usize> {
    let matching: Vec<usize> = raw
        .lines()
        .iter()
        .enumerate()
        .filter(|(_, line)| match line.classification() {
            RawJournalLineClassification::Record(record) => record.record_identifier == head,
            RawJournalLineClassification::ParserSkippedNoSpace
            | RawJournalLineClassification::InvalidRecordIdentifier { .. } => false,
        })
        .map(|(index, _)| index)
        .collect();
    match matching.as_slice() {
        [only] => Ok(*only),
        [] => Err(Error::InvalidFormat {
            details: format!("the journal holds no line naming the compacted head {head}"),
        }),
        many => Err(Error::InvalidFormat {
            details: format!(
                "the journal holds {} lines naming the compacted head {head}; refusing to choose one",
                many.len()
            ),
        }),
    }
}

fn verify_retained_journal_roots(
    expected: &[RecordIdentifier],
    actual_readable: &[RecordIdentifier],
) -> Result<()> {
    let mut counts = HashMap::new();
    for &identifier in actual_readable {
        *counts.entry(identifier).or_insert(0usize) += 1;
    }
    for &identifier in expected {
        let Some(count) = counts.get_mut(&identifier) else {
            return Err(Error::InvalidFormat {
                details: format!(
                    "cleanup made previously readable journal root {identifier} unreadable or removed its journal line"
                ),
            });
        };
        if *count == 0 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "cleanup removed a duplicate readable journal line for root {identifier}"
                ),
            });
        }
        *count -= 1;
    }
    Ok(())
}

fn inject_final_retained_root_fault(actual: &mut Vec<RecordIdentifier>) {
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::omit_last_if_armed(
        "cleanup.before-final-retained-root-verification",
        actual,
    );
    #[cfg(not(test))]
    let _ = actual;
}

fn final_expected_retained_lines(expected: &[Vec<u8>]) -> Cow<'_, [Vec<u8>]> {
    #[cfg(test)]
    {
        let mut injected = expected.to_vec();
        crate::writer::maintenance_fault_injection::append_missing_journal_line_if_armed(
            "cleanup.before-final-retained-line-verification",
            &mut injected,
        );
        Cow::Owned(injected)
    }
    #[cfg(not(test))]
    {
        Cow::Borrowed(expected)
    }
}

fn verify_retained_journal_lines(journal: &RawJournal, expected: &[Vec<u8>]) -> Result<()> {
    let mut remaining = expected.iter();
    let mut wanted = remaining.next();
    for line in journal.lines() {
        if wanted.is_some_and(|raw| retained_raw_line_matches(raw, line.raw_bytes())) {
            wanted = remaining.next();
        }
    }
    if wanted.is_some() {
        return Err(Error::InvalidFormat {
            details: "cleanup did not preserve every previously readable physical journal line byte-for-byte, with its original terminator and order"
                .to_owned(),
        });
    }
    Ok(())
}

fn retained_raw_line_matches(expected: &[u8], actual: &[u8]) -> bool {
    if actual == expected {
        return true;
    }
    // A checkpoint append and the byte-preserving rewrite both insert the one
    // separator Oak needs after an originally unterminated final record. No
    // other terminator normalization is permitted: LF, CRLF, and bare CR must
    // otherwise remain byte-exact.
    !matches!(expected.last(), Some(b'\n' | b'\r'))
        && actual.len() == expected.len() + 1
        && actual.starts_with(expected)
        && actual.last() == Some(&b'\n')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlannedFileRemovalFailureMode {
    RequireCertifiedTarget,
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlannedFileTargetVerification {
    Exact,
    Absent,
}

fn verify_planned_file_target(
    held: &File,
    path: &Path,
    expected_name: &OsStr,
    expected_fingerprint: &FileFingerprint,
) -> Result<PlannedFileTargetVerification> {
    let held_metadata = held.metadata()?;
    let path_metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PlannedFileTargetVerification::Absent);
        }
        Err(error) => return Err(error.into()),
    };
    if !held_metadata.file_type().is_file() || !path_metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!(
                "planned cleanup target {} ceased to be a regular file",
                path.display()
            ),
        });
    }
    let held_fingerprint = file_fingerprint(expected_name.to_owned(), &held_metadata);
    let path_fingerprint = file_fingerprint(expected_name.to_owned(), &path_metadata);
    if held_fingerprint != *expected_fingerprint || path_fingerprint != *expected_fingerprint {
        return Err(Error::InvalidFormat {
            details: format!(
                "planned cleanup target {} changed after its redundancy/retention proof; refusing to unlink replacement recovery material",
                path.display()
            ),
        });
    }
    Ok(PlannedFileTargetVerification::Exact)
}

fn accept_planned_file_verification(
    verification: Result<PlannedFileTargetVerification>,
    failure_mode: PlannedFileRemovalFailureMode,
    failures: &mut Vec<FileDeletionFailure>,
    file_name: &str,
) -> Result<bool> {
    match verification {
        Ok(PlannedFileTargetVerification::Exact) => Ok(true),
        Ok(PlannedFileTargetVerification::Absent) => {
            record_planned_file_removal_failure(
                PlannedFileRemovalFailureMode::Partial,
                failures,
                FileDeletionFailure::already_absent(
                    file_name.to_owned(),
                    ALREADY_ABSENT_DELETION_DETAIL,
                ),
            )?;
            Ok(false)
        }
        Err(error) => {
            record_planned_file_removal_failure(
                failure_mode,
                failures,
                FileDeletionFailure::retained(file_name.to_owned(), error.to_string()),
            )?;
            Ok(false)
        }
    }
}

fn record_planned_file_removal_failure(
    mode: PlannedFileRemovalFailureMode,
    failures: &mut Vec<FileDeletionFailure>,
    failure: FileDeletionFailure,
) -> Result<()> {
    if mode == PlannedFileRemovalFailureMode::RequireCertifiedTarget {
        return Err(Error::InvalidFormat {
            details: format!(
                "planned cleanup deletion of {} failed: {}",
                failure.file_name, failure.error
            ),
        });
    }
    failures.push(failure);
    Ok(())
}

fn remove_planned_files(
    directory: &Path,
    files: impl IntoIterator<Item = PlannedFileRemoval>,
    failure_mode: PlannedFileRemovalFailureMode,
    observer: &mut dyn ProgressObserver,
) -> Result<(usize, Vec<FileDeletionFailure>)> {
    // Count files the engine has *finished with*. The removal engine is
    // a lazy consumer, so pulling file N means files 0..N have been
    // resolved — reporting the count *before* the increment is therefore
    // exactly "items behind you". Reporting after it instead claimed a
    // file the moment it was taken, before its removal was attempted, so
    // a run that failed on its last file still counted it as done.
    let mut resolved = 0u64;
    let outcome = {
        let observer = &mut *observer;
        let counted = files.into_iter().inspect(|_file| {
            observer.step_advanced(resolved);
            resolved += 1;
        });
        remove_planned_files_with(directory, counted, failure_mode, |path| {
            std::fs::remove_file(path)
        })
    };
    // The last file is resolved only once the engine has stopped pulling.
    observer.step_advanced(resolved);
    outcome
}

fn remove_planned_files_with(
    directory: &Path,
    files: impl IntoIterator<Item = PlannedFileRemoval>,
    failure_mode: PlannedFileRemovalFailureMode,
    unlink: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(usize, Vec<FileDeletionFailure>)> {
    #[cfg(test)]
    {
        remove_planned_files_core(directory, files, failure_mode, |_, _| {}, unlink)
    }
    #[cfg(not(test))]
    {
        remove_planned_files_core(directory, files, failure_mode, unlink)
    }
}

#[cfg(all(test, unix))]
fn remove_planned_files_with_after_open(
    directory: &Path,
    files: impl IntoIterator<Item = PlannedFileRemoval>,
    failure_mode: PlannedFileRemovalFailureMode,
    after_open: impl FnMut(&Path, usize),
    unlink: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(usize, Vec<FileDeletionFailure>)> {
    remove_planned_files_core(directory, files, failure_mode, after_open, unlink)
}

fn remove_planned_files_core(
    directory: &Path,
    files: impl IntoIterator<Item = PlannedFileRemoval>,
    failure_mode: PlannedFileRemovalFailureMode,
    #[cfg(test)] mut after_open: impl FnMut(&Path, usize),
    mut unlink: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(usize, Vec<FileDeletionFailure>)> {
    let mut removed = 0usize;
    let mut failures = Vec::new();
    for file in files {
        let path = directory.join(&file.file_name);
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let held = match options.open(&path) {
            Ok(held) => held,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Absence already satisfies the deletion's repository-state
                // goal. Preserve it as an auditable partial result, but do not
                // discard earlier successful mutations merely because another
                // lock-breaking actor won the unlink race.
                record_planned_file_removal_failure(
                    PlannedFileRemovalFailureMode::Partial,
                    &mut failures,
                    FileDeletionFailure::already_absent(
                        file.file_name,
                        ALREADY_ABSENT_DELETION_DETAIL,
                    ),
                )?;
                continue;
            }
            Err(error) => {
                record_planned_file_removal_failure(
                    failure_mode,
                    &mut failures,
                    FileDeletionFailure::retained(file.file_name, error.to_string()),
                )?;
                continue;
            }
        };
        let expected_name = OsString::from(file.file_name.as_str());
        #[cfg(test)]
        after_open(&path, 0);
        if !accept_planned_file_verification(
            verify_planned_file_target(&held, &path, &expected_name, &file.fingerprint),
            failure_mode,
            &mut failures,
            &file.file_name,
        )? {
            continue;
        }
        #[cfg(test)]
        if let Err(error) = crate::writer::maintenance_fault_injection::substitute_path_if_armed(
            "remove-planned-file.before-final-identity",
            &path,
        ) {
            record_planned_file_removal_failure(
                failure_mode,
                &mut failures,
                FileDeletionFailure::retained(file.file_name, error.to_string()),
            )?;
            continue;
        }
        // Recheck both the held descriptor and its directory name at the last
        // portable point before unlink. A one-shot pathname substitution can
        // no longer make the confirmed retention/redundancy proof authorize a
        // different inode.
        #[cfg(test)]
        after_open(&path, 1);
        if !accept_planned_file_verification(
            verify_planned_file_target(&held, &path, &expected_name, &file.fingerprint),
            failure_mode,
            &mut failures,
            &file.file_name,
        )? {
            continue;
        }
        // From here the descriptor and pathname still identify the exact
        // certified source. A syscall failure leaves that known-safe source in
        // place and is therefore a structured partial result even in the
        // pre-head stale-archive phase. Only failure to certify the target is
        // fatal in `RequireCertifiedTarget` mode.
        match unlink(&path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                record_planned_file_removal_failure(
                    PlannedFileRemovalFailureMode::Partial,
                    &mut failures,
                    FileDeletionFailure::already_absent(
                        file.file_name,
                        ALREADY_ABSENT_DELETION_DETAIL,
                    ),
                )?;
            }
            Err(error) => record_planned_file_removal_failure(
                PlannedFileRemovalFailureMode::Partial,
                &mut failures,
                FileDeletionFailure::retained(file.file_name, error.to_string()),
            )?,
        }
    }
    Ok((removed, failures))
}

fn archive_file_bytes(directory: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if ArchiveFileName::parse(&name).is_some() {
            bytes = bytes
                .checked_add(std::fs::symlink_metadata(entry.path())?.len())
                .ok_or_else(|| Error::InvalidFormat {
                    details: "archive byte accounting overflow".to_owned(),
                })?;
        }
    }
    Ok(bytes)
}

/// Bytes held by every recognized recovery backup in the directory.
///
/// Counted with the same predicate that decides what the backup retention
/// policy may retire, so the reported figure is exactly the material a later
/// `--task recovery-backups` run can reclaim — and, before that run, exactly
/// the growth the archive byte line does not show.
fn recovery_backup_file_bytes(directory: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if recovery_backup_target(&name).is_none() {
            continue;
        }
        add_estimate(&mut bytes, std::fs::symlink_metadata(entry.path())?.len())?;
    }
    Ok(bytes)
}

fn read_optional_regular_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(std::fs::read(path)?)),
        Ok(_) => Err(Error::InvalidFormat {
            details: format!("{} is not a regular file", path.display()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "source/staging certification and every durability cutpoint form one atomic publication protocol"
)]
fn upgrade_manifest_atomically(directory: &Path) -> Result<()> {
    let manifest_path = directory.join("manifest");
    let metadata = std::fs::symlink_metadata(&manifest_path)?;
    if !metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!("{} is not a regular file", manifest_path.display()),
        });
    }
    if crate::store::read_manifest_store_version(&manifest_path)? >= 2 {
        return Ok(());
    }
    let source = std::fs::read(&manifest_path)?;
    let source_certificate = certify_manifest_file(
        &manifest_path,
        &metadata,
        &source,
        ManifestFileAccess::Read,
        "source manifest",
    )?;
    let output = manifest_upgrade_bytes(&source);

    let (temporary_path, mut temporary) =
        create_exclusive_numbered_file(directory, "manifest.cleaning")?;
    let mut guard = UncommittedFile::new(temporary_path.clone());
    temporary.write_all(&output)?;
    preserve_file_metadata(&temporary, &metadata)?;
    let temporary_identity = temporary.metadata()?;
    drop(temporary);
    let temporary_certificate = certify_manifest_file(
        &temporary_path,
        &temporary_identity,
        &output,
        ManifestFileAccess::ReadWrite,
        "staged manifest replacement",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.temporary-durable")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.temporary-durable");
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.before-rename")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.before-rename");
    source_certificate.recertify(
        &manifest_path,
        &source,
        ManifestFileAccess::Read,
        "source manifest",
    )?;
    temporary_certificate.recertify(
        &temporary_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "staged manifest replacement",
    )?;
    std::fs::rename(&temporary_path, &manifest_path)?;
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    drop(source_certificate);
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "manifest.renamed-before-directory-sync",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed(
        "manifest.renamed-before-directory-sync",
    );
    guard.commit();
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "manifest.before-post-rename-directory-sync",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed(
        "manifest.before-post-rename-directory-sync",
    );
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    sync_directory_strict(directory)?;
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.rename-durable")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.rename-durable");
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    if crate::store::read_manifest_store_version(&manifest_path)? != 2 {
        return Err(Error::InvalidFormat {
            details: "atomic manifest upgrade did not install store.version=2".to_owned(),
        });
    }
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    Ok(())
}

fn manifest_upgrade_bytes(source: &[u8]) -> Vec<u8> {
    let mut output = source.to_vec();
    if !output.is_empty() {
        // Properties.load joins a physical line ending in an odd number of
        // backslashes with the next natural line. Install an empty natural
        // line before our comment so a trailing continuation cannot consume
        // it and alter the preceding custom property's value.
        match output.last() {
            Some(b'\n') => output.push(b'\n'),
            Some(_) => output.extend_from_slice(b"\n\n"),
            None => unreachable!("non-empty output has a final byte"),
        }
    }
    output.extend_from_slice(b"# upgraded atomically by froe cleanup\nstore.version=2\n");
    output
}

#[derive(Clone, Copy)]
enum ManifestFileAccess {
    Read,
    ReadWrite,
}

struct ManifestFileCertificate {
    held: File,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl ManifestFileCertificate {
    fn recertify(
        &self,
        path: &Path,
        expected: &[u8],
        access: ManifestFileAccess,
        label: &str,
    ) -> Result<()> {
        let held_metadata = self.held.metadata()?;
        if !held_metadata.file_type().is_file() {
            return Err(Error::InvalidFormat {
                details: format!("held {label} is no longer regular"),
            });
        }
        #[cfg(unix)]
        if (held_metadata.dev(), held_metadata.ino()) != (self.device, self.inode) {
            return Err(Error::InvalidFormat {
                details: format!("held {label} changed identity before publication"),
            });
        }
        let recertified =
            open_verified_manifest_file(path, &held_metadata, expected, access, label)?;
        drop(recertified);
        Ok(())
    }
}

fn certify_manifest_file(
    path: &Path,
    expected_identity: &Metadata,
    expected: &[u8],
    access: ManifestFileAccess,
    label: &str,
) -> Result<ManifestFileCertificate> {
    let held = open_verified_manifest_file(path, expected_identity, expected, access, label)?;
    #[cfg(unix)]
    let held_metadata = held.metadata()?;
    Ok(ManifestFileCertificate {
        held,
        #[cfg(unix)]
        device: held_metadata.dev(),
        #[cfg(unix)]
        inode: held_metadata.ino(),
    })
}

fn open_verified_manifest_file(
    path: &Path,
    expected_identity: &Metadata,
    expected: &[u8],
    access: ManifestFileAccess,
    label: &str,
) -> Result<File> {
    #[cfg(not(unix))]
    let _ = expected_identity;
    let link_metadata = std::fs::symlink_metadata(path)?;
    if !link_metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!("{label} {} is not regular", path.display()),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true);
    if matches!(access, ManifestFileAccess::ReadWrite) {
        options.write(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let reopened_metadata = file.metadata()?;
    if !reopened_metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!("{label} {} is not regular", path.display()),
        });
    }
    #[cfg(unix)]
    if (link_metadata.dev(), link_metadata.ino())
        != (expected_identity.dev(), expected_identity.ino())
        || (reopened_metadata.dev(), reopened_metadata.ino())
            != (expected_identity.dev(), expected_identity.ino())
    {
        return Err(Error::InvalidFormat {
            details: format!(
                "{label} {} changed identity before publication",
                path.display()
            ),
        });
    }
    let mut actual = Vec::new();
    file.read_to_end(&mut actual)?;
    if actual != expected {
        return Err(Error::InvalidFormat {
            details: format!("{label} {} changed before publication", path.display()),
        });
    }
    Ok(file)
}

fn create_exclusive_numbered_file(directory: &Path, stem: &str) -> Result<(PathBuf, File)> {
    for counter in 0..1000u16 {
        let path = directory.join(format!("{stem}.{counter:03}"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::InvalidFormat {
        details: format!("all numbered names for {stem} (000-999) are occupied"),
    })
}

struct UncommittedFile {
    path: PathBuf,
    committed: bool,
}

impl UncommittedFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for UncommittedFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::fs::File;
    #[cfg(unix)]
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    #[cfg(unix)]
    use super::{
        ApplyCredentials, ManifestFileAccess, SETGID_MODE,
        append_apply_identity_preview_warning_for_credentials, certify_manifest_file,
        journal_service_user_issue, metadata_source_apply_identity_issue,
        planned_apply_identity_issue, planned_metadata_sources, possible_created_group_ids,
        preview_apply_identity_issue, remove_planned_files_with_after_open,
        take_possible_created_group_ids_input, validate_apply_identity_for_uid,
        validate_plan_apply_identity_for_credentials,
    };
    use super::{
        CompactionAction, CompactionOptions, JOURNAL_LINE_PREVIEW_LIMIT, JournalRemovalReason,
        MaintenanceTask, PlannedFileRemoval, PlannedFileRemovalFailureMode, PreparedCompaction,
        RecoveryBackupPolicy, attach_planning_warnings, compact, compact_with_progress,
        file_fingerprint, indexless_archive_refusal, manifest_upgrade_bytes, plan_compaction,
        recovery_backup_target, remove_planned_files, remove_planned_files_with,
    };
    use crate::checksum::crc32;
    use crate::content::provider::SegmentProvider as _;
    use crate::progress::{ProgressObserver, Step};
    use crate::segment::identifier::SegmentIdentifier;
    use crate::segment::record::RecordIdentifier;
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::tar_archive::file_name::ArchiveFileName;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::store_writer::WritableRepository;
    use crate::writer::tar_writer::TarArchiveWriter;
    use std::num::NonZeroUsize;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "froe-cleanup-{name}-{}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("create test directory");
            Self { path }
        }

        fn repository(name: &str) -> Self {
            let directory = Self::new(name);
            WritableRepository::open(&directory.path)
                .expect("bootstrap")
                .close()
                .expect("close bootstrap");
            directory
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    fn checked_timespec_field<T>(value: i64) -> T
    where
        T: TryFrom<i64>,
    {
        T::try_from(value).unwrap_or_else(|_| {
            panic!("filesystem timestamp component {value} does not fit libc::timespec")
        })
    }

    /// The canonical repository directory of a test fixture.
    ///
    /// Planning resolves the directory once through
    /// [`canonical_repository_directory`] and reports every path relative to
    /// that target, so an expectation built from the raw fixture path is not
    /// comparable on a platform whose temporary directory is reached through a
    /// symlink — macOS resolves `/var` to `/private/var`. Join the managed file
    /// name onto this rather than canonicalizing the file itself: a managed
    /// path under test may be a symlink, and following it would assert the
    /// wrong name.
    fn canonical_fixture_directory(directory: &std::path::Path) -> std::path::PathBuf {
        directory
            .canonicalize()
            .expect("canonicalize the fixture directory")
    }

    fn file_bytes(directory: &std::path::Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(directory).expect("read directory") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("type").is_file() {
                files.push((
                    entry.file_name(),
                    std::fs::read(entry.path()).expect("read file"),
                ));
            }
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    #[cfg(unix)]
    fn relative_path_from(base: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
        let base_components: Vec<_> = base.components().collect();
        let target_components: Vec<_> = target.components().collect();
        let common = base_components
            .iter()
            .zip(&target_components)
            .take_while(|(left, right)| left == right)
            .count();
        assert!(common != 0, "absolute Unix paths share their root");

        let mut relative = std::path::PathBuf::new();
        for component in &base_components[common..] {
            assert!(matches!(component, std::path::Component::Normal(_)));
            relative.push("..");
        }
        for component in &target_components[common..] {
            relative.push(component.as_os_str());
        }
        if relative.as_os_str().is_empty() {
            relative.push(".");
        }
        relative
    }

    fn file_mtimes(
        directory: &std::path::Path,
    ) -> Vec<(std::ffi::OsString, u64, std::time::SystemTime)> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(directory).expect("read directory") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("type").is_file() {
                let metadata = entry.metadata().expect("metadata");
                files.push((
                    entry.file_name(),
                    metadata.len(),
                    metadata.modified().expect("mtime"),
                ));
            }
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    fn corrupt_first_magic(path: &std::path::Path, magic: [u8; 4]) {
        let mut bytes = std::fs::read(path).expect("read archive fixture");
        let position = bytes
            .windows(magic.len())
            .position(|window| window == magic)
            .expect("trailer magic exists");
        bytes[position] ^= 0x01;
        std::fs::write(path, bytes).expect("corrupt trailer magic");
    }

    fn change_index_generation(
        path: &std::path::Path,
        identifier: SegmentIdentifier,
        generation: i32,
    ) {
        const TERMINATING_ZERO_BLOCKS: usize = 1024;
        const FOOTER_SIZE: usize = 16;

        let reader = TarArchiveReader::open(path).expect("open indexed archive fixture");
        let index = reader.index().expect("fixture has an index");
        let entry_size = if index.version == 2 { 33 } else { 28 };
        let entry_position = index
            .entries()
            .iter()
            .position(|entry| entry.segment_identifier == identifier)
            .expect("fixture index contains segment");
        let entry_count = index.entries().len();
        drop(reader);

        let mut bytes = std::fs::read(path).expect("read indexed archive fixture");
        let entries_end = bytes.len() - TERMINATING_ZERO_BLOCKS - FOOTER_SIZE;
        let entries_start = entries_end - entry_count * entry_size;
        let generation_start = entries_start + entry_position * entry_size + 24;
        bytes[generation_start..generation_start + 4].copy_from_slice(&generation.to_be_bytes());
        let checksum = crc32(&bytes[entries_start..entries_end]);
        bytes[entries_end..entries_end + 4].copy_from_slice(&checksum.to_be_bytes());
        std::fs::write(path, bytes).expect("write mismatched index generation");
    }

    fn repack_without_graph_or_brf(
        directory: &std::path::Path,
        source_name: &str,
        target_name: &str,
    ) {
        let source = TarArchiveReader::open(&directory.join(source_name)).expect("source archive");
        let mut entries = source.index().expect("source index").entries().to_vec();
        entries.sort_by_key(|entry| entry.position);
        let mut target = TarArchiveWriter::new(directory, target_name);
        for entry in entries {
            target
                .write_segment(
                    entry.segment_identifier,
                    source
                        .segment_data(entry.segment_identifier)
                        .expect("source payload"),
                    GarbageCollectionGeneration {
                        generation: entry.generation,
                        full_generation: entry.full_generation,
                        is_compacted: entry.is_compacted,
                    },
                    &[],
                    &[],
                )
                .expect("repack segment without metadata");
        }
        target.close().expect("close repacked archive");
    }

    #[derive(Clone, Copy)]
    enum OmittedArchiveMetadata {
        Graph,
        BinaryReferences,
    }

    fn repack_omitting_archive_metadata(
        directory: &std::path::Path,
        source_name: &str,
        omitted: OmittedArchiveMetadata,
    ) {
        let source_path = directory.join(source_name);
        let source = TarArchiveReader::open(&source_path).expect("source archive");
        let graph_by_source: HashMap<_, _> = source
            .segment_graph()
            .expect("source graph")
            .adjacency
            .into_iter()
            .collect();
        let mut binary_references_by_source = HashMap::new();
        for generation in source
            .binary_references()
            .expect("source binary-reference catalog")
            .generations
        {
            for (identifier, references) in generation.segments {
                assert!(
                    binary_references_by_source
                        .insert(identifier, references)
                        .is_none(),
                    "fixture source repeats a BRF segment"
                );
            }
        }
        let mut entries = source.index().expect("source index").entries().to_vec();
        entries.sort_by_key(|entry| entry.position);
        let temporary_name = format!("{source_name}.certificate-corrupt");
        let temporary_path = directory.join(&temporary_name);
        let mut target =
            TarArchiveWriter::new_exclusive_staged(directory, &temporary_name, source_name);
        for entry in entries {
            let references = if matches!(omitted, OmittedArchiveMetadata::Graph) {
                &[][..]
            } else {
                graph_by_source
                    .get(&entry.segment_identifier)
                    .map_or(&[][..], Vec::as_slice)
            };
            let binary_references = if matches!(omitted, OmittedArchiveMetadata::BinaryReferences) {
                &[][..]
            } else {
                binary_references_by_source
                    .get(&entry.segment_identifier)
                    .map_or(&[][..], Vec::as_slice)
            };
            target
                .write_segment(
                    entry.segment_identifier,
                    source
                        .segment_data(entry.segment_identifier)
                        .expect("source payload"),
                    GarbageCollectionGeneration {
                        generation: entry.generation,
                        full_generation: entry.full_generation,
                        is_compacted: entry.is_compacted,
                    },
                    references,
                    binary_references,
                )
                .expect("repack selectively omitted metadata");
        }
        target.close().expect("close corrupt repack");
        drop(source);
        std::fs::remove_file(&source_path).expect("remove original fixture archive");
        std::fs::rename(temporary_path, source_path).expect("install corrupt fixture archive");
    }

    fn corrupt_segment_payload_crc(path: &std::path::Path, identifier: SegmentIdentifier) {
        let reader = TarArchiveReader::open(path).expect("open indexed archive fixture");
        let entry = *reader
            .index_entry(identifier)
            .expect("fixture index contains survivor");
        drop(reader);
        let mut bytes = std::fs::read(path).expect("read archive fixture");
        let payload_byte = entry.position as usize + entry.size as usize - 1;
        bytes[payload_byte] ^= 0x01;
        std::fs::write(path, bytes).expect("corrupt segment payload without changing its name CRC");
    }

    fn write_empty_node_segment(
        store: &WritableRepository,
        generation: GarbageCollectionGeneration,
    ) -> crate::segment::record::RecordIdentifier {
        let mut writer = store.record_writer(generation);
        let node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("write fixture node");
        writer.finish().expect("finish fixture segment");
        node
    }

    fn rewrite_certificate_fixture(
        name: &str,
    ) -> (TestDirectory, String, String, SegmentIdentifier) {
        let directory = TestDirectory::repository(name);
        let store = WritableRepository::open(&directory.path).expect("open fixture writer");
        let old_generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        write_empty_node_segment(&store, old_generation);
        write_empty_node_segment(&store, old_generation);

        let current_generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };
        let mut survivor_writer = store.record_writer(current_generation);
        let external = survivor_writer
            .write_external_binary_identifier("source-certificate-live-external-blob")
            .expect("write external binary identifier");
        let survivor = survivor_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "binary".to_owned(),
                    property_type: crate::content::property::PropertyType::Binary,
                    values: PropertyValuesToWrite::Single(external),
                }],
            )
            .expect("write unreferenced survivor");
        survivor_writer.finish().expect("finish survivor segment");
        let content_root = write_empty_node_segment(&store, current_generation);
        let mut head_writer = store.record_writer(current_generation);
        let new_head = head_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("write cross-segment head");
        head_writer.finish().expect("finish head segment");
        assert!(store.set_head(store.head(), new_head));
        store.close().expect("close fixture writer");

        let repository = Repository::open(&directory.path).expect("open healthy fixture");
        let source_name = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(survivor.segment))
            .expect("session archive contains survivor")
            .file_name()
            .to_owned();
        drop(repository);
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("healthy rewrite plan");
        let replacement_name = plan
            .actions()
            .iter()
            .find_map(|action| match action {
                CompactionAction::RewriteArchive {
                    file_name,
                    replacement_name,
                    ..
                } if file_name == &source_name => Some(replacement_name.clone()),
                _ => None,
            })
            .expect("fixture produces an actionable rewrite");
        (directory, source_name, replacement_name, survivor.segment)
    }

    fn whole_removal_certificate_fixture(name: &str) -> (TestDirectory, String, SegmentIdentifier) {
        let directory = TestDirectory::repository(name);
        let orphan = {
            let store = WritableRepository::open(&directory.path).expect("open orphan writer");
            let orphan = write_empty_node_segment(
                &store,
                GarbageCollectionGeneration {
                    generation: 0,
                    full_generation: 0,
                    is_compacted: false,
                },
            );
            store.close().expect("close orphan writer");
            orphan
        };
        {
            let store = WritableRepository::open(&directory.path).expect("open head writer");
            let generation = GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            };
            let content_root = write_empty_node_segment(&store, generation);
            let mut writer = store.record_writer(generation);
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: content_root,
                    },
                    &[],
                )
                .expect("write new head");
            writer.finish().expect("finish new head segment");
            assert!(store.set_head(store.head(), head));
            store.close().expect("close head writer");
        }
        let repository = Repository::open(&directory.path).expect("open healthy fixture");
        let source_name = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(orphan.segment))
            .expect("orphan archive")
            .file_name()
            .to_owned();
        drop(repository);
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("healthy removal plan");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == &source_name
        )));
        (directory, source_name, orphan.segment)
    }

    fn assert_source_certificate_refusal(
        directory: &TestDirectory,
        source_name: &str,
        replacement_name: Option<&str>,
        expected_error: &str,
    ) {
        let source_path = directory.path.join(source_name);
        let source_before = std::fs::read(&source_path).expect("read corrupt source");
        let journal_before = std::fs::read(directory.path.join("journal.log")).expect("journal");
        let before = file_bytes(&directory.path);
        for options in [
            CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
            CompactionOptions::default(),
        ] {
            let error = plan_compaction(&directory.path, &options)
                .expect_err("read-only planning must reject an uncertified active archive");
            assert!(
                error.to_string().contains(expected_error),
                "unexpected certificate error: {error}"
            );
            assert_eq!(
                file_bytes(&directory.path),
                before,
                "planning mutated files"
            );
        }

        let error = compact(
            &directory.path,
            CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect_err("locked replan must reject an uncertified active archive");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected locked certificate error: {error}"
        );
        assert_eq!(
            std::fs::read(&source_path).expect("source remains"),
            source_before,
            "cleanup changed the uncertified source"
        );
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("journal remains"),
            journal_before,
            "cleanup changed the journal before source certification"
        );
        if let Some(replacement_name) = replacement_name {
            assert!(
                !directory.path.join(replacement_name).exists(),
                "cleanup published a replacement before source certification"
            );
        }
    }

    #[test]
    fn dry_run_is_byte_exact_and_never_creates_the_lock_file() {
        let directory = TestDirectory::repository("dry-run");
        std::fs::remove_file(directory.path.join("repo.lock")).expect("remove old lock inode");
        let before = file_bytes(&directory.path);
        let mtimes_before = file_mtimes(&directory.path);

        let plan = plan_compaction(&directory.path, &CompactionOptions::default()).expect("plan");

        assert!(plan.is_empty());
        assert_eq!(file_bytes(&directory.path), before);
        assert_eq!(file_mtimes(&directory.path), mtimes_before);
        assert!(!directory.path.join("repo.lock").exists());
    }

    #[test]
    fn journal_removal_preview_is_an_exact_bounded_byte_prefix() {
        let directory = TestDirectory::repository("bounded-journal-preview");
        let mut hostile = vec![0xff];
        hostile.extend(std::iter::repeat_n(b'x', JOURNAL_LINE_PREVIEW_LIMIT + 20));
        hostile.push(b'\n');
        std::fs::OpenOptions::new()
            .append(true)
            .open(directory.path.join("journal.log"))
            .expect("open journal")
            .write_all(&hostile)
            .expect("append long invalid line");

        let plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Journal]),
        )
        .expect("plan long invalid line");
        let removal = plan
            .journal_line_removals()
            .last()
            .expect("invalid line removal");
        assert_eq!(
            removal.preview_bytes(),
            &hostile[..JOURNAL_LINE_PREVIEW_LIMIT]
        );
        assert!(removal.preview_truncated());
        assert_eq!(removal.reason(), JournalRemovalReason::ParserSkippedNoSpace);
    }

    #[test]
    fn non_journal_task_hides_internal_journal_removal_candidates() {
        let directory = TestDirectory::repository("non-journal-removal-visibility");
        std::fs::OpenOptions::new()
            .append(true)
            .open(directory.path.join("journal.log"))
            .expect("open journal")
            .write_all(b"parser-skipped\n")
            .expect("append parser-skipped line");

        let plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("segment-only plan");

        assert_eq!(plan.tasks(), &[MaintenanceTask::Segments]);
        assert!(plan.journal_line_removals().is_empty());
        assert!(
            !plan
                .actions()
                .iter()
                .any(|action| matches!(action, CompactionAction::PruneJournal { .. }))
        );
    }

    #[test]
    fn prospective_plan_refuses_a_survivor_that_references_a_planned_removal() {
        let directory = TestDirectory::repository("prospective-survivor-reference");
        let old_generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        let target = {
            let store = WritableRepository::open(&directory.path).expect("open old-target writer");
            let target = write_empty_node_segment(&store, old_generation);
            store.close().expect("close old-target archive");
            target
        };
        let store = WritableRepository::open(&directory.path).expect("open survivor writer");
        let current_generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };
        let mut survivor_writer = store.record_writer(current_generation);
        let survivor = survivor_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "old-target".to_owned(),
                    node: target,
                },
                &[],
            )
            .expect("write unjournaled newer-generation survivor");
        survivor_writer.finish().expect("finish survivor segment");
        let content_root = write_empty_node_segment(&store, current_generation);
        let mut head_writer = store.record_writer(current_generation);
        let head = head_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("write unrelated current head");
        head_writer.finish().expect("finish current head segment");
        assert!(store.set_head(store.head(), head));
        store.close().expect("close fixture writer");
        let before = file_bytes(&directory.path);

        let error = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect_err("prospective deletion must reject a surviving cross-reference");

        assert_eq!(
            error.to_string(),
            format!(
                "invalid segment-tar data: surviving data segment {} references segment {}, which the cleanup plan would remove",
                survivor.segment, target.segment
            )
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "prospective validation remains read-only"
        );
        Repository::open(&directory.path).expect("refused fixture remains readable");
    }

    #[cfg(unix)]
    #[test]
    fn apply_identity_mismatch_is_detected_before_lock_creation() {
        use std::os::unix::fs::MetadataExt;

        let directory = TestDirectory::repository("wrong-service-user");
        std::fs::remove_file(directory.path.join("repo.lock")).expect("remove old lock inode");
        let owner = std::fs::metadata(directory.path.join("journal.log"))
            .expect("journal metadata")
            .uid();
        let different_uid = if owner == u32::MAX {
            owner - 1
        } else {
            owner + 1
        };

        let error = validate_apply_identity_for_uid(&directory.path, different_uid)
            .expect_err("different service uid must be rejected");

        assert!(error.to_string().contains("service user"));
        assert!(!directory.path.join("repo.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn authoritative_plan_rejects_a_foreign_owned_archive_rewrite_before_mutation() {
        use std::os::unix::fs::MetadataExt as _;

        let (directory, source_name, _, _) =
            rewrite_certificate_fixture("foreign-owned-rewrite-preflight");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("healthy rewrite plan");
        let owner = std::fs::metadata(directory.path.join(&source_name))
            .expect("source metadata")
            .uid();
        let source_gid = std::fs::metadata(directory.path.join(&source_name))
            .expect("source metadata")
            .gid();
        let different_uid = if owner == u32::MAX {
            owner - 1
        } else {
            owner + 1
        };
        let credentials = ApplyCredentials {
            effective_uid: different_uid,
            effective_gid: source_gid,
            group_ids: BTreeSet::from([source_gid]),
        };
        let before = file_bytes(&directory.path);

        let error =
            validate_plan_apply_identity_for_credentials(&directory.path, &plan, &credentials)
                .expect_err("foreign-owned rewrite source must fail preflight");

        assert_eq!(
            error.to_string(),
            format!(
                "invalid segment-tar data: cleanup cannot safely replace {} while preserving its metadata: it is owned by uid {owner}, but the effective uid is {different_uid}; conservatively refusing before planned repository mutations",
                directory.path.join(source_name).display()
            )
        );
        assert_eq!(file_bytes(&directory.path), before);
        Repository::open(&directory.path).expect("preflight refusal leaves repository healthy");
    }

    #[cfg(unix)]
    #[test]
    fn planned_identity_preflight_uses_the_real_repository_directory_gid_and_mode() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let directory = TestDirectory::repository("planned-identity-directory-metadata");
        let plan = plan_compaction(&directory.path, &CompactionOptions::new().with_tasks([]))
            .expect("plan health-only cleanup");
        let mut permissions = std::fs::symlink_metadata(&directory.path)
            .expect("repository metadata before mode change")
            .permissions();
        permissions.set_mode(0o731);
        std::fs::set_permissions(&directory.path, permissions)
            .expect("install a distinctive repository mode");
        let metadata = std::fs::symlink_metadata(&directory.path).expect("repository metadata");
        let synthetic_gid = if metadata.gid() == u32::MAX {
            metadata.gid() - 1
        } else {
            metadata.gid() + 1
        };
        let credentials = ApplyCredentials {
            effective_uid: 42_424,
            effective_gid: synthetic_gid,
            group_ids: BTreeSet::from([synthetic_gid]),
        };
        let _ = take_possible_created_group_ids_input();

        let issue = planned_apply_identity_issue(&directory.path, &plan, &credentials)
            .expect("analyze planned metadata identity");

        assert_eq!(issue, None, "a health-only plan has no metadata sources");
        assert_eq!(
            take_possible_created_group_ids_input(),
            Some((metadata.gid(), metadata.permissions().mode())),
            "the group model must receive the repository directory's real gid and mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ownership_preview_emits_a_known_mismatch_and_matches_the_apply_gate() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = TestDirectory::repository("preview-service-user-warning");
        let mut plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("read-only plan");
        let journal_owner = std::fs::symlink_metadata(directory.path.join("journal.log"))
            .expect("journal metadata")
            .uid();
        let other_uid = if journal_owner == u32::MAX {
            journal_owner - 1
        } else {
            journal_owner + 1
        };
        let credentials = ApplyCredentials {
            effective_uid: other_uid,
            effective_gid: 0,
            group_ids: BTreeSet::from([0]),
        };

        let issue = preview_apply_identity_issue(&directory.path, &plan, &credentials)
            .expect("preview identity analysis")
            .expect("foreign service user must produce a preview warning");
        let shared_issue = journal_service_user_issue(&directory.path, other_uid)
            .expect("shared journal ownership analysis")
            .expect("foreign service user must fail the shared gate");

        assert_eq!(issue, shared_issue);
        let apply_error = validate_apply_identity_for_uid(&directory.path, other_uid)
            .expect_err("authoritative apply rejects the same mismatch")
            .to_string();
        assert!(apply_error.contains(&shared_issue), "{apply_error}");

        let warnings_before = plan.warnings.len();
        append_apply_identity_preview_warning_for_credentials(
            &directory.path,
            &mut plan,
            Ok(credentials),
        );
        assert_eq!(plan.warnings.len(), warnings_before + 1);
        let warning = plan.warnings.last().expect("known-mismatch warning");
        assert!(
            warning.contains("apply ownership preflight warning"),
            "{warning}"
        );
        assert!(warning.contains(&shared_issue), "{warning}");
        assert!(warning.contains("authoritative apply"), "{warning}");
    }

    #[cfg(unix)]
    #[test]
    fn ownership_preview_emits_a_warning_when_analysis_is_unprovable() {
        use std::os::unix::fs::MetadataExt as _;

        let (directory, source_name, _, _) =
            rewrite_certificate_fixture("preview-unprovable-warning");
        let mut plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("read-only rewrite plan");
        let journal_metadata = std::fs::symlink_metadata(directory.path.join("journal.log"))
            .expect("journal metadata");
        let credentials = ApplyCredentials {
            effective_uid: journal_metadata.uid(),
            effective_gid: journal_metadata.gid(),
            group_ids: BTreeSet::from([journal_metadata.gid()]),
        };
        std::fs::rename(
            directory.path.join(&source_name),
            directory
                .path
                .join(format!("{source_name}.removed-after-plan")),
        )
        .expect("make the planned metadata source unavailable");
        assert!(
            preview_apply_identity_issue(&directory.path, &plan, &credentials).is_err(),
            "the fixture must exercise the analysis-error arm"
        );

        let warnings_before = plan.warnings.len();
        append_apply_identity_preview_warning_for_credentials(
            &directory.path,
            &mut plan,
            Ok(credentials),
        );

        assert_eq!(plan.warnings.len(), warnings_before + 1);
        let warning = plan.warnings.last().expect("unprovable-analysis warning");
        assert!(
            warning.contains("apply ownership could not be proved"),
            "{warning}"
        );
        assert!(
            warning.contains("authoritative apply will retry"),
            "{warning}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn metadata_preflight_models_inherited_gid_and_setgid_mode_conservatively() {
        const SYNTHETIC_NON_ROOT_UID: u32 = 42_424;
        const ARCHIVE_GROUP: u32 = 27_182;
        const UNRELATED_GROUP: u32 = 31_415;

        let credentials = ApplyCredentials {
            effective_uid: SYNTHETIC_NON_ROOT_UID,
            effective_gid: UNRELATED_GROUP,
            group_ids: BTreeSet::from([UNRELATED_GROUP]),
        };
        let possible_created_gids =
            possible_created_group_ids(ARCHIVE_GROUP, SETGID_MODE | 0o750, &credentials);
        let source_path = std::path::Path::new("data00000a.tar");
        let source_mode = 0o640;

        assert_eq!(
            possible_created_gids,
            BTreeSet::from([ARCHIVE_GROUP]),
            "a setgid directory fixes the staging-file group"
        );
        assert_eq!(
            metadata_source_apply_identity_issue(
                source_path,
                SYNTHETIC_NON_ROOT_UID,
                ARCHIVE_GROUP,
                source_mode,
                &possible_created_gids,
                &credentials,
            ),
            None,
            "an already inherited source gid needs neither fchown nor group membership when no setgid bit is requested"
        );

        let issue = metadata_source_apply_identity_issue(
            source_path,
            SYNTHETIC_NON_ROOT_UID,
            ARCHIVE_GROUP,
            source_mode | SETGID_MODE,
            &possible_created_gids,
            &credentials,
        )
        .expect("setgid preservation outside caller groups must refuse conservatively");
        assert!(issue.contains(&format!("gid {ARCHIVE_GROUP}")), "{issue}");
        assert!(issue.contains("setgid-mode"), "{issue}");
        assert!(issue.contains("cannot be guaranteed read-only"), "{issue}");
    }

    #[cfg(unix)]
    #[test]
    fn metadata_preflight_models_both_permitted_non_setgid_creation_groups() {
        const SYNTHETIC_NON_ROOT_UID: u32 = 42_424;
        const SOURCE_GROUP: u32 = 27_182;
        const EFFECTIVE_GROUP: u32 = 31_415;

        let credentials = ApplyCredentials {
            effective_uid: SYNTHETIC_NON_ROOT_UID,
            effective_gid: EFFECTIVE_GROUP,
            group_ids: BTreeSet::from([EFFECTIVE_GROUP]),
        };
        let possible_gids = possible_created_group_ids(SOURCE_GROUP, 0o750, &credentials);

        let issue = metadata_source_apply_identity_issue(
            std::path::Path::new("data00000a.tar"),
            SYNTHETIC_NON_ROOT_UID,
            SOURCE_GROUP,
            0o640,
            &possible_gids,
            &credentials,
        )
        .expect("a possible System V group outcome must be treated conservatively");

        assert_eq!(
            possible_gids,
            BTreeSet::from([SOURCE_GROUP, EFFECTIVE_GROUP])
        );
        assert!(
            issue.contains(&format!("may have gid {possible_gids:?}")),
            "the diagnostic must record both POSIX-permitted creation groups: {issue}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_metadata_preflight_checks_only_the_newest_possible_template() {
        let directory = TestDirectory::repository("checkpoint-template-prefix");
        std::fs::copy(
            directory.path.join("data00000a.tar"),
            directory.path.join("data00001a.tar"),
        )
        .expect("create a second readable archive number");

        let sources = planned_metadata_sources(&directory.path, false, None, true, false)
            .expect("derive checkpoint metadata sources");

        assert_eq!(sources, BTreeSet::from(["data00001a.tar".to_owned()]));
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_metadata_preflight_includes_the_leading_removal_outcome_prefix() {
        let directory = TestDirectory::repository("checkpoint-template-removal-prefix");
        let current_generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };
        let current_head = {
            let store = WritableRepository::open(&directory.path).expect("open head writer");
            let content_root = write_empty_node_segment(&store, current_generation);
            let mut writer = store.record_writer(current_generation);
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: content_root,
                    },
                    &[],
                )
                .expect("write current head");
            writer.finish().expect("finish current head segment");
            assert!(store.set_head(store.head(), head));
            store.close().expect("close head writer");
            head
        };
        let orphan = {
            let store = WritableRepository::open(&directory.path).expect("open orphan writer");
            let orphan = write_empty_node_segment(
                &store,
                GarbageCollectionGeneration {
                    generation: 0,
                    full_generation: 0,
                    is_compacted: false,
                },
            );
            store.close().expect("close unjournaled orphan writer");
            orphan
        };
        let repository = Repository::open(&directory.path).expect("open prefix fixture");
        let current_archive = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(current_head.segment))
            .expect("current head archive")
            .file_name()
            .to_owned();
        let orphan_archive = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(orphan.segment))
            .expect("newest orphan archive")
            .file_name()
            .to_owned();
        drop(repository);
        let plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("plan newest whole removal");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == &orphan_archive
        )));

        let sources = planned_metadata_sources(
            &directory.path,
            false,
            plan.segment_plan.as_ref(),
            true,
            false,
        )
        .expect("derive possible checkpoint templates");

        assert_eq!(
            sources,
            BTreeSet::from([orphan_archive, current_archive]),
            "a failed newest unlink uses that source; a successful unlink promotes only the next active archive"
        );
    }

    #[test]
    fn recovery_backup_task_requires_an_explicit_policy_without_writing() {
        let directory = TestDirectory::repository("backup-policy-required");
        let before = file_bytes(&directory.path);
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::RecoveryBackups]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("recovery backup cleanup without retention must be rejected");

        let crate::error::Error::InvalidFormat { details } = error else {
            panic!("unexpected options error: {error}");
        };
        assert_eq!(
            details,
            "recovery-backups requires an explicit age/count retention policy"
        );
        assert_eq!(file_bytes(&directory.path), before);
    }

    /// Overwrites an archive's index magic so it opens through the recovery
    /// scan: the payload entries stay readable, only the trailer stops
    /// validating. This is the shape a writer killed before closing its
    /// archive leaves behind, without needing a writer to kill.
    fn break_index_magic(path: &std::path::Path) {
        use std::io::{Seek as _, SeekFrom};

        let length = std::fs::metadata(path).expect("archive metadata").len();
        // Fully qualified: the module's `OpenOptions` import is Unix-only,
        // and this helper is not.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open archive for damage");
        file.seek(SeekFrom::Start(length - 1028))
            .expect("seek to the index magic");
        file.write_all(&[0xde, 0xad, 0xbe, 0xef])
            .expect("overwrite the index magic");
    }

    #[test]
    fn indexless_archive_refusal_counts_every_offender_and_states_ordinality() {
        const MAGIC: &str = "unrecognized index magic number 0x000115a9";
        const CRC: &str = "index checksum mismatch";
        let one = indexless_archive_refusal(43, 1, &[("data00042a.tar", MAGIC)], true, false);
        assert!(
            one.contains("1 of 43 active archive numbers has no index metadata (data00042a.tar)"),
            "singular subject names the archive and the total: {one}"
        );
        assert!(
            one.contains("the newest active archive is among them"),
            "a killed writer is distinguishable from damage: {one}"
        );
        assert!(
            one.contains(MAGIC),
            "the reason the index was rejected reaches the operator: {one}"
        );
        assert!(
            one.contains("no archive, journal, or checkpoint has been changed"),
            "the refusal states precisely what is untouched: {one}"
        );
        assert!(
            one.contains("froe archives") && one.contains(".bak"),
            "the refusal states what to run next and what repair costs: {one}"
        );

        let two = indexless_archive_refusal(
            3,
            2,
            &[("data00001a.tar", CRC), ("data00000a.tar", CRC)],
            false,
            false,
        );
        assert!(
            two.contains("2 of 3 active archive numbers have no index metadata"),
            "plural subject agrees: {two}"
        );
        assert_eq!(
            two.matches(CRC).count(),
            1,
            "one shared reason is stated once, not repeated per archive: {two}"
        );
        assert!(
            two.contains("the newest active archive is not among them"),
            "mid-store damage is not reported as a killed writer: {two}"
        );
        assert!(
            two.contains("inspect before repairing"),
            "mid-store damage does not get the unconditional repair advice: {two}"
        );
        assert!(
            one.contains("--repair-archive-indexes"),
            "a killed writer is pointed at the task that repairs it: {one}"
        );

        // An archive whose scan read nothing cannot be rebuilt, and the
        // write open refuses it. Advising a write command would be circular.
        let unrecoverable =
            indexless_archive_refusal(2, 1, &[("data00009a.tar", MAGIC)], true, true);
        assert!(
            !unrecoverable.contains("--repair-archive-indexes"),
            "an unrecoverable archive must not be sent to a command that will refuse it: \
             {unrecoverable}"
        );
        assert!(
            unrecoverable.contains("move that file aside"),
            "an unrecoverable archive gets the remedy that actually works: {unrecoverable}"
        );

        let many: Vec<String> = (0..8)
            .map(|index| format!("data0000{index}a.tar"))
            .collect();
        let borrowed: Vec<(&str, &str)> = many.iter().map(|n| (n.as_str(), MAGIC)).collect();
        let truncated = indexless_archive_refusal(40, 8, &borrowed, true, false);
        assert!(
            truncated.contains("8 of 40 active archive numbers"),
            "the count is the whole census, not the shown names: {truncated}"
        );
        assert!(
            truncated.contains("and 3 more"),
            "the omitted names are counted rather than silently dropped: {truncated}"
        );
    }

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

    /// The production refusal, end to end. Before the census this reported
    /// the first offending segment of the first offending archive and
    /// nothing else — no count, no ordinality, no remedy — and discarded
    /// the stale-archive warning the same run had already established.
    #[test]
    fn cleanup_refuses_an_index_less_active_archive_with_a_full_census() {
        let directory = TestDirectory::new("index-less-census");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));
        let before = file_bytes(&directory.path);

        let error = plan_compaction(&directory.path, &CompactionOptions::default())
            .expect_err("an index-less active archive must refuse generation cleanup");
        let crate::error::Error::InvalidFormat { details } = error else {
            panic!("unexpected refusal variant");
        };
        assert!(
            details
                .contains("1 of 1 active archive numbers has no index metadata (data00000a.tar)"),
            "the refusal names every offender and the total: {details}"
        );
        assert!(
            details.contains("the newest active archive is among them"),
            "the refusal states whether this is a killed writer: {details}"
        );
        assert!(
            details.contains("no archive, journal, or checkpoint has been changed"),
            "the refusal states precisely what is untouched: {details}"
        );
        assert!(
            details.contains("has no valid indexed generation"),
            "the stale-archive warning established before the refusal is carried out with it: {details}"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning a refusal changes no byte"
        );
    }

    /// The whole point of the task: a store a killed writer left behind is
    /// cleaned to a healthy shape by cleanup itself, in one run, instead of
    /// sending the operator to a different command as a workaround.
    #[test]
    fn repair_archives_heals_an_index_less_store_and_the_rest_then_plans() {
        let directory = TestDirectory::new("repair-heals");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));

        // Without the task, the default set still refuses and points at it.
        let refusal = plan_compaction(&directory.path, &CompactionOptions::default())
            .expect_err("the default set must still refuse");
        assert!(
            refusal.to_string().contains("--repair-archive-indexes"),
            "the refusal names the task that fixes it: {refusal}"
        );

        // With it, the preview names the repair and nothing else yet.
        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        let preview = plan_compaction(&directory.path, &options).expect("preview must not refuse");
        assert!(
            preview.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RepairArchiveIndex { file_name, .. }
                    if file_name == "data00000a.tar"
            )),
            "the preview names the repair: {:?}",
            preview.actions()
        );

        let outcome = PreparedCompaction::prepare(&directory.path, options)
            .expect("prepare repairs under the lock")
            .apply()
            .expect("apply");
        assert_eq!(outcome.repaired_archives, 1, "the rebuild is reported");
        assert!(
            directory.path.join("data00000a.tar.bak").exists(),
            "the original bytes are retained"
        );

        // Healthy: every archive indexed, and the default set now plans.
        let repository = Repository::open(&directory.path).expect("reopen");
        assert!(
            !repository
                .archives()
                .iter()
                .any(TarArchiveReader::is_recovered),
            "no archive is served through the recovery scan any more"
        );
        plan_compaction(&directory.path, &CompactionOptions::default())
            .expect("the default set plans cleanly against the healed store");
    }

    /// An archive number that cannot be rebuilt dooms the run however it is
    /// retried, so it is refused where nothing has been touched — not after
    /// paying a durable rewrite of every repairable archive to discover it.
    #[test]
    fn an_unrepairable_archive_refuses_before_anything_is_rewritten() {
        let directory = TestDirectory::new("repair-unrepairable");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            writer
                .write_string("forces a second archive")
                .expect("string");
            writer.finish().expect("finish");
            store.close().expect("close");
        }
        // A repairable archive, and — at a higher number, so the repair loop
        // would have reached it second — bytes no scan can recover.
        break_index_magic(&directory.path.join("data00000a.tar"));
        std::fs::write(directory.path.join("data00500a.tar"), vec![0x5au8; 4096])
            .expect("unrecoverable residue");
        let before = file_bytes(&directory.path);

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);

        // The read-only preview says so, before any authorization.
        let preview = plan_compaction(&directory.path, &options)
            .expect_err("the preview must refuse an unrepairable archive");
        assert!(
            preview.to_string().contains("data00500a.tar"),
            "the preview names the archive that dooms the run: {preview}"
        );

        // And a library caller skipping the preview pays no rewrite either.
        match PreparedCompaction::prepare(&directory.path, options) {
            Ok(_) => panic!("prepare must refuse too"),
            Err(error) => assert!(
                error.to_string().contains("data00500a.tar"),
                "prepare names it as well: {error}"
            ),
        }
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "and neither path rewrote a single archive"
        );
    }

    /// A repair that fails part-way through still happens — a staging residue
    /// is found per number, not by the survey — and reporting only the
    /// failure would leave the operator believing nothing moved.
    #[test]
    fn a_failed_repair_reports_the_rebuilds_it_already_completed() {
        let directory = TestDirectory::new("repair-partial");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // A second archive number, from an independent bootstrap so it shares
        // no segment identifier with the first — a copy would instead trip
        // the cross-number duplicate guard.
        let donor = TestDirectory::new("repair-partial-donor");
        {
            let store = WritableRepository::open(&donor.path).expect("donor bootstrap");
            store.close().expect("close");
        }
        std::fs::copy(
            donor.path.join("data00000a.tar"),
            directory.path.join("data00001a.tar"),
        )
        .expect("second archive number");
        // Both repairable; the higher number carries the residue of an
        // interrupted rebuild, which repair must refuse rather than clobber.
        break_index_magic(&directory.path.join("data00000a.tar"));
        break_index_magic(&directory.path.join("data00001a.tar"));
        std::fs::write(
            directory.path.join("data00001a.tar.recovering"),
            b"an interrupted rebuild",
        )
        .expect("staging residue");

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        let details = match PreparedCompaction::prepare(&directory.path, options) {
            Ok(_) => panic!("the staging residue must refuse the run"),
            Err(crate::error::Error::InvalidFormat { details }) => details,
            Err(other) => panic!("unexpected refusal variant: {other}"),
        };
        assert!(
            details.contains("data00001a.tar.recovering"),
            "the refusal names what stopped it: {details}"
        );
        assert!(
            details.contains("data00000a.tar"),
            "the refusal names what it already rebuilt: {details}"
        );
        assert!(
            details.contains("no second attempt"),
            "the refusal says the completed work need not be redone: {details}"
        );
        assert!(
            directory.path.join("data00000a.tar.bak").exists(),
            "and that rebuild really is durable"
        );
        assert!(
            directory.path.join("data00001a.tar.recovering").exists(),
            "the residue is left for the stale-temporaries task to adjudicate"
        );
    }

    /// Selecting the task is not the same as having work to do, and the
    /// difference used to be a one-way `store.version` 1→2 transition that
    /// appeared in no plan, was never confirmed, and survived cancelling.
    #[test]
    fn selecting_repair_with_nothing_to_repair_changes_no_byte() {
        let directory = TestDirectory::repository("repair-noop");
        std::fs::write(
            directory.path.join("manifest"),
            "#a version one store\nstore.version=1\n",
        )
        .expect("v1 manifest");
        // Something to do, so the run is not short-circuited as empty.
        std::fs::write(directory.path.join("journal.log.compacting"), b"").expect("temporary");
        let before = file_bytes(&directory.path);

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        let prepared =
            PreparedCompaction::prepare(&directory.path, options).expect("prepare must succeed");
        assert_eq!(
            prepared.repaired_archives(),
            0,
            "there was nothing to repair"
        );
        assert_eq!(
            crate::store::read_manifest_store_version(&directory.path.join("manifest"))
                .expect("read manifest"),
            1,
            "the manifest must not be upgraded when no repair happens"
        );
        drop(prepared);
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "preparing a repair run with nothing to repair changes no byte"
        );
    }

    /// The repair used to run before any store-version gate, so froe would
    /// rewrite the archives of a store it then declared itself unable to
    /// read. The library API reaches `prepare` without the lockless preview
    /// that would otherwise have refused.
    #[test]
    fn a_store_from_a_newer_oak_is_refused_before_any_repair() {
        let directory = TestDirectory::new("repair-newer-store");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));
        std::fs::write(
            directory.path.join("manifest"),
            "#from a newer Oak\nstore.version=3\n",
        )
        .expect("v3 manifest");
        let before = file_bytes(&directory.path);

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        match PreparedCompaction::prepare(&directory.path, options) {
            Ok(_) => panic!("a store version this reader does not support must be refused"),
            Err(error) => assert!(
                error.to_string().contains("newer"),
                "the refusal names the store version: {error}"
            ),
        }
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "and it is refused before a single archive is rewritten"
        );
    }

    /// Every index-dependent gate evaluates the store for the first time
    /// after the repair, so a refusal there is ordinary — and must not claim
    /// the store is as the operator left it.
    #[test]
    fn a_refusal_after_a_repair_says_the_repair_already_happened() {
        let directory = TestDirectory::new("repair-then-refuse");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // A second number sharing every segment with the first: a real,
        // unrepairable defect that only the post-repair gates can see.
        std::fs::copy(
            directory.path.join("data00000a.tar"),
            directory.path.join("data00001a.tar"),
        )
        .expect("duplicate archive number");
        break_index_magic(&directory.path.join("data00000a.tar"));

        // The preview must state the unfitness itself, before authorizing.
        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        let preview = plan_compaction(&directory.path, &options)
            .expect_err("the cross-number duplicate must refuse in the read-only preview");
        assert!(
            preview.to_string().contains("occurs in active archives"),
            "the dry run reports the real reason: {preview}"
        );
    }

    /// The one-way v1 to v2 transition is charged when a rebuilt archive is
    /// about to become visible, not when one is merely predicted. A repair
    /// can still fail per archive for reasons no survey models, and paying an
    /// irreversible format change for a run that rebuilds nothing would leave
    /// the store damaged AND closed to an older Oak.
    #[test]
    fn a_repair_that_installs_nothing_does_not_upgrade_the_manifest() {
        let directory = TestDirectory::new("repair-v1-no-install");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));
        // Repairable by the survey, but the rebuild refuses: a staging
        // residue is found per number, after the survey has had its say.
        std::fs::write(
            directory.path.join("data00000a.tar.recovering"),
            b"an interrupted rebuild",
        )
        .expect("staging residue");
        std::fs::write(
            directory.path.join("manifest"),
            "#a version one store\nstore.version=1\n",
        )
        .expect("v1 manifest");
        let before = file_bytes(&directory.path);

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        assert!(
            PreparedCompaction::prepare(&directory.path, options).is_err(),
            "the residue must refuse the run"
        );
        assert_eq!(
            crate::store::read_manifest_store_version(&directory.path.join("manifest"))
                .expect("read manifest"),
            1,
            "a run that installed no rebuilt archive must not have raised the store version"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "and nothing else moved either"
        );
    }

    /// A rebuild ends by matching the replaced archive's ownership, which
    /// fails for a foreign-owned file. Discovering that after the rewrite
    /// means no rerun ever converges, so it is a stat(2) check up front.
    #[cfg(unix)]
    #[test]
    fn a_repair_target_this_process_cannot_match_refuses_before_rewriting() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = TestDirectory::new("repair-foreign-owner");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));
        let owner = std::fs::symlink_metadata(directory.path.join("data00000a.tar"))
            .expect("archive metadata")
            .uid();
        // Only meaningful where the archive is not already ours; as root
        // every chown succeeds and there is nothing to refuse.
        if owner != unsafe { libc::geteuid() } || owner == 0 {
            return;
        }
        let targets = crate::writer::store_writer::repair_target_names(&directory.path)
            .expect("survey the repair targets");
        assert_eq!(
            targets,
            vec!["data00000a.tar".to_owned()],
            "the preflight inspects the file the rebuild would replace"
        );
    }

    /// Repair makes the `.bak` that the backup policy retires. Doing both in
    /// one run can delete the only copy of what the rebuild could not read.
    #[test]
    fn repair_archives_and_recovery_backups_cannot_run_together() {
        let options = CompactionOptions::default()
            .with_task(MaintenanceTask::RepairArchives)
            .with_recovery_backup_policy(RecoveryBackupPolicy::new(Duration::ZERO, 0));
        let directory = TestDirectory::repository("repair-and-backups");
        let error = plan_compaction(&directory.path, &options)
            .expect_err("the combination must be refused before anything is read");
        let crate::error::Error::InvalidFormat { details } = error else {
            panic!("unexpected refusal variant");
        };
        assert!(
            details.contains("cannot run together"),
            "the refusal explains the conflict: {details}"
        );
        assert!(
            details.contains("repair first"),
            "the refusal states the safe sequence: {details}"
        );
    }

    #[test]
    fn empty_directory_is_refused_without_bootstrapping_anything() {
        let directory = TestDirectory::new("empty");
        let error = plan_compaction(&directory.path, &CompactionOptions::default())
            .expect_err("an empty directory is not a repository");
        let crate::error::Error::InvalidFormat { details } = error else {
            panic!("unexpected repository-shape error: {error}");
        };
        assert_eq!(
            details,
            format!(
                "{} is not an existing segment-tar repository (manifest and journal.log are required)",
                canonical_fixture_directory(&directory.path).display()
            )
        );
        assert!(file_bytes(&directory.path).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn managed_symlink_is_rejected_without_following_its_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::repository("managed-symlink");
        let victim = directory.path.join("victim");
        std::fs::write(&victim, b"do not touch").expect("victim");
        let staging = directory.path.join("journal.log.compacting");
        symlink("victim", &staging).expect("staging symlink");
        let before = file_bytes(&directory.path);

        let error = plan_compaction(&directory.path, &CompactionOptions::default())
            .expect_err("managed symlink must be rejected");
        let crate::error::Error::InvalidFormat { details } = error else {
            panic!("unexpected managed-file-type error: {error}");
        };
        assert_eq!(
            details,
            format!(
                "managed repository path {} is not a regular file",
                canonical_fixture_directory(&directory.path)
                    .join("journal.log.compacting")
                    .display()
            )
        );
        assert_eq!(file_bytes(&directory.path), before);
        assert_eq!(std::fs::read(victim).expect("victim"), b"do not touch");
    }

    #[cfg(unix)]
    #[test]
    fn repository_root_symlink_is_resolved_to_the_canonical_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::repository("root-symlink-target");
        let link = directory.path.with_extension("repository-link");
        let _ = std::fs::remove_file(&link);
        symlink(&directory.path, &link).expect("create repository root symlink");

        let expected = std::fs::canonicalize(&directory.path).expect("canonical target");
        let plan =
            plan_compaction(&link, &CompactionOptions::default()).expect("plan through alias");
        assert_eq!(plan.directory(), expected);
        let prepared = PreparedCompaction::prepare(
            &link,
            CompactionOptions::default().with_tasks(std::iter::empty()),
        )
        .expect("prepare through alias");
        assert_eq!(prepared.plan().directory(), expected);
        drop(prepared);
        std::fs::remove_file(link).expect("remove repository link");
    }

    #[cfg(unix)]
    #[test]
    fn prepared_cleanup_is_bound_to_an_ancestor_symlinks_resolved_target() {
        use std::os::unix::fs::symlink;

        let first_parent = TestDirectory::new("ancestor-alias-first");
        let second_parent = TestDirectory::new("ancestor-alias-second");
        let first_repository = first_parent.path.join("segmentstore");
        let second_repository = second_parent.path.join("segmentstore");
        for repository in [&first_repository, &second_repository] {
            std::fs::create_dir(repository).expect("create repository directory");
            WritableRepository::open(repository)
                .expect("bootstrap repository")
                .close()
                .expect("close repository");
            std::fs::copy(
                repository.join("journal.log"),
                repository.join("journal.log.compacting"),
            )
            .expect("create removable staging file");
        }
        let alias = first_parent.path.with_extension("ancestor-alias");
        let _ = std::fs::remove_file(&alias);
        symlink(&first_parent.path, &alias).expect("create ancestor alias");
        let aliased_repository = alias.join("segmentstore");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);

        let prepared = PreparedCompaction::prepare(&aliased_repository, options)
            .expect("prepare first target");
        assert_eq!(
            prepared.plan().directory(),
            std::fs::canonicalize(&first_repository).expect("canonical first repository")
        );
        std::fs::remove_file(&alias).expect("remove first alias");
        symlink(&second_parent.path, &alias).expect("retarget ancestor alias");

        prepared.apply().expect("apply captured first target");
        assert!(!first_repository.join("journal.log.compacting").exists());
        assert!(second_repository.join("journal.log.compacting").exists());
        Repository::open(&first_repository).expect("first repository remains healthy");
        Repository::open(&second_repository).expect("second repository remains healthy");
        std::fs::remove_file(alias).expect("remove ancestor alias");
    }

    #[cfg(unix)]
    #[test]
    fn relative_repository_path_is_stored_as_an_absolute_canonical_target() {
        let directory = TestDirectory::repository("relative-canonical-target");
        let current = std::fs::canonicalize(std::env::current_dir().expect("current directory"))
            .expect("canonical current directory");
        let target = std::fs::canonicalize(&directory.path).expect("canonical repository");
        let relative = relative_path_from(&current, &target);
        assert!(!relative.is_absolute());

        let plan =
            plan_compaction(&relative, &CompactionOptions::default()).expect("relative plan");

        assert_eq!(plan.directory(), target);
        assert!(plan.directory().is_absolute());
    }

    #[test]
    fn dangling_journal_line_is_pruned_with_backup_and_archives_untouched() {
        let directory = TestDirectory::repository("dangling-journal");
        let missing = SegmentIdentifier::new(7, 0xA000_0000_0000_0007);
        let journal_path = directory.path.join("journal.log");
        let retained_journal = std::fs::read(&journal_path).expect("read retained journal");
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open journal");
        writeln!(journal, "{missing}:0 root 123").expect("append dangling line");
        drop(journal);
        let archive_before =
            std::fs::read(directory.path.join("data00000a.tar")).expect("read archive");
        std::fs::write(
            directory.path.join("manifest"),
            b"custom.property=untouched\nstore.version=1\n",
        )
        .expect("version-one manifest");
        let manifest_before = std::fs::read(directory.path.join("manifest")).expect("manifest");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Journal]);

        let plan = plan_compaction(&directory.path, &options).expect("plan");
        assert_eq!(plan.tasks(), &[MaintenanceTask::Journal]);
        assert_eq!(plan.journal_line_removals().len(), 1);
        let removal = &plan.journal_line_removals()[0];
        assert_eq!(
            removal.record_identifier().map(|record| record.segment),
            Some(missing)
        );
        assert_eq!(removal.reason(), JournalRemovalReason::MissingSegment);
        assert!(
            removal
                .preview_bytes()
                .starts_with(missing.to_string().as_bytes())
        );
        assert!(!removal.preview_truncated());
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PruneJournal {
                missing_segments: 1,
                ..
            }
        )));
        let outcome = compact(&directory.path, options).expect("apply");

        assert_eq!(outcome.removed_journal_lines, 1);
        let expected_backup =
            canonical_fixture_directory(&directory.path).join("journal.log.bak.000");
        assert_eq!(
            outcome.journal_backup_path(),
            Some(expected_backup.as_path())
        );
        assert!(outcome.is_complete());
        assert!(directory.path.join("journal.log.bak.000").is_file());
        assert!(
            !std::fs::read_to_string(&journal_path)
                .expect("journal")
                .contains(&missing.to_string())
        );
        assert_eq!(
            std::fs::read(&journal_path).expect("rewritten journal"),
            retained_journal,
            "the retained physical journal line must be byte-exact"
        );
        assert_eq!(
            std::fs::read(directory.path.join("data00000a.tar")).expect("archive"),
            archive_before
        );
        assert_eq!(
            std::fs::read(directory.path.join("manifest")).expect("manifest"),
            manifest_before
        );
        Repository::open(&directory.path).expect("healthy repository");
    }

    #[test]
    fn journal_cleanup_preserves_mixed_physical_terminators_exactly() {
        let directory = TestDirectory::repository("mixed-journal-terminators");
        let head = Repository::open(&directory.path)
            .expect("repository")
            .head_record_identifier();
        let retained = format!("{head} tag-lf 1\n{head} tag-crlf 2\r\n{head} tag-cr 3\r");
        let source = format!("{retained}parser-skipped\n");
        std::fs::write(directory.path.join("journal.log"), source.as_bytes())
            .expect("write mixed journal fixture");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Journal]);

        let plan = plan_compaction(&directory.path, &options).expect("plan mixed cleanup");
        assert_eq!(plan.journal_line_removals().len(), 1);
        assert_eq!(plan.journal_line_removals()[0].line_number(), 4);
        compact(&directory.path, options).expect("apply mixed cleanup");

        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("read rewritten journal"),
            retained.as_bytes(),
            "LF, CRLF, and bare-CR terminators must remain byte-exact"
        );
        Repository::open(&directory.path).expect("mixed-terminator repository remains healthy");
    }

    #[test]
    fn exhausted_journal_replacement_namespace_fails_during_read_only_planning() {
        let directory = TestDirectory::repository("journal-namespace-exhausted");
        let missing = SegmentIdentifier::new(17, 0xA000_0000_0000_0017);
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(directory.path.join("journal.log"))
                .expect("open journal"),
            "{missing}:0 root 123"
        )
        .expect("append dangling line");
        for counter in 0..1000u16 {
            std::fs::write(
                directory.path.join(format!("journal.log.bak.{counter:03}")),
                [],
            )
            .expect("occupy backup name");
        }
        let before = file_bytes(&directory.path);
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Journal]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("planning must discover exhausted backup names before apply");
        assert!(error.to_string().contains("journal.log.bak"));
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning remains read-only"
        );
        Repository::open(&directory.path).expect("repository remains healthy");
    }

    #[test]
    fn corrupt_record_in_the_selected_head_segment_never_rolls_back_silently() {
        let directory = TestDirectory::repository("corrupt-current-record");
        let head = Repository::open(&directory.path)
            .expect("repository")
            .head_record_identifier();
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(directory.path.join("journal.log"))
            .expect("open journal");
        writeln!(journal, "{}:2147483647 root 123", head.segment)
            .expect("append corrupt current revision");
        drop(journal);
        let before = file_bytes(&directory.path);

        let error = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Journal]),
        )
        .expect_err("the exact selected head record is corrupt");

        assert!(error.to_string().contains("current journal head"));
        assert_eq!(file_bytes(&directory.path), before);
    }

    #[test]
    fn checkpoint_without_a_snapshot_root_fails_the_health_gate() {
        let directory = TestDirectory::repository("malformed-checkpoint");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        let content_root = store
            .head_node()
            .child_node("root")
            .expect("read root")
            .expect("root exists")
            .record_identifier();
        let mut writer = store.record_writer(store.writing_generation().expect("generation"));
        let malformed_checkpoint = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("malformed checkpoint");
        let checkpoints = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "broken".to_owned(),
                    node: malformed_checkpoint,
                },
                &[],
            )
            .expect("checkpoint container");
        let malformed_head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("checkpoints".to_owned(), checkpoints),
                    ("root".to_owned(), content_root),
                ]),
                &[],
            )
            .expect("malformed super-root");
        writer.finish().expect("finish");
        assert!(store.set_head(store.head(), malformed_head));
        store.close().expect("close");

        let result = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([]),
        );
        assert!(
            result.is_err(),
            "cleanup must not bless a checkpoint without its snapshot root"
        );
    }

    #[test]
    fn valid_newer_archive_generation_makes_the_lower_letter_stale() {
        let directory = TestDirectory::repository("stale-letter");
        std::fs::copy(
            directory.path.join("data00000a.tar"),
            directory.path.join("data00000b.tar"),
        )
        .expect("copy archive generation");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
        let plan = plan_compaction(&directory.path, &options).expect("plan");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveStaleArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )));

        compact(&directory.path, options).expect("cleanup");
        assert!(!directory.path.join("data00000a.tar").exists());
        assert!(directory.path.join("data00000b.tar").exists());
        Repository::open(&directory.path).expect("healthy repository");
    }

    #[test]
    fn stale_archive_cleanup_preserves_alternates_when_active_trailers_are_invalid() {
        for (name, magic) in [
            ("invalid-graph", 0x0A30_470Au32.to_be_bytes()),
            ("invalid-brf", 0x0A31_420Au32.to_be_bytes()),
        ] {
            let directory = TestDirectory::repository(name);
            let newer = directory.path.join("data00000b.tar");
            std::fs::copy(directory.path.join("data00000a.tar"), &newer)
                .expect("copy newer archive generation");
            corrupt_first_magic(&newer, magic);

            let selected = TarArchiveReader::open(&newer).expect("index remains valid");
            assert!(!selected.is_recovered());
            assert!(selected.segment_graph().is_none() || selected.binary_references().is_none());
            let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
            let plan = plan_compaction(&directory.path, &options).expect("plan");

            assert!(!plan.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RemoveStaleArchive { file_name, .. }
                    if file_name == "data00000a.tar"
            )));
            assert!(
                plan.warnings()
                    .iter()
                    .any(|warning| warning.contains("incomplete recovery metadata"))
            );
            assert!(directory.path.join("data00000a.tar").exists());
            assert!(newer.exists());
        }
    }

    #[test]
    fn stale_archive_cleanup_reconstructs_semantic_graph_and_brf_before_deletion() {
        for (name, write_metadata_record) in [("omitted-graph", 0u8), ("omitted-brf", 1u8)] {
            let directory = TestDirectory::repository(name);
            let store = WritableRepository::open(&directory.path).expect("open writer");
            let mut writer = store.record_writer(store.writing_generation().expect("generation"));
            match write_metadata_record {
                0 => {
                    writer
                        .write_string(&"graph-block".repeat(40_000))
                        .expect("long string with bulk references");
                }
                _ => {
                    writer
                        .write_external_binary_identifier("external-blob-that-must-survive")
                        .expect("external blob identifier");
                }
            }
            writer.finish().expect("finish metadata segment");
            store.close().expect("close writer");
            let source = directory.path.join("data00001a.tar");
            assert!(source.exists());
            let source_reader = TarArchiveReader::open(&source).expect("source reader");
            if write_metadata_record == 0 {
                assert!(
                    source_reader
                        .segment_graph()
                        .is_some_and(|graph| !graph.adjacency.is_empty())
                );
            } else {
                assert!(source_reader.binary_references().is_some_and(|catalog| {
                    catalog
                        .generations
                        .iter()
                        .any(|generation| !generation.segments.is_empty())
                }));
            }
            repack_without_graph_or_brf(&directory.path, "data00001a.tar", "data00001b.tar");
            let repacked = TarArchiveReader::open(&directory.path.join("data00001b.tar"))
                .expect("repacked reader");
            assert!(repacked.segment_graph().is_some());
            assert!(repacked.binary_references().is_some());

            let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
            let plan = plan_compaction(&directory.path, &options).expect("plan");

            assert!(!plan.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RemoveStaleArchive { file_name, .. }
                    if file_name == "data00001a.tar"
            )));
            assert!(
                plan.warnings()
                    .iter()
                    .any(|warning| warning.contains("incomplete recovery metadata"))
            );
            assert!(source.exists());
        }
    }

    #[test]
    fn foreign_tar_and_unknown_files_are_never_cleanup_targets() {
        let directory = TestDirectory::repository("foreign-files");
        std::fs::write(directory.path.join("notes.tar"), b"foreign tar").expect("foreign tar");
        std::fs::write(directory.path.join("operator-notes.txt"), b"keep me").expect("notes");

        let plan = plan_compaction(&directory.path, &CompactionOptions::default()).expect("plan");
        assert!(plan.is_empty());
        assert_eq!(
            std::fs::read(directory.path.join("notes.tar")).expect("foreign tar"),
            b"foreign tar"
        );
        assert_eq!(
            std::fs::read(directory.path.join("operator-notes.txt")).expect("notes"),
            b"keep me"
        );
    }

    #[test]
    fn nonempty_archive_without_a_valid_index_is_preserved_for_recovery() {
        let directory = TestDirectory::repository("archive-needs-recovery");
        let damaged = directory.path.join("data00001a.tar");
        std::fs::write(&damaged, b"not a complete tar archive").expect("damaged archive");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);

        let plan = plan_compaction(&directory.path, &options).expect("plan");

        assert!(plan.actions().is_empty());
        assert!(
            plan.warnings()
                .iter()
                .any(|warning| warning.contains("no valid indexed generation"))
        );
        assert_eq!(
            std::fs::read(&damaged).expect("damaged bytes"),
            b"not a complete tar archive"
        );
    }

    #[test]
    fn prepared_plan_rejects_same_length_inode_replacement() {
        let directory = TestDirectory::repository("stale-plan");
        let options = CompactionOptions::default().with_tasks([]);
        let prepared = PreparedCompaction::prepare(&directory.path, options).expect("prepare");
        let journal_path = directory.path.join("journal.log");
        let bytes = std::fs::read(&journal_path).expect("journal");
        let replacement = directory.path.join("replacement");
        std::fs::write(&replacement, &bytes).expect("write replacement");
        std::fs::rename(&replacement, &journal_path).expect("replace same-size journal");

        assert!(prepared.apply().is_err());
        Repository::open(&directory.path).expect("replacement bytes remain healthy");
    }

    #[test]
    fn deferred_removal_rechecks_the_exact_planned_file_identity() {
        let directory = TestDirectory::new("deferred-removal-identity");
        let removable_name = "journal.log.bak.998";
        let removable_path = directory.path.join(removable_name);
        std::fs::write(&removable_path, b"independent old recovery copy")
            .expect("write independently removable backup");
        let removable_metadata =
            std::fs::symlink_metadata(&removable_path).expect("removable metadata");
        let removable = PlannedFileRemoval {
            file_name: removable_name.to_owned(),
            bytes: removable_metadata.len(),
            fingerprint: file_fingerprint(
                std::ffi::OsString::from(removable_name),
                &removable_metadata,
            ),
        };
        let name = "journal.log.bak.999";
        let path = directory.path.join(name);
        std::fs::write(&path, b"first recovery copy").expect("write planned backup");
        let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
        let planned = PlannedFileRemoval {
            file_name: name.to_owned(),
            bytes: metadata.len(),
            fingerprint: file_fingerprint(std::ffi::OsString::from(name), &metadata),
        };
        std::fs::remove_file(&path).expect("remove original backup");
        std::fs::write(&path, b"new recovery material").expect("replace backup");

        let (removed, failures) = remove_planned_files(
            &directory.path,
            [removable, planned],
            PlannedFileRemovalFailureMode::Partial,
            &mut crate::progress::DiscardedProgress,
        )
        .expect("late deletion refusals are a partial outcome");

        assert_eq!(removed, 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].file_name(), name);
        assert!(failures[0].error().contains("changed after"));
        assert!(
            !removable_path.exists(),
            "a late identity refusal must not discard an earlier successful deletion"
        );
        assert_eq!(
            std::fs::read(&path).expect("replacement remains"),
            b"new recovery material"
        );
    }

    #[test]
    fn deletion_absence_state_does_not_depend_on_diagnostic_text() {
        let retained = super::FileDeletionFailure::retained(
            "data00000a.tar".to_owned(),
            super::ALREADY_ABSENT_DELETION_DETAIL,
        );
        let absent = super::FileDeletionFailure::already_absent(
            "data00001a.tar".to_owned(),
            "a deliberately different ENOENT diagnostic",
        );

        assert!(!retained.target_was_already_absent());
        assert!(absent.target_was_already_absent());
    }

    #[test]
    fn strict_stale_removal_reports_an_already_absent_file_without_losing_the_outcome() {
        let directory = TestDirectory::new("strict-removal-already-absent");
        let name = "data00001a.tar";
        let path = directory.path.join(name);
        std::fs::write(&path, b"certified stale archive").expect("write planned source");
        let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
        let planned = PlannedFileRemoval {
            file_name: name.to_owned(),
            bytes: metadata.len(),
            fingerprint: file_fingerprint(OsString::from(name), &metadata),
        };
        std::fs::remove_file(path).expect("another actor won the unlink race");

        let (removed, failures) = remove_planned_files(
            &directory.path,
            [planned],
            PlannedFileRemovalFailureMode::RequireCertifiedTarget,
            &mut crate::progress::DiscardedProgress,
        )
        .expect("absence is a reportable partial result, not a lost cleanup outcome");

        assert_eq!(removed, 0);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].file_name(), name);
        assert!(failures[0].target_was_already_absent());
        assert_eq!(
            failures[0].error(),
            "file was already absent when deletion was attempted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn strict_stale_removal_treats_disappearance_at_each_recertification_as_partial() {
        for disappearance_before_verification in 0..=1 {
            let directory = TestDirectory::new(&format!(
                "strict-removal-post-open-absence-{disappearance_before_verification}"
            ));
            let name = "data00001a.tar";
            let path = directory.path.join(name);
            std::fs::write(&path, b"certified stale archive").expect("write planned source");
            let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
            let planned = PlannedFileRemoval {
                file_name: name.to_owned(),
                bytes: metadata.len(),
                fingerprint: file_fingerprint(OsString::from(name), &metadata),
            };

            let (removed, failures) = remove_planned_files_with_after_open(
                &directory.path,
                [planned],
                PlannedFileRemovalFailureMode::RequireCertifiedTarget,
                |path, verification| {
                    if verification == disappearance_before_verification {
                        std::fs::remove_file(path)
                            .expect("another actor removes the held pathname");
                    } else if disappearance_before_verification == 0 && verification == 1 {
                        // Reached only if the first production recertification
                        // has been removed or neutralized. Make the second one
                        // reject instead of accidentally preserving the same
                        // partial outcome, so each call remains load-bearing.
                        std::fs::write(path, b"replacement recovery material")
                            .expect("install a replacement pathname");
                    }
                },
                |_| panic!("an absent pathname must never reach unlink"),
            )
            .expect("strict mode keeps an already achieved deletion as partial");

            assert_eq!(removed, 0);
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].file_name(), name);
            assert!(failures[0].target_was_already_absent());
            assert_eq!(
                failures[0].error(),
                "file was already absent when deletion was attempted"
            );
        }
    }

    #[test]
    fn strict_stale_removal_reports_an_exact_source_unlink_error_as_partial() {
        let directory = TestDirectory::new("strict-removal-unlink-error");
        let name = "data00001a.tar";
        let path = directory.path.join(name);
        std::fs::write(&path, b"certified stale archive").expect("write planned source");
        let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
        let planned = PlannedFileRemoval {
            file_name: name.to_owned(),
            bytes: metadata.len(),
            fingerprint: file_fingerprint(OsString::from(name), &metadata),
        };

        let (removed, failures) = remove_planned_files_with(
            &directory.path,
            [planned],
            PlannedFileRemovalFailureMode::RequireCertifiedTarget,
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected exact-source unlink refusal",
                ))
            },
        )
        .expect("a certified source left in place is a reportable partial result");

        assert_eq!(removed, 0);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].file_name(), name);
        assert!(!failures[0].target_was_already_absent());
        assert_eq!(failures[0].error(), "injected exact-source unlink refusal");
        assert_eq!(
            std::fs::read(path).expect("certified source remains"),
            b"certified stale archive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deferred_removal_reports_a_non_not_found_open_error_as_partial() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("deferred-removal-open-error");
        let name = "journal.log.bak.999";
        let path = directory.path.join(name);
        std::fs::write(&path, b"planned recovery copy").expect("write planned backup");
        let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
        let planned = PlannedFileRemoval {
            file_name: name.to_owned(),
            bytes: metadata.len(),
            fingerprint: file_fingerprint(std::ffi::OsString::from(name), &metadata),
        };
        let victim = directory.path.join("recovery-evidence");
        std::fs::write(&victim, b"do not follow").expect("write symlink target");
        std::fs::remove_file(&path).expect("remove planned inode");
        symlink("recovery-evidence", &path).expect("install non-followable replacement");
        let expected_open_error = {
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut options = OpenOptions::new();
            options.read(true).custom_flags(libc::O_NOFOLLOW);
            let error = options
                .open(&path)
                .expect_err("O_NOFOLLOW must reject the substituted symlink");
            assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
            error.to_string()
        };

        let (removed, failures) = remove_planned_files(
            &directory.path,
            [planned],
            PlannedFileRemovalFailureMode::Partial,
            &mut crate::progress::DiscardedProgress,
        )
        .expect("late open refusal is a partial outcome");

        assert_eq!(removed, 0);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].file_name(), name);
        assert_eq!(failures[0].error(), expected_open_error);
        assert_eq!(
            std::fs::read(&victim).expect("symlink target remains"),
            b"do not follow"
        );
        assert!(path.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn prepared_plan_rejects_in_place_change_with_restored_mtime() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;

        let directory = TestDirectory::repository("stale-plan-ctime");
        let staging = directory.path.join("journal.log.compacting");
        std::fs::copy(directory.path.join("journal.log"), &staging)
            .expect("create redundant staging journal");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);
        let prepared = PreparedCompaction::prepare(&directory.path, options).expect("prepare");
        let metadata = std::fs::metadata(&staging).expect("staging metadata");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let changed = vec![b'x'; metadata.len() as usize];
        std::fs::write(&staging, changed).expect("same-inode same-size overwrite");
        let path = CString::new(staging.as_os_str().as_bytes()).expect("path without NUL");
        let times = [
            libc::timespec {
                tv_sec: checked_timespec_field(metadata.atime()),
                tv_nsec: checked_timespec_field(metadata.atime_nsec()),
            },
            libc::timespec {
                tv_sec: checked_timespec_field(metadata.mtime()),
                tv_nsec: checked_timespec_field(metadata.mtime_nsec()),
            },
        ];
        // SAFETY: the path is NUL-terminated and `times` contains two valid
        // timespec values copied from stat(2).
        let result = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(result, 0, "restore fixture mtime");

        assert!(prepared.apply().is_err());
        assert!(
            staging.exists(),
            "stale proof must not delete changed evidence"
        );
        Repository::open(&directory.path).expect("repository remains healthy");
    }

    #[test]
    fn prepared_cleanup_is_excluded_by_an_existing_writer_lock() {
        let directory = TestDirectory::repository("lock-exclusion");
        let writer = WritableRepository::open(&directory.path).expect("hold writer lock");
        plan_compaction(&directory.path, &CompactionOptions::default())
            .expect("lock-free preview remains read-only");

        assert!(
            PreparedCompaction::prepare(&directory.path, CompactionOptions::default()).is_err()
        );
        writer.close().expect("close writer");
        Repository::open(&directory.path).expect("repository healthy");
    }

    #[test]
    fn prepared_cleanup_refuses_a_replaced_lock_inode() {
        let directory = TestDirectory::repository("replaced-lock");
        let staging = directory.path.join("journal.log.compacting");
        std::fs::copy(directory.path.join("journal.log"), &staging)
            .expect("create removable staging file");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);
        let prepared = PreparedCompaction::prepare(&directory.path, options).expect("prepare");
        let lock_path = directory.path.join("repo.lock");
        std::fs::remove_file(&lock_path).expect("unlink held lock pathname");
        std::fs::write(&lock_path, b"replacement inode").expect("replace lock inode");

        let error = prepared
            .apply()
            .expect_err("replacement lock must abort apply");

        assert!(error.to_string().contains("lock inode"));
        assert!(staging.exists());
        Repository::open(&directory.path).expect("repository remains healthy");
    }

    #[test]
    fn recovery_backup_target_accepts_only_exact_oak_and_journal_counter_forms() {
        for (name, target) in [
            ("journal.log.bak.000", "journal.log"),
            ("journal.log.bak.999", "journal.log"),
            ("data00000a.tar.bak", "data00000a.tar"),
            ("data00000a.tar.2.bak", "data00000a.tar"),
            ("data00000a.tar.2147483647.bak", "data00000a.tar"),
            ("data00000a.tar.ro.bak", "data00000a.tar"),
            ("data00000a.tar.2.ro.bak", "data00000a.tar"),
        ] {
            assert_eq!(
                recovery_backup_target(name).as_deref(),
                Some(target),
                "{name}"
            );
        }

        for hostile in [
            "journal.log.bak.",
            "journal.log.bak.00",
            "journal.log.bak.0000",
            "journal.log.bak.+00",
            "data00000a.tar.0.bak",
            "data00000a.tar.1.bak",
            "data00000a.tar.02.bak",
            "data00000a.tar.007.ro.bak",
            "data00000a.tar.2147483648.bak",
            "data00000a.tar.-2.ro.bak",
            "data00000a.tar..2.ro.bak",
            "data00000a.tar.2.ro.bak.extra",
        ] {
            assert_eq!(recovery_backup_target(hostile), None, "{hostile}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_backup_like_name_is_not_promoted_into_the_managed_allowlist() {
        use std::os::unix::ffi::OsStrExt as _;

        let hostile = std::ffi::OsStr::from_bytes(b"data00000a.tar.\xff.ro.bak");
        assert!(!super::is_managed_name(hostile));
    }

    #[test]
    fn non_regular_numbered_read_only_backup_is_refused_even_during_preview() {
        let directory = TestDirectory::repository("non-regular-numbered-ro-backup");
        let backup = directory.path.join("data00000a.tar.2.ro.bak");
        std::fs::create_dir(&backup).expect("create hostile managed-name directory");

        let error = plan_compaction(&directory.path, &CompactionOptions::default())
            .expect_err("managed backup names must remain regular files in dry-run");

        assert_eq!(
            error.to_string(),
            format!(
                "invalid segment-tar data: managed repository path {} is not a regular file",
                canonical_fixture_directory(&directory.path)
                    .join("data00000a.tar.2.ro.bak")
                    .display()
            )
        );
    }

    #[test]
    fn all_archive_backup_spellings_share_one_keep_latest_slot() {
        let directory = TestDirectory::repository("archive-backup-shared-retention-slot");
        let now = SystemTime::now();
        for (name, age_hours) in [
            ("data00000a.tar.ro.bak", 1),
            ("data00000a.tar.2.ro.bak", 2),
            ("data00000a.tar.bak", 3),
            ("data00000a.tar.2.bak", 4),
        ] {
            let file = File::create(directory.path.join(name)).expect("create recovery backup");
            file.set_times(
                std::fs::FileTimes::new().set_modified(now - Duration::from_secs(age_hours * 3600)),
            )
            .expect("set distinct backup time");
        }
        let options = CompactionOptions::default()
            .with_tasks([])
            .with_recovery_backup_policy(RecoveryBackupPolicy::new(Duration::ZERO, 1));

        let plan = plan_compaction(&directory.path, &options).expect("plan grouped backups");
        let removals: Vec<_> = plan
            .actions()
            .iter()
            .filter_map(|action| match action {
                CompactionAction::RemoveRecoveryBackup { file_name, .. } => {
                    Some(file_name.as_str())
                }
                _ => None,
            })
            .collect();

        assert_eq!(
            removals,
            [
                "data00000a.tar.2.bak",
                "data00000a.tar.2.ro.bak",
                "data00000a.tar.bak",
            ]
        );
        assert!(
            !removals.contains(&"data00000a.tar.ro.bak"),
            "the newest spelling consumes the one shared target slot"
        );
    }

    #[test]
    fn default_preserves_backups_but_explicit_zero_retention_removes_them() {
        let directory = TestDirectory::repository("backup-retention");
        let backup = directory.path.join("journal.log.bak.999");
        std::fs::write(&backup, b"recovery material").expect("write backup");
        let future_backup = directory.path.join("journal.log.bak.998");
        let future_file =
            std::fs::File::create(&future_backup).expect("create future-dated backup");
        future_file
            .set_times(
                std::fs::FileTimes::new().set_modified(
                    std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                ),
            )
            .expect("future-date backup");
        let default_plan =
            plan_compaction(&directory.path, &CompactionOptions::default()).expect("plan");
        assert!(
            !default_plan
                .actions()
                .iter()
                .any(|action| matches!(action, CompactionAction::RemoveRecoveryBackup { .. }))
        );
        assert!(backup.exists());

        let options = CompactionOptions::default()
            .with_tasks([])
            .with_recovery_backup_policy(RecoveryBackupPolicy {
                minimum_age: std::time::Duration::ZERO,
                keep_latest_per_target: 0,
            });
        compact(&directory.path, options).expect("remove backup");
        assert!(!backup.exists());
        assert!(
            future_backup.exists(),
            "a future-dated backup is never old enough, even at a zero age floor"
        );
        Repository::open(&directory.path).expect("healthy repository");
    }

    #[test]
    fn numbered_read_only_archive_backups_are_recognized_and_policy_managed() {
        let directory = TestDirectory::repository("numbered-read-only-backup");
        let name = "data00000a.tar.2.ro.bak";
        let backup = directory.path.join(name);
        std::fs::copy(directory.path.join("data00000a.tar"), &backup)
            .expect("create Oak-style numbered read-only backup");

        let default_plan = plan_compaction(&directory.path, &CompactionOptions::default())
            .expect("default plan preserves recovery evidence");
        assert!(!default_plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveRecoveryBackup { file_name, .. } if file_name == name
        )));

        let options = CompactionOptions::default()
            .with_tasks([])
            .with_recovery_backup_policy(RecoveryBackupPolicy::new(std::time::Duration::ZERO, 0));
        let plan = plan_compaction(&directory.path, &options).expect("plan numbered backup");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveRecoveryBackup { file_name, .. } if file_name == name
        )));

        let outcome = compact(&directory.path, options).expect("remove numbered backup");
        assert_eq!(outcome.removed_recovery_backups, 1);
        assert!(outcome.is_complete());
        assert!(!backup.exists());
        Repository::open(&directory.path).expect("repository remains healthy");
    }

    #[test]
    fn backup_count_retains_every_file_tied_at_the_newest_cutoff() {
        let directory = TestDirectory::repository("backup-retention-mtime-tie");
        let now = std::time::SystemTime::now();
        let tied = now - std::time::Duration::from_secs(7200);
        let older = now - std::time::Duration::from_secs(10_800);
        for (name, modified) in [
            ("journal.log.bak.000", tied),
            ("journal.log.bak.001", tied),
            ("journal.log.bak.002", tied),
            ("journal.log.bak.003", older),
        ] {
            let file = std::fs::File::create(directory.path.join(name)).expect("create backup");
            file.set_times(std::fs::FileTimes::new().set_modified(modified))
                .expect("set exact backup mtime");
        }
        let options = CompactionOptions::default()
            .with_tasks([])
            .with_recovery_backup_policy(RecoveryBackupPolicy::new(std::time::Duration::ZERO, 1));

        let plan = plan_compaction(&directory.path, &options).expect("plan tied backups");
        let removals: Vec<_> = plan
            .actions()
            .iter()
            .filter_map(|action| match action {
                CompactionAction::RemoveRecoveryBackup { file_name, .. } => {
                    Some(file_name.as_str())
                }
                _ => None,
            })
            .collect();

        assert_eq!(removals, ["journal.log.bak.003"]);
    }

    #[test]
    fn manifest_upgrade_separates_a_trailing_properties_continuation() {
        let suffix = b"# upgraded atomically by froe cleanup\nstore.version=2\n";
        for (source, expected_prefix) in [
            (
                &b"custom.property=kept\\"[..],
                &b"custom.property=kept\\\n\n"[..],
            ),
            (
                &b"custom.property=kept\\\n"[..],
                &b"custom.property=kept\\\n\n"[..],
            ),
            (
                &b"custom.property=kept\\\r"[..],
                &b"custom.property=kept\\\r\n\n"[..],
            ),
        ] {
            let upgraded = manifest_upgrade_bytes(source);
            assert!(upgraded.starts_with(expected_prefix), "{upgraded:?}");
            assert!(upgraded.ends_with(suffix), "{upgraded:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn manifest_certificate_rejects_path_substitution_around_publication() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = TestDirectory::new("manifest-certificate-substitution");
        let canonical = directory.path.join("manifest");
        let canonical_bytes = b"custom.property=kept\nstore.version=1\n";
        std::fs::write(&canonical, canonical_bytes).expect("write canonical manifest");

        let staged = directory.path.join("manifest.cleaning.000");
        let expected = b"custom.property=kept\nstore.version=2\n";
        std::fs::write(&staged, expected).expect("write staged manifest");
        let staged_metadata = std::fs::symlink_metadata(&staged).expect("staged metadata");
        let certificate = certify_manifest_file(
            &staged,
            &staged_metadata,
            expected,
            ManifestFileAccess::ReadWrite,
            "staged manifest replacement",
        )
        .expect("certify staged manifest");

        let retained_inode = directory.path.join("retained-manifest-inode");
        std::fs::rename(&staged, &retained_inode).expect("move certified inode aside");
        std::fs::write(&staged, expected).expect("substitute same-byte staged manifest");
        let substituted_metadata =
            std::fs::symlink_metadata(&staged).expect("substituted staged metadata");
        assert_ne!(
            (substituted_metadata.dev(), substituted_metadata.ino()),
            (staged_metadata.dev(), staged_metadata.ino()),
            "the fixture must isolate identity checking from byte checking"
        );
        certificate
            .recertify(
                &staged,
                expected,
                ManifestFileAccess::ReadWrite,
                "staged manifest replacement",
            )
            .expect_err("same bytes on a different inode must not be publishable");
        assert_eq!(
            std::fs::read(&canonical).expect("read canonical manifest"),
            canonical_bytes,
            "a rejected staging substitution must leave the source canonical"
        );

        std::fs::remove_file(&staged).expect("remove substituted staging file");
        std::fs::rename(&retained_inode, &staged).expect("restore certified inode");
        certificate
            .recertify(
                &staged,
                expected,
                ManifestFileAccess::ReadWrite,
                "staged manifest replacement",
            )
            .expect("restored certified inode");

        let installed = directory.path.join("installed-manifest");
        std::fs::rename(&staged, &installed).expect("publish certified inode");
        certificate
            .recertify(
                &installed,
                expected,
                ManifestFileAccess::ReadWrite,
                "installed manifest replacement",
            )
            .expect("certificate follows the inode through rename");

        let displaced = directory.path.join("displaced-installed-manifest");
        std::fs::rename(&installed, &displaced).expect("displace installed inode");
        std::fs::write(&installed, expected).expect("substitute installed manifest");
        let installed_substitute =
            std::fs::symlink_metadata(&installed).expect("installed substitute metadata");
        assert_ne!(
            (installed_substitute.dev(), installed_substitute.ino()),
            (staged_metadata.dev(), staged_metadata.ino()),
            "the post-publication fixture must install a different inode"
        );
        certificate
            .recertify(
                &installed,
                expected,
                ManifestFileAccess::ReadWrite,
                "installed manifest replacement",
            )
            .expect_err("post-rename same-byte inode substitution must be detected");
    }

    #[test]
    fn redundant_journal_staging_is_removed_and_second_run_is_a_true_noop() {
        let directory = TestDirectory::repository("temporary-idempotence");
        let staging = directory.path.join("journal.log.compacting");
        std::fs::copy(directory.path.join("journal.log"), &staging).expect("copy staging");
        let forensic_staging = directory.path.join("journal.log.recovered");
        std::fs::write(&forensic_staging, b"unterminated recovery evidence")
            .expect("write ambiguous staging journal");
        std::fs::write(directory.path.join("gc.log"), b"operator gc state\n").expect("seed gc log");

        compact(&directory.path, CompactionOptions::default()).expect("first cleanup");
        assert!(!staging.exists());
        assert!(forensic_staging.exists());
        assert_eq!(
            std::fs::read(directory.path.join("gc.log")).expect("gc log"),
            b"operator gc state\n"
        );
        let before = file_bytes(&directory.path);
        let second =
            plan_compaction(&directory.path, &CompactionOptions::default()).expect("second plan");
        assert!(second.is_empty(), "second plan: {:?}", second.actions());
        let outcome = PreparedCompaction::prepare(&directory.path, CompactionOptions::default())
            .expect("prepare no-op")
            .apply()
            .expect("apply no-op");
        assert_eq!(outcome.head_before, outcome.head_after);
        assert_eq!(file_bytes(&directory.path), before);
    }

    #[test]
    fn archive_staging_requires_complete_byte_identity_before_removal() {
        let directory = TestDirectory::repository("archive-staging-proof");
        let exact = directory.path.join("data00000b.tar.cleaning.000");
        std::fs::copy(directory.path.join("data00000a.tar"), &exact)
            .expect("copy exact staging archive");
        let ambiguous = directory.path.join("data00001a.tar.recovering");
        std::fs::write(&ambiguous, b"nonempty recovery evidence")
            .expect("write ambiguous staging archive");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);

        let plan = plan_compaction(&directory.path, &options).expect("plan");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveTemporary { file_name, .. }
                if file_name == "data00000b.tar.cleaning.000"
        )));
        assert!(!plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveTemporary { file_name, .. }
                if file_name == "data00001a.tar.recovering"
        )));

        compact(&directory.path, options).expect("cleanup");
        assert!(!exact.exists());
        assert!(ambiguous.exists());
        Repository::open(&directory.path).expect("healthy repository");
    }

    #[test]
    fn manifest_staging_requires_exact_canonical_or_upgrade_bytes() {
        let directory = TestDirectory::repository("manifest-staging-proof");
        let canonical = b"custom.property=kept\nstore.version=1\n";
        std::fs::write(directory.path.join("manifest"), canonical)
            .expect("write version-one manifest");
        let identical = directory.path.join("manifest.cleaning.000");
        std::fs::write(&identical, canonical).expect("write identical staging manifest");
        let exact_upgrade = directory.path.join("manifest.cleaning.001");
        std::fs::write(&exact_upgrade, manifest_upgrade_bytes(canonical))
            .expect("write exact upgrade staging manifest");
        let divergent = directory.path.join("manifest.cleaning.002");
        std::fs::write(&divergent, b"store.version=2\noperator.data=lost\n")
            .expect("write divergent staging manifest");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);

        let plan = plan_compaction(&directory.path, &options).expect("plan stale manifests");
        let planned_names: Vec<_> = plan
            .actions()
            .iter()
            .filter_map(|action| match action {
                CompactionAction::RemoveTemporary { file_name, .. } => Some(file_name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            planned_names,
            ["manifest.cleaning.000", "manifest.cleaning.001"]
        );
        assert!(plan.warnings().iter().any(|warning| {
            warning.contains("manifest.cleaning.002") && warning.contains("not provably redundant")
        }));

        compact(&directory.path, options).expect("remove proven manifest staging files");
        assert!(!identical.exists());
        assert!(!exact_upgrade.exists());
        assert!(divergent.exists());
        assert_eq!(
            std::fs::read(directory.path.join("manifest")).expect("read canonical manifest"),
            canonical
        );
        Repository::open(&directory.path).expect("healthy repository");
    }

    #[test]
    fn expired_checkpoints_are_removed_in_one_healthy_head_update() {
        let directory = TestDirectory::repository("expired-checkpoint");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let outcome = compact(&directory.path, options).expect("cleanup");

        assert_eq!(outcome.removed_checkpoints, 1);
        let repository = Repository::open(&directory.path).expect("healthy repository");
        assert!(repository.checkpoints().expect("checkpoints").is_empty());
    }

    #[test]
    fn a_retention_bound_beside_a_checkpoint_head_update_is_refused_while_planning() {
        let directory = TestDirectory::repository("retention-with-checkpoint");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Checkpoint removal installs a new head and appends its journal
        // line, which moves the newest resolvable revision. The bound would
        // then retire the very head the plan retained, and the run would
        // abort its own apply — after the checkpoint removal had already
        // been committed. Refuse while planning, where nothing has moved.
        let options = CompactionOptions::default()
            .with_tasks([
                MaintenanceTask::ExpiredCheckpoints,
                MaintenanceTask::Journal,
            ])
            .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision"));
        let error = plan_compaction(&directory.path, &options).expect_err("must refuse");
        assert!(
            error.to_string().contains("checkpoint"),
            "unexpected refusal: {error}"
        );

        // Nothing was touched: the checkpoint is still there to be removed
        // by a run that does not also bound the journal.
        let repository = Repository::open(&directory.path).expect("healthy repository");
        assert_eq!(repository.checkpoints().expect("checkpoints").len(), 1);
    }

    #[test]
    fn checkpoint_planning_rejects_a_physically_exhausted_archive_namespace() {
        let directory = TestDirectory::repository("checkpoint-archive-number-exhausted");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Zero-byte files are skipped by archive discovery, but their exact
        // Oak archive names still occupy the physical namespace.
        std::fs::write(directory.path.join("data4294967295z.tar"), b"")
            .expect("install maximum-number residue");
        let before = file_bytes(&directory.path);
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("planning must reject u32::MAX before checkpoint mutation");

        assert!(
            error.to_string().contains("namespace is exhausted"),
            "{error}"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "namespace preflight must remain byte-exact"
        );
    }

    #[test]
    fn checkpoint_cleanup_allocates_after_zero_byte_next_archive_residue() {
        let directory = TestDirectory::repository("checkpoint-zero-byte-next-archive");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));

        let repository = Repository::open(&directory.path).expect("open checkpoint repository");
        let active_maximum = repository
            .archives()
            .iter()
            .filter_map(|archive| ArchiveFileName::parse(archive.file_name()))
            .map(|name| name.archive_number)
            .max()
            .expect("fixture has an active archive");
        drop(repository);
        let occupied_number = active_maximum.checked_add(1).expect("fixture namespace");
        let certified_number = occupied_number.checked_add(1).expect("fixture namespace");
        let occupied_name = format!("data{occupied_number:05}a.tar");
        std::fs::write(directory.path.join(&occupied_name), b"")
            .expect("install zero-byte otherwise-next residue");
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let plan = plan_compaction(&directory.path, &options).expect("plan checkpoint cleanup");
        assert_eq!(plan.checkpoint_archive_number, Some(certified_number));
        let outcome = compact(&directory.path, options).expect("apply checkpoint cleanup");

        assert_eq!(outcome.removed_checkpoints, 1);
        assert_eq!(
            std::fs::read(directory.path.join(&occupied_name)).expect("read zero-byte residue"),
            b"",
            "cleanup must neither truncate nor reuse physical residue"
        );
        assert!(
            directory
                .path
                .join(format!("data{certified_number:05}a.tar"))
                .exists()
        );
        let repository = Repository::open(&directory.path).expect("healthy repository");
        assert!(repository.checkpoints().expect("checkpoints").is_empty());
    }

    #[test]
    fn checkpoint_only_cleanup_rejects_current_index_generation_mismatch() {
        let directory = TestDirectory::repository("checkpoint-index-generation-mismatch");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let repository = Repository::open(&directory.path).expect("open checkpoint repository");
        let head = repository.head_record_identifier();
        let archive_name = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(head.segment))
            .expect("active archive contains head")
            .file_name()
            .to_owned();
        let header_generation = repository
            .segment(head.segment)
            .expect("read head segment")
            .structure
            .generation;
        drop(repository);
        change_index_generation(
            &directory.path.join(archive_name),
            head.segment,
            header_generation.saturating_add(1),
        );
        let before = file_bytes(&directory.path);
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("checkpoint head update must reject corrupt index generation");

        assert!(error.to_string().contains("index generation"), "{error}");
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning must not mutate"
        );
    }

    #[test]
    fn checkpoint_only_cleanup_rejects_duplicate_head_segments_before_generation_validation() {
        let directory = TestDirectory::repository("checkpoint-duplicate-head-generation");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));

        let repository = Repository::open(&directory.path).expect("open checkpoint repository");
        let head = repository.head_record_identifier();
        let source_name = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(head.segment))
            .expect("active archive contains head")
            .file_name()
            .to_owned();
        let next_number = repository
            .archives()
            .iter()
            .filter_map(|archive| {
                crate::tar_archive::file_name::ArchiveFileName::parse(archive.file_name())
                    .map(|name| name.archive_number)
            })
            .max()
            .expect("fixture has archives")
            .checked_add(1)
            .expect("fixture archive namespace");
        let duplicate_name = format!("data{next_number:05}a.tar");
        let header_generation = repository
            .segment(head.segment)
            .expect("read head segment")
            .structure
            .generation;
        drop(repository);

        let duplicate_path = directory.path.join(&duplicate_name);
        std::fs::copy(directory.path.join(source_name), &duplicate_path)
            .expect("copy head archive under a newer number");
        change_index_generation(
            &duplicate_path,
            head.segment,
            header_generation.saturating_add(1),
        );
        let before = file_bytes(&directory.path);
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("checkpoint write must reject ambiguous duplicate segment locations");

        assert!(
            error.to_string().contains("occurs in active archives"),
            "{error}"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning must not mutate"
        );
    }

    #[test]
    fn a_head_moving_cleanup_upgrades_a_version_one_manifest_atomically() {
        let directory = TestDirectory::repository("manifest-upgrade");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            directory.path.join("manifest"),
            b"custom.property=kept\nstore.version=\\\n 1\n",
        )
        .expect("install Java-continuation version-one manifest");
        #[cfg(unix)]
        let source_identity = {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            std::fs::set_permissions(
                directory.path.join("manifest"),
                std::fs::Permissions::from_mode(0o640),
            )
            .expect("set manifest permissions");
            let metadata = std::fs::metadata(directory.path.join("manifest"))
                .expect("source manifest metadata");
            (metadata.uid(), metadata.gid())
        };
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);
        let plan = plan_compaction(&directory.path, &options).expect("plan");
        assert!(
            plan.actions()
                .iter()
                .any(|action| matches!(action, CompactionAction::UpgradeManifest))
        );

        compact(&directory.path, options).expect("cleanup");

        let manifest = std::fs::read_to_string(directory.path.join("manifest"))
            .expect("read upgraded manifest");
        assert!(manifest.contains("custom.property=kept"));
        assert!(manifest.ends_with("store.version=2\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let metadata = std::fs::metadata(directory.path.join("manifest"))
                .expect("upgraded manifest metadata");
            assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
            assert_eq!((metadata.uid(), metadata.gid()), source_identity);
        }
        assert!(
            !std::fs::read_dir(&directory.path)
                .expect("read directory")
                .any(|entry| entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("manifest.cleaning."))
        );
        Repository::open(&directory.path).expect("healthy v2 repository");
    }

    #[test]
    fn segment_source_certificate_rejects_a_survivor_payload_crc_mismatch() {
        let (directory, source_name, replacement_name, survivor) =
            rewrite_certificate_fixture("source-certificate-survivor-crc");
        corrupt_segment_payload_crc(&directory.path.join(&source_name), survivor);

        assert_source_certificate_refusal(
            &directory,
            &source_name,
            Some(&replacement_name),
            "payload CRC",
        );
    }

    #[test]
    fn segment_source_certificate_rejects_exact_graph_or_brf_omissions() {
        for (name, omitted, expected_error) in [
            (
                "source-certificate-omitted-graph",
                OmittedArchiveMetadata::Graph,
                "segment graph differs",
            ),
            (
                "source-certificate-omitted-brf",
                OmittedArchiveMetadata::BinaryReferences,
                "binary-reference catalog differs",
            ),
        ] {
            let (directory, source_name, replacement_name, _) = rewrite_certificate_fixture(name);
            repack_omitting_archive_metadata(&directory.path, &source_name, omitted);

            assert_source_certificate_refusal(
                &directory,
                &source_name,
                Some(&replacement_name),
                expected_error,
            );
        }
    }

    #[test]
    fn segment_source_certificate_precedes_a_whole_archive_removal() {
        let (directory, source_name, orphan) =
            whole_removal_certificate_fixture("source-certificate-whole-removal");
        change_index_generation(&directory.path.join(&source_name), orphan, -1);

        assert_source_certificate_refusal(
            &directory,
            &source_name,
            None,
            "index/header generation disagreement",
        );
        assert!(
            directory.path.join(source_name).exists(),
            "the whole-removal source must survive a failed certificate"
        );
    }

    #[test]
    fn segment_cleanup_removes_old_unjournaled_archive_but_preserves_history() {
        let directory = TestDirectory::repository("orphan-segment-history");
        let old_head = Repository::open(&directory.path)
            .expect("old repository")
            .head_record_identifier();

        // A separate, unjournaled generation-zero archive: representative of
        // a failed write/CAS whose records never became repository state.
        {
            let store = WritableRepository::open(&directory.path).expect("open orphan writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("orphan node");
            writer.finish().expect("finish orphan segment");
            store.close().expect("close orphan writer");
        }
        assert!(directory.path.join("data00001a.tar").is_file());

        // Publish a completely independent generation-two head. It does not
        // reference generation zero; only the older journal line roots the
        // original bootstrap revision.
        let new_head = {
            let store = WritableRepository::open(&directory.path).expect("open new head writer");
            let generation = GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            };
            let mut writer = store.record_writer(generation);
            let root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("new content root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("new super root");
            writer.finish().expect("finish new head");
            assert!(store.set_head(store.head(), head));
            store.close().expect("close new head writer");
            head
        };
        assert!(directory.path.join("data00002a.tar").is_file());

        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("segment plan");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == "data00001a.tar"
        )));
        assert!(!plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )));
        let planned_removed_segments: usize = plan
            .actions()
            .iter()
            .filter_map(|action| match action {
                CompactionAction::RemoveReclaimableArchive { segments, .. }
                | CompactionAction::RewriteArchive { segments, .. } => Some(*segments),
                _ => None,
            })
            .sum();
        assert!(planned_removed_segments != 0);

        let outcome = compact(&directory.path, options).expect("segment cleanup");
        assert_eq!(outcome.head_after, new_head);
        assert_eq!(outcome.removed_segments(), planned_removed_segments);
        assert!(!directory.path.join("data00001a.tar").exists());
        let repository = Repository::open(&directory.path).expect("healthy final repository");
        assert_eq!(repository.head_record_identifier(), new_head);
        crate::tooling::verify_node_tree(&repository, old_head)
            .expect("historical root remains readable");
    }

    /// Builds the fixture the field report reduces to: an independent
    /// generation-two head, plus a generation-zero archive that only the
    /// original journal line still roots. Oak's predicate would reclaim that
    /// archive; froe's history keep-veto does not. Returns the old head, the
    /// new head, and the directory.
    fn history_veto_fixture(name: &str) -> (TestDirectory, RecordIdentifier, RecordIdentifier) {
        let directory = TestDirectory::repository(name);
        let old_head = Repository::open(&directory.path)
            .expect("old repository")
            .head_record_identifier();
        let new_head = {
            let store = WritableRepository::open(&directory.path).expect("open new head writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            });
            let root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("new content root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("new super root");
            writer.finish().expect("finish new head");
            assert!(store.set_head(store.head(), head));
            store.close().expect("close new head writer");
            head
        };
        (directory, old_head, new_head)
    }

    /// The shape a real store has once reclamation actually starts removing
    /// things: dead segments that survive only because their archive failed
    /// the rewrite gate, still pointing at dead segments in an archive that
    /// goes away entirely.
    #[test]
    fn a_dead_survivor_pointing_at_a_removed_segment_is_handled() {
        let directory = TestDirectory::repository("dead-survivor-reference");
        let dead_generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        let live_generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };

        // An archive of nothing but dead segments: fully reclaimable, so the
        // sweep unlinks it whole.
        let target = {
            let store = WritableRepository::open(&directory.path).expect("open target writer");
            let mut writer = store.record_writer(dead_generation);
            let node = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("dead target node");
            writer.finish().expect("finish target");
            store.close().expect("close target writer");
            node
        };

        // An archive that keeps one dead segment referencing that target,
        // beside enough live segments that removing the dead one cannot
        // repay a rewrite — so the dead segment stays on disk.
        let new_head = {
            let store = WritableRepository::open(&directory.path).expect("open mixed writer");
            let mut referencing = store.record_writer(dead_generation);
            referencing
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "target".to_owned(),
                        node: target,
                    },
                    &[],
                )
                .expect("dead referencing node");
            referencing.finish().expect("finish referencing");
            for _ in 0..8 {
                let mut filler = store.record_writer(live_generation);
                filler
                    .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                    .expect("live filler");
                filler.finish().expect("finish filler");
            }
            let mut writer = store.record_writer(live_generation);
            let root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("content root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("super root");
            writer.finish().expect("finish head");
            assert!(store.set_head(store.head(), head));
            store.close().expect("close mixed writer");
            head
        };

        // The default task set, with no retention bound: the loosened check
        // lives on this path, so this is where it must be pinned.
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);

        // Unconditional. Restoring the stricter check must fail this, so the
        // plan is required to exist rather than merely be well-explained if
        // it does not.
        let plan = plan_compaction(&directory.path, &options)
            .expect("a dead survivor pointing at removed garbage must not refuse the plan");
        let removed: usize = plan
            .actions()
            .iter()
            .filter_map(|action| match action {
                CompactionAction::RemoveReclaimableArchive { segments, .. }
                | CompactionAction::RewriteArchive { segments, .. } => Some(*segments),
                _ => None,
            })
            .sum();
        assert!(
            removed != 0,
            "the fixture must actually remove something, or it proves nothing"
        );

        let outcome = compact(&directory.path, options).expect("apply the plan");
        assert_eq!(outcome.head_after, new_head);
        assert!(outcome.removed_segments() != 0);
        let repository = Repository::open(&directory.path).expect("healthy store");
        assert_eq!(repository.head_record_identifier(), new_head);
        crate::tooling::verify_node_tree(&repository, new_head).expect("head verifies");
    }

    #[test]
    fn the_plan_prices_the_history_veto_against_oaks_own_predicate() {
        let (directory, _old_head, _new_head) = history_veto_fixture("history-veto-price");

        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("segment plan");

        // The bootstrap revision's segments are reachable from the old
        // journal line and from nothing else.
        assert!(
            plan.history_protected_segments() != 0,
            "the bootstrap revision must be counted as history-only"
        );
        // And they are two full generations behind the head, so Oak would
        // have reclaimed them. That difference is the veto's price, and
        // reporting it is the whole point: it is what turns "reclaimed
        // nothing" into a number the operator can act on.
        let (reclaimable_segments, reclaimable_bytes) = plan.history_protected_reclaimable();
        assert!(
            reclaimable_segments != 0,
            "generation-zero history must be priced as reclaimable-but-protected"
        );
        assert!(reclaimable_bytes != 0);
        assert!(reclaimable_segments <= plan.history_protected_segments());
    }

    #[test]
    fn the_history_price_counts_the_bulk_segments_held_behind_the_data_ones() {
        // The veto holds bulk segments only indirectly: a vetoed data segment
        // keeps seeding its references, so its binary content stays too.
        // Counting protected *data* segments alone therefore prices a store
        // full of inline binaries at a rounding error, and an operator
        // reading that figure would decline a run worth most of the store.
        let directory = TestDirectory::repository("history-price-bulk");
        {
            let store = WritableRepository::open(&directory.path).expect("open binary writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            // Comfortably past the 256 KiB segment limit, so the content
            // lands in bulk segments rather than inline in the data segment.
            let content: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
            let binary = writer.write_binary_content(&content).expect("binary");
            let file = writer
                .write_node(
                    Some("nt:file"),
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "data".to_owned(),
                        property_type: crate::content::property::PropertyType::Binary,
                        values: PropertyValuesToWrite::Single(binary),
                    }],
                )
                .expect("file node");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: file,
                    },
                    &[],
                )
                .expect("binary super root");
            writer.finish().expect("finish binary segments");
            assert!(store.set_head(store.head(), head));
            store.close().expect("close binary writer");
        }
        // An independent generation-two head that reaches none of it.
        {
            let store = WritableRepository::open(&directory.path).expect("open new head writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            });
            let root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("new content root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("new super root");
            writer.finish().expect("finish new head");
            assert!(store.set_head(store.head(), head));
            store.close().expect("close new head writer");
        }

        let priced = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("priced plan");
        let (_priced_segments, priced_bytes) = priced.history_protected_reclaimable();

        // Now actually retire the history and compare. The quoted price must
        // be what the operation delivers, not the data-segment fraction of it.
        let outcome = compact(
            &directory.path,
            CompactionOptions::default()
                .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
                .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision")),
        )
        .expect("bounded cleanup");
        let freed = outcome.archive_bytes_before - outcome.archive_bytes_after;
        assert!(
            freed > 1024 * 1024,
            "retiring the history must free the binary content: {freed}"
        );
        assert_eq!(
            priced_bytes, freed,
            "the quoted price must be what retiring the history delivers"
        );
    }

    #[test]
    fn a_journal_retention_bound_retires_the_history_the_veto_protects() {
        let (directory, old_head, new_head) = history_veto_fixture("history-veto-retention");
        let protected = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("unbounded plan");
        assert!(protected.history_protected_reclaimable().0 != 0);
        assert!(
            !protected.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RemoveReclaimableArchive { file_name, .. }
                    if file_name == "data00000a.tar"
            )),
            "without a bound the veto must keep the bootstrap archive"
        );

        let bounded = CompactionOptions::default()
            .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
            .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision"));
        let plan = plan_compaction(&directory.path, &bounded).expect("bounded plan");

        // The older line is pruned for the retention reason, not for damage.
        assert!(
            plan.journal_line_removals().iter().any(|removal| {
                removal.reason() == JournalRemovalReason::BeyondRetention
                    && removal.record_identifier() == Some(old_head)
            }),
            "the superseded revision must be removed as beyond retention"
        );
        // Releasing that root is what makes the archive eligible.
        assert!(
            plan.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RemoveReclaimableArchive { file_name, .. }
                    if file_name == "data00000a.tar"
            )),
            "the bound must release the bootstrap archive to Oak's predicate"
        );
        assert!(plan.estimated_reclaimable_bytes() != 0);

        let outcome = compact(&directory.path, bounded).expect("bounded cleanup");
        assert_eq!(outcome.head_after, new_head);
        assert!(!directory.path.join("data00000a.tar").exists());
        let repository = Repository::open(&directory.path).expect("healthy final repository");
        assert_eq!(repository.head_record_identifier(), new_head);
        // The journal keeps exactly the bound's worth of revisions, and the
        // retired history is genuinely gone rather than merely unrooted.
        let journal =
            std::fs::read_to_string(directory.path.join("journal.log")).expect("read journal");
        assert_eq!(
            journal.lines().count(),
            1,
            "a bound of one leaves one journal line"
        );
        assert!(
            crate::tooling::verify_node_tree(&repository, old_head).is_err(),
            "the retired revision must no longer resolve"
        );
    }

    #[test]
    fn a_retention_bound_without_the_journal_task_is_refused() {
        let (directory, _old_head, _new_head) = history_veto_fixture("history-veto-task-guard");
        // Un-rooting without pruning would leave the line in the journal for
        // the prospective-plan check to verify as retained history, and the
        // run would refuse itself with a far less actionable message.
        let options = CompactionOptions::default()
            .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision"))
            .with_tasks([MaintenanceTask::Segments]);
        let error = plan_compaction(&directory.path, &options).expect_err("must refuse");
        assert!(
            error.to_string().contains("requires the journal task"),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn a_bound_counts_only_revisions_that_actually_resolve() {
        // A line whose segment exists but whose tree does not verify used to
        // fill a slot in the bound and then be removed as unreadable anyway,
        // so `N = 2` kept one revision and irreversibly retired a readable
        // one to make room for it. Every earlier retention test used N = 1,
        // which cannot expose this: the head is always the newest resolvable
        // line and always verifies.
        let (directory, old_head, new_head) = history_veto_fixture("retention-counts-readable");

        // A journal line naming a record that resolves to a segment but not
        // to a readable node tree: the head's own segment, at a record
        // number that is not a node record.
        let unreadable = RecordIdentifier::new(new_head.segment, new_head.record_number + 1);
        // Second newest, not newest: the newest line is the head, and a
        // head that is not a node record is refused long before any bound.
        let journal_path = directory.path.join("journal.log");
        let journal = std::fs::read_to_string(&journal_path).expect("read journal");
        let mut lines: Vec<&str> = journal.lines().collect();
        let head_line = lines.pop().expect("a head line");
        let unreadable_line = format!("{unreadable} root 0");
        lines.push(&unreadable_line);
        lines.push(head_line);
        std::fs::write(&journal_path, format!("{}\n", lines.join("\n")))
            .expect("insert unreadable line");

        let options = CompactionOptions::default()
            .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
            .with_journal_revision_retention(NonZeroUsize::new(2).expect("two revisions"));
        let plan = plan_compaction(&directory.path, &options).expect("bounded plan");

        // The unreadable line goes, as it always did. What must not happen is
        // the older *readable* revision going with it to satisfy a bound the
        // unreadable line was counted against.
        assert!(
            !plan.journal_line_removals().iter().any(|removal| {
                removal.reason() == JournalRemovalReason::BeyondRetention
                    && removal.record_identifier() == Some(old_head)
            }),
            "a readable revision was retired to make room for an unreadable one: {:?}",
            plan.journal_line_removals()
        );
    }

    #[test]
    fn a_bound_larger_than_the_journal_removes_nothing() {
        let (directory, _old_head, _new_head) = history_veto_fixture("history-veto-wide-bound");
        let options = CompactionOptions::default()
            .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
            .with_journal_revision_retention(NonZeroUsize::new(64).expect("wide bound"));
        let plan = plan_compaction(&directory.path, &options).expect("wide plan");
        assert!(
            !plan
                .journal_line_removals()
                .iter()
                .any(|removal| { removal.reason() == JournalRemovalReason::BeyondRetention }),
            "a bound wider than the journal must retire nothing"
        );
        assert!(plan.history_protected_reclaimable().0 != 0);
    }

    /// One archive holding a single dead generation-zero segment beside
    /// enough live head-generation segments that removing the dead one
    /// frees less than a quarter of the file. This is the field report in
    /// miniature: real garbage, correctly identified, and — under Oak's
    /// heuristic — declined by every run forever.
    fn sub_gate_garbage_fixture(name: &str) -> (TestDirectory, RecordIdentifier) {
        let directory = TestDirectory::repository(name);
        let new_head = {
            let store = WritableRepository::open(&directory.path).expect("open writer");
            let mut dead = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            dead.write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("dead node");
            dead.finish().expect("finish dead segment");

            let live_generation = GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            };
            for _ in 0..8 {
                let mut live = store.record_writer(live_generation);
                live.write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                    .expect("live node");
                live.finish().expect("finish live segment");
            }
            let mut writer = store.record_writer(live_generation);
            let root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("content root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("super root");
            writer.finish().expect("finish head segment");
            assert!(store.set_head(store.head(), head));
            store.close().expect("close writer");
            head
        };
        (directory, new_head)
    }

    #[test]
    fn gate_deferred_garbage_is_counted_rather_than_reported_as_nothing() {
        let (directory, new_head) = sub_gate_garbage_fixture("retained-reclaimable");

        let options = CompactionOptions::default()
            .with_tasks([MaintenanceTask::Segments])
            .with_oak_savings_gate();
        let plan = plan_compaction(&directory.path, &options).expect("segment plan");
        assert_eq!(plan.current_head(), new_head);
        let deferred = plan
            .warnings()
            .iter()
            .any(|warning| warning.contains("25% rewrite gate"));
        assert!(
            deferred,
            "expected a savings deferral: {:?}",
            plan.warnings()
        );
        assert!(
            plan.retained_reclaimable_segments() != 0,
            "declined garbage must be counted, not silently dropped"
        );
        assert!(plan.retained_reclaimable_bytes() != 0);
        // The distinction the old output could not express: nothing is
        // reclaimable *by this run*, yet the store is not clean.
        assert_eq!(plan.estimated_reclaimable_bytes(), 0);
    }

    #[test]
    fn the_default_policy_reclaims_what_the_oak_gate_declines() {
        let (directory, new_head) = sub_gate_garbage_fixture("default-policy-reclaims");

        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("segment plan");
        assert_eq!(plan.current_head(), new_head);
        assert!(
            !plan
                .warnings()
                .iter()
                .any(|warning| warning.contains("rewrite gate")),
            "the default policy never defers for savings: {:?}",
            plan.warnings()
        );
        assert_eq!(
            plan.retained_reclaimable_segments(),
            0,
            "nothing identified may be left behind on an archive with letters to spare"
        );
        assert_eq!(plan.retained_reclaimable_bytes(), 0);
        assert!(
            plan.estimated_reclaimable_bytes() != 0,
            "the garbage the gate declined is now actually reclaimed"
        );
        assert!(
            plan.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RewriteArchive { file_name, replacement_name, .. }
                    if file_name == "data00001a.tar" && replacement_name == "data00001b.tar"
            )),
            "the sub-gate archive is rewritten to its next generation: {:?}",
            plan.actions()
        );
    }

    /// Records the step names an operation reports, so a test can assert
    /// what the operator was told froe was doing.
    struct StepNameObserver {
        names: Vec<String>,
    }

    impl ProgressObserver for StepNameObserver {
        fn step_began(&mut self, step: &Step<'_>) {
            self.names.push(step.description().to_owned());
        }

        fn step_advanced(&mut self, _completed: u64) {}

        fn step_ended(&mut self) {}
    }

    #[test]
    fn an_unselected_task_reports_no_step_of_its_own() {
        let (directory, _old_head, _new_head) = history_veto_fixture("unselected-task-step");
        let mut observer = StepNameObserver { names: Vec::new() };
        // recovery-backups is not among the defaults, so announcing its
        // removal step told the operator froe had considered backups it was
        // never asked to touch.
        compact_with_progress(
            &directory.path,
            CompactionOptions::default()
                .with_tasks([
                    MaintenanceTask::Segments,
                    MaintenanceTask::Journal,
                    MaintenanceTask::StaleTemporaries,
                ])
                .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision")),
            &mut observer,
        )
        .expect("bounded cleanup");
        for unselected in ["removing old recovery backups", "removing stale archives"] {
            assert!(
                !observer.names.iter().any(|name| name == unselected),
                "unselected task reported {unselected:?}: {:?}",
                observer.names
            );
        }
        assert!(
            observer
                .names
                .iter()
                .any(|name| name == "removing stale temporary files"),
            "a selected task still reports its step: {:?}",
            observer.names
        );
    }

    #[test]
    fn the_outcome_states_the_backup_bytes_the_archive_figures_omit() {
        let (directory, _old_head, _new_head) = history_veto_fixture("retained-backup-bytes");
        // What a preceding index repair leaves behind: bytes on disk that no
        // archive figure counts, so a run that grew the directory reported
        // its size as unchanged.
        let retired = directory.path.join("data00000a.tar.bak");
        std::fs::write(&retired, vec![7u8; 4096]).expect("write retired original");

        let outcome = compact(
            &directory.path,
            CompactionOptions::default()
                .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
                .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision")),
        )
        .expect("bounded cleanup");

        // Summed independently here rather than trusting the crate's own
        // predicate: every retained backup form, straight off the directory.
        let mut expected = 0u64;
        for entry in std::fs::read_dir(&directory.path).expect("read directory") {
            let entry = entry.expect("directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let journal_backup = name.starts_with("journal.log.bak.")
                && name.len() == "journal.log.bak.".len() + 3
                && name.ends_with(|last: char| last.is_ascii_digit());
            let archive_backup = std::path::Path::new(&name)
                .extension()
                .is_some_and(|extension| extension == "bak");
            if archive_backup || journal_backup {
                expected += entry.metadata().expect("entry metadata").len();
            }
        }
        assert!(expected >= 4096, "the retired original must be counted");
        assert_eq!(outcome.retained_recovery_backup_bytes, expected);
        // The two figures are disjoint: the archive line falls, while the
        // backup bytes it does not count stay on disk.
        assert!(outcome.archive_bytes_after < outcome.archive_bytes_before);
    }

    #[test]
    fn current_head_reaching_a_one_generation_old_segment_fails_closed() {
        let directory = TestDirectory::repository("generation-invariant");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        // Exactly one generation behind the head: the boundary the retention
        // value moved. At two retained generations the arithmetic spared this
        // child, so the run proceeded on the predicate alone; at one it is
        // reclaimable and only the reference guard keeps the head's own data.
        let mut root_writer = store.record_writer(GarbageCollectionGeneration {
            generation: 1,
            full_generation: 1,
            is_compacted: false,
        });
        let old_root = root_writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("one-generation-old content root");
        root_writer.finish().expect("finish the older generation");
        let mut writer = store.record_writer(GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        });
        let new_head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: old_root,
                },
                &[],
            )
            .expect("new super root");
        writer.finish().expect("finish");
        assert!(store.set_head(store.head(), new_head));
        store.close().expect("close");
        let before = file_bytes(&directory.path);
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("a live one-generation-old child is unsafe at one retained generation");
        assert!(
            error
                .to_string()
                .contains("current head reaches data segment")
        );
        assert_eq!(file_bytes(&directory.path), before);
        Repository::open(&directory.path).expect("refusal leaves repository healthy");
    }
}
