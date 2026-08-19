//! What a segment run would reclaim: the head's record closure, the
//! history the journal still protects, and the sweeps predicted from
//! both.

use crate::content::provider::SegmentProvider as _;

use super::{
    BTreeSet, CheckpointPlan, CompactionKind, Error, GarbageCollectionGeneration, HashSet,
    HistoryProtection, JournalAnalysis, MaintenanceTask, NodeTreeVerifier, Path,
    PlannedArchiveSweep, ProgressObserver, ReclaimRule, RecordIdentifier, RecordType, Repository,
    Result, SegmentIdentifier, StaleArchive, StandaloneSegmentCompactionPlan, Step, WorkUnit,
    active_index_generations, compaction_target_generation, extend_segment_closure,
    generation_from_header, plan_stale_archives, plan_standalone_segment_cleanup,
    predict_shared_bulk_segments, prospective_retained_roots, segments_ahead_of_the_head,
    validate_prospective_segment_plan, validate_reclaim_reference_invariant,
};

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

/// What tracing the head's reachable set established.
pub(crate) struct HeadClosure {
    /// Every segment the current head reaches.
    pub(crate) segments: HashSet<SegmentIdentifier>,
    /// Active data segments stamped ahead of the head — the residue an
    /// interrupted run left behind.
    pub(crate) residue_segments: usize,
    /// The closure's indexed bytes as `(data, bulk)`, or `None` when the
    /// trace did not run. Data bytes bound what a copy writes; bulk bytes
    /// are shared in place and never copied.
    pub(crate) indexed_bytes: Option<(u64, u64)>,
    /// Whether every data segment the head reaches carries the head
    /// segment's own compacted generation triple — the store-side half of
    /// the convergence gate. Triple equality against the reference, never
    /// ordering and never a store-wide maximum: old Java-written archives
    /// can carry version-1-index synthesized full generations far ahead of
    /// the real ones, generation arithmetic wraps, and killed-run residue
    /// stamped ahead of the head sits outside the closure entirely, so none
    /// of them can confuse an equality test. `false` when the trace did not
    /// run.
    pub(crate) uniformly_compacted_at_reference: bool,
}

/// Every segment the current head reaches, its indexed byte composition,
/// and how many segments sit ahead of it — the residue an interrupted run
/// left behind.
pub(crate) fn trace_head_closure(
    inputs: &HeadClosureInputs<'_>,
    observer: &mut dyn ProgressObserver,
) -> Result<HeadClosure> {
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
    let mut indexed_bytes = None;
    let mut uniformly_compacted_at_reference = false;
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
                )?;
                // The closure's composition is what an operator needs to
                // read a store at a glance: how much is node data the copy
                // rewrites, and how much is binary blocks it shares.
                let (data_bytes, bulk_bytes) =
                    crate::writer::maintenance::reclamation::indexed_bytes_by_kind(
                        repository,
                        &current_closure,
                    );
                indexed_bytes = Some((data_bytes, bulk_bytes));
                if bulk_bytes == 0 {
                    observer.step_concluded(&format!(
                        "the head reaches {} of node data",
                        crate::units::format_byte_size(data_bytes),
                    ));
                } else {
                    observer.step_concluded(&format!(
                        "the head reaches {} of node data and {} of shared binary blocks",
                        crate::units::format_byte_size(data_bytes),
                        crate::units::format_byte_size(bulk_bytes),
                    ));
                }
                Ok::<(), Error>(())
            },
        )?;
        validate_reclaim_reference_invariant(
            repository,
            &current_closure,
            &active_index_generations,
            reclaim_rule,
        )?;
        uniformly_compacted_at_reference = reference_generation.is_compacted
            && current_closure
                .iter()
                .filter(|identifier| identifier.is_data_segment())
                .all(|identifier| {
                    active_index_generations.get(identifier) == Some(&reference_generation)
                });
    }
    // The residue sweep, planned with the generation predicate disabled so
    // nothing is reclaimed by age: `i32::MAX` retained generations makes
    // `is_reclaimable` false for every realistic triple, leaving only the
    // dangling-future rule — which reclaims exactly the compacted entries
    // positioned after the head in global reverse order — and the rule that
    // frees a bulk segment nothing points at. Planned before the copy, so the
    // next run heals what a killed run left rather than accumulating it.
    Ok(HeadClosure {
        segments: current_closure,
        residue_segments,
        indexed_bytes,
        uniformly_compacted_at_reference,
    })
}

