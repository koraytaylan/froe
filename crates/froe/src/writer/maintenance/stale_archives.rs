//! Planning stale-archive removal and index repair, and refusing a store
//! whose active archives carry no usable index.

use super::plan::{CompactionAction, StaleArchiveReason};
use super::planning::{StaleArchive, file_fingerprint};
use crate::content::provider::SegmentProvider as _;
use crate::error::{Error, Result};
use crate::progress::ProgressObserver;
use crate::segment::identifier::SegmentIdentifier;
use crate::store::Repository;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::{ArchiveFileName, group_file_generations_newest_first};
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::certify_active_archive;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;

pub(super) fn plan_stale_archives(
    directory: &Path,
    repository: &Repository,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<StaleArchive>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if ArchiveFileName::parse(&name).is_some() {
            names.push(name);
        }
    }
    let groups = group_file_generations_newest_first(&names)?;
    let group_count = groups.len();
    observer.step_total_resolved(crate::progress::count(group_count));
    let mut stale = Vec::new();
    for (examined, group) in groups.into_iter().enumerate() {
        observer.step_advanced(crate::progress::count(examined));
        let mut winner = None;
        let mut indexed_but_incomplete = None;
        for candidate in &group {
            let path = directory.join(&candidate.file_name);
            if std::fs::symlink_metadata(&path)?.len() == 0 {
                continue;
            }
            let Some(opened) =
                archive_reader_for_letter(directory, &candidate.file_name, repository)
            else {
                continue;
            };
            if opened.reader().is_recovered() {
                continue;
            }
            // This is the exact generation normal repository discovery
            // will select. Never skip past it to promote an older letter:
            // doing so could roll the active archive back. Its graph and
            // BRF are recovery-critical even when content reads happen to
            // succeed, so alternate letters are removable only when both
            // trailers validate as well as the index.
            match certify_active_archive(repository, opened.reader()) {
                Ok(()) => winner = Some(candidate.file_name.as_str()),
                Err(error) => {
                    indexed_but_incomplete = Some(format!(
                        "active archive {} has incomplete recovery metadata ({error})",
                        candidate.file_name
                    ));
                }
            }
            break;
        }
        if let Some(winner) = winner {
            for candidate in &group {
                if candidate.file_name != winner {
                    let metadata = std::fs::symlink_metadata(directory.join(&candidate.file_name))?;
                    let bytes = metadata.len();
                    stale.push(StaleArchive {
                        file_name: candidate.file_name.clone(),
                        reason: if bytes == 0 {
                            StaleArchiveReason::EmptyIncomplete
                        } else {
                            StaleArchiveReason::Superseded
                        },
                        bytes,
                        fingerprint: file_fingerprint(
                            OsString::from(candidate.file_name.as_str()),
                            &metadata,
                        ),
                    });
                }
            }
        } else {
            let mut nonempty = Vec::new();
            for candidate in &group {
                let metadata = std::fs::symlink_metadata(directory.join(&candidate.file_name))?;
                let bytes = metadata.len();
                if bytes == 0 {
                    stale.push(StaleArchive {
                        file_name: candidate.file_name.clone(),
                        reason: StaleArchiveReason::EmptyIncomplete,
                        bytes,
                        fingerprint: file_fingerprint(
                            OsString::from(candidate.file_name.as_str()),
                            &metadata,
                        ),
                    });
                } else {
                    nonempty.push(candidate.file_name.clone());
                }
            }
            if !nonempty.is_empty() {
                if let Some(reason) = indexed_but_incomplete {
                    warnings.push(format!(
                        "{reason}; preserving every non-empty letter of archive number {} as recovery evidence",
                        group[0].archive_number,
                    ));
                } else {
                    warnings.push(format!(
                        "archive number {} has no valid indexed generation; preserving recoverable files {}",
                        group[0].archive_number,
                        nonempty.join(", ")
                    ));
                }
            }
        }
    }
    observer.step_advanced(crate::progress::count(group_count));
    stale.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    Ok(stale)
}

