//! What a segment run would reclaim: the head's record closure, the
//! history the journal still protects, and the sweeps predicted from
//! both.

use super::*;

/// What tracing the head's reachable set needs.
pub(crate) struct HeadClosureInputs<'inputs> {
    pub(crate) repository: &'inputs Repository,
    pub(crate) options: &'inputs crate::writer::maintenance::options::CompactionOptions,
    pub(crate) checkpoints: &'inputs CheckpointPlan,
    pub(crate) current_head: RecordIdentifier,
    pub(crate) reference_generation: GarbageCollectionGeneration,
    pub(crate) reclaim_rule: ReclaimRule,
    pub(crate) index_available: bool,
}

/// Every segment the current head reaches, and how many segments sit ahead
/// of it — the residue an interrupted run left behind.
pub(crate) fn trace_head_closure(
    inputs: &HeadClosureInputs<'_>,
    observer: &mut dyn ProgressObserver,
) -> Result<(HashSet<SegmentIdentifier>, usize)> {
    let &HeadClosureInputs {
        repository,
        options,
        checkpoints,
        current_head,
        reference_generation,
        reclaim_rule,
        index_available,
    } = inputs;
    let mut current_closure = HashSet::new();
    let mut residue_segments = 0usize;
    if index_available
        && (options.contains(MaintenanceTask::Segments) || !checkpoints.names.is_empty())
    {
        let active_index_generations = active_index_generations(repository)?;
        residue_segments =
            segments_ahead_of_the_head(&active_index_generations, reference_generation);
        crate::progress::observe(
            observer,
            &Step::new(
                "tracing segments reachable from the head",
                WorkUnit::Segments,
            ),
            |observer| {
                extend_segment_closure(
                    repository,
                    [current_head.segment],
                    &mut current_closure,
                    observer,
                )
            },
        )?;
        validate_reclaim_reference_invariant(
            repository,
            &current_closure,
            &active_index_generations,
            reclaim_rule,
        )?;
    }
    // The residue sweep, planned with the generation predicate disabled so
    // nothing is reclaimed by age: `i32::MAX` retained generations makes
    // `is_reclaimable` false for every realistic triple, leaving only the
    // dangling-future rule — which reclaims exactly the compacted entries
    // positioned after the head in global reverse order — and the rule that
    // frees a bulk segment nothing points at. Planned before the copy, so the
    // next run heals what a killed run left rather than accumulating it.
    Ok((current_closure, residue_segments))
}

/// What predicting a run's sweeps needs.
pub(crate) struct PredictedSweepInputs<'inputs> {
    pub(crate) directory: &'inputs Path,
    pub(crate) repository: &'inputs Repository,
    pub(crate) options: &'inputs crate::writer::maintenance::options::CompactionOptions,
    pub(crate) index_available: bool,
    pub(crate) current_head: RecordIdentifier,
    pub(crate) reference_generation: GarbageCollectionGeneration,
    pub(crate) residue_segments: usize,
    pub(crate) checkpoints: &'inputs CheckpointPlan,
    pub(crate) stale_archives: &'inputs [StaleArchive],
}

/// Predicts the residue sweep and, when a copy is selected, the sweep that
/// copy's own reclaim pass will perform.
///
/// Both are planned read-only so the operator sees which archives go before
/// authorizing the run — exact rather than estimated, using the same mark
/// and per-archive planner the run itself uses.
pub(crate) fn predict_sweeps(
    inputs: &PredictedSweepInputs<'_>,
    observer: &mut dyn ProgressObserver,
) -> Result<(
    Option<StandaloneSegmentCompactionPlan>,
    Option<StandaloneSegmentCompactionPlan>,
)> {
    let &PredictedSweepInputs {
        directory,
        repository,
        options,
        index_available,
        current_head,
        reference_generation,
        residue_segments,
        checkpoints,
        stale_archives,
    } = inputs;
    let residue_sweep = if index_available && residue_segments != 0 {
        Some(crate::progress::observe(
            observer,
            &Step::new("planning residue retirement", WorkUnit::Archives)
                .with_total(crate::progress::count(repository.archives().len())),
            |observer| {
                plan_standalone_segment_cleanup(
                    directory,
                    repository,
                    ReclaimRule {
                        reference: reference_generation,
                        kind: CompactionKind::Full,
                        retained_generations: i32::MAX,
                    },
                    current_head.segment,
                    &HashSet::new(),
                    options.archive_rewrite_policy,
                    observer,
                )
            },
        )?)
    } else {
        None
    };
    // What the copy's own reclaim pass will do, planned read-only so the
    // operator sees which archives go before authorizing the run. Exact rather
    // than estimated: the same mark and the same per-archive planner the run
    // itself uses, seeded with the bulk segments the copy will share in place.
    let predicted_sweep = match options.compaction_kind {
        Some(kind) if index_available => {
            let omitted: BTreeSet<String> = checkpoints.names.iter().cloned().collect();
            let shared =
                predict_shared_bulk_segments(repository, current_head, &omitted, observer)?;
            let target = compaction_target_generation(reference_generation, kind);
            let absent: HashSet<String> = stale_archives
                .iter()
                .map(|stale| stale.file_name.clone())
                .collect();
            Some(crate::progress::observe(
                observer,
                &Step::new("predicting the reclamation", WorkUnit::Archives)
                    .with_total(crate::progress::count(repository.archives().len())),
                |_observer| {
                    crate::writer::store_writer::predict_post_compaction_reclamation(
                        directory,
                        repository,
                        ReclaimRule {
                            reference: target,
                            kind,
                            retained_generations: crate::writer::store_writer::RETAINED_GENERATIONS,
                        },
                        &shared,
                        options.archive_rewrite_policy,
                        &absent,
                    )
                },
            )?)
        }
        _ => None,
    };
    Ok((residue_sweep, predicted_sweep))
}