/// What predicting a run's sweeps needs.
pub(crate) struct PredictedSweepInputs<'inputs> {
    pub(crate) directory: &'inputs Path,
    pub(crate) repository: &'inputs Repository,
    pub(crate) options: &'inputs crate::writer::maintenance::options::CompactionOptions,
    /// The compaction the run will actually perform — the gate's verdict,
    /// not the raw selection, so no prediction is spent on a dropped copy.
    pub(crate) compaction_kind: Option<CompactionKind>,
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
        compaction_kind,
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
    let predicted_sweep = match compaction_kind {
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
                |observer| {
                    let predicted =
                        crate::writer::store_writer::predict_post_compaction_reclamation(
                            directory,
                            repository,
                            ReclaimRule {
                                reference: target,
                                kind,
                                retained_generations:
                                    crate::writer::store_writer::RETAINED_GENERATIONS,
                            },
                            &shared,
                            options.archive_rewrite_policy,
                            &absent,
                        )?;
                    observer.step_concluded(&predicted_sweep_conclusion(&predicted));
                    Ok::<_, Error>(predicted)
                },
            )?)
        }
        _ => None,
    };
    Ok((residue_sweep, predicted_sweep))
}

/// One clause summarizing a predicted sweep, for its step's completion
/// line: which archives go whole, which are rewritten, and how many bytes
/// each disposition frees.
fn predicted_sweep_conclusion(predicted: &StandaloneSegmentCompactionPlan) -> String {
    let mut removed_archives = 0u64;
    let mut removed_bytes = 0u64;
    let mut rewritten_archives = 0u64;
    let mut rewritten_entry_bytes = 0u64;
    for archive in &predicted.archives {
        match archive {
            PlannedArchiveSweep::Remove { file_bytes, .. } => {
                removed_archives += 1;
                removed_bytes = removed_bytes.saturating_add(*file_bytes);
            }
            PlannedArchiveSweep::Rewrite {
                eligible_entry_bytes,
                ..
            } => {
                rewritten_archives += 1;
                rewritten_entry_bytes = rewritten_entry_bytes.saturating_add(*eligible_entry_bytes);
            }
            PlannedArchiveSweep::DeferredBySavings { .. }
            | PlannedArchiveSweep::DeferredAtLastGeneration { .. }
            | PlannedArchiveSweep::BlockedByOccupiedGeneration { .. } => {}
        }
    }
    let archives_phrase = |count: u64| {
        let noun = if count == 1 { "archive" } else { "archives" };
        format!("{} {noun}", crate::units::format_count(count))
    };
    match (removed_archives, rewritten_archives) {
        (0, 0) => "the sweep has nothing to reclaim".to_owned(),
        (_, 0) => format!(
            "the sweep removes {} ({})",
            archives_phrase(removed_archives),
            crate::units::format_byte_size(removed_bytes),
        ),
        (0, _) => format!(
            "the sweep rewrites {} ({} of entries)",
            archives_phrase(rewritten_archives),
            crate::units::format_byte_size(rewritten_entry_bytes),
        ),
        (_, _) => format!(
            "the sweep removes {} ({}) and rewrites {} ({} of entries)",
            archives_phrase(removed_archives),
            crate::units::format_byte_size(removed_bytes),
            archives_phrase(rewritten_archives),
            crate::units::format_byte_size(rewritten_entry_bytes),
        ),
    }
}

/// What planning a standalone segment reclamation needs.
pub(crate) struct SegmentPlanInputs<'inputs> {
    pub(crate) directory: &'inputs Path,
    pub(crate) repository: &'inputs Repository,
    pub(crate) options: &'inputs crate::writer::maintenance::options::CompactionOptions,
    /// The gate's verdict: a standalone sweep is planned exactly when no
    /// copy will run, whether none was selected or the gate dropped it.
    pub(crate) effective_compaction_kind: Option<CompactionKind>,
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
        effective_compaction_kind,
        index_available,
        current_head,
        reclaim_rule,
        journal_analysis,
    } = inputs;
    let segment_plan = if index_available
        && options.contains(MaintenanceTask::Segments)
        && effective_compaction_kind.is_none()
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
    /// The head closure's `(data, bulk)` bytes, when the trace ran.
    pub(crate) closure_indexed_bytes: Option<(u64, u64)>,
    /// The compaction this run will actually perform: the selected kind,
    /// unless the convergence gate proved the copy pointless and dropped it.
    pub(crate) effective_compaction_kind: Option<CompactionKind>,
    /// Whether the gate held: the head is already fully compacted, so a
    /// selected copy was dropped from the plan.
    pub(crate) already_fully_compacted: bool,
}

