//! Classifying and planning removal of the temporaries an interrupted
//! run leaves behind.

use super::manifest::manifest_upgrade_bytes;
use super::planning::{PlannedFileRemoval, file_fingerprint};
use super::recovery_backups::files_are_identical;
use crate::error::Result;
use crate::progress::ProgressObserver;
use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::maintenance::journal::{
    RawJournal, RawJournalLineClassification, scan_raw_journal_file,
};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TemporaryKind {
    Journal,
    RecoveringArchive,
    Manifest,
}

pub(super) fn temporary_kind(name: &str) -> Option<TemporaryKind> {
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

pub(super) fn plan_stale_temporaries(
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
