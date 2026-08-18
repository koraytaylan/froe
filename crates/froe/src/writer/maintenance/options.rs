//! Operator-facing maintenance options: which tasks run, what a
//! recovery backup costs, and how many journal revisions survive.

use crate::writer::compaction::CompactionKind;
use crate::writer::store_writer::ArchiveRewritePolicy;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::time::Duration;

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
    pub(super) tasks: BTreeSet<MaintenanceTask>,
    pub(super) recovery_backup_policy: Option<RecoveryBackupPolicy>,
    pub(super) journal_revision_retention: Option<NonZeroUsize>,
    pub(super) archive_rewrite_policy: ArchiveRewritePolicy,
    pub(super) compaction_kind: Option<CompactionKind>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::num::NonZeroUsize;

    use crate::writer::maintenance::plan::*;

    use crate::writer::maintenance::prepared::*;

    use crate::writer::maintenance::test_support::*;
    use std::fs::File;
    use std::time::SystemTime;

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
}