/// What planning a standalone segment reclamation needs.
pub(crate) struct SegmentPlanInputs<'inputs> {
    pub(crate) directory: &'inputs Path,
    pub(crate) repository: &'inputs Repository,
    pub(crate) options: &'inputs crate::writer::maintenance::options::CompactionOptions,
    pub(crate) index_available: bool,
    pub(crate) current_head: RecordIdentifier,
    pub(crate) reclaim_rule: ReclaimRule,
    pub(crate) journal_analysis: &'inputs JournalAnalysis,
}

/// Extends a closure with every segment the retained journal history
/// still reaches, so history-only segments survive the sweep.
pub(crate) fn trace_history_closure(
    repository: &Repository,
    journal_analysis: &JournalAnalysis,
    retained_closure: &mut HashSet<SegmentIdentifier>,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    crate::progress::observe(
        observer,
        &Step::new(
            "tracing segments reachable from history",
            WorkUnit::Segments,
        ),
        |observer| {
            extend_segment_closure(
                repository,
                journal_analysis
                    .retained_record_ids
                    .iter()
                    .map(|record| record.segment),
                retained_closure,
                observer,
            )
        },
    )?;
    Ok(())
}

/// Plans the reclamation a run without a copy would perform.
///
/// A sweep planned here judges every segment against the *current* head
/// generation. When a run copies the head into a fresh generation that
/// reference is superseded before a single archive is touched, so the
/// copy's own reclaim pass sweeps instead — which is why this is skipped
/// whenever a compaction kind was selected.
pub(crate) fn plan_standalone_segments(
    inputs: &SegmentPlanInputs<'_>,
    current_closure: HashSet<SegmentIdentifier>,
    protected_history_segments: &mut HashSet<SegmentIdentifier>,
    history_protection: &mut HistoryProtection,
    observer: &mut dyn ProgressObserver,
) -> Result<Option<StandaloneSegmentCompactionPlan>> {
    let &SegmentPlanInputs {
        directory,
        repository,
        options,
        index_available,
        current_head,
        reclaim_rule,
        journal_analysis,
    } = inputs;
    let segment_plan = if index_available
        && options.contains(MaintenanceTask::Segments)
        && options.compaction_kind.is_none()
    {
        // Captured before the head closure is consumed as the history seed:
        // afterwards the two are indistinguishable, and the difference is
        // exactly what the journal history costs.
        let head_data_segments: HashSet<SegmentIdentifier> = current_closure
            .iter()
            .copied()
            .filter(|identifier| identifier.is_data_segment())
            .collect();
        let mut retained_closure = current_closure;
        trace_history_closure(
            repository,
            journal_analysis,
            &mut retained_closure,
            observer,
        )?;
        protected_history_segments.extend(
            retained_closure
                .into_iter()
                .filter(|identifier| identifier.is_data_segment()),
        );
        history_protection.history_only_segments = protected_history_segments
            .iter()
            .filter(|identifier| !head_data_segments.contains(identifier))
            .count();

        let plan = crate::progress::observe(
            observer,
            &Step::new("planning segment reclamation", WorkUnit::Archives)
                .with_total(crate::progress::count(repository.archives().len())),
            |observer| {
                plan_standalone_segment_cleanup(
                    directory,
                    repository,
                    reclaim_rule,
                    current_head.segment,
                    protected_history_segments,
                    options.archive_rewrite_policy,
                    observer,
                )
            },
        )?;
        // What the veto costs, priced by the sweep itself rather than
        // estimated beside it. Skipped when the veto protects nothing the
        // head does not already reach, because then there is nothing to
        // price and the second pass would be pure cost.
        if history_protection.history_only_segments != 0 {
            let (unvetoed_segments, unvetoed_bytes) = crate::progress::observe(
                observer,
                &Step::new("pricing the journal-history protection", WorkUnit::Archives)
                    .with_total(crate::progress::count(repository.archives().len())),
                |observer| {
                    crate::writer::store_writer::measure_unvetoed_reclamation(
                        directory,
                        repository,
                        reclaim_rule,
                        current_head.segment,
                        options.archive_rewrite_policy,
                        observer,
                    )
                },
            )?;
            let (vetoed_segments, vetoed_bytes) =
                crate::writer::store_writer::plan_reclaimed_totals(&plan);
            history_protection.would_be_reclaimable_segments =
                unvetoed_segments.saturating_sub(vetoed_segments);
            history_protection.would_be_reclaimable_bytes =
                unvetoed_bytes.saturating_sub(vetoed_bytes);
        }
        let retained_roots = prospective_retained_roots(
            directory,
            repository,
            &plan,
            &journal_analysis.retained_record_ids,
        );
        crate::progress::observe(
            observer,
            &Step::new("validating the prospective plan", WorkUnit::Nodes),
            |observer| {
                validate_prospective_segment_plan(
                    directory,
                    repository,
                    &plan,
                    &retained_roots,
                    observer,
                )
            },
        )?;
        Some(plan)
    } else {
        None
    };

    Ok(segment_plan)
}

