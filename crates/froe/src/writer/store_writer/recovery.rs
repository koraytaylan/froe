//! Recovering an archive number whose newest generation cannot be
//! opened, by scanning the segments still readable in it.

use super::file_identity::preserve_file_metadata;
use super::providers::ArchiveSegmentsProvider;
use super::providers::read_blob_identifiers;
use super::repair::{AuthorizeVersionTwoWrite, VersionTwoAlreadyEstablished};
use super::startup::install_target_generation;
use crate::content::provider::SegmentProvider as _;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::tar_writer::TarArchiveWriter;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Picks the generation letter of one archive number to write against:
/// newest letter first, the first valid index wins. Also reports whether
/// any letter held bytes at all.
///
/// Zero-length letters are skipped exactly as the read path skips them
/// (`crate::store::open_archives_newest_valid_first`): a writer creates its
/// next archive lazily, and an empty file is that creation's race window —
/// or what it leaves behind when it is killed inside it. Opening one yields
/// no segments, so recovering the number would rebuild it as an archive
/// with no entries, which is not a file `TarArchiveWriter` ever creates.
pub(super) fn select_writable_generation(
    directory: &Path,
    generations: &[ArchiveFileName],
) -> (Option<TarArchiveReader>, bool) {
    let mut any_nonempty = false;
    for candidate in generations.iter().rev() {
        let path = directory.join(&candidate.file_name);
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == 0) {
            continue;
        }
        any_nonempty = true;
        if let Ok(reader) = TarArchiveReader::open(&path)
            && !reader.is_recovered()
        {
            return (Some(reader), any_nonempty);
        }
    }
    (None, any_nonempty)
}

