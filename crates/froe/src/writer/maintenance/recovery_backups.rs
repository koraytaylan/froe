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
}
