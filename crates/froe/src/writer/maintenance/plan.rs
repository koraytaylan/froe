//! What a planned run reports: the actions it would take, why an
//! archive or journal line is stale, and what the applied run did.

use super::options::MaintenanceTask;
use super::planning::{
    CheckpointPlan, DirectoryFingerprint, JournalPlan, PlannedFileRemoval, StaleArchive,
};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::writer::compaction::CompactionKind;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::StandaloneSegmentCompactionPlan;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
    pub(super) line_number: usize,
    pub(super) record_identifier: Option<RecordIdentifier>,
    pub(super) reason: JournalRemovalReason,
    pub(super) preview: Vec<u8>,
    pub(super) preview_truncated: bool,
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

pub(super) const ALREADY_ABSENT_DELETION_DETAIL: &str =
    "file was already absent when deletion was attempted";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileDeletionFailureKind {
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
    pub(super) file_name: String,
    pub(super) error: String,
    pub(super) kind: FileDeletionFailureKind,
}

impl FileDeletionFailure {
    pub(super) fn retained(file_name: String, error: impl Into<String>) -> Self {
        Self {
            file_name,
            error: error.into(),
            kind: FileDeletionFailureKind::Retained,
        }
    }

    pub(super) fn already_absent(file_name: String, error: impl Into<String>) -> Self {
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
pub(super) struct RetainedReclaimable {
    /// Segments kept by Oak's 25% savings gate.
    pub(super) below_savings_gate: usize,
    /// Segments kept because the archive exhausted the `a`–`z` namespace.
    pub(super) at_last_generation: usize,
    /// Segments kept because another generation pathname is occupied.
    pub(super) blocked_by_occupied_generation: usize,
    /// TAR entry bytes those segments occupy, summed across every reason.
    pub(super) bytes: u64,
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
pub(super) struct HistoryProtection {
    /// Data segments reachable only from a historical journal revision, and
    /// not from the current head.
    pub(super) history_only_segments: usize,
    /// Segments this same sweep would physically free with the veto lifted
    /// and nothing else changed. Measured by replanning rather than reasoned
    /// about: the veto holds bulk segments only through the data segments
    /// that reference them, and releasing more of an archive can carry it
    /// over the 25% rewrite gate. Counting protected data segments alone
    /// understates this by orders of magnitude on a store whose history
    /// holds inline binaries.
    pub(super) would_be_reclaimable_segments: usize,
    /// Bytes those segments occupy, whole archive files included where the
    /// unvetoed sweep would unlink one outright.
    pub(super) would_be_reclaimable_bytes: u64,
}

/// A strictly read-only cleanup analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionPlan {
    pub(super) directory: PathBuf,
    pub(super) tasks: Vec<MaintenanceTask>,
    pub(super) current_head: RecordIdentifier,
    pub(super) actions: Vec<CompactionAction>,
    pub(super) warnings: Vec<String>,
    pub(super) estimated_reclaimable_bytes: u64,
    pub(super) estimated_archive_rewrite_source_bytes: u64,
    pub(super) retained_reclaimable: RetainedReclaimable,
    pub(super) history_protection: HistoryProtection,
    pub(super) fingerprint: DirectoryFingerprint,
    pub(super) journal: JournalPlan,
    pub(super) checkpoints: CheckpointPlan,
    pub(super) checkpoint_archive_number: Option<u32>,
    pub(super) stale_archives: Vec<StaleArchive>,
    pub(super) temporaries: Vec<PlannedFileRemoval>,
    pub(super) recovery_backups: Vec<PlannedFileRemoval>,
    pub(super) segment_plan: Option<StandaloneSegmentCompactionPlan>,
    /// The sweep that retires an interrupted earlier compaction's output,
    /// applied before this run's own copy.
    pub(super) residue_sweep: Option<StandaloneSegmentCompactionPlan>,
    pub(super) reference_generation: GarbageCollectionGeneration,
    pub(super) protected_history_segments: HashSet<SegmentIdentifier>,
    pub(super) manifest_upgrade: bool,
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
    pub(super) removed_segments: usize,
    pub(super) journal_backup_path: Option<PathBuf>,
    pub(super) deletion_failures: Vec<FileDeletionFailure>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Repository;

    use crate::writer::maintenance::options::*;

    use crate::writer::maintenance::prepared::*;

    use crate::writer::maintenance::test_support::*;
    use std::io::Write as _;
    use std::num::NonZeroUsize;

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
    fn deletion_absence_state_does_not_depend_on_diagnostic_text() {
        let retained = super::FileDeletionFailure::retained(
            "data00000a.tar".to_owned(),
            ALREADY_ABSENT_DELETION_DETAIL,
        );
        let absent = super::FileDeletionFailure::already_absent(
            "data00001a.tar".to_owned(),
            "a deliberately different ENOENT diagnostic",
        );

        assert!(!retained.target_was_already_absent());
        assert!(absent.target_was_already_absent());
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
}