/// Everything a plan decides about segments and archives.
pub(crate) struct SegmentWork {
    pub(crate) stale_archives: Vec<StaleArchive>,
    pub(crate) reference_generation: GarbageCollectionGeneration,
    pub(crate) segment_plan: Option<StandaloneSegmentCompactionPlan>,
    pub(crate) residue_sweep: Option<StandaloneSegmentCompactionPlan>,
    pub(crate) predicted_sweep: Option<StandaloneSegmentCompactionPlan>,
    pub(crate) protected_history_segments: HashSet<SegmentIdentifier>,
    pub(crate) history_protection: HistoryProtection,
    pub(crate) residue_segments: usize,
}

/// What tracing the head and predicting the sweeps yields.
pub(crate) struct TracedSweeps {
    pub(crate) current_closure: HashSet<SegmentIdentifier>,
    pub(crate) residue_sweep: Option<StandaloneSegmentCompactionPlan>,
    pub(crate) predicted_sweep: Option<StandaloneSegmentCompactionPlan>,
    pub(crate) residue_segments: usize,
}

/// Traces what the head reaches and predicts the sweeps a run would
/// perform against it.
#[allow(
    clippy::too_many_arguments,
    reason = "the trace and the prediction judge the same store against the same rule"
)]
pub(crate) fn trace_and_predict(
    directory: &Path,
    repository: &Repository,
    options: &crate::writer::maintenance::options::CompactionOptions,
    current_head: RecordIdentifier,
    index_available: bool,
    checkpoints: &CheckpointPlan,
    stale_archives: &[StaleArchive],
    reference_generation: GarbageCollectionGeneration,
    reclaim_rule: ReclaimRule,
    observer: &mut dyn ProgressObserver,
) -> Result<TracedSweeps> {
    let (current_closure, residue_segments) = trace_head_closure(
        &HeadClosureInputs {
            repository,
            options,
            checkpoints,
            current_head,
            reference_generation,
            reclaim_rule,
            index_available,
        },
        observer,
    )?;
    let (residue_sweep, predicted_sweep) = predict_sweeps(
        &PredictedSweepInputs {
            directory,
            repository,
            options,
            index_available,
            current_head,
            reference_generation,
            residue_segments,
            checkpoints,
            stale_archives,
        },
        observer,
    )?;
    Ok(TracedSweeps {
        current_closure,
        residue_sweep,
        predicted_sweep,
        residue_segments,
    })
}

