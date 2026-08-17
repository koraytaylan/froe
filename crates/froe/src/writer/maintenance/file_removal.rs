//! Removing planned files, verifying each target is still the file the
//! plan certified before it is unlinked.

use super::plan::{ALREADY_ABSENT_DELETION_DETAIL, FileDeletionFailure};
use super::planning::{FileFingerprint, PlannedFileRemoval, add_estimate, file_fingerprint};
use super::recovery_backups::recovery_backup_target;
use crate::error::{Error, Result};
use crate::progress::ProgressObserver;
use crate::tar_archive::file_name::ArchiveFileName;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlannedFileRemovalFailureMode {
    RequireCertifiedTarget,
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlannedFileTargetVerification {
    Exact,
    Absent,
}

pub(super) fn verify_planned_file_target(
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

pub(super) fn accept_planned_file_verification(
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

pub(super) fn record_planned_file_removal_failure(
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

pub(super) fn remove_planned_files(
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

pub(super) fn remove_planned_files_with(
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
pub(super) fn remove_planned_files_with_after_open(
    directory: &Path,
    files: impl IntoIterator<Item = PlannedFileRemoval>,
    failure_mode: PlannedFileRemovalFailureMode,
    after_open: impl FnMut(&Path, usize),
    unlink: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(usize, Vec<FileDeletionFailure>)> {
    remove_planned_files_core(directory, files, failure_mode, after_open, unlink)
}

pub(super) fn remove_planned_files_core(
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

pub(super) fn archive_file_bytes(directory: &Path) -> Result<u64> {
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
pub(super) fn recovery_backup_file_bytes(directory: &Path) -> Result<u64> {
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

pub(super) fn read_optional_regular_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(std::fs::read(path)?)),
        Ok(_) => Err(Error::InvalidFormat {
            details: format!("{} is not a regular file", path.display()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
