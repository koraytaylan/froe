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
    if crate::writer::fault_injection::is_substitution_armed(
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

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::plan::*;
    use crate::writer::maintenance::prepared::*;
    use crate::writer::maintenance::test_support::*;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::store_writer::WritableRepository;

    #[test]
    fn prospective_plan_refuses_a_survivor_that_references_a_planned_removal() {
        let directory = TestDirectory::repository("prospective-survivor-reference");
        let old_generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        let target = {
            let store = WritableRepository::open(&directory.path).expect("open old-target writer");
            let target = write_empty_node_segment(&store, old_generation);
            store.close().expect("close old-target archive");
            target
        };
        let store = WritableRepository::open(&directory.path).expect("open survivor writer");
        let current_generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };
        let mut survivor_writer = store.record_writer(current_generation);
        let survivor = survivor_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "old-target".to_owned(),
                    node: target,
                },
                &[],
            )
            .expect("write unjournaled newer-generation survivor");
        survivor_writer.finish().expect("finish survivor segment");
        let content_root = write_empty_node_segment(&store, current_generation);
        let mut head_writer = store.record_writer(current_generation);
        let head = head_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("write unrelated current head");
        head_writer.finish().expect("finish current head segment");
        assert!(store.compare_and_set_head(store.head(), head));
        store.close().expect("close fixture writer");
        let before = file_bytes(&directory.path);

        let error = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect_err("prospective deletion must reject a surviving cross-reference");

        assert_eq!(
            error.to_string(),
            format!(
                "invalid segment-tar data: surviving data segment {} references segment {}, which the cleanup plan would remove",
                survivor.segment, target.segment
            )
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "prospective validation remains read-only"
        );
        Repository::open(&directory.path).expect("refused fixture remains readable");
    }

    #[test]
    fn current_head_reaching_a_one_generation_old_segment_fails_closed() {
        let directory = TestDirectory::repository("generation-invariant");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        // Exactly one generation behind the head: the boundary the retention
        // value moved. At two retained generations the arithmetic spared this
        // child, so the run proceeded on the predicate alone; at one it is
        // reclaimable and only the reference guard keeps the head's own data.
        let mut root_writer = store.record_writer(GarbageCollectionGeneration {
            generation: 1,
            full_generation: 1,
            is_compacted: false,
        });
        let old_root = root_writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("one-generation-old content root");
        root_writer.finish().expect("finish the older generation");
        let mut writer = store.record_writer(GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        });
        let new_head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: old_root,
                },
                &[],
            )
            .expect("new super root");
        writer.finish().expect("finish");
        assert!(store.compare_and_set_head(store.head(), new_head));
        store.close().expect("close");
        let before = file_bytes(&directory.path);
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("a live one-generation-old child is unsafe at one retained generation");
        assert!(
            error
                .to_string()
                .contains("current head reaches data segment")
        );
        assert_eq!(file_bytes(&directory.path), before);
        Repository::open(&directory.path).expect("refusal leaves repository healthy");
    }

    #[test]
    fn gate_deferred_garbage_is_counted_rather_than_reported_as_nothing() {
        let (directory, new_head) = sub_gate_garbage_fixture("retained-reclaimable");

        let options = CompactionOptions::default()
            .with_tasks([MaintenanceTask::Segments])
            .with_oak_savings_gate();
        let plan = plan_compaction(&directory.path, &options).expect("segment plan");
        assert_eq!(plan.current_head(), new_head);
        let deferred = plan
            .warnings()
            .iter()
            .any(|warning| warning.contains("25% rewrite gate"));
        assert!(
            deferred,
            "expected a savings deferral: {:?}",
            plan.warnings()
        );
        assert!(
            plan.retained_reclaimable_segments() != 0,
            "declined garbage must be counted, not silently dropped"
        );
        assert!(plan.retained_reclaimable_bytes() != 0);
        // The distinction the old output could not express: nothing is
        // reclaimable *by this run*, yet the store is not clean.
        assert_eq!(plan.estimated_reclaimable_bytes(), 0);
    }

    #[test]
    fn the_default_policy_reclaims_what_the_oak_gate_declines() {
        let (directory, new_head) = sub_gate_garbage_fixture("default-policy-reclaims");

        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("segment plan");
        assert_eq!(plan.current_head(), new_head);
        assert!(
            !plan
                .warnings()
                .iter()
                .any(|warning| warning.contains("rewrite gate")),
            "the default policy never defers for savings: {:?}",
            plan.warnings()
        );
        assert_eq!(
            plan.retained_reclaimable_segments(),
            0,
            "nothing identified may be left behind on an archive with letters to spare"
        );
        assert_eq!(plan.retained_reclaimable_bytes(), 0);
        assert!(
            plan.estimated_reclaimable_bytes() != 0,
            "the garbage the gate declined is now actually reclaimed"
        );
        assert!(
            plan.actions().iter().any(|action| matches!(
                action,
                CompactionAction::RewriteArchive { file_name, replacement_name, .. }
                    if file_name == "data00001a.tar" && replacement_name == "data00001b.tar"
            )),
            "the sub-gate archive is rewritten to its next generation: {:?}",
            plan.actions()
        );
    }

    #[test]
    fn the_plan_prices_the_history_veto_against_oaks_own_predicate() {
        let (directory, _old_head, _new_head) = history_veto_fixture("history-veto-price");

        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("segment plan");

        // The bootstrap revision's segments are reachable from the old
        // journal line and from nothing else.
        assert!(
            plan.history_protected_segments() != 0,
            "the bootstrap revision must be counted as history-only"
        );
        // And they are two full generations behind the head, so Oak would
        // have reclaimed them. That difference is the veto's price, and
        // reporting it is the whole point: it is what turns "reclaimed
        // nothing" into a number the operator can act on.
        let (reclaimable_segments, reclaimable_bytes) = plan.history_protected_reclaimable();
        assert!(
            reclaimable_segments != 0,
            "generation-zero history must be priced as reclaimable-but-protected"
        );
        assert!(reclaimable_bytes != 0);
        assert!(reclaimable_segments <= plan.history_protected_segments());
    }

    #[test]
    fn segment_cleanup_removes_old_unjournaled_archive_but_preserves_history() {
        let directory = TestDirectory::repository("orphan-segment-history");
        let old_head = Repository::open(&directory.path)
            .expect("old repository")
            .head_record_identifier();

        // A separate, unjournaled generation-zero archive: representative of
        // a failed write/CAS whose records never became repository state.
        {
            let store = WritableRepository::open(&directory.path).expect("open orphan writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("orphan node");
            writer.finish().expect("finish orphan segment");
            store.close().expect("close orphan writer");
        }
        assert!(directory.path.join("data00001a.tar").is_file());

        // Publish a completely independent generation-two head. It does not
        // reference generation zero; only the older journal line roots the
        // original bootstrap revision.
        let new_head = {
            let store = WritableRepository::open(&directory.path).expect("open new head writer");
            let generation = GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            };
            let mut writer = store.record_writer(generation);
            let root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("new content root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("new super root");
            writer.finish().expect("finish new head");
            assert!(store.compare_and_set_head(store.head(), head));
            store.close().expect("close new head writer");
            head
        };
        assert!(directory.path.join("data00002a.tar").is_file());

        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("segment plan");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == "data00001a.tar"
        )));
        assert!(!plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )));
        let planned_removed_segments: usize = plan
            .actions()
            .iter()
            .filter_map(|action| match action {
                CompactionAction::RemoveReclaimableArchive { segments, .. }
                | CompactionAction::RewriteArchive { segments, .. } => Some(*segments),
                _ => None,
            })
            .sum();
        assert!(planned_removed_segments != 0);

        let outcome = compact(&directory.path, options).expect("segment cleanup");
        assert_eq!(outcome.head_after, new_head);
        assert_eq!(outcome.removed_segments(), planned_removed_segments);
        assert!(!directory.path.join("data00001a.tar").exists());
        let repository = Repository::open(&directory.path).expect("healthy final repository");
        assert_eq!(repository.head_record_identifier(), new_head);
        crate::tooling::verify_node_tree(&repository, old_head)
            .expect("historical root remains readable");
    }

    /// The shape a real store has once reclamation actually starts removing
    /// things: dead segments that survive only because their archive failed
    /// the rewrite gate, still pointing at dead segments in an archive that
    /// goes away entirely.
    #[test]
    fn a_dead_survivor_pointing_at_a_removed_segment_is_handled() {
        let directory = TestDirectory::repository("dead-survivor-reference");
        let dead_generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        let live_generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };

        // An archive of nothing but dead segments: fully reclaimable, so the
        // sweep unlinks it whole.
        let target = {
            let store = WritableRepository::open(&directory.path).expect("open target writer");
            let mut writer = store.record_writer(dead_generation);
            let node = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("dead target node");
            writer.finish().expect("finish target");
            store.close().expect("close target writer");
            node
        };

        // An archive that keeps one dead segment referencing that target,
        // beside enough live segments that removing the dead one cannot
        // repay a rewrite — so the dead segment stays on disk.
        let new_head = {
            let store = WritableRepository::open(&directory.path).expect("open mixed writer");
            let mut referencing = store.record_writer(dead_generation);
            referencing
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "target".to_owned(),
                        node: target,
                    },
                    &[],
                )
                .expect("dead referencing node");
            referencing.finish().expect("finish referencing");
            for _ in 0..8 {
                let mut filler = store.record_writer(live_generation);
                filler
                    .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                    .expect("live filler");
                filler.finish().expect("finish filler");
            }
            let mut writer = store.record_writer(live_generation);
            let root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("content root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("super root");
            writer.finish().expect("finish head");
            assert!(store.compare_and_set_head(store.head(), head));
            store.close().expect("close mixed writer");
            head
        };

        // The default task set, with no retention bound: the loosened check
        // lives on this path, so this is where it must be pinned.
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);

        // Unconditional. Restoring the stricter check must fail this, so the
        // plan is required to exist rather than merely be well-explained if
        // it does not.
        let plan = plan_compaction(&directory.path, &options)
            .expect("a dead survivor pointing at removed garbage must not refuse the plan");
        let removed: usize = plan
            .actions()
            .iter()
            .filter_map(|action| match action {
                CompactionAction::RemoveReclaimableArchive { segments, .. }
                | CompactionAction::RewriteArchive { segments, .. } => Some(*segments),
                _ => None,
            })
            .sum();
        assert!(
            removed != 0,
            "the fixture must actually remove something, or it proves nothing"
        );

        let outcome = compact(&directory.path, options).expect("apply the plan");
        assert_eq!(outcome.head_after, new_head);
        assert!(outcome.removed_segments() != 0);
        let repository = Repository::open(&directory.path).expect("healthy store");
        assert_eq!(repository.head_record_identifier(), new_head);
        crate::tooling::verify_node_tree(&repository, new_head).expect("head verifies");
    }
}
