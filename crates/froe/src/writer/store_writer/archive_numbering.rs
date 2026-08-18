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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::repository_lock::RepositoryLock;

    use crate::writer::store_writer::repository::*;

    use crate::writer::store_writer::test_support::*;
    use std::sync::Arc;

    #[test]
    fn archive_number_exhaustion_never_wraps_or_truncates_archive_zero() {
        let directory = TestDirectory::new("archive-number-exhaustion");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let archive_zero = directory.path.join("data00000a.tar");
        let archive_max = directory.path.join("data4294967295a.tar");
        std::fs::copy(&archive_zero, &archive_max).expect("install maximum-number fixture");
        let zero_before = std::fs::read(&archive_zero).expect("archive zero before");
        let max_before = std::fs::read(&archive_max).expect("archive max before");
        let journal_before =
            std::fs::read(directory.path.join("journal.log")).expect("journal before");

        {
            let store = WritableRepository::open(&directory.path).expect("normal open at max");
            assert_eq!(store.lock_write_state().next_archive_number, None);
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("buffer node");
            let Err(error) = writer.finish() else {
                panic!("normal writer must refuse namespace wrap");
            };
            assert!(error.to_string().contains("namespace is exhausted"));
            store.close().expect("unchanged normal store closes");
        }

        let error = next_cleanup_archive_number(&directory.path)
            .expect_err("prepared cleanup planning must refuse namespace wrap");
        assert!(error.to_string().contains("namespace is exhausted"));

        assert_eq!(
            std::fs::read(&archive_zero).expect("archive zero after"),
            zero_before
        );
        assert_eq!(
            std::fs::read(&archive_max).expect("archive max after"),
            max_before
        );
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("journal after"),
            journal_before
        );
        assert!(!directory.path.join("data00001a.tar").exists());
    }
    #[test]
    fn prepared_writer_never_truncates_a_next_archive_occupied_after_open() {
        let directory = TestDirectory::new("prepared-next-archive-race");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        assert_eq!(store.lock_write_state().next_archive_number, Some(1));

        let occupied_path = directory.path.join("data00001a.tar");
        let residue = b"interrupted writer recovery evidence";
        std::fs::write(&occupied_path, residue).expect("occupy planned next archive after open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("buffer node");
        let Err(error) = writer.finish() else {
            panic!("exclusive prepared writer must reject the occupied path");
        };
        assert!(error.to_string().contains("exists"));
        assert_eq!(
            std::fs::read(&occupied_path).expect("read occupied path"),
            residue,
            "neither the failed write nor later close may truncate or rewrite residue"
        );
        store.close().expect("unchanged prepared store closes");
        drop(repository_lock);
        assert_eq!(
            std::fs::read(occupied_path).expect("read residue after close"),
            residue
        );
    }
    #[test]
    fn prepared_open_rechecks_certified_archive_aliases_and_higher_numbers() {
        for (case, occupied_name, expected_error) in [
            (
                "prepared-certified-lettered-alias",
                "data00001a.tar",
                "output alias",
            ),
            (
                "prepared-certified-letterless-alias",
                "data00001.tar",
                "output alias",
            ),
            (
                "prepared-certified-higher-number",
                "data00002b.tar",
                "at or above",
            ),
        ] {
            let directory = TestDirectory::new(case);
            {
                let store = WritableRepository::open(&directory.path).expect("bootstrap");
                store.close().expect("close bootstrap");
            }
            let certified =
                next_cleanup_archive_number(&directory.path).expect("initial certificate");
            assert_eq!(certified, 1);
            let repository_lock =
                Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
            std::fs::write(directory.path.join(occupied_name), b"")
                .expect("occupy namespace after certification");

            let Err(error) = WritableRepository::open_prepared(
                &directory.path,
                Arc::clone(&repository_lock),
                certified,
            ) else {
                panic!("strict prepared open must reject {occupied_name}");
            };
            assert!(error.to_string().contains(expected_error), "{error}");
        }
    }
}
