//! Classifying journal lines: which resolve, which are stale, and where
//! the retention boundary falls.

use super::super::plan::{JournalLineRemoval, JournalRemovalReason};
use super::super::planning::{JournalPlan, verify_exact_super_root_with_verifier};
use crate::error::{Error, Result};
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::tooling::NodeTreeVerifier;
use crate::writer::maintenance::journal::{
    RawJournal, RawJournalLine, RawJournalLineClassification,
};
use std::collections::HashMap;
use std::num::NonZeroUsize;

pub(in crate::writer::maintenance) struct JournalAnalysis {
    pub(in crate::writer::maintenance) plan: JournalPlan,
    pub(in crate::writer::maintenance) retained_indexes: Vec<usize>,
    pub(in crate::writer::maintenance) retained_record_ids: Vec<RecordIdentifier>,
}

pub(in crate::writer::maintenance) const JOURNAL_LINE_PREVIEW_LIMIT: usize = 160;

pub(in crate::writer::maintenance) fn journal_line_removal(
    index: usize,
    line: &RawJournalLine,
    record_identifier: Option<RecordIdentifier>,
    reason: JournalRemovalReason,
) -> JournalLineRemoval {
    let content = line.content_bytes();
    let preview_length = content.len().min(JOURNAL_LINE_PREVIEW_LIMIT);
    JournalLineRemoval {
        line_number: index + 1,
        record_identifier,
        reason,
        preview: content[..preview_length].to_vec(),
        preview_truncated: preview_length != content.len(),
    }
}

pub(in crate::writer::maintenance) fn analyze_journal(
    repository: &Repository,
    raw: &RawJournal,
    current_head: RecordIdentifier,
    retention: Option<NonZeroUsize>,
    observer: &mut dyn ProgressObserver,
) -> Result<JournalAnalysis> {
    crate::progress::observe(
        observer,
        &Step::new("analyzing journal revisions", WorkUnit::JournalLines)
            .with_total(crate::progress::count(raw.lines().len())),
        |observer| analyze_journal_lines(repository, raw, current_head, retention, observer),
    )
}

/// The first line index within the newest `retention` resolvable revisions.
///
/// Resolvability here is only "parses and its head segment exists" — the
/// cheap half of the classification. That is deliberate: a line older than
/// the bound is removed whether or not its tree verifies, so bounding before
/// the walk skips the expensive per-revision verification for exactly the
/// lines being discarded. On a store with tens of thousands of journal lines
/// that is the difference between minutes and seconds.
pub(in crate::writer::maintenance) fn journal_retention_boundary(
    repository: &Repository,
    raw: &RawJournal,
    retention: NonZeroUsize,
    validity: &mut HashMap<RecordIdentifier, bool>,
    verifier: &mut NodeTreeVerifier<'_>,
) -> Option<usize> {
    let mut remaining = retention.get();
    for (index, line) in raw.lines().iter().enumerate().rev() {
        let RawJournalLineClassification::Record(record) = line.classification() else {
            continue;
        };
        let identifier = record.record_identifier;
        if !repository.contains_segment(identifier.segment) {
            continue;
        }
        // Only revisions that actually resolve fill a slot. Counting on
        // segment existence alone let a line that fails its walk consume one
        // and then be removed as unreadable anyway, so a bound of N kept
        // fewer than N revisions and irreversibly retired a readable one to
        // make room for it. Verdicts are memoised into the same map the
        // classification pass uses, so nothing is walked twice and the lines
        // beyond the bound are still never walked at all.
        let readable = if let Some(readable) = validity.get(&identifier) {
            *readable
        } else {
            let readable = verify_exact_super_root_with_verifier(
                repository,
                identifier,
                verifier,
                &mut DiscardedProgress,
            )
            .is_ok();
            validity.insert(identifier, readable);
            readable
        };
        if !readable {
            continue;
        }
        remaining -= 1;
        if remaining == 0 {
            return Some(index);
        }
    }
    // Fewer resolvable revisions than the bound allows: nothing is beyond it.
    None
}

/// What one pass over the journal found: which lines survive, which go,
/// and why each one goes.
#[derive(Default)]
struct JournalTally {
    parser_ignored: usize,
    missing_segments: usize,
    unreadable_revisions: usize,
    beyond_retention: usize,
    removals: Vec<JournalLineRemoval>,
    retained_indexes: Vec<usize>,
    retained_record_ids: Vec<RecordIdentifier>,
}

