//! Predicting what a compaction would reclaim, and proving the head
//! never reaches a segment the sweep would remove.

/// How many segments a closure trace visits between progress reports.
/// The trace resolves one segment per step, so reporting every segment
/// would call the observer millions of times on a large store.
const SEGMENT_TRACE_REPORT_STRIDE: u64 = 256;

use super::plan::RetainedReclaimable;
use super::planning::add_estimate;
use super::stale_archives::{IndexlessCensus, generation_from_header, indexless_archive_refusal};
use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::progress::{ProgressObserver, Step, WorkUnit};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
#[cfg(test)]
use crate::segment::record::RecordType;
use crate::segment::view::SegmentView;
use crate::store::Repository;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::tooling::NodeTreeVerifier;
use crate::writer::compaction::CompactionKind;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::{
    PlannedArchiveSweep, ReclaimRule, StandaloneSegmentCompactionPlan, is_reclaimable,
    planned_unavailable_segments,
};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

/// Pre-existing bulk segments the compacted generation will reference where
/// they lie rather than re-encoding.
///
/// This is a second implementation of the sharing rule in
/// `RecordWriter::copy_binary_value`, and it must agree with it exactly or the
/// predicted sweep names the wrong archives. The rule, in the order that
/// function applies it: only `PropertyType::Binary`; only
/// `BinaryValue::Inline` (an external binary carries a blob identifier and no
/// blocks); only at or above `MEDIUM_VALUE_LIMIT`, because a shorter value is
/// materialized and re-encoded whole; and then only those blocks whose own
/// segment `is_bulk_segment()`, because a block in a *data* segment is copied.
///
/// Checkpoints this run retires are not walked: their content is not copied,
/// so it seeds nothing.
/// The generation a compaction of this kind writes into.
///
/// Named once, because the plan states it and the apply writes it: two
/// spellings of the same arithmetic is how a plan comes to describe a
/// generation the run does not produce.
pub(super) fn compaction_target_generation(
    base: GarbageCollectionGeneration,
    kind: CompactionKind,
) -> GarbageCollectionGeneration {
    match kind {
        CompactionKind::Full => GarbageCollectionGeneration {
            generation: base.generation.wrapping_add(1),
            full_generation: base.full_generation.wrapping_add(1),
            is_compacted: true,
        },
        CompactionKind::Tail => GarbageCollectionGeneration {
            generation: base.generation.wrapping_add(1),
            full_generation: base.full_generation,
            is_compacted: true,
        },
    }
}

/// Records one non-actionable disposition: counted, and named in a warning.
pub(super) fn retained_reclaimable_from(
    planned: &PlannedArchiveSweep,
    retained: &mut RetainedReclaimable,
    warnings: &mut Vec<String>,
) -> Result<()> {
    match planned {
        PlannedArchiveSweep::DeferredBySavings {
            file_name,
            segment_count,
            eligible_entry_bytes,
        } => {
            retained.below_savings_gate += segment_count;
            add_estimate(&mut retained.bytes, *eligible_entry_bytes)?;
            warnings.push(format!(
                "{file_name}: {segment_count} reclaimable segments ({}) retained because savings do not exceed Oak's 25% rewrite gate",
                crate::units::format_byte_size(*eligible_entry_bytes)
            ));
        }
        PlannedArchiveSweep::DeferredAtLastGeneration {
            file_name,
            segment_count,
            eligible_entry_bytes,
        } => {
            retained.at_last_generation += segment_count;
            add_estimate(&mut retained.bytes, *eligible_entry_bytes)?;
            warnings.push(format!(
                "{file_name}: {segment_count} reclaimable segments ({}) retained because archive generation z cannot be rewritten",
                crate::units::format_byte_size(*eligible_entry_bytes)
            ));
        }
        PlannedArchiveSweep::BlockedByOccupiedGeneration {
            file_name,
            occupied_name,
            segment_count,
            eligible_entry_bytes,
        } => {
            retained.blocked_by_occupied_generation += segment_count;
            add_estimate(&mut retained.bytes, *eligible_entry_bytes)?;
            warnings.push(format!(
                "{file_name}: {segment_count} reclaimable segments ({}) retained because {occupied_name} already exists",
                crate::units::format_byte_size(*eligible_entry_bytes)
            ));
        }
        PlannedArchiveSweep::Remove { .. } | PlannedArchiveSweep::Rewrite { .. } => {}
    }
    Ok(())
}

