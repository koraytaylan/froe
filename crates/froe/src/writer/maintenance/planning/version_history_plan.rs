//! Assembling the plan's version-history parts: the always-on orphan
//! report, and the purge selection when one is requested — the census's
//! facts turned into the figures and actions the plan carries.

use super::{
    PlanningContentCensus, PurgeSelection, candidate_internal_identifiers,
    demoted_by_inbound_references, select_purge,
};
use crate::content::node::NodeState;
use crate::error::{Error, Result};
use crate::progress::ProgressObserver;
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::writer::maintenance::options::CompactionOptions;
use std::collections::HashSet;
use std::time::SystemTime;

/// Selects the version-history purge when one is requested: the orphans,
/// minus configurations, minus anything younger than the age bound, minus
/// the advisory reference demotions — the last computed by its own pass,
/// which only runs when there are candidates to check.
pub(super) fn select_version_history_purge(
    repository: &Repository,
    options: &CompactionOptions,
    current_head: RecordIdentifier,
    census: &PlanningContentCensus,
    now: SystemTime,
    warnings: &mut Vec<String>,
    observer: &mut dyn ProgressObserver,
) -> Result<Option<PurgeSelection>> {
    if !options.purges_orphaned_version_histories() {
        return Ok(None);
    }
    let now_epoch_seconds = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let preliminary = select_purge(
        &census.version_storage,
        options.purged_history_minimum_age,
        now_epoch_seconds,
        &HashSet::new(),
    );
    let demoted = if preliminary.histories == 0 {
        HashSet::new()
    } else {
        let content_root = repository
            .node(current_head)
            .child_node("root")?
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("journal root {current_head} has no content \"root\" child node"),
            })?;
        let version_storage_record = content_root
            .child_node("jcr:system")?
            .map(|system| system.child_node("jcr:versionStorage"))
            .transpose()?
            .flatten()
            .map(|storage| storage.record_identifier());
        let candidates = candidate_internal_identifiers(
            &census.version_storage,
            &preliminary.selected_identifiers,
        );
        demoted_by_inbound_references(
            repository,
            content_root.record_identifier(),
            version_storage_record,
            &candidates,
            observer,
        )?
    };
    let selection = select_purge(
        &census.version_storage,
        options.purged_history_minimum_age,
        now_epoch_seconds,
        &demoted,
    );
    if selection.kept_configurations != 0 {
        warnings.push(format!(
            "{} orphaned version histories kept: they freeze nt:configuration versionables, which this purge does not touch",
            selection.kept_configurations
        ));
    }
    if selection.kept_by_age != 0 {
        warnings.push(format!(
            "{} orphaned version histories kept by the age bound (younger than the minimum, or without a parsable version creation date)",
            selection.kept_by_age
        ));
    }
    if selection.kept_by_references != 0 {
        warnings.push(format!(
            "{} orphaned version histories kept: REFERENCE or WEAKREFERENCE values outside version storage still name records inside them",
            selection.kept_by_references
        ));
    }
    if census.version_storage.malformed_identifiers != 0 {
        warnings.push(format!(
            "{} version histories carry versionable identifiers that do not parse and were not classified",
            census.version_storage.malformed_identifiers
        ));
    }
    Ok(Some(selection))
}

