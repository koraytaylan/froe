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