/// What the journal contributes to the convergence gate: whether it already
/// holds exactly one line, and that line names the current head.
pub(crate) struct JournalConvergence {
    pub(crate) single_line_naming_head: bool,
}

/// Decides whether a selected copy is worth performing.
///
/// **`AlreadyCompact`** exactly when every data segment the head reaches
/// carries the head's own compacted triple, no checkpoint is selected for
/// omission, no version history is selected for purge — content omissions
/// force the copy however compact the store is — and the journal already
/// holds the single line a completed compaction leaves — the journal condition is a belt only, since any
/// write after a compaction already breaks triple uniformity: non-compacting
/// writes stamp the head generation with the compacted flag cleared
/// (`store_writer/repository/writes.rs`). A gated run performs the
/// standalone sweep instead, so garbage never hides behind the gate; the
/// only thing it drops is the swap that would replace a generation with an
/// identical one.
fn convergence_gate(
    options: &crate::writer::maintenance::options::CompactionOptions,
    closure: &HeadClosure,
    checkpoints: &CheckpointPlan,
    journal: &JournalConvergence,
    purge_selected: bool,
) -> (Option<CompactionKind>, bool) {
    let Some(kind) = options.compaction_kind else {
        return (None, false);
    };
    let already_fully_compacted = closure.uniformly_compacted_at_reference
        && checkpoints.names.is_empty()
        && journal.single_line_naming_head
        && !purge_selected;
    if already_fully_compacted && !options.always_copy {
        (None, true)
    } else {
        (Some(kind), already_fully_compacted)
    }
}

/// What deciding a plan's segment work needs: one store, one head, one
/// option set, and the journal facts the convergence gate reads.
pub(crate) struct SegmentWorkInputs<'inputs> {
    pub(crate) directory: &'inputs Path,
    pub(crate) repository: &'inputs Repository,
    pub(crate) options: &'inputs crate::writer::maintenance::options::CompactionOptions,
    pub(crate) current_head: RecordIdentifier,
    pub(crate) index_available: bool,
    pub(crate) checkpoints: &'inputs CheckpointPlan,
    pub(crate) journal_analysis: &'inputs JournalAnalysis,
    pub(crate) journal_convergence: &'inputs JournalConvergence,
    /// Whether this run omits version histories from the copy — content
    /// omissions force the copy, however compact the store already is.
    pub(crate) purge_selected: bool,
}