/// The journal pass itself; [`analyze_journal`] owns the step around it.
pub(in crate::writer::maintenance) fn analyze_journal_lines(
    repository: &Repository,
    raw: &RawJournal,
    current_head: RecordIdentifier,
    retention: Option<NonZeroUsize>,
    observer: &mut dyn ProgressObserver,
) -> Result<JournalAnalysis> {
    reject_head_disagreement(repository, raw, current_head)?;

    let mut validity: HashMap<RecordIdentifier, bool> = HashMap::new();
    validity.insert(current_head, true);
    let mut verifier = NodeTreeVerifier::new(repository);
    let retention_boundary = retention.and_then(|bound| {
        journal_retention_boundary(repository, raw, bound, &mut validity, &mut verifier)
    });

    let tally = classify_journal_lines(
        repository,
        raw,
        retention_boundary,
        &mut validity,
        &mut verifier,
        observer,
    );

    if !tally.retained_record_ids.contains(&current_head) {
        return Err(Error::InvalidFormat {
            details: "journal analysis would not retain the exact current head".to_owned(),
        });
    }
    let removed_lines = tally
        .parser_ignored
        .checked_add(tally.missing_segments)
        .and_then(|count| count.checked_add(tally.unreadable_revisions))
        .and_then(|count| count.checked_add(tally.beyond_retention))
        .ok_or_else(|| Error::InvalidFormat {
            details: "journal line accounting overflow".to_owned(),
        })?;
    Ok(JournalAnalysis {
        plan: JournalPlan {
            retained_record_ids: tally.retained_record_ids.clone(),
            retained_raw_lines: tally
                .retained_indexes
                .iter()
                .map(|&index| raw.lines()[index].raw_bytes().to_vec())
                .collect(),
            removals: tally.removals,
            removed_lines,
            parser_ignored: tally.parser_ignored,
            missing_segments: tally.missing_segments,
            unreadable_revisions: tally.unreadable_revisions,
            beyond_retention: tally.beyond_retention,
        },
        retained_indexes: tally.retained_indexes,
        retained_record_ids: tally.retained_record_ids,
    })
}

/// The raw journal's own newest resolvable record must be the head the
/// repository reader selected; disagreement means one of the two is
/// reading a store the other is not.
fn reject_head_disagreement(
    repository: &Repository,
    raw: &RawJournal,
    current_head: RecordIdentifier,
) -> Result<()> {
    let selected = raw
        .lines()
        .iter()
        .rev()
        .find_map(|line| match line.classification() {
            RawJournalLineClassification::Record(record)
                if repository.contains_segment(record.record_identifier.segment) =>
            {
                Some(record.record_identifier)
            }
            _ => None,
        })
        .ok_or_else(|| Error::InvalidFormat {
            details: "no raw journal record references an existing segment".to_owned(),
        })?;
    if selected != current_head {
        return Err(Error::InvalidFormat {
            details: format!(
                "raw journal selected {selected}, but the repository reader selected {current_head}"
            ),
        });
    }
    Ok(())
}

/// Sorts every line into retained or removed, verifying each distinct
/// revision's tree at most once.
fn classify_journal_lines(
    repository: &Repository,
    raw: &RawJournal,
    retention_boundary: Option<usize>,
    validity: &mut HashMap<RecordIdentifier, bool>,
    verifier: &mut NodeTreeVerifier<'_>,
    observer: &mut dyn ProgressObserver,
) -> JournalTally {
    let mut tally = JournalTally::default();
    for (index, line) in raw.lines().iter().enumerate() {
        observer.step_advanced(crate::progress::count(index));
        match line.classification() {
            RawJournalLineClassification::ParserSkippedNoSpace => {
                tally.parser_ignored += 1;
                tally.removals.push(journal_line_removal(
                    index,
                    line,
                    None,
                    JournalRemovalReason::ParserSkippedNoSpace,
                ));
            }
            RawJournalLineClassification::InvalidRecordIdentifier { .. } => {
                tally.parser_ignored += 1;
                tally.removals.push(journal_line_removal(
                    index,
                    line,
                    None,
                    JournalRemovalReason::InvalidRecordIdentifier,
                ));
            }
            RawJournalLineClassification::Record(record) => {
                let identifier = record.record_identifier;
                if !repository.contains_segment(identifier.segment) {
                    tally.missing_segments += 1;
                    tally.removals.push(journal_line_removal(
                        index,
                        line,
                        Some(identifier),
                        JournalRemovalReason::MissingSegment,
                    ));
                    continue;
                }
                // Before the verification walk, not after: a line the bound
                // discards is discarded whether or not its tree reads, so
                // proving readability first would be work spent on a line
                // already destined for removal.
                if retention_boundary.is_some_and(|boundary| index < boundary) {
                    tally.beyond_retention += 1;
                    tally.removals.push(journal_line_removal(
                        index,
                        line,
                        Some(identifier),
                        JournalRemovalReason::BeyondRetention,
                    ));
                    continue;
                }
                if revision_is_readable(repository, identifier, validity, verifier) {
                    tally.retained_indexes.push(index);
                    tally.retained_record_ids.push(identifier);
                } else {
                    tally.unreadable_revisions += 1;
                    tally.removals.push(journal_line_removal(
                        index,
                        line,
                        Some(identifier),
                        JournalRemovalReason::UnreadableRevision,
                    ));
                }
            }
        }
    }
    observer.step_advanced(crate::progress::count(raw.lines().len()));
    tally
}

