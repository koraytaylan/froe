//! What a repository directory must look like before a plan is built,
//! and the fingerprint of every entry the prepared session later
//! rechecks.

#[cfg(unix)]
use super::MetadataExt;
use super::{
    ArchiveFileName, CompactionOptions, Error, MaintenanceTask, Metadata, OsStr, OsString, Path,
    PathBuf, Result, SystemTime, recovery_backup_target, temporary_kind,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::writer::maintenance) struct DirectoryFingerprint {
    pub(in crate::writer::maintenance) entries: Vec<FileFingerprint>,
    #[cfg(unix)]
    pub(in crate::writer::maintenance) device: u64,
    #[cfg(unix)]
    pub(in crate::writer::maintenance) inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::writer::maintenance) struct FileFingerprint {
    pub(in crate::writer::maintenance) name: OsString,
    pub(in crate::writer::maintenance) kind: u8,
    pub(in crate::writer::maintenance) length: u64,
    pub(in crate::writer::maintenance) modified: Option<SystemTime>,
    #[cfg(unix)]
    pub(in crate::writer::maintenance) device: u64,
    #[cfg(unix)]
    pub(in crate::writer::maintenance) inode: u64,
    #[cfg(unix)]
    pub(in crate::writer::maintenance) change_time_seconds: i64,
    #[cfg(unix)]
    pub(in crate::writer::maintenance) change_time_nanoseconds: i64,
}

pub(in crate::writer::maintenance) fn validate_options(options: &CompactionOptions) -> Result<()> {
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

pub(in crate::writer::maintenance) fn canonical_repository_directory(
    directory: &Path,
) -> Result<PathBuf> {
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

pub(in crate::writer::maintenance) fn validate_repository_shape(directory: &Path) -> Result<()> {
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

pub(in crate::writer::maintenance) fn validate_managed_file_types(directory: &Path) -> Result<()> {
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

pub(in crate::writer::maintenance) fn is_managed_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(name, "manifest" | "journal.log" | "gc.log" | "repo.lock")
        || ArchiveFileName::parse(name).is_some()
        || temporary_kind(name).is_some()
        || recovery_backup_target(name).is_some()
}

pub(in crate::writer::maintenance) fn directory_fingerprint(
    directory: &Path,
) -> Result<DirectoryFingerprint> {
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

pub(in crate::writer::maintenance) fn file_fingerprint(
    name: OsString,
    metadata: &Metadata,
) -> FileFingerprint {
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;
    use crate::store::Repository;
    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::prepared::*;
    use crate::writer::maintenance::test_support::*;

    #[cfg(unix)]
    #[test]
    fn non_utf8_backup_like_name_is_not_promoted_into_the_managed_allowlist() {
        use std::os::unix::ffi::OsStrExt as _;

        let hostile = std::ffi::OsStr::from_bytes(b"data00000a.tar.\xff.ro.bak");
        assert!(!is_managed_name(hostile));
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
}