pub(crate) fn plan_segment_work(
    inputs: &SegmentWorkInputs<'_>,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<SegmentWork> {
    let &SegmentWorkInputs {
        directory,
        repository,
        options,
        current_head,
        index_available,
        checkpoints,
        journal_analysis,
        journal_convergence,
        purge_selected,
    } = inputs;
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
    let closure = trace_head_closure(
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
    // The gate sits exactly here — after the trace established the facts,
    // before a single prediction is spent on a copy that will not happen.
    let (effective_compaction_kind, already_fully_compacted) = convergence_gate(
        options,
        &closure,
        checkpoints,
        journal_convergence,
        purge_selected,
    );
    let HeadClosure {
        segments: current_closure,
        residue_segments,
        indexed_bytes: closure_indexed_bytes,
        ..
    } = closure;
    let (residue_sweep, predicted_sweep) = predict_sweeps(
        &PredictedSweepInputs {
            directory,
            repository,
            options,
            compaction_kind: effective_compaction_kind,
            index_available,
            current_head,
            reference_generation,
            residue_segments,
            checkpoints,
            stale_archives: &stale_archives,
        },
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
            effective_compaction_kind,
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
        closure_indexed_bytes,
        effective_compaction_kind,
        already_fully_compacted,
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
/// Verifies the exact head for planning, with the census riding the walks
/// in an order that makes its classifications exact by construction:
///
/// 1. the head's content root is walked first with live-identifier matching
///    on, *pruned by path* at `/jcr:system/jcr:versionStorage` — so every
///    live `jcr:uuid` is seen under live content before any version-storage
///    certificate exists, and a record shared between live content and a
///    frozen subtree can never be memo-skipped out of the live set;
/// 2. the version-storage subtree is then walked through the same verifier
///    with the pre-scan collector, covering exactly what the pruning left
///    out — including records the content walk already certified, whose
///    facts are then simply not re-attributed, which is correct: content a
///    history shares with the live tree is not content a purge releases;
/// 3. the super-root is walked with matching off, covering checkpoints and
///    the super-root structure — cheap, because the memo already holds
///    everything they share with the head.
///
/// The reported node total is the verifier's running count across all
/// three walks: exactly the distinct records the super-root reaches, the
/// same figure a single-walk verification reports.
pub(in crate::writer::maintenance) fn verify_head_for_planning(
    repository: &Repository,
    head: RecordIdentifier,
    census: &mut super::PlanningContentCensus,
    collect_internal_identifiers: bool,
    observer: &mut dyn ProgressObserver,
) -> Result<u64> {
    let view = repository.segment(head.segment)?;
    if view.structure.record_type(head.record_number) != Some(RecordType::Node) {
        return Err(Error::InvalidFormat {
            details: format!("current journal head {head} is not a node record"),
        });
    }
    let super_root = repository.node(head);
    let content_root = super_root
        .child_node("root")?
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("journal root {head} has no content \"root\" child node"),
        })?;
    let mut verifier = NodeTreeVerifier::new(repository);
    let version_storage = content_root
        .child_node("jcr:system")?
        .map(|system| system.child_node("jcr:versionStorage"))
        .transpose()?
        .flatten();
    crate::progress::observe(
        observer,
        &Step::new("verifying the current head", WorkUnit::Nodes),
        |observer| {
            census.match_live_identifiers = true;
            let outcome = verifier.verify_collecting_pruned_with_progress(
                content_root.record_identifier(),
                census,
                &["/jcr:system/jcr:versionStorage"],
                observer,
            );
            census.match_live_identifiers = false;
            outcome
        },
    )?;
    if let Some(version_storage) = &version_storage {
        crate::progress::observe(
            observer,
            &Step::new("scanning version storage", WorkUnit::Nodes),
            |observer| {
                let mut scan = super::VersionStoragePreScan::new(
                    &mut census.version_storage,
                    &mut census.external_binaries,
                    collect_internal_identifiers,
                );
                verifier.verify_collecting_with_progress(
                    version_storage.record_identifier(),
                    &mut scan,
                    observer,
                )
            },
        )?;
    }
    // Both walks are done: histories are registered and every live
    // identifier is recorded, in whichever order the store put them.
    census.version_storage.resolve_live_matches();
    crate::progress::observe(
        observer,
        &Step::new("verifying the checkpoints", WorkUnit::Nodes),
        |observer| verifier.verify_collecting_with_progress(head, census, observer),
    )?;
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
    Ok(verifier.verified_nodes())
}

pub(in crate::writer::maintenance) fn verify_exact_super_root_counting_nodes(
    repository: &Repository,
    head: RecordIdentifier,
    observer: &mut dyn ProgressObserver,
) -> Result<u64> {
    verify_exact_super_root_collecting(
        repository,
        head,
        &mut crate::tooling::DiscardedVerifiedContent,
        observer,
    )
}

/// Verifies exactly like [`verify_exact_super_root_counting_nodes`], handing
/// every distinct node the walk certifies to `content` — the one pass the
/// planning census rides, so plan-time facts about the content never cost a
/// second walk.
pub(in crate::writer::maintenance) fn verify_exact_super_root_collecting(
    repository: &Repository,
    head: RecordIdentifier,
    content: &mut dyn crate::tooling::VerifiedContentObserver,
    observer: &mut dyn ProgressObserver,
) -> Result<u64> {
    crate::progress::observe(
        observer,
        &Step::new("verifying the current head", WorkUnit::Nodes),
        |observer| {
            let mut verifier = NodeTreeVerifier::new(repository);
            verify_exact_super_root_collecting_with_verifier(
                repository,
                head,
                &mut verifier,
                content,
                observer,
            )?;
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
    verify_exact_super_root_collecting_with_verifier(
        repository,
        head,
        verifier,
        &mut crate::tooling::DiscardedVerifiedContent,
        observer,
    )
}

pub(in crate::writer::maintenance) fn verify_exact_super_root_collecting_with_verifier(
    provider: &dyn crate::content::provider::SegmentProvider,
    head: RecordIdentifier,
    verifier: &mut NodeTreeVerifier<'_>,
    content: &mut dyn crate::tooling::VerifiedContentObserver,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let view = provider.segment(head.segment)?;
    if view.structure.record_type(head.record_number) != Some(RecordType::Node) {
        return Err(Error::InvalidFormat {
            details: format!("current journal head {head} is not a node record"),
        });
    }
    verifier.verify_collecting_with_progress(head, content, observer)?;
    let super_root = crate::content::node::NodeState::new(provider, head);
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