pub(super) fn predict_shared_bulk_segments(
    repository: &Repository,
    head: RecordIdentifier,
    omitted_checkpoints: &BTreeSet<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<HashSet<SegmentIdentifier>> {
    crate::progress::observe(
        observer,
        &Step::new("predicting the shared binary content", WorkUnit::Nodes),
        |observer| {
            let mut shared = HashSet::new();
            let mut visited: HashSet<RecordIdentifier> = HashSet::new();
            let mut pending = vec![head];
            let mut traced = crate::progress::StrideCounter::new(SEGMENT_TRACE_REPORT_STRIDE);
            while let Some(record) = pending.pop() {
                if !visited.insert(record) {
                    continue;
                }
                traced.advance(observer);
                let node = repository.node(record);
                for property in node.properties()? {
                    if property.property_type != crate::content::property::PropertyType::Binary {
                        continue;
                    }
                    let values = match &property.values {
                        crate::content::node::PropertyValues::Single(value) => {
                            std::slice::from_ref(value)
                        }
                        crate::content::node::PropertyValues::Multiple(values) => values.as_slice(),
                    };
                    for value in values {
                        let crate::content::property::PropertyValue::Binary(
                            crate::content::value::BinaryValue::Inline {
                                length,
                                record_identifier,
                            },
                        ) = value
                        else {
                            continue;
                        };
                        if *length < crate::writer::record_writer::MEDIUM_VALUE_LIMIT as u64 {
                            continue;
                        }
                        collect_shared_bulk_blocks(
                            repository,
                            *record_identifier,
                            *length,
                            &mut shared,
                        )?;
                    }
                }
                let is_checkpoint_container = record == head;
                for (name, child) in node.child_node_entries()? {
                    // The one place a name matters, and the same one the copy
                    // reads: a checkpoint this run retires is not descended
                    // into, exactly as `deep_copy_super_root_with_progress`
                    // declines to enter it.
                    if !is_checkpoint_container
                        && !omitted_checkpoints.is_empty()
                        && omitted_checkpoints.contains(&name)
                    {
                        continue;
                    }
                    pending.push(child.record_identifier());
                }
            }
            traced.finish(observer);
            Ok(shared)
        },
    )
}

/// The bulk segments holding one long inline binary's blocks.
pub(super) fn collect_shared_bulk_blocks(
    repository: &Repository,
    value: RecordIdentifier,
    length: u64,
    shared: &mut HashSet<SegmentIdentifier>,
) -> Result<()> {
    let block_count = length.div_ceil(crate::content::value::BLOCK_SIZE);
    let view = repository.segment(value.segment)?;
    let list_identifier = view.read_record_identifier(value.record_number, 8, 0)?;
    for block in
        crate::content::list::uncounted_list_entries(repository, list_identifier, block_count)?
    {
        if block.segment.is_bulk_segment() {
            shared.insert(block.segment);
        }
    }
    Ok(())
}

/// Active data segments stamped ahead of the head's own generation.
///
/// Nothing legitimate sits ahead of the head: the writer stamps what it writes
/// at the head's generation, and a compaction publishes its head before
/// anything else observes the generation it wrote. So every one of these is
/// output from an earlier run that died between finishing its copy and
/// committing the head.
///
/// They matter because no ordinary rule removes them. Compaction's own mark
/// disables the dangling-future rule, and the generation predicate spares a
/// segment *newer* than its reference on both clauses — so residue would
/// survive every future run while continuing to hold bulk segments alive
/// through its references. Left alone it accumulates once per killed run.
pub(super) fn segments_ahead_of_the_head(
    active_index_generations: &HashMap<SegmentIdentifier, GarbageCollectionGeneration>,
    reference: GarbageCollectionGeneration,
) -> usize {
    active_index_generations
        .iter()
        .filter(|(identifier, generation)| {
            identifier.is_data_segment() && generation.generation > reference.generation
        })
        .count()
}

pub(super) fn active_index_generations(
    repository: &Repository,
) -> Result<HashMap<SegmentIdentifier, GarbageCollectionGeneration>> {
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
    if !indexless.is_empty() {
        // By number, not by reader: an unindexed number is served through
        // every one of its non-empty letters, and reporting those letters as
        // separate damaged archives would overstate the damage.
        let number_of =
            |file_name: &str| ArchiveFileName::parse(file_name).map(|n| n.archive_number);
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
        return Err(Error::InvalidFormat {
            details: indexless_archive_refusal(IndexlessCensus {
                total_numbers: total_numbers.len(),
                indexless_numbers: indexless_numbers.len(),
                offenders: &indexless,
                newest_is_indexless,
                any_scan_is_empty,
            }),
        });
    }
    let mut generations = HashMap::new();
    for archive in repository.archives() {
        for identifier in archive.segment_identifiers() {
            // Unreachable once the census above passes: an indexed archive
            // enumerates its segments out of the very index this looks them
            // up in. Kept so a change to either side fails closed.
            let entry = archive
                .index_entry(identifier)
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "active archive {} has no index metadata for its own segment {identifier}; refusing generation cleanup",
                        archive.file_name()
                    ),
                })?;
            generations.insert(
                identifier,
                GarbageCollectionGeneration {
                    generation: entry.generation,
                    full_generation: entry.full_generation,
                    is_compacted: entry.is_compacted,
                },
            );
        }
    }
    Ok(generations)
}

