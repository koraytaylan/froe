//! Choosing the next archive number and letter, and refusing a cleanup
//! target number that the store's own files contradict.

use crate::error::{Error, Result};
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::ArchiveFileName;
use std::path::Path;

/// Returns the next unused archive number. `None` is the explicit exhausted
/// state after an active `u32::MAX` archive; wrapping to zero is never valid.
pub(super) fn next_archive_number(archives: &[TarArchiveReader]) -> Option<u32> {
    match archives
        .iter()
        .filter_map(|archive| ArchiveFileName::parse(archive.file_name()))
        .map(|name| name.archive_number)
        .max()
    {
        None => Some(0),
        Some(maximum) => maximum.checked_add(1),
    }
}

/// Computes the first archive number above every physical Oak archive name in
/// `directory`, without opening or selecting any of those archives. Prepared
/// cleanup uses this stronger namespace view so a zero-byte, invalid-index, or
/// otherwise unselected residue can never be reused as checkpoint output.
pub(crate) fn next_cleanup_archive_number(directory: &Path) -> Result<u32> {
    let maximum = physical_archive_names(directory)?
        .into_iter()
        .map(|name| name.archive_number)
        .max();
    match maximum {
        None => Ok(0),
        Some(maximum) => maximum.checked_add(1).ok_or_else(|| Error::InvalidFormat {
            details: "the physical archive-number namespace is exhausted at u32::MAX; cleanup cannot allocate a checkpoint output archive"
                .to_owned(),
        }),
    }
}

/// The next archive number a write session may allocate: above every
/// physical Oak archive name in `directory` *and* above every archive the
/// session actually opened. `None` is the explicit exhausted state.
///
/// Opening deliberately serves fewer archives than the directory holds — an
/// archive number whose every generation letter is empty contributes none —
/// so allocating out of the opened set alone would hand back a number a
/// residue file still claims. For the letterless spelling that collision is
/// unrecoverable rather than untidy: `data00007.tar` and a freshly written
/// `data00007a.tar` both parse as number 7 generation `'a'`, which
/// `group_file_generations_newest_first` refuses outright, so every later
/// open of the store fails. Cleanup allocates from the same stronger view;
/// see `next_cleanup_archive_number`.
pub(super) fn next_physical_archive_number(
    directory: &Path,
    opened: &[TarArchiveReader],
) -> Result<Option<u32>> {
    let physical = physical_archive_names(directory)?
        .into_iter()
        .map(|name| name.archive_number)
        .max();
    // The opened set is a subset of the physical names, so this only ever
    // agrees with `physical`. It is consulted anyway so the two views can
    // never silently drift apart.
    let maximum = match (physical, next_archive_number(opened)) {
        (None, next) => return Ok(next),
        (Some(physical), _) => physical,
    };
    Ok(maximum.checked_add(1))
}

pub(super) fn physical_archive_names(directory: &Path) -> Result<Vec<ArchiveFileName>> {
    let mut archives = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some(archive) = ArchiveFileName::parse(&file_name) {
            archives.push(archive);
        }
    }
    Ok(archives)
}

/// Rechecks a plan-certified checkpoint output number immediately before the
/// strict writer is opened. Earlier cleanup phases may remove physical names,
/// but none may introduce a number at or above the certificate. Both spellings
/// of generation `a` are checked explicitly because Oak treats a missing
/// letter as `a`.
pub(super) fn validate_cleanup_archive_number(directory: &Path, certified: u32) -> Result<()> {
    for alias in [
        format!("data{certified:05}a.tar"),
        format!("data{certified:05}.tar"),
    ] {
        match std::fs::symlink_metadata(directory.join(&alias)) {
            Ok(_) => {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "certified checkpoint output alias {alias} is occupied; refusing prepared cleanup"
                    ),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    if let Some(conflict) = physical_archive_names(directory)?
        .into_iter()
        .filter(|name| name.archive_number >= certified)
        .max_by_key(|name| (name.archive_number, name.file_generation))
    {
        return Err(Error::InvalidFormat {
            details: format!(
                "physical archive {} has number {} at or above the certified checkpoint output number {certified}; refusing prepared cleanup",
                conflict.file_name, conflict.archive_number
            ),
        });
    }
    Ok(())
}
