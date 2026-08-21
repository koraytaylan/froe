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
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
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
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::plan::*;
    use crate::writer::maintenance::prepared::*;
    use crate::writer::store_writer::WritableRepository;

    use crate::writer::maintenance::test_support::*;

    /// The production refusal, end to end. Before the census this reported
    /// the first offending segment of the first offending archive and
    /// nothing else — no count, no ordinality, no remedy. The census is
    /// now also raised *early*: right after the archives open, before the
    /// head verification that costs minutes on a real store, so the
    /// refusal must be complete on its own rather than leaning on warnings
    /// later scans would have established.
    #[test]
    fn cleanup_refuses_an_index_less_active_archive_with_a_full_census() {
        let directory = TestDirectory::new("index-less-census");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));
        let before = file_bytes(&directory.path);

        let error = plan_compaction(&directory.path, &CompactionOptions::default())
            .expect_err("an index-less active archive must refuse generation cleanup");
        let crate::error::Error::InvalidFormat { details } = error else {
            panic!("unexpected refusal variant");
        };
        assert!(
            details
                .contains("1 of 1 active archive numbers has no index metadata (data00000a.tar)"),
            "the refusal names every offender and the total: {details}"
        );
        assert!(
            details.contains("the newest active archive is among them"),
            "the refusal states whether this is a killed writer: {details}"
        );
        assert!(
            details.contains("no archive, journal, or checkpoint has been changed"),
            "the refusal states precisely what is untouched: {details}"
        );
        assert!(
            !details.contains("Also established before the refusal"),
            "the early refusal is raised before any scan establishes warnings, and must not \
             carry a redundant restatement of its own census: {details}"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning a refusal changes no byte"
        );
    }

    /// The whole point of the task: a store a killed writer left behind is
    /// cleaned to a healthy shape by cleanup itself, in one run, instead of
    /// sending the operator to a different command as a workaround.
    #[test]
    fn repair_archives_heals_an_index_less_store_and_the_rest_then_plans() {
        let directory = TestDirectory::new("repair-heals");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));

        // Without the task, the default set still refuses and points at
        // authorizing the repair that fixes it.
        let refusal = plan_compaction(&directory.path, &CompactionOptions::default())
            .expect_err("the default set must still refuse");
        assert!(
            refusal.to_string().contains("authorize the repair"),
            "the refusal names the remedy that fixes it: {refusal}"
        );

        // With it, the preview names the repair and nothing else yet.
        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        let preview = plan_compaction(&directory.path, &options).expect("preview must not refuse");
        assert!(
            preview.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RepairArchiveIndex { file_name, .. }
                    if file_name == "data00000a.tar"
            )),
            "the preview names the repair: {:?}",
            preview.actions()
        );

        let outcome = PreparedCompaction::prepare(&directory.path, options)
            .expect("prepare repairs under the lock")
            .apply()
            .expect("apply");
        assert_eq!(outcome.repaired_archives, 1, "the rebuild is reported");
        assert!(
            directory.path.join("data00000a.tar.bak").exists(),
            "the original bytes are retained"
        );

        // Healthy: every archive indexed, and the default set now plans.
        let repository = Repository::open(&directory.path).expect("reopen");
        assert!(
            !repository
                .archives()
                .iter()
                .any(TarArchiveReader::is_recovered),
            "no archive is served through the recovery scan any more"
        );
        plan_compaction(&directory.path, &CompactionOptions::default())
            .expect("the default set plans cleanly against the healed store");
    }

    /// A repair that fails part-way through still happens — a staging residue
    /// is found per number, not by the survey — and reporting only the
    /// failure would leave the operator believing nothing moved.
    #[test]
    fn a_failed_repair_reports_the_rebuilds_it_already_completed() {
        let directory = TestDirectory::new("repair-partial");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // A second archive number, from an independent bootstrap so it shares
        // no segment identifier with the first — a copy would instead trip
        // the cross-number duplicate guard.
        let donor = TestDirectory::new("repair-partial-donor");
        {
            let store = WritableRepository::open(&donor.path).expect("donor bootstrap");
            store.close().expect("close");
        }
        std::fs::copy(
            donor.path.join("data00000a.tar"),
            directory.path.join("data00001a.tar"),
        )
        .expect("second archive number");
        // Both repairable; the higher number carries the residue of an
        // interrupted rebuild, which repair must refuse rather than clobber.
        break_index_magic(&directory.path.join("data00000a.tar"));
        break_index_magic(&directory.path.join("data00001a.tar"));
        std::fs::write(
            directory.path.join("data00001a.tar.recovering"),
            b"an interrupted rebuild",
        )
        .expect("staging residue");

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        let details = match PreparedCompaction::prepare(&directory.path, options) {
            Ok(_) => panic!("the staging residue must refuse the run"),
            Err(crate::error::Error::InvalidFormat { details }) => details,
            Err(other) => panic!("unexpected refusal variant: {other}"),
        };
        assert!(
            details.contains("data00001a.tar.recovering"),
            "the refusal names what stopped it: {details}"
        );
        assert!(
            details.contains("data00000a.tar"),
            "the refusal names what it already rebuilt: {details}"
        );
        assert!(
            details.contains("no second attempt"),
            "the refusal says the completed work need not be redone: {details}"
        );
        assert!(
            directory.path.join("data00000a.tar.bak").exists(),
            "and that rebuild really is durable"
        );
        assert!(
            directory.path.join("data00001a.tar.recovering").exists(),
            "the residue is left for the stale-temporaries task to adjudicate"
        );
    }

    /// Selecting the task is not the same as having work to do, and the
    /// difference used to be a one-way `store.version` 1→2 transition that
    /// appeared in no plan, was never confirmed, and survived cancelling.
    #[test]
    fn selecting_repair_with_nothing_to_repair_changes_no_byte() {
        let directory = TestDirectory::repository("repair-noop");
        std::fs::write(
            directory.path.join("manifest"),
            "#a version one store\nstore.version=1\n",
        )
        .expect("v1 manifest");
        // Something to do, so the run is not short-circuited as empty.
        std::fs::write(directory.path.join("journal.log.compacting"), b"").expect("temporary");
        let before = file_bytes(&directory.path);

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        let prepared =
            PreparedCompaction::prepare(&directory.path, options).expect("prepare must succeed");
        assert_eq!(
            prepared.repaired_archives(),
            0,
            "there was nothing to repair"
        );
        assert_eq!(
            crate::store::read_manifest_store_version(&directory.path.join("manifest"))
                .expect("read manifest"),
            1,
            "the manifest must not be upgraded when no repair happens"
        );
        drop(prepared);
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "preparing a repair run with nothing to repair changes no byte"
        );
    }

    /// Every index-dependent gate evaluates the store for the first time
    /// after the repair, so a refusal there is ordinary — and must not claim
    /// the store is as the operator left it.
    #[test]
    fn a_refusal_after_a_repair_says_the_repair_already_happened() {
        let directory = TestDirectory::new("repair-then-refuse");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // A second number sharing every segment with the first: a real,
        // unrepairable defect that only the post-repair gates can see.
        std::fs::copy(
            directory.path.join("data00000a.tar"),
            directory.path.join("data00001a.tar"),
        )
        .expect("duplicate archive number");
        break_index_magic(&directory.path.join("data00000a.tar"));

        // The preview must state the unfitness itself, before authorizing.
        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        let preview = plan_compaction(&directory.path, &options)
            .expect_err("the cross-number duplicate must refuse in the read-only preview");
        assert!(
            preview.to_string().contains("occurs in active archives"),
            "the dry run reports the real reason: {preview}"
        );
    }

    #[test]
    fn valid_newer_archive_generation_makes_the_lower_letter_stale() {
        let directory = TestDirectory::repository("stale-letter");
        std::fs::copy(
            directory.path.join("data00000a.tar"),
            directory.path.join("data00000b.tar"),
        )
        .expect("copy archive generation");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
        let plan = plan_compaction(&directory.path, &options).expect("plan");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveStaleArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )));

        compact(&directory.path, options).expect("cleanup");
        assert!(!directory.path.join("data00000a.tar").exists());
        assert!(directory.path.join("data00000b.tar").exists());
        Repository::open(&directory.path).expect("healthy repository");
    }

    #[test]
    fn stale_archive_cleanup_preserves_alternates_when_active_trailers_are_invalid() {
        for (name, magic) in [
            ("invalid-graph", 0x0A30_470Au32.to_be_bytes()),
            ("invalid-brf", 0x0A31_420Au32.to_be_bytes()),
        ] {
            let directory = TestDirectory::repository(name);
            let newer = directory.path.join("data00000b.tar");
            std::fs::copy(directory.path.join("data00000a.tar"), &newer)
                .expect("copy newer archive generation");
            corrupt_first_magic(&newer, magic);

            let selected = TarArchiveReader::open(&newer).expect("index remains valid");
            assert!(!selected.is_recovered());
            assert!(selected.segment_graph().is_none() || selected.binary_references().is_none());
            let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
            let plan = plan_compaction(&directory.path, &options).expect("plan");

            assert!(!plan.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RemoveStaleArchive { file_name, .. }
                    if file_name == "data00000a.tar"
            )));
            assert!(
                plan.warnings()
                    .iter()
                    .any(|warning| warning.contains("incomplete recovery metadata"))
            );
            assert!(directory.path.join("data00000a.tar").exists());
            assert!(newer.exists());
        }
    }

    #[test]
    fn stale_archive_cleanup_reconstructs_semantic_graph_and_brf_before_deletion() {
        for (name, write_metadata_record) in [("omitted-graph", 0u8), ("omitted-brf", 1u8)] {
            let directory = TestDirectory::repository(name);
            let store = WritableRepository::open(&directory.path).expect("open writer");
            let mut writer = store.record_writer(store.writing_generation().expect("generation"));
            match write_metadata_record {
                0 => {
                    writer
                        .write_string(&"graph-block".repeat(40_000))
                        .expect("long string with bulk references");
                }
                _ => {
                    writer
                        .write_external_binary_identifier("external-blob-that-must-survive")
                        .expect("external blob identifier");
                }
            }
            writer.finish().expect("finish metadata segment");
            store.close().expect("close writer");
            let source = directory.path.join("data00001a.tar");
            assert!(source.exists());
            let source_reader = TarArchiveReader::open(&source).expect("source reader");
            if write_metadata_record == 0 {
                assert!(
                    source_reader
                        .segment_graph()
                        .is_some_and(|graph| !graph.adjacency.is_empty())
                );
            } else {
                assert!(source_reader.binary_references().is_some_and(|catalog| {
                    catalog
                        .generations
                        .iter()
                        .any(|generation| !generation.segments.is_empty())
                }));
            }
            repack_without_graph_or_brf(&directory.path, "data00001a.tar", "data00001b.tar");
            let repacked = TarArchiveReader::open(&directory.path.join("data00001b.tar"))
                .expect("repacked reader");
            assert!(repacked.segment_graph().is_some());
            assert!(repacked.binary_references().is_some());

            let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
            let plan = plan_compaction(&directory.path, &options).expect("plan");

            assert!(!plan.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RemoveStaleArchive { file_name, .. }
                    if file_name == "data00001a.tar"
            )));
            assert!(
                plan.warnings()
                    .iter()
                    .any(|warning| warning.contains("incomplete recovery metadata"))
            );
            assert!(source.exists());
        }
    }

    #[test]
    fn nonempty_archive_without_a_valid_index_is_preserved_for_recovery() {
        let directory = TestDirectory::repository("archive-needs-recovery");
        let damaged = directory.path.join("data00001a.tar");
        std::fs::write(&damaged, b"not a complete tar archive").expect("damaged archive");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);

        let plan = plan_compaction(&directory.path, &options).expect("plan");

        assert!(plan.actions().is_empty());
        assert!(
            plan.warnings()
                .iter()
                .any(|warning| warning.contains("no valid indexed generation"))
        );
        assert_eq!(
            std::fs::read(&damaged).expect("damaged bytes"),
            b"not a complete tar archive"
        );
    }
}