/// The already-mapped selected generation when this letter is the live
/// archive, otherwise a fresh mapping of a non-selected letter.
///
/// Planning used to mmap every winner a second time while `repository`
/// already held it. Alternate letters still need their own mapping.
enum OpenedArchive<'repository> {
    Selected(&'repository TarArchiveReader),
    Other(TarArchiveReader),
}

impl OpenedArchive<'_> {
    fn reader(&self) -> &TarArchiveReader {
        match self {
            Self::Selected(reader) => reader,
            Self::Other(reader) => reader,
        }
    }
}

fn archive_reader_for_letter<'repository>(
    directory: &Path,
    file_name: &str,
    repository: &'repository Repository,
) -> Option<OpenedArchive<'repository>> {
    if let Some(selected) = repository
        .archives()
        .iter()
        .find(|archive| archive.file_name() == file_name)
    {
        return Some(OpenedArchive::Selected(selected));
    }
    TarArchiveReader::open(&directory.join(file_name))
        .ok()
        .map(OpenedArchive::Other)
}

pub(super) fn generation_from_header(
    repository: &Repository,
    identifier: SegmentIdentifier,
) -> Result<GarbageCollectionGeneration> {
    let view = repository.segment(identifier)?;
    if !identifier.is_data_segment() {
        return Err(Error::InvalidFormat {
            details: format!("journal head segment {identifier} is not a data segment"),
        });
    }
    Ok(GarbageCollectionGeneration {
        generation: view.structure.generation,
        full_generation: view.structure.full_generation,
        is_compacted: view.structure.is_compacted,
    })
}

pub(super) fn reject_duplicate_active_segments(repository: &Repository) -> Result<()> {
    reject_duplicate_active_segments_across(repository, false)
}

/// Refuses a segment served by two archives of *different* numbers.
///
/// Split out because the whole check cannot run while repairs are pending: an
/// archive number with no valid index is served through every non-empty
/// letter it has, and those letters share segments by construction, so the
/// full check would refuse a store the repair is about to collapse back to
/// one letter. Two different *numbers* sharing a segment is not by
/// construction — it is a real, unrepairable defect, and suppressing it would
/// mean the preview cannot tell the operator the store is unfit and the guard
/// fires only after an irreversible rewrite.
pub(super) fn reject_cross_number_duplicate_active_segments(repository: &Repository) -> Result<()> {
    reject_duplicate_active_segments_across(repository, true)
}

pub(super) fn reject_duplicate_active_segments_across(
    repository: &Repository,
    ignore_within_one_number: bool,
) -> Result<()> {
    // An unparsable archive name counts as its own number rather than being
    // excused: failing to classify a file is not a reason to stop checking it.
    let number_of = |file_name: &str| -> Option<u32> {
        ArchiveFileName::parse(file_name).map(|parsed| parsed.archive_number)
    };
    let mut locations: HashMap<SegmentIdentifier, (&str, Option<u32>)> = HashMap::new();
    for archive in repository.archives() {
        let here = (archive.file_name(), number_of(archive.file_name()));
        for identifier in archive.segment_identifiers() {
            if let Some(previous) = locations.insert(identifier, here) {
                let same_number = previous.1.is_some() && previous.1 == here.1;
                if ignore_within_one_number && same_number {
                    continue;
                }
                return Err(Error::InvalidFormat {
                    details: format!(
                        "segment {identifier} occurs in active archives {} and {}; refusing cleanup",
                        previous.0, here.0
                    ),
                });
            }
        }
    }
    Ok(())
}

/// The refusal an index-less active archive earns.
///
/// Naming only the first offender cannot separate a writer killed before it
/// closed its newest archive — benign, and repaired by any froe write
/// command — from a store damaged throughout, and that is exactly the call
/// the operator has to make before touching the repository. So every
/// archive is counted, and whether the newest one is affected is stated
/// rather than left to be inferred from a file name.
///
/// Counting is by archive *number*, not by open reader. When no letter of a
/// number carries a valid index the reader serves every non-empty letter of
/// it, so counting readers would report one damaged number as two or three
/// damaged archives — and the warning carried alongside this refusal already
/// speaks in archive numbers.
/// The census of index-less active archives a refusal is written from.
#[derive(Clone, Copy)]
pub(super) struct IndexlessCensus<'census> {
    /// Active archive numbers in the store.
    pub(super) total_numbers: usize,
    /// How many of those numbers carry no valid index.
    pub(super) indexless_numbers: usize,
    /// Each offending archive's file name with the reason its index was
    /// rejected.
    pub(super) offenders: &'census [(&'census str, &'census str)],
    pub(super) newest_is_indexless: bool,
    /// Whether any offender's recovery scan read no segment at all, which
    /// is residue no repair can rebuild.
    pub(super) any_scan_is_empty: bool,
}