/// Decides which archives are stale, what the head reaches, what a run
/// would sweep, and what the journal history protects from it.
#[allow(
    clippy::too_many_arguments,
    reason = "every phase here judges the same store against the same head and options"
)]
pub(crate) fn plan_segment_work(
    directory: &Path,
    repository: &Repository,
    options: &crate::writer::maintenance::options::CompactionOptions,
    current_head: RecordIdentifier,
    index_available: bool,
    checkpoints: &CheckpointPlan,
    journal_analysis: &JournalAnalysis,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<SegmentWork> {
    let stale_archives = if index_available && options.contains(MaintenanceTask::StaleArchives) {
        crate::progress::observe(
            observer,
            &Step::new("scanning for stale archives", WorkUnit::Archives)
                .with_total(crate::progress::count(repository.archives().len())),
            |observer| plan_stale_archives(directory, repository, warnings, observer),
        )?
    } else {
        Vec::new()
    };

    let reference_generation = generation_from_header(repository, current_head.segment)?;
    // One rule per run, read by the head-safety guard and by the mark phase
    // below. Binding it here rather than rebuilding it at each use is what
    // makes "the guard proved exactly what the sweep will apply" a property of
    // the code instead of a coincidence between two constants.
    let reclaim_rule = ReclaimRule {
        reference: reference_generation,
        kind: CompactionKind::Full,
        retained_generations: crate::writer::store_writer::RETAINED_GENERATIONS,
    };
    let TracedSweeps {
        current_closure,
        residue_sweep,
        predicted_sweep,
        residue_segments,
    } = trace_and_predict(
        directory,
        repository,
        options,
        current_head,
        index_available,
        checkpoints,
        &stale_archives,
        reference_generation,
        reclaim_rule,
        observer,
    )?;
    let mut protected_history_segments = HashSet::new();
    let mut history_protection = HistoryProtection::default();
    // A sweep planned here judges every segment against the *current* head
    // generation. When this run copies the head into a fresh generation, that
    // reference is superseded before a single archive is touched and the plan
    // would authorize the wrong set. The copy's own reclaim pass sweeps
    // instead, against the generation it just created.
    let segment_plan = plan_standalone_segments(
        &SegmentPlanInputs {
            directory,
            repository,
            options,
            index_available,
            current_head,
            reclaim_rule,
            journal_analysis,
        },
        current_closure,
        &mut protected_history_segments,
        &mut history_protection,
        observer,
    )?;
    Ok(SegmentWork {
        stale_archives,
        reference_generation,
        segment_plan,
        residue_sweep,
        predicted_sweep,
        protected_history_segments,
        history_protection,
        residue_segments,
    })
}

/// Verifies the exact head tree, reporting it as one step of its own.
/// A function that reports owns its step: wrapping this call in a step
/// belonging to a caller would put two different counters — nodes here,
/// whatever the caller counts — inside one report.
pub(in crate::writer::maintenance) fn verify_exact_super_root(
    repository: &Repository,
    head: RecordIdentifier,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    verify_exact_super_root_counting_nodes(repository, head, observer).map(|_| ())
}

/// Verifies exactly like [`verify_exact_super_root`], returning how many
/// distinct node records the head reaches. The walk happens either way, so
/// the count is free; it is what the plan reports as the size of the tree a
/// copy would rewrite.
pub(in crate::writer::maintenance) fn verify_exact_super_root_counting_nodes(
    repository: &Repository,
    head: RecordIdentifier,
    observer: &mut dyn ProgressObserver,
) -> Result<u64> {
    crate::progress::observe(
        observer,
        &Step::new("verifying the current head", WorkUnit::Nodes),
        |observer| {
            let mut verifier = NodeTreeVerifier::new(repository);
            verify_exact_super_root_with_verifier(repository, head, &mut verifier, observer)?;
            Ok(verifier.verified_nodes())
        },
    )
}

pub(in crate::writer::maintenance) fn verify_exact_super_root_with_verifier(
    repository: &Repository,
    head: RecordIdentifier,
    verifier: &mut NodeTreeVerifier<'_>,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let view = repository.segment(head.segment)?;
    if view.structure.record_type(head.record_number) != Some(RecordType::Node) {
        return Err(Error::InvalidFormat {
            details: format!("current journal head {head} is not a node record"),
        });
    }
    verifier.verify_with_progress(head, observer)?;
    let super_root = repository.node(head);
    super_root
        .child_node("root")?
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("journal root {head} has no content \"root\" child node"),
        })?;
    if let Some(checkpoints) = super_root.child_node("checkpoints")? {
        for (name, checkpoint) in checkpoints.child_node_entries()? {
            checkpoint
                .child_node("root")?
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "checkpoint {name} under journal root {head} has no snapshot \"root\" child node"
                    ),
                })?;
        }
    }
    Ok(())
}