pub(super) fn extend_segment_closure(
    provider: &dyn SegmentProvider,
    roots: impl IntoIterator<Item = SegmentIdentifier>,
    seen: &mut HashSet<SegmentIdentifier>,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let mut pending: VecDeque<_> = roots.into_iter().collect();
    let mut traced = crate::progress::StrideCounter::new(SEGMENT_TRACE_REPORT_STRIDE);
    while let Some(identifier) = pending.pop_front() {
        if !seen.insert(identifier) {
            continue;
        }
        let segment = provider.segment(identifier)?;
        traced.advance(observer);
        pending.extend(segment.structure.referenced_segments.iter().copied());
    }
    traced.finish(observer);
    Ok(())
}

/// Proves the current head reaches nothing the run's own reclaim rule would
/// discard.
///
/// The predicate alone does not make head-reachable data safe — it only makes
/// *older-generation* data reclaimable, and whether the head reaches any is a
/// property of the store, not of the arithmetic. So this re-evaluates the
/// identical rule the mark phase will consume, over the head's transitive
/// segment closure, and refuses before any mutation. Bulk segments are skipped
/// deliberately: they carry the null generation triple by format mandate and
/// are reclaimed by reachability, which the mark phase's reference set decides.
pub(super) fn validate_reclaim_reference_invariant(
    repository: &Repository,
    current_closure: &HashSet<SegmentIdentifier>,
    active_index_generations: &HashMap<SegmentIdentifier, GarbageCollectionGeneration>,
    rule: ReclaimRule,
) -> Result<()> {
    for &identifier in current_closure {
        if !identifier.is_data_segment() {
            continue;
        }
        let header = generation_from_header(repository, identifier)?;
        let indexed = active_index_generations.get(&identifier).ok_or_else(|| {
            Error::InvalidFormat {
                details: format!(
                    "current head reaches data segment {identifier}, but no active archive index describes it"
                ),
            }
        })?;
        if *indexed != header {
            return Err(Error::InvalidFormat {
                details: format!(
                    "segment {identifier} has index generation {indexed:?}, but its header says {header:?}"
                ),
            });
        }
        if is_reclaimable(rule.reference, header, rule.kind, rule.retained_generations) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "current head reaches data segment {identifier} in reclaimable generation {header:?}; refusing to trust generation cleanup"
                ),
            });
        }
    }
    Ok(())
}

