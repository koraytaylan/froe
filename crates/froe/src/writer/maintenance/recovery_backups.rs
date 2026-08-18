//! Planning the recovery backups a destructive run takes first.

use super::options::RecoveryBackupPolicy;
use super::planning::{PlannedFileRemoval, file_fingerprint};
use crate::error::Result;
use crate::tar_archive::file_name::ArchiveFileName;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

pub(super) fn files_are_identical(left: &Path, right: &Path) -> Result<bool> {
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

pub(super) fn recovery_backup_target(name: &str) -> Option<String> {
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

pub(super) fn is_oak_archive_backup_counter(counter: &str) -> bool {
    counter
        .parse::<i32>()
        .is_ok_and(|parsed| parsed >= 2 && parsed.to_string().as_bytes() == counter.as_bytes())
}

pub(super) fn plan_recovery_backups(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Repository;
    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::plan::*;
    use crate::writer::maintenance::prepared::*;
    use crate::writer::maintenance::test_support::*;
    use std::num::NonZeroUsize;

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
}