pub(super) fn indexless_archive_refusal(census: IndexlessCensus<'_>) -> String {
    const NAMES_SHOWN: usize = 5;
    let IndexlessCensus {
        total_numbers,
        indexless_numbers,
        offenders: indexless,
        newest_is_indexless,
        any_scan_is_empty,
    } = census;
    let mut names = indexless
        .iter()
        .take(NAMES_SHOWN)
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    if indexless.len() > NAMES_SHOWN {
        let _ = write!(names, ", and {} more", indexless.len() - NAMES_SHOWN);
    }
    // Reasons are deduplicated rather than listed per archive: a store
    // damaged one way is damaged that way throughout far more often than
    // not, and the shape of the failure is what decides the response. Where
    // they genuinely differ, saying so is itself the finding.
    let distinct_reasons: BTreeSet<&str> = indexless.iter().map(|(_, reason)| *reason).collect();
    let reasons = distinct_reasons
        .iter()
        .copied()
        .collect::<Vec<_>>()
        .join("; ");
    let subject = if indexless_numbers == 1 {
        format!("1 of {total_numbers} active archive numbers has no index metadata")
    } else {
        format!(
            "{indexless_numbers} of {total_numbers} active archive numbers have no index metadata"
        )
    };
    // The remedy is stated conditionally, because the two shapes deserve
    // different advice. A writer killed before it closed its newest archive
    // left a complete archive missing only its trailer, and rebuilding the
    // index from a scan of it loses nothing. An index-less archive in the
    // middle of the store was closed once and stopped validating since, so
    // a scan may legitimately fail to read segments that were there —
    // repairing before looking would make that permanent in everything but
    // the `.bak`.
    let ordinality = if newest_is_indexless {
        "the newest active archive is among them, which is what a writer killed before it \
         closed its archive leaves behind"
    } else {
        "the newest active archive is not among them, so this is not simply a writer killed \
         before it closed its archive"
    };
    // Whether a repair would even succeed is knowable here, so it is not
    // guessed: a recovery-scanned archive reports the segments the scan
    // read, and one that read none is residue the write open refuses rather
    // than rebuilds. Advising a write command for it would send the operator
    // to a refusal, which is the circularity this branch exists to avoid.
    let remedy = if any_scan_is_empty {
        "at least one of them holds no segment the recovery scan can read, so nothing can rebuild \
         it — move that file aside to proceed, and keep it, it is the only copy of whatever it \
         holds"
    } else if newest_is_indexless {
        "rerun with `--repair-archive-indexes` to rebuild the missing index from the \
         archive's own entries, retaining the original bytes under a `.bak` name"
    } else {
        // A closed archive that stopped validating is not a missing trailer;
        // a scan of it may read fewer segments than it holds, and repairing
        // makes that the served truth in everything but the `.bak`.
        "inspect before repairing: `--repair-archive-indexes` would rebuild the missing \
         indexes from a recovery scan, which retains the original bytes under a `.bak` name but \
         cannot recover a segment the scan cannot read"
    };
    format!(
        "{subject} ({names}); the index was rejected because {reasons}; {ordinality}. Refusing \
         this cleanup run; no archive, journal, or checkpoint has been changed. Run \
         `froe archives` on this repository to see every archive's index state; {remedy}."
    )
}