pub(super) struct ExcludingProvider<'repository> {
    pub(super) repository: &'repository Repository,
    pub(super) unavailable: &'repository HashSet<SegmentIdentifier>,
}

impl SegmentProvider for ExcludingProvider<'_> {
    fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        if self.unavailable.contains(&identifier) {
            return Err(Error::SegmentNotFound {
                segment_identifier: identifier,
            });
        }
        self.repository.segment(identifier)
    }

    fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, identifier).map(Arc::from)
    }

    fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, identifier).map(Arc::new)
    }
}

pub(super) fn validate_prospective_segment_plan(
    directory: &Path,
    repository: &Repository,
    plan: &StandaloneSegmentCompactionPlan,
    retained_roots: &[RecordIdentifier],
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let unavailable = planned_unavailable_segments(directory, plan)?;
    if unavailable.is_empty() {
        return Ok(());
    }
    let provider = ExcludingProvider {
        repository,
        unavailable: &unavailable,
    };
    let mut verifier = NodeTreeVerifier::new(&provider);
    for &root in retained_roots {
        verifier
            .verify_with_progress(root, observer)
            .map_err(|error| Error::InvalidFormat {
                details: format!(
                    "segment cleanup would make retained journal root {root} unreadable: {error}"
                ),
            })?;
    }

    // Live survivors only. A segment the mark phase already proved
    // reclaimable is garbage that merely outlived the sweep, because the
    // archive holding it did not repay a rewrite; nothing reachable reads it,
    // and a dangling reference out of garbage is the ordinary state Oak
    // leaves behind every partial sweep. Refusing on those made the whole
    // plan fail on exactly the stores this reclamation exists for — ones
    // with dead segments scattered through archives the 25% gate defers.
    // What must not dangle is anything still reachable, and that is proved
    // above against every retained root.
    let reclaimable = plan.reclaimable_segments();
    for identifier in repository.distinct_segment_identifiers() {
        if !identifier.is_data_segment()
            || unavailable.contains(&identifier)
            || reclaimable.contains(&identifier)
        {
            continue;
        }
        let segment = repository.segment(identifier)?;
        if let Some(target) = segment
            .structure
            .referenced_segments
            .iter()
            .find(|target| unavailable.contains(target))
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "surviving data segment {identifier} references segment {target}, which the cleanup plan would remove"
                ),
            });
        }
    }
    Ok(())
}

pub(super) fn prospective_retained_roots<'roots>(
    directory: &Path,
    repository: &Repository,
    plan: &StandaloneSegmentCompactionPlan,
    retained_roots: &'roots [RecordIdentifier],
) -> Cow<'roots, [RecordIdentifier]> {
    #[cfg(test)]
    if crate::writer::maintenance_fault_injection::is_substitution_armed(
        "cleanup.before-prospective-retained-root-verification",
    ) {
        let unavailable = planned_unavailable_segments(directory, plan)
            .expect("armed prospective-root fixture must have a valid physical plan");
        for identifier in unavailable {
            let segment = repository
                .segment(identifier)
                .expect("armed prospective-root fixture segment must be readable");
            if let Some(entry) = segment
                .structure
                .record_table()
                .iter()
                .find(|entry| entry.record_type() == Some(RecordType::Node))
            {
                let mut injected = retained_roots.to_vec();
                injected.push(RecordIdentifier::new(identifier, entry.record_number));
                return Cow::Owned(injected);
            }
        }
        panic!("prospective retained-root fault fixture has no removable node record");
    }
    let _ = (directory, repository, plan);
    Cow::Borrowed(retained_roots)
}
