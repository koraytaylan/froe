//! Refusing a store whose active archives carry no usable index: the
//! census of the damage, and the refusal text built from it.

use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

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
        "the newest active archive is among them — the damage a writer leaves behind when it \
         is killed before closing its archive"
    } else {
        "the newest active archive is not among them, so this is not simply a killed writer's \
         unfinished archive"
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
        "authorize the repair — interactively at the prompt, or with `--yes` — to rebuild the \
         missing index from the archive's own entries, retaining the original bytes under a \
         `.bak` name"
    } else {
        // A closed archive that stopped validating is not a missing trailer;
        // a scan of it may read fewer segments than it holds, and repairing
        // makes that the served truth in everything but the `.bak`.
        "inspect before repairing: an authorized repair (yes at the prompt, or `--yes`) rebuilds \
         the missing indexes from a recovery scan, which retains the original bytes under `.bak` \
         names but cannot recover a segment the scan cannot read"
    };
    format!(
        "{subject} ({names}); the index was rejected because {reasons}; {ordinality}. Refusing \
         this run; no archive, journal, or checkpoint has been changed. Run \
         `froe archives` on this repository to see every archive's index state; {remedy}."
    )
}

/// The refusal an index-less active archive earns, built from the open
/// readers' census — or `None` when every active archive carries a valid
/// index.
///
/// Raised in two places for one behavior: early in planning, before the
/// minutes-long verification walks, when no repair is selected to fix the
/// state; and again where generation decisions would otherwise rest on a
/// recovery scan, so the gate fails closed even for a caller that skipped
/// the early check.
pub(super) fn indexless_active_archive_refusal(repository: &Repository) -> Option<String> {
    // Census before refusal. `Repository::archives()` is ordered newest
    // archive number first, so this preserves that order and the newest
    // served archive is affected exactly when it is the first element.
    let indexless: Vec<(&str, &str)> = repository
        .archives()
        .iter()
        .filter_map(|archive| {
            archive
                .recovery_reason()
                .map(|reason| (archive.file_name(), reason))
        })
        .collect();
    if indexless.is_empty() {
        return None;
    }
    // By number, not by reader: an unindexed number is served through
    // every one of its non-empty letters, and reporting those letters as
    // separate damaged archives would overstate the damage.
    let number_of = |file_name: &str| ArchiveFileName::parse(file_name).map(|n| n.archive_number);
    let indexless_numbers: BTreeSet<u32> = indexless
        .iter()
        .filter_map(|(name, _)| number_of(name))
        .collect();
    let total_numbers: BTreeSet<u32> = repository
        .archives()
        .iter()
        .filter_map(|archive| number_of(archive.file_name()))
        .collect();
    let newest_is_indexless = repository
        .archives()
        .first()
        .is_some_and(|newest| newest.index().is_none());
    // `segment_count()` on a recovery-scanned archive is what the scan
    // actually read, which is exactly what a rebuild would work from —
    // summed per archive *number*, because the rebuild merges every
    // letter of a number. A letter that scans empty beside one that does
    // not is still repairable, and reporting it as unrepairable withholds
    // the task that would fix the store and sends the operator to
    // hand-edit a damaged production directory instead.
    let mut scanned_segments: BTreeMap<u32, usize> = BTreeMap::new();
    for archive in repository.archives() {
        if archive.index().is_some() {
            continue;
        }
        if let Some(number) = number_of(archive.file_name()) {
            *scanned_segments.entry(number).or_default() += archive.segment_count();
        }
    }
    let any_scan_is_empty = scanned_segments.values().any(|count| *count == 0);
    Some(indexless_archive_refusal(IndexlessCensus {
        total_numbers: total_numbers.len(),
        indexless_numbers: indexless_numbers.len(),
        offenders: &indexless,
        newest_is_indexless,
        any_scan_is_empty,
    }))
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
            killed_writer.contains("authorize the repair") && killed_writer.contains("`--yes`"),
            "a killed writer is pointed at authorizing the repair that fixes it: {killed_writer}"
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
        // write open refuses it. Advising a repair would be circular.
        let unrecoverable = indexless_archive_refusal(IndexlessCensus {
            total_numbers: 2,
            indexless_numbers: 1,
            offenders: &[("data00009a.tar", MAGIC_REASON)],
            newest_is_indexless: true,
            any_scan_is_empty: true,
        });
        assert!(
            !unrecoverable.contains("authorize the repair"),
            "an unrecoverable archive must not be sent to a repair that will refuse it: \
             {unrecoverable}"
        );
        assert!(
            unrecoverable.contains("move that file aside"),
            "an unrecoverable archive gets the remedy that actually works: {unrecoverable}"
        );
    }
}