/// The repairs an index-less store needs, one per archive *number*.
///
/// Named from the same census the refusal uses, so an operator who opts in
/// sees exactly the archives they would otherwise have been refused over.
///
/// Grouped by number rather than by open reader, because a number with no
/// valid index is served through every one of its non-empty letters while
/// the repair rebuilds it once — and installs the result under the *lowest*
/// letter, which is the name reported here so the plan names the file the
/// repair will actually write. Reporting per reader would promise two or
/// three repairs for one, under names that never appear again.
/// Index-less archive numbers whose letters together scan to nothing, named
/// by the file a rebuild would have installed under.
///
/// Read off the open readers rather than rescanning: `Repository::open`
/// already performed the recovery scan, and `segment_count()` on a recovered
/// archive is exactly what it read. Summed per number, because a rebuild
/// merges every letter — one empty letter beside a readable one is still
/// repairable.
pub(super) fn unrepairable_archive_names(repository: &Repository) -> Vec<String> {
    let mut by_number: BTreeMap<u32, (String, usize)> = BTreeMap::new();
    for archive in repository.archives() {
        if archive.index().is_some() {
            continue;
        }
        let Some(parsed) = ArchiveFileName::parse(archive.file_name()) else {
            continue;
        };
        let name = archive.file_name().to_owned();
        let entry = by_number
            .entry(parsed.archive_number)
            .or_insert_with(|| (name.clone(), 0));
        if name < entry.0 {
            entry.0 = name;
        }
        entry.1 += archive.segment_count();
    }
    by_number
        .into_values()
        .filter(|(_, segments)| *segments == 0)
        .map(|(name, _)| name)
        .collect()
}