/// Whether a revision's whole tree reads, memoized: the same revision
/// appears on many journal lines and is walked only once.
///
/// The historical revision's own node walk shares the caller's journal-line
/// counter rather than reporting nodes: a step counts one unit, and the
/// line index is the one the reader can act on.
fn revision_is_readable(
    repository: &Repository,
    identifier: RecordIdentifier,
    validity: &mut HashMap<RecordIdentifier, bool>,
    verifier: &mut NodeTreeVerifier<'_>,
) -> bool {
    if let Some(readable) = validity.get(&identifier) {
        return *readable;
    }
    let readable = verify_exact_super_root_with_verifier(
        repository,
        identifier,
        verifier,
        &mut DiscardedProgress,
    )
    .is_ok();
    validity.insert(identifier, readable);
    readable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::identifier::SegmentIdentifier;
    use crate::store::Repository;
    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::plan::*;
    use crate::writer::maintenance::prepared::*;
    use crate::writer::maintenance::test_support::*;
    use std::io::Write as _;

    #[test]
    fn journal_removal_preview_is_an_exact_bounded_byte_prefix() {
        let directory = TestDirectory::repository("bounded-journal-preview");
        let mut hostile = vec![0xff];
        hostile.extend(std::iter::repeat_n(b'x', JOURNAL_LINE_PREVIEW_LIMIT + 20));
        hostile.push(b'\n');
        std::fs::OpenOptions::new()
            .append(true)
            .open(directory.path.join("journal.log"))
            .expect("open journal")
            .write_all(&hostile)
            .expect("append long invalid line");

        let plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Journal]),
        )
        .expect("plan long invalid line");
        let removal = plan
            .journal_line_removals()
            .last()
            .expect("invalid line removal");
        assert_eq!(
            removal.preview_bytes(),
            &hostile[..JOURNAL_LINE_PREVIEW_LIMIT]
        );
        assert!(removal.preview_truncated());
        assert_eq!(removal.reason(), JournalRemovalReason::ParserSkippedNoSpace);
    }

    #[test]
    fn exhausted_journal_replacement_namespace_fails_during_read_only_planning() {
        let directory = TestDirectory::repository("journal-namespace-exhausted");
        let missing = SegmentIdentifier::new(17, 0xA000_0000_0000_0017);
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(directory.path.join("journal.log"))
                .expect("open journal"),
            "{missing}:0 root 123"
        )
        .expect("append dangling line");
        for counter in 0..1000u16 {
            std::fs::write(
                directory.path.join(format!("journal.log.bak.{counter:03}")),
                [],
            )
            .expect("occupy backup name");
        }
        let before = file_bytes(&directory.path);
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Journal]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("planning must discover exhausted backup names before apply");
        assert!(error.to_string().contains("journal.log.bak"));
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning remains read-only"
        );
        Repository::open(&directory.path).expect("repository remains healthy");
    }

    #[test]
    fn corrupt_record_in_the_selected_head_segment_never_rolls_back_silently() {
        let directory = TestDirectory::repository("corrupt-current-record");
        let head = Repository::open(&directory.path)
            .expect("repository")
            .head_record_identifier();
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(directory.path.join("journal.log"))
            .expect("open journal");
        writeln!(journal, "{}:2147483647 root 123", head.segment)
            .expect("append corrupt current revision");
        drop(journal);
        let before = file_bytes(&directory.path);

        let error = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Journal]),
        )
        .expect_err("the exact selected head record is corrupt");

        assert!(error.to_string().contains("current journal head"));
        assert_eq!(file_bytes(&directory.path), before);
    }
}
