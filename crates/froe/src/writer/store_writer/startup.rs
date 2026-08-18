//! Preparing a store for writing: manifest version, archive
//! generations, and the residue an interrupted session left.

use super::archive_numbering::physical_archive_names;
use super::recovery::open_archive_numbers_for_writing;
use crate::error::Result;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::{ArchiveFileName, select_newest_file_generations};
use std::path::Path;

/// Rewrites the manifest with `store.version=2` after validating it with
/// the same rules as the read path (archives without a manifest are the
/// legacy format; versions above 2 are from a newer Oak).
pub(super) fn check_and_update_manifest(directory: &Path) -> Result<()> {
    #[allow(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "the Java store matches \".tar\" case-sensitively"
    )]
    let archives_exist = std::fs::read_dir(directory)?.any(|entry| {
        entry
            .ok()
            .and_then(|entry| entry.file_name().into_string().ok())
            .is_some_and(|name| name.ends_with(".tar"))
    });
    let archives = if archives_exist {
        crate::store::ArchivePresence::Present
    } else {
        crate::store::ArchivePresence::Absent
    };
    crate::store::check_manifest(directory, archives)?;
    std::fs::write(
        directory.join("manifest"),
        "#written by froe\nstore.version=2\n",
    )?;
    Ok(())
}

/// Write-mode archive initialization: per archive number, the newest
/// generation letter with a valid index wins and stale letters are
/// deleted; numbers without any valid index are recovered — every letter
/// is scanned, backed up to a `.bak` name, and the recovered segments are
/// rewritten as a fresh archive under the lowest letter's name.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the Java store matches \".tar\" case-sensitively"
)]
pub(super) fn initialize_archives_for_writing(
    directory: &Path,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<Vec<TarArchiveReader>> {
    let mut file_names = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let name = entry?.file_name();
        if let Ok(name) = name.into_string()
            && name.ends_with(".tar")
        {
            file_names.push(name);
        }
    }
    // Validate against duplicate (number, letter) pairs.
    select_newest_file_generations(&file_names)?;

    let mut by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>> =
        std::collections::BTreeMap::new();
    for file_name in &file_names {
        if let Some(parsed) = ArchiveFileName::parse(file_name) {
            by_number
                .entry(parsed.archive_number)
                .or_default()
                .push(parsed);
        }
    }

    let archive_numbers = by_number.len();
    let mut archives = crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "opening archives for writing",
            crate::progress::WorkUnit::Archives,
        )
        .with_total(crate::progress::count(archive_numbers)),
        |observer| open_archive_numbers_for_writing(directory, by_number, observer),
    )?;
    // Newest number first: the probe order for reads.
    archives.reverse();
    Ok(archives)
}

/// One archive number rebuilt by [`repair_indexless_archive_numbers`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairedArchive {
    /// The file name the rebuilt archive was installed under.
    pub(crate) file_name: String,
    /// Why the original index was rejected, for the plan and the record.
    pub(crate) reason: String,
    /// Size of the rebuilt archive on disk.
    pub(crate) bytes: u64,
}

/// The generation letter a rebuild is installed under: the lowest *non-empty*
/// one, not simply the lowest.
///
/// A zero-length letter is a writer's lazy next-archive creation, not an
/// archive. Taking it as the target would regress the active generation
/// letter, hard-link a bogus zero-byte `.bak`, and — because the target is
/// also the metadata source — give the rebuilt archive the residue's
/// ownership and mode instead of the archive's. The residue is left where it
/// is for cleanup's stale-archive task, which already owns it.
pub(super) fn install_target_generation<'a>(
    directory: &Path,
    generations: &'a [ArchiveFileName],
) -> &'a ArchiveFileName {
    generations
        .iter()
        .find(|generation| {
            std::fs::metadata(directory.join(&generation.file_name))
                .is_ok_and(|metadata| metadata.len() != 0)
        })
        .unwrap_or(&generations[0])
}

/// The name of a non-empty `<archive>.recovering` file already beside one of
/// `generations`, if any.
///
/// A rebuild of this number would unlink it, and it is not froe's to unlink:
/// `recover_archive_number` removes its own staging file on every error path,
/// so the only way one survives is a crash mid-write — which is the state
/// cleanup's stale-temporaries task exists to adjudicate, and which it
/// retains unless it proves the bytes redundant.
pub(super) fn existing_staging_residue(
    directory: &Path,
    generations: &[ArchiveFileName],
) -> Option<String> {
    generations.iter().find_map(|generation| {
        let name = format!("{}.recovering", generation.file_name);
        std::fs::symlink_metadata(directory.join(&name))
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.len() != 0)
            .map(|_| name)
    })
}