/// The plan's two version-history parts: the always-on report, and the
/// purge when a non-empty one is selected.
pub(super) fn version_history_plan_parts(
    repository: &Repository,
    current_head: RecordIdentifier,
    census: &PlanningContentCensus,
    head_nodes: u64,
    closure_indexed_bytes: Option<(u64, u64)>,
    retired_checkpoints: u64,
    selection: Option<PurgeSelection>,
) -> Result<(
    crate::writer::maintenance::plan::OrphanedVersionHistoryReport,
    Option<crate::writer::maintenance::plan::VersionHistoryPurge>,
)> {
    let mut report = orphaned_version_history_report(
        repository,
        census,
        selection.as_ref(),
        head_nodes,
        closure_indexed_bytes,
    );
    report.retained_checkpoints =
        crate::progress::count(repository.checkpoints()?.len()).saturating_sub(retired_checkpoints);
    let Some(selection) = selection.filter(|selection| selection.histories != 0) else {
        return Ok((report, None));
    };
    // The ancestors whose rewritten form depends on the scope: the chain
    // from the content root down to version storage, plus every partially
    // emptied intermediate the selection identified.
    let mut context_dependent_records = selection.context_dependent_intermediates;
    let mut chain = repository.node(current_head).child_node("root")?;
    context_dependent_records.extend(chain.as_ref().map(NodeState::record_identifier));
    for name in ["jcr:system", "jcr:versionStorage"] {
        chain = match &chain {
            Some(node) => node.child_node(name)?,
            None => None,
        };
        context_dependent_records.extend(chain.as_ref().map(NodeState::record_identifier));
    }
    Ok((
        report,
        Some(crate::writer::maintenance::plan::VersionHistoryPurge {
            omitted_records: selection.omitted_records,
            context_dependent_records,
            histories: selection.histories,
            nodes: selection.nodes,
            retained_checkpoints: report.retained_checkpoints,
        }),
    ))
}

/// Assembles the always-on orphan report from what the walks established.
pub(super) fn orphaned_version_history_report(
    repository: &Repository,
    census: &PlanningContentCensus,
    selection: Option<&PurgeSelection>,
    head_nodes: u64,
    closure_indexed_bytes: Option<(u64, u64)>,
) -> crate::writer::maintenance::plan::OrphanedVersionHistoryReport {
    let mut report = crate::writer::maintenance::plan::OrphanedVersionHistoryReport {
        malformed_identifiers: census.version_storage.malformed_identifiers,
        ..Default::default()
    };
    let mut orphan_bulk: HashSet<SegmentIdentifier> = HashSet::new();
    let mut kept_orphan_bulk: HashSet<SegmentIdentifier> = HashSet::new();
    let mut released_nodes = 0_u64;
    for (identifier, facts) in census.version_storage.orphans() {
        report.orphaned_histories += 1;
        report.orphaned_nodes += facts.nodes;
        report.inline_binary_bytes = report
            .inline_binary_bytes
            .saturating_add(facts.inline_binary_bytes);
        report.external_references += facts.external_references;
        // Released bulk describes what the *selected* purge frees; without
        // a selection it describes a purge of every orphan. A block a kept
        // orphan still references is not released.
        let released_by_this_run = selection.is_none_or(|selection| {
            selection.histories == 0 || selection.selected_identifiers.contains(identifier)
        });
        if released_by_this_run {
            released_nodes += facts.nodes;
            orphan_bulk.extend(facts.bulk_segments.iter().copied());
        } else {
            kept_orphan_bulk.extend(facts.bulk_segments.iter().copied());
        }
    }
    // Bulk a purge releases: what only the purged histories reference. A
    // block shared with the head, a checkpoint's own content, a live
    // history, or a kept orphan stays, so this is a subtraction, not a
    // sum. Blocks a retained checkpoint's *shared* snapshot pins cannot be
    // seen here, which is why the printed figure carries a checkpoint
    // caveat whenever checkpoints are retained.
    for live in census
        .live_bulk
        .iter()
        .chain(census.version_storage.live_history_bulk.iter())
        .chain(kept_orphan_bulk.iter())
    {
        orphan_bulk.remove(live);
    }
    report.released_bulk_segments = crate::progress::count(orphan_bulk.len());
    let (_, bulk_bytes) =
        crate::writer::maintenance::reclamation::indexed_bytes_by_kind(repository, &orphan_bulk);
    report.released_bulk_bytes = bulk_bytes;
    // The released histories' node-record share, scaled from the head's
    // average bytes per node: an estimate the copy realizes. Scoped like
    // the bulk figure — a kept orphan's nodes are not a saving this run
    // delivers.
    if let (Some((data_bytes, _)), true) = (closure_indexed_bytes, head_nodes != 0) {
        report.node_record_bytes_estimate = data_bytes / head_nodes * released_nodes
            + (data_bytes % head_nodes) * released_nodes / head_nodes;
    }
    report
}