pub(super) fn planned_archive_repairs(repository: &Repository) -> Vec<CompactionAction> {
    struct Group {
        target: String,
        retired: Vec<String>,
        reason: String,
        bytes: u64,
        scanned_segments: usize,
    }
    let mut by_number: BTreeMap<u32, Group> = BTreeMap::new();
    for archive in repository.archives() {
        let Some(reason) = archive.recovery_reason() else {
            continue;
        };
        let Some(parsed) = ArchiveFileName::parse(archive.file_name()) else {
            continue;
        };
        let name = archive.file_name().to_owned();
        let group = by_number
            .entry(parsed.archive_number)
            .or_insert_with(|| Group {
                target: name.clone(),
                retired: Vec::new(),
                reason: reason.to_owned(),
                bytes: 0,
                scanned_segments: 0,
            });
        // `Repository::archives()` already skips zero-length letters, so the
        // lowest name here is the lowest non-empty letter — the same target
        // `recover_archive_number` installs under. Every other letter is
        // retired to a `.bak`, so the plan names them too: confirmation is
        // scoped to the files it printed.
        if name < group.target {
            group
                .retired
                .push(std::mem::replace(&mut group.target, name));
            reason.clone_into(&mut group.reason);
        } else if name != group.target {
            group.retired.push(name);
        }
        group.bytes = group.bytes.saturating_add(archive.file_size());
        group.scanned_segments += archive.segment_count();
    }
    by_number
        .into_values()
        // A number whose letters together scan to nothing cannot be rebuilt —
        // `recover_archive_number` refuses it. Planning it anyway would have
        // the operator confirm a run cleanup can already prove will fail,
        // after paying a durable rewrite of every repairable archive for it,
        // with no rerun ever converging. Dropped here, the store falls through
        // to the refusal that states the impossibility up front. Summed per
        // number, never per letter: one empty letter beside a readable one is
        // still repairable, because the scan merges every letter.
        .filter(|group| group.scanned_segments > 0)
        .map(|mut group| {
            group.retired.sort();
            CompactionAction::RepairArchiveIndex {
                file_name: group.target,
                retired_file_names: group.retired,
                reason: group.reason,
                bytes: group.bytes,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::writer::maintenance::test_support::*;

    #[test]
    fn a_refusal_names_the_offenders_and_agrees_in_number() {
        let one = indexless_archive_refusal(IndexlessCensus {
            total_numbers: 43,
            indexless_numbers: 1,
            offenders: &[("data00042a.tar", MAGIC_REASON)],
            newest_is_indexless: true,
            any_scan_is_empty: false,
        });
        assert!(
            one.contains("1 of 43 active archive numbers has no index metadata (data00042a.tar)"),
            "singular subject names the archive and the total: {one}"
        );
        assert!(
            one.contains(MAGIC_REASON),
            "the reason the index was rejected reaches the operator: {one}"
        );
        assert!(
            one.contains("no archive, journal, or checkpoint has been changed"),
            "the refusal states precisely what is untouched: {one}"
        );

        let two = indexless_archive_refusal(IndexlessCensus {
            total_numbers: 3,
            indexless_numbers: 2,
            offenders: &[
                ("data00001a.tar", CHECKSUM_REASON),
                ("data00000a.tar", CHECKSUM_REASON),
            ],
            newest_is_indexless: false,
            any_scan_is_empty: false,
        });
        assert!(
            two.contains("2 of 3 active archive numbers have no index metadata"),
            "plural subject agrees: {two}"
        );
        assert_eq!(
            two.matches(CHECKSUM_REASON).count(),
            1,
            "one shared reason is stated once, not repeated per archive: {two}"
        );
    }
    /// The census counts every offender even though only five are named,
    /// so an operator cannot read the shown list as the whole damage.
    #[test]
    fn a_refusal_counts_the_offenders_it_does_not_name() {
        let many: Vec<String> = (0..8)
            .map(|index| format!("data0000{index}a.tar"))
            .collect();
        let borrowed: Vec<(&str, &str)> = many.iter().map(|n| (n.as_str(), MAGIC_REASON)).collect();

        let truncated = indexless_archive_refusal(IndexlessCensus {
            total_numbers: 40,
            indexless_numbers: 8,
            offenders: &borrowed,
            newest_is_indexless: true,
            any_scan_is_empty: false,
        });

        assert!(
            truncated.contains("8 of 40 active archive numbers"),
            "the count is the whole census, not the shown names: {truncated}"
        );
        assert!(
            truncated.contains("and 3 more"),
            "the omitted names are counted rather than silently dropped: {truncated}"
        );
    }
    /// The remedy is the branch an operator acts on, and the three shapes
    /// need different advice: a killed writer can be repaired, mid-store
    /// damage should be inspected first, and an archive whose scan reads
    /// nothing cannot be repaired at all.
    #[test]
    fn a_refusal_advises_the_remedy_that_fits_the_damage() {
        let killed_writer = indexless_archive_refusal(IndexlessCensus {
            total_numbers: 43,
            indexless_numbers: 1,
            offenders: &[("data00042a.tar", MAGIC_REASON)],
            newest_is_indexless: true,
            any_scan_is_empty: false,
        });
        assert!(
            killed_writer.contains("the newest active archive is among them"),
            "a killed writer is distinguishable from damage: {killed_writer}"
        );
        assert!(
            killed_writer.contains("--repair-archive-indexes"),
            "a killed writer is pointed at the task that repairs it: {killed_writer}"
        );

        let mid_store = indexless_archive_refusal(IndexlessCensus {
            total_numbers: 3,
            indexless_numbers: 2,
            offenders: &[
                ("data00001a.tar", CHECKSUM_REASON),
                ("data00000a.tar", CHECKSUM_REASON),
            ],
            newest_is_indexless: false,
            any_scan_is_empty: false,
        });
        assert!(
            mid_store.contains("the newest active archive is not among them"),
            "mid-store damage is not reported as a killed writer: {mid_store}"
        );
        assert!(
            mid_store.contains("inspect before repairing"),
            "mid-store damage does not get the unconditional repair advice: {mid_store}"
        );

        // An archive whose scan read nothing cannot be rebuilt, and the
        // write open refuses it. Advising a write command would be circular.
        let unrecoverable = indexless_archive_refusal(IndexlessCensus {
            total_numbers: 2,
            indexless_numbers: 1,
            offenders: &[("data00009a.tar", MAGIC_REASON)],
            newest_is_indexless: true,
            any_scan_is_empty: true,
        });
        assert!(
            !unrecoverable.contains("--repair-archive-indexes"),
            "an unrecoverable archive must not be sent to a command that will refuse it: \
             {unrecoverable}"
        );
        assert!(
            unrecoverable.contains("move that file aside"),
            "an unrecoverable archive gets the remedy that actually works: {unrecoverable}"
        );
    }
}