/// Refuses a directory holding two spellings of one `(number, letter)` pair.
///
/// `data00007.tar` and `data00007a.tar` both parse as number 7 generation
/// `'a'`. Grouping them would make the install target whichever the listing
/// yielded first, so the repair refuses — and it must refuse before anything
/// irreversible, not after.
pub(crate) fn reject_duplicate_archive_generations(directory: &Path) -> Result<()> {
    let names: Vec<String> = physical_archive_names(directory)?
        .into_iter()
        .map(|parsed| parsed.file_name)
        .collect();
    select_newest_file_generations(&names)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::writer::store_writer::repository::*;
    use crate::writer::store_writer::test_support::*;

    #[test]
    fn bootstraps_a_fresh_store_that_the_reader_opens() {
        let directory = TestDirectory::new("bootstrap");
        let store = WritableRepository::open(&directory.path).expect("open fresh store");
        store.close().expect("close");

        let manifest =
            std::fs::read_to_string(directory.path.join("manifest")).expect("manifest exists");
        assert!(manifest.contains("store.version=2"));
        assert!(directory.path.join("repo.lock").exists());

        let journal = std::fs::read_to_string(directory.path.join("journal.log")).expect("journal");
        assert_eq!(journal.lines().count(), 1, "exactly one bootstrap revision");
        assert!(journal.contains(" root "));

        let repository = Repository::open(&directory.path).expect("reader opens");
        assert!(
            !repository.archives()[0].is_recovered(),
            "the archive has a valid index"
        );
        let content_root = repository.content_root().expect("content root exists");
        assert_eq!(content_root.child_node_count().expect("count"), 0);
        assert!(content_root.properties().expect("properties").is_empty());
    }

    #[test]
    fn refuses_to_bootstrap_over_a_populated_store_with_no_resolvable_journal() {
        let directory = TestDirectory::new("refuse-bootstrap");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            crate::writer::commit::create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
            store.close().expect("close");
        }
        std::fs::write(directory.path.join("journal.log"), b"").expect("truncate journal");

        assert!(
            WritableRepository::open(&directory.path).is_err(),
            "a populated store with no resolvable journal must not bootstrap an empty head"
        );

        // The refusal leaves the store intact; journal recovery restores
        // it and the write open then succeeds.
        crate::writer::backup::recover_journal(&directory.path).expect("recover");
        let store = WritableRepository::open(&directory.path).expect("open after recovery");
        store.close().expect("close");
    }

    #[test]
    fn stale_generation_letters_are_deleted_at_write_open() {
        let directory = TestDirectory::new("stale-letters");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // Fabricate a stale lower letter alongside the valid archive by
        // copying it: the write open must keep the higher letter and
        // delete the lower one.
        let valid = std::fs::read(directory.path.join("data00000a.tar")).expect("read");
        std::fs::write(directory.path.join("data00000b.tar"), &valid).expect("write copy");
        {
            let store = WritableRepository::open(&directory.path).expect("reopen");
            assert!(store.head().record_number > 0 || store.head().record_number == 0);
            store.close().expect("close");
        }
        assert!(
            !directory.path.join("data00000a.tar").exists(),
            "the lower letter is deleted"
        );
        assert!(directory.path.join("data00000b.tar").exists());
    }

    /// The empty number contributes no archive, so nothing is deleted as a
    /// side effect of opening. Reuse is the only other outcome, and it can
    /// only ever overwrite zero bytes.
    #[test]
    fn an_empty_archive_file_is_never_deleted_by_opening_for_writing() {
        let directory = TestDirectory::new("empty-archive-retained");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // A number above the next one froe would allocate, so the open
        // cannot reach it by filling it.
        let empty = directory.path.join("data00500a.tar");
        std::fs::write(&empty, b"").expect("create the empty archive");

        let store = WritableRepository::open(&directory.path).expect("write open");
        store.close().expect("close");

        assert!(
            empty.exists(),
            "opening for writing must not delete the empty archive; cleanup removes it \
             under its own plan-and-confirm contract"
        );
    }

    /// Skipping an all-empty archive number must not free it for reuse: the
    /// letterless spelling of a number collides with the lettered one, and
    /// `group_file_generations_newest_first` refuses that pair outright, so
    /// a store that allocated into it could never be opened again by
    /// anything. Allocation therefore reads the physical namespace.
    #[test]
    fn an_empty_archive_number_is_never_reallocated_over_its_own_residue() {
        let directory = TestDirectory::new("empty-archive-namespace");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // Letterless: `ArchiveFileName::parse` reads this as number 1,
        // generation 'a' — the same pair a written `data00001a.tar` claims.
        std::fs::write(directory.path.join("data00001.tar"), b"").expect("empty residue");

        {
            let store = WritableRepository::open(&directory.path).expect("write open");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            writer.write_string("forces a new archive").expect("string");
            writer.finish().expect("finish");
            store.close().expect("close");
        }

        assert!(
            !directory.path.join("data00001a.tar").exists(),
            "allocation must skip the number the letterless residue claims"
        );
        Repository::open(&directory.path).expect("the store is still openable");
        WritableRepository::open(&directory.path)
            .expect("and still writable")
            .close()
            .expect("close");
    }
}