/// Opens the winning generation letter of each archive number, deleting
/// the losers, and reports one completed archive number at a time.
pub(super) fn open_archive_numbers_for_writing(
    directory: &Path,
    by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>>,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<Vec<TarArchiveReader>> {
    let archive_numbers = by_number.len();
    let mut archives = Vec::new();
    for (opened, (_, mut generations)) in by_number.into_iter().enumerate() {
        observer.step_advanced(crate::progress::count(opened));
        generations.sort_by_key(|name| name.file_generation);
        let (winner, any_nonempty) = select_writable_generation(directory, &generations);
        match winner {
            Some(reader) => {
                // Delete every other generation letter of this number.
                for stale in &generations {
                    if stale.file_name != reader.file_name() {
                        std::fs::remove_file(directory.join(&stale.file_name))?;
                    }
                }
                archives.push(reader);
            }
            // Every letter of this number is empty, so there is nothing to
            // recover and nothing to serve. Nothing is deleted here: the
            // number simply contributes no archive, which either frees it
            // for the next write to fill — the same thing the writer that
            // created it was about to do — or leaves the files for
            // cleanup's stale-archive task to remove under its own
            // plan-and-confirm contract. Reuse can only ever land on a
            // zero-byte file, because a single non-empty letter sends the
            // whole number down the recovery path instead.
            None if !any_nonempty => {}
            None => {
                archives.push(recover_archive_number(
                    directory,
                    &generations,
                    &mut VersionTwoAlreadyEstablished,
                )?);
            }
        }
    }
    observer.step_advanced(crate::progress::count(archive_numbers));
    Ok(archives)
}

/// The refusal an archive number earns when it holds bytes but no segment
/// the recovery scan can read. Naming the files matters more than usual
/// here: the operator has to decide whether to move them aside or keep them
/// as evidence, and neither the number nor an errno tells them which files
/// are involved.
///
/// The remedy deliberately does not name cleanup. `plan_stale_archives`
/// marks only *zero-byte* letters of an unindexed number stale; a non-empty
/// letter is preserved with a warning, precisely because it may still hold
/// unrecovered bytes. Telling the operator to run cleanup here would send
/// them to a command that will decline to act.
pub(super) fn unrecoverable_archive_number_refusal(generations: &[ArchiveFileName]) -> Error {
    let names: Vec<&str> = generations
        .iter()
        .map(|generation| generation.file_name.as_str())
        .collect();
    Error::InvalidFormat {
        details: format!(
            "archive number {} has no valid index and no recoverable segment in {}; \
             refusing to replace it with an empty archive. Cleanup preserves this file \
             rather than removing it, so opening the store for writing needs it moved \
             aside — keep it, it is the only copy of whatever it holds",
            generations.first().map_or(0, |first| first.archive_number),
            names.join(", ")
        ),
    }
}

/// Recovers one archive number with no valid index: scans every letter in
/// ascending order (later letters overwrite duplicates), rebuilds the
/// recovered segments as a fresh archive, and only after that archive is
/// written, fsynced, and re-validated are the originals retired to
/// `.bak` names and the replacement installed under the lowest letter's
/// file name. A failure before installation leaves every original in
/// place; a failure during installation rolls back best-effort (see
/// [`install_recovered_archive`]).
/// Gives a rebuilt archive the ownership and mode of the archive it replaces,
/// rather than the process umask.
///
/// Every other replacement path in maintenance does this; without it a store
/// whose archives are group-owned and setgid silently loses both on the one
/// file that was rewritten, and a later cleanup's apply-identity preflight
/// reads the wrong metadata. A target that does not exist yet has nothing to
/// inherit, which is not an error.
pub(super) fn inherit_replaced_archive_metadata(
    directory: &Path,
    target_name: &str,
    temporary_path: &Path,
) -> Result<()> {
    let Ok(source_metadata) = std::fs::metadata(directory.join(target_name)) else {
        return Ok(());
    };
    let staged = std::fs::OpenOptions::new()
        .write(true)
        .open(temporary_path)?;
    preserve_file_metadata(&staged, &source_metadata)
}

/// Charges the caller's version-2 price at the last instant before a rebuilt
/// archive becomes visible.
///
/// The staged rebuild already exists, is durable, and has re-opened with a
/// valid index; nothing version-2 is visible yet. If authorization fails the
/// staging file is removed like every other pre-install failure, so the
/// number is left exactly as it was found.
pub(super) fn authorize_before_install(
    authorize: &mut dyn AuthorizeVersionTwoWrite,
    temporary_path: &Path,
) -> Result<()> {
    if let Err(error) = authorize.authorize() {
        let _ = std::fs::remove_file(temporary_path);
        return Err(error);
    }
    Ok(())
}

pub(super) fn recover_archive_number(
    directory: &Path,
    generations: &[ArchiveFileName],
    authorize: &mut dyn AuthorizeVersionTwoWrite,
) -> Result<TarArchiveReader> {
    let recovered = scan_recoverable_segments(directory, generations);
    // A non-empty file that yields no segment is residue this function
    // cannot act on: writing the replacement would produce an archive with
    // no entries, which `TarArchiveWriter` never creates at all, and the
    // re-open below would then fail on a missing path with a bare errno.
    // Refuse with the file names instead, and say what clears them.
    if recovered.is_empty() {
        return Err(unrecoverable_archive_number_refusal(generations));
    }

    // Parse every segment once — data *and* bulk, so blob identifier
    // strings whose block lists spill into bulk segments resolve too.
    // The parsed structures also back the provider that resolves blob
    // identifier strings across the recovered segments of this archive
    // number.
    let mut parsed_segments: HashMap<SegmentIdentifier, Arc<ParsedSegment>> = HashMap::new();
    for (identifier, bytes) in &recovered {
        parsed_segments.insert(
            *identifier,
            Arc::new(ParsedSegment::parse(*identifier, bytes)?),
        );
    }
    let provider = ArchiveSegmentsProvider {
        segments: recovered
            .iter()
            .filter_map(|(identifier, bytes)| {
                parsed_segments
                    .get(identifier)
                    .map(|parsed| (*identifier, (Arc::clone(parsed), bytes.as_slice())))
            })
            .collect(),
    };

    // Build the replacement beside the originals; nothing is renamed or
    // deleted until it exists, is durable, and re-opens with a valid
    // index.
    let target_name = &install_target_generation(directory, generations).file_name;
    let temporary_name = format!("{target_name}.recovering");
    let temporary_path = directory.join(&temporary_name);
    let _ = std::fs::remove_file(&temporary_path);
    let write_replacement =
        || -> Result<()> {
            let mut writer = TarArchiveWriter::new(directory, &temporary_name);
            for (identifier, bytes) in &recovered {
                let (generation, references, binary_references) =
                    if let Some(parsed) = parsed_segments.get(identifier) {
                        // Fail closed when a blob identifier cannot be
                        // resolved: publishing an incomplete catalog would let
                        // AEM's blob garbage collection delete a
                        // still-referenced binary.
                        let segment = provider.segment(*identifier)?;
                        let binary_references = read_blob_identifiers(&provider, &segment)
                            .map_err(|error| Error::InvalidFormat {
                                details: format!(
                                    "cannot rebuild the binary references catalog while \
                                     recovering {target_name}: an external blob identifier in \
                                     segment {identifier} does not resolve within the recovered \
                                     segments ({error}); refusing to publish an incomplete \
                                     catalog, which could let blob garbage collection delete \
                                     referenced binaries"
                                ),
                            })?;
                        (
                            GarbageCollectionGeneration {
                                generation: parsed.generation,
                                full_generation: parsed.full_generation,
                                is_compacted: parsed.is_compacted,
                            },
                            parsed.referenced_segments.clone(),
                            binary_references,
                        )
                    } else {
                        (
                            GarbageCollectionGeneration {
                                generation: 0,
                                full_generation: 0,
                                is_compacted: false,
                            },
                            Vec::new(),
                            Vec::new(),
                        )
                    };
                writer.write_segment(
                    *identifier,
                    bytes,
                    generation,
                    &references,
                    &binary_references,
                )?;
            }
            writer.close()?;
            Ok(())
        };
    if let Err(error) = write_replacement() {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = inherit_replaced_archive_metadata(directory, target_name, &temporary_path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    crate::writer::compaction::fsync_directory(directory);
    match TarArchiveReader::open(&temporary_path) {
        Ok(validated) if !validated.is_recovered() => drop(validated),
        Ok(_) => {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(Error::InvalidFormat {
                details: format!("the rebuilt archive {temporary_name} failed index validation"),
            });
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error);
        }
    }
    authorize_before_install(authorize, &temporary_path)?;
    install_recovered_archive(directory, generations, target_name, &temporary_path)
}

/// Scans every generation letter of one archive number in ascending
/// order — later letters overwrite duplicate segments — returning the
/// recovered segments in scan order.
/// Whether a rebuild of this archive number would find anything to rebuild
/// from — the same question [`scan_recoverable_segments`] answers, without
/// materializing an answer nobody wants.
///
/// The scan copies every segment's bytes into owned buffers, so asking it
/// this question allocates the whole archive to discard it, and does so in
/// the survey that now runs before every repair. `segment_count()` on a
/// recovery-scanned reader is the length of that same scan's entry list,
/// read straight off the memory map. It is also exactly what the cleanup
/// side reads off its already-open readers, so both callers now derive the
/// predicate the same way and cannot drift apart.
pub(super) fn any_recoverable_segment(directory: &Path, generations: &[ArchiveFileName]) -> bool {
    generations.iter().any(|generation| {
        TarArchiveReader::open(&directory.join(&generation.file_name))
            .is_ok_and(|reader| reader.segment_count() > 0)
    })
}

pub(super) fn scan_recoverable_segments(
    directory: &Path,
    generations: &[ArchiveFileName],
) -> Vec<(SegmentIdentifier, Vec<u8>)> {
    let mut recovered: Vec<(SegmentIdentifier, Vec<u8>)> = Vec::new();
    let mut positions: HashMap<SegmentIdentifier, usize> = HashMap::new();
    for generation in generations {
        let path = directory.join(&generation.file_name);
        if let Ok(reader) = TarArchiveReader::open(&path) {
            for identifier in reader.segment_identifiers() {
                if let Some(bytes) = reader.segment_data(identifier) {
                    if let Some(&position) = positions.get(&identifier) {
                        recovered[position].1 = bytes.to_vec();
                    } else {
                        positions.insert(identifier, recovered.len());
                        recovered.push((identifier, bytes.to_vec()));
                    }
                }
            }
        }
    }
    recovered
}

/// Retires the original generation letters to `.bak` names and installs
/// the validated replacement under the target name. The target's own
/// original is preserved through a hard link (or, on filesystems without
/// hard links, a full copy), so a `.tar` under the target name exists at
/// every instant; the other letters are plain renames. An *error* at any
/// step — including the final re-open — rolls every completed step back,
/// normally leaving the originals under their own names. The rollback is
/// best effort: a rollback rename that itself fails cannot be recovered
/// further, is dropped in favor of reporting the primary error, and can
/// leave a mix of `.bak` and installed states — as can a *crash*
/// mid-installation, the inherent limit of multi-file replacement. The
/// `.bak` copies always preserve the original bytes for manual repair.
pub(super) fn install_recovered_archive(
    directory: &Path,
    generations: &[ArchiveFileName],
    target_name: &str,
    temporary_path: &Path,
) -> Result<TarArchiveReader> {
    let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut target_backup: Option<PathBuf> = None;
    let roll_back = |renamed: &[(PathBuf, PathBuf)]| {
        for (original, backup) in renamed.iter().rev() {
            let _ = std::fs::rename(backup, original);
        }
    };
    for generation in generations {
        let path = directory.join(&generation.file_name);
        // Zero-length letters hold nothing to preserve and are not archives;
        // retiring one would only manufacture an empty `.bak`. They stay for
        // the stale-archive task, which plans and confirms their removal.
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == 0) {
            continue;
        }
        let backup = backup_path(directory, &generation.file_name);
        if generation.file_name == *target_name {
            // The target keeps its directory entry: the backup is a
            // second link (or a copy) of the same content, never a
            // rename away.
            if std::fs::hard_link(&path, &backup).is_err()
                && let Err(error) = std::fs::copy(&path, &backup)
            {
                roll_back(&renamed);
                return Err(error.into());
            }
            target_backup = Some(backup);
        } else if let Err(error) = std::fs::rename(&path, &backup) {
            roll_back(&renamed);
            return Err(error.into());
        } else {
            renamed.push((path, backup));
        }
    }
    let target_path = directory.join(target_name);
    if let Err(error) = std::fs::rename(temporary_path, &target_path) {
        if let Some(backup) = &target_backup {
            let _ = std::fs::remove_file(backup);
        }
        roll_back(&renamed);
        return Err(error.into());
    }
    crate::writer::compaction::fsync_directory(directory);
    match TarArchiveReader::open(&target_path) {
        Ok(reader) => Ok(reader),
        Err(error) => {
            // The replacement was validated before installation, so a
            // failing re-open is environmental (for example an I/O
            // error). Restore the original atomically from its backup
            // link and undo the other renames before reporting.
            if let Some(backup) = &target_backup {
                let _ = std::fs::rename(backup, &target_path);
            }
            roll_back(&renamed);
            crate::writer::compaction::fsync_directory(directory);
            Err(error)
        }
    }
}

/// The first free `.bak` name for a damaged archive: `name.bak`, then
/// `name.2.bak`, `name.3.bak`, …
pub(super) fn backup_path(directory: &Path, file_name: &str) -> PathBuf {
    let first = directory.join(format!("{file_name}.bak"));
    if !first.exists() {
        return first;
    }
    let mut counter = 2u32;
    loop {
        let candidate = directory.join(format!("{file_name}.{counter}.bak"));
        if !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}
