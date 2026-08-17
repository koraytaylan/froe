//! Classifying journal lines: which resolve, which are stale, and where
//! the retention boundary falls.

use super::plan::{JournalLineRemoval, JournalRemovalReason};
use super::planning::{JournalPlan, verify_exact_super_root_with_verifier};
use crate::error::{Error, Result};
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::tooling::NodeTreeVerifier;
use crate::writer::journal_maintenance::{
    RawJournal, RawJournalLine, RawJournalLineClassification,
};
use std::collections::HashMap;
use std::num::NonZeroUsize;

pub(super) struct JournalAnalysis {
    pub(super) plan: JournalPlan,
    pub(super) retained_indexes: Vec<usize>,
    pub(super) retained_record_ids: Vec<RecordIdentifier>,
}

pub(super) const JOURNAL_LINE_PREVIEW_LIMIT: usize = 160;

pub(super) fn journal_line_removal(
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

pub(super) fn analyze_journal(
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
pub(super) fn journal_retention_boundary(
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

/// The journal pass itself; [`analyze_journal`] owns the step around it.
#[allow(
    clippy::too_many_lines,
    reason = "classification, exact retained-line evidence, and removal diagnostics form one auditable journal pass"
)]
pub(super) fn analyze_journal_lines(
    repository: &Repository,
    raw: &RawJournal,
    current_head: RecordIdentifier,
    retention: Option<NonZeroUsize>,
    observer: &mut dyn ProgressObserver,
) -> Result<JournalAnalysis> {
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

    let mut parser_ignored = 0usize;
    let mut missing_segments = 0usize;
    let mut unreadable_revisions = 0usize;
    let mut beyond_retention = 0usize;
    let mut removals = Vec::new();
    let mut retained_indexes = Vec::new();
    let mut retained_record_ids = Vec::new();
    let mut validity: HashMap<RecordIdentifier, bool> = HashMap::new();
    validity.insert(current_head, true);
    let mut verifier = NodeTreeVerifier::new(repository);
    let retention_boundary = retention.and_then(|bound| {
        journal_retention_boundary(repository, raw, bound, &mut validity, &mut verifier)
    });

    for (index, line) in raw.lines().iter().enumerate() {
        observer.step_advanced(crate::progress::count(index));
        match line.classification() {
            RawJournalLineClassification::ParserSkippedNoSpace => {
                parser_ignored += 1;
                removals.push(journal_line_removal(
                    index,
                    line,
                    None,
                    JournalRemovalReason::ParserSkippedNoSpace,
                ));
            }
            RawJournalLineClassification::InvalidRecordIdentifier { .. } => {
                parser_ignored += 1;
                removals.push(journal_line_removal(
                    index,
                    line,
                    None,
                    JournalRemovalReason::InvalidRecordIdentifier,
                ));
            }
            RawJournalLineClassification::Record(record) => {
                let identifier = record.record_identifier;
                if !repository.contains_segment(identifier.segment) {
                    missing_segments += 1;
                    removals.push(journal_line_removal(
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
                    beyond_retention += 1;
                    removals.push(journal_line_removal(
                        index,
                        line,
                        Some(identifier),
                        JournalRemovalReason::BeyondRetention,
                    ));
                    continue;
                }
                let readable = if let Some(readable) = validity.get(&identifier) {
                    *readable
                } else {
                    // The historical revision's own node walk shares this
                    // step's journal-line counter rather than reporting
                    // nodes: a step counts one unit, and the line index is
                    // the one the reader can act on.
                    let readable = verify_exact_super_root_with_verifier(
                        repository,
                        identifier,
                        &mut verifier,
                        &mut DiscardedProgress,
                    )
                    .is_ok();
                    validity.insert(identifier, readable);
                    readable
                };
                if readable {
                    retained_indexes.push(index);
                    retained_record_ids.push(identifier);
                } else {
                    unreadable_revisions += 1;
                    removals.push(journal_line_removal(
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
    if !retained_record_ids.contains(&current_head) {
        return Err(Error::InvalidFormat {
            details: "journal analysis would not retain the exact current head".to_owned(),
        });
    }
    let removed_lines = parser_ignored
        .checked_add(missing_segments)
        .and_then(|count| count.checked_add(unreadable_revisions))
        .and_then(|count| count.checked_add(beyond_retention))
        .ok_or_else(|| Error::InvalidFormat {
            details: "journal line accounting overflow".to_owned(),
        })?;
    Ok(JournalAnalysis {
        plan: JournalPlan {
            retained_record_ids: retained_record_ids.clone(),
            retained_raw_lines: retained_indexes
                .iter()
                .map(|&index| raw.lines()[index].raw_bytes().to_vec())
                .collect(),
            removals,
            removed_lines,
            parser_ignored,
            missing_segments,
            unreadable_revisions,
            beyond_retention,
        },
        retained_indexes,
        retained_record_ids,
    })
}
