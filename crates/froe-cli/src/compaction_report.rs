//! What `froe compact` prints on the way in: the plan, one line per
//! action, and the totals and reports the operator confirms it from.
//! Standard output carries the plan — it is the command's data — while
//! warnings go to standard error.
//!
//! Every line is built to be read in its own context: a cleanup that is
//! not part of the run says why it is not, counts agree with their nouns,
//! and a zero is omitted rather than reported.

use froe::{CompactionAction, CompactionPlan};

use crate::compaction::SkipReason;
use crate::compaction_summary::PurgeFacts;
use crate::output::count_noun;
use crate::progress::format_count;

/// One line per planned action, in the order the plan lists them.
fn print_planned_action(action: &CompactionAction) {
    match action {
        CompactionAction::RepairArchiveIndex {
            file_name,
            retired_file_names,
            reason,
            bytes,
        } => {
            println!(
                "  repair the index of {} ({}; original retained as {}.bak)",
                crate::output::sanitize_terminal_text(file_name),
                crate::output::sanitize_terminal_text(reason),
                crate::output::sanitize_terminal_text(file_name),
            );
            if !retired_file_names.is_empty() {
                // These leave the archive namespace, so they are named
                // rather than counted: the confirmation covers the files
                // the plan printed.
                let names: Vec<_> = retired_file_names
                    .iter()
                    .map(|name| crate::output::sanitize_terminal_text(name))
                    .collect();
                println!(
                    "    merging and retiring {} to .bak names",
                    names.join(", ")
                );
            }
            println!(
                "    {} retained across the retired originals",
                froe::format_byte_size(*bytes)
            );
        }
        CompactionAction::RemoveReclaimableArchive {
            file_name,
            segments,
            bytes,
        } => println!(
            "  remove {file_name}: {}, {}",
            count_noun(*segments as u64, "orphan segment", "orphan segments"),
            froe::format_byte_size(*bytes)
        ),
        CompactionAction::RewriteArchive {
            file_name,
            replacement_name,
            segments,
            eligible_bytes,
        } => println!(
            "  rewrite {file_name} as {replacement_name}: omit {} ({} of entries)",
            count_noun(*segments as u64, "orphan segment", "orphan segments"),
            froe::format_byte_size(*eligible_bytes)
        ),
        CompactionAction::RemoveStaleArchive {
            file_name,
            reason,
            bytes,
        } => println!(
            "  remove stale archive {file_name} ({reason}; {})",
            froe::format_byte_size(*bytes)
        ),
        CompactionAction::CopyHeadIntoFreshGeneration {
            head_nodes,
            target_generation,
            kind,
        } => {
            println!(
                "  {} compaction: copy the head into generation ({},{},compacted)",
                match kind {
                    froe::CompactionKind::Full => "full",
                    froe::CompactionKind::Tail => "tail",
                },
                target_generation.generation,
                target_generation.full_generation
            );
            println!(
                "    the head reaches {} nodes; the copy rewrites those it still retains",
                format_count(*head_nodes)
            );
        }
        other => print_store_action(other),
    }
}

/// Actions on the journal, checkpoints, manifest, and the files an
/// interrupted run left behind.
fn print_store_action(action: &CompactionAction) {
    match action {
        CompactionAction::PruneJournal {
            lines,
            parser_ignored,
            missing_segments,
            unreadable_revisions,
            beyond_retention,
        } => println!(
            "  prune {} ({parser_ignored} parser-ignored, {missing_segments} missing-segment, {unreadable_revisions} unreadable historical, {beyond_retention} beyond retention)",
            count_noun(*lines as u64, "journal line", "journal lines"),
        ),
        CompactionAction::UpgradeManifest => {
            println!("  atomically upgrade manifest to store.version=2");
        }
        CompactionAction::RemoveCheckpoints {
            names,
            expired,
            unreferenced,
        } => print_checkpoint_removal_action(names, *expired, *unreferenced),
        CompactionAction::RemoveTemporary { file_name, bytes } => {
            println!(
                "  remove redundant temporary {file_name} ({})",
                froe::format_byte_size(*bytes)
            );
        }
        CompactionAction::PurgeOrphanedVersionHistories {
            histories,
            nodes,
            retained_checkpoints,
        } => print_purge_action(*histories, *nodes, *retained_checkpoints),
        CompactionAction::RetireJournalHistory { revisions } => {
            if *revisions == 1 {
                println!(
                    "  replace the journal's single line with the one naming the compacted head"
                );
            } else {
                println!(
                    "  retire all {} journal lines, keeping only the compacted head",
                    format_count(*revisions as u64)
                );
            }
            println!("    journal.log is copied to a numbered .bak first");
            println!("    the removed history is not recoverable from the store afterwards");
        }
        CompactionAction::RetireInterruptedCompactionResidue { segments } => {
            println!(
                "  retire {} of interrupted-compaction residue",
                count_noun(*segments as u64, "segment", "segments"),
            );
        }
        CompactionAction::RemoveRecoveryBackup { file_name, bytes } => {
            println!(
                "  remove old recovery backup {file_name} ({})",
                froe::format_byte_size(*bytes)
            );
        }
        _ => println!("  apply an action added by this froe version"),
    }
}

/// The checkpoint-removal plan lines: the count with only the selection
/// categories that apply, then every exact name, because confirmation is
/// scoped to what the plan printed.
fn print_checkpoint_removal_action(names: &[String], expired: usize, unreferenced: usize) {
    let mut categories = Vec::new();
    if expired != 0 {
        categories.push(format!("{expired} expired"));
    }
    if unreferenced != 0 {
        categories.push(format!("{unreferenced} unreferenced"));
    }
    let categories = if categories.is_empty() {
        String::new()
    } else {
        format!(" ({})", categories.join(", "))
    };
    println!(
        "  omit {} from the copy{categories}:",
        count_noun(names.len() as u64, "checkpoint", "checkpoints"),
    );
    for name in names {
        println!(
            "    checkpoint {}",
            crate::output::quote_terminal_text(name)
        );
    }
}

/// The purge plan lines: the counts, the irreversibility, and — when
/// retained checkpoints keep their own snapshots — the forewarning that
/// the copy's node count can exceed the head's, so the progress figures
/// that follow have already been explained.
fn print_purge_action(histories: u64, nodes: u64, retained_checkpoints: u64) {
    let (pronoun, existence) = if histories == 1 {
        ("it", "its versionable no longer exists")
    } else {
        ("them", "their versionables no longer exist")
    };
    println!(
        "  purge {} ({}) by omitting {pronoun} from the copy",
        count_noun(
            histories,
            "orphaned version history",
            "orphaned version histories"
        ),
        count_noun(nodes, "node", "nodes"),
    );
    println!("    {existence}; removal is permanent");
    println!(
        "    a versionable recreated with its old identifier will no longer re-attach a purged history"
    );
    if retained_checkpoints != 0 {
        println!(
            "    {} of these histories; that storage returns when the checkpoints expire",
            count_noun(
                retained_checkpoints,
                "retained checkpoint keeps its own snapshot",
                "retained checkpoints keep their own snapshots"
            ),
        );
        println!(
            "    until then the copy writes the head's rewritten version storage beside those snapshots, so it can visit more nodes than the head reaches today"
        );
    }
}

/// Where the content's external binaries live, so blob-store expectations
/// land on blob-store garbage collection rather than on this run. Silent
/// when the store references none: a line about an absent blob store would
/// only raise the question it exists to answer.
fn print_external_binary_footprint(footprint: froe::ExternalBinaryFootprint) {
    if footprint.distinct_references == 0 {
        return;
    }
    let count = format_count(footprint.distinct_references);
    let size = match (footprint.measured_bytes, footprint.unmeasured_references) {
        (0, _) => String::new(),
        (bytes, 0) => format!(" (about {})", froe::format_byte_size(bytes)),
        (bytes, unmeasured) => format!(
            " (about {}; length unknown for {})",
            froe::format_byte_size(bytes),
            format_count(unmeasured)
        ),
    };
    println!(
        "content references {count} external binaries{size} in the blob store; compaction never affects those bytes, which return only through blob-store garbage collection after content deletion"
    );
}

/// The disposition line the orphan report ends with: what this very run
/// does about the histories, or exactly why it does nothing.
fn orphan_report_disposition(
    report_histories: u64,
    selected: Option<&PurgeFacts>,
    purge_skipped: Option<SkipReason>,
) -> String {
    match (selected, purge_skipped) {
        (Some(purge), _) => {
            if purge.histories == report_histories {
                "  this run purges all of them, as listed in the plan above".to_owned()
            } else {
                format!(
                    "  this run purges {} of them, as listed in the plan above; the warnings say why the rest are kept",
                    format_count(purge.histories)
                )
            }
        }
        (None, None) => {
            "  none can be purged this run; the warnings above say why each is kept".to_owned()
        }
        (None, Some(SkipReason::Flag)) => {
            "  kept, as --skip-purging-orphaned-version-histories requests".to_owned()
        }
        (None, Some(SkipReason::Declined)) => {
            "  kept: the purge was declined at the prompt; a later run can still purge them"
                .to_owned()
        }
        (None, Some(SkipReason::Unconfirmed)) => {
            "  kept: the purge had no confirmation; rerun with --yes (or answer the prompt) to purge them"
                .to_owned()
        }
        (None, Some(SkipReason::TailCompaction)) => {
            "  kept: a tail compaction retains the generation a purge must reclaim; a full compaction purges them"
                .to_owned()
        }
        (None, Some(SkipReason::RepairRunsFirst)) => {
            // Not a reason the purge resolution ever produces; stated
            // plainly rather than debug-formatted if it ever is.
            "  kept this run".to_owned()
        }
    }
}

/// The always-on orphaned-version-history report: what the semantically
/// dead histories hold, what removing them releases, and what this run
/// does about them.
fn print_orphaned_version_history_report(plan: &CompactionPlan, purge_skipped: Option<SkipReason>) {
    let report = plan.orphaned_version_histories();
    if report.orphaned_histories == 0 && report.malformed_identifiers == 0 {
        return;
    }
    println!(
        "orphaned version histories: {} (their versionables no longer exist)",
        format_count(report.orphaned_histories)
    );
    if report.orphaned_histories != 0 {
        println!(
            "  holding {} nodes, {} of inline binary content, and {} external binary references",
            format_count(report.orphaned_nodes),
            froe::format_byte_size(report.inline_binary_bytes),
            format_count(report.external_references),
        );
        // A ceiling, not a promise: a record shared between a purged
        // history and one the store keeps retains its blocks (Oak's writer
        // dedups identical frozen subtrees into shared records), and a
        // retained checkpoint's snapshot can pin blocks until it expires.
        let checkpoint_caveat = if report.retained_checkpoints == 0 {
            String::new()
        } else {
            format!(
                "; {} retained checkpoints may pin some until they expire",
                format_count(report.retained_checkpoints)
            )
        };
        // "Realized by the copy" is only true while no retained checkpoint
        // keeps its own snapshot of the histories: a snapshot keeps the
        // purged records alive, so the node-record saving waits for the
        // checkpoints too.
        let node_record_realization = if report.retained_checkpoints == 0 {
            "realized by the copy"
        } else {
            "mostly deferred until the retained checkpoints expire, whose snapshots keep the purged records alive"
        };
        println!(
            "  a purge releases up to {} bulk segments ({}{checkpoint_caveat}) and about {} of node records ({node_record_realization})",
            format_count(report.released_bulk_segments),
            froe::format_byte_size(report.released_bulk_bytes),
            froe::format_byte_size(report.node_record_bytes_estimate),
        );
        let selected = plan.actions().iter().find_map(|action| match action {
            CompactionAction::PurgeOrphanedVersionHistories {
                histories,
                nodes,
                retained_checkpoints,
            } => Some(PurgeFacts {
                histories: *histories,
                nodes: *nodes,
                retained_checkpoints: *retained_checkpoints,
            }),
            _ => None,
        });
        println!(
            "{}",
            orphan_report_disposition(report.orphaned_histories, selected.as_ref(), purge_skipped)
        );
    }
    if report.malformed_identifiers != 0 {
        println!(
            "  {} histories carry versionable identifiers that do not parse and were not classified",
            format_count(report.malformed_identifiers)
        );
    }
}

/// The plan's totals: what it would reclaim, what it deliberately keeps,
/// and what the rewrites cost while they run.
fn print_plan_totals(plan: &CompactionPlan, purge_skipped: Option<SkipReason>) {
    let repair_bytes: u64 = plan
        .actions()
        .iter()
        .filter_map(|action| match action {
            CompactionAction::RepairArchiveIndex { bytes, .. } => Some(*bytes),
            _ => None,
        })
        .sum();
    if repair_bytes != 0 {
        let repair_size = froe::format_byte_size(repair_bytes);
        println!(
            "index rebuilds need {repair_size} of transient space and leave {repair_size} of .bak files: the repository grows until those are retired"
        );
    }
    // A run that copies has two sides, and an estimate that states only the
    // reclaimed one over-promises by the size of the generation it writes:
    // a swap that removes one generation and writes an equal one nets
    // nothing. The net line says which way the store actually moves.
    if let Some(copy_output) = plan.predicted_copy_output_bytes() {
        let reclaimable = plan.estimated_reclaimable_bytes();
        let target_generation = plan.actions().iter().find_map(|action| match action {
            CompactionAction::CopyHeadIntoFreshGeneration {
                target_generation, ..
            } => Some(*target_generation),
            _ => None,
        });
        match target_generation {
            Some(generation) => println!(
                "the copy writes about {} into generation ({},{},compacted)",
                froe::format_byte_size(copy_output),
                generation.generation,
                generation.full_generation,
            ),
            None => println!(
                "the copy writes about {} into the fresh generation",
                froe::format_byte_size(copy_output)
            ),
        }
        println!(
            "the sweep reclaims about {} of archives and entries",
            froe::format_byte_size(reclaimable)
        );
        if reclaimable >= copy_output {
            println!(
                "estimated net change: about {} reclaimed",
                froe::format_byte_size(reclaimable - copy_output)
            );
        } else {
            println!(
                "estimated net change: about {} of growth (the fresh generation exceeds what the sweep removes)",
                froe::format_byte_size(copy_output - reclaimable)
            );
        }
    } else {
        println!(
            "estimated reclaimable: {}",
            froe::format_byte_size(plan.estimated_reclaimable_bytes())
        );
    }
    print_external_binary_footprint(plan.external_binary_footprint());
    print_orphaned_version_history_report(plan, purge_skipped);
    // A zero estimate has two very different meanings — "no garbage" and
    // "garbage this run declined to move" — and the run used to print the
    // same line for both. These two say which one it is.
    if plan.retained_reclaimable_segments() != 0 {
        println!(
            "identified but retained: {} segments / {} of reclaimable garbage, left in archives this run cannot rewrite (see the warnings above)",
            format_count(plan.retained_reclaimable_segments() as u64),
            froe::format_byte_size(plan.retained_reclaimable_bytes()),
        );
    }
    if plan.estimated_archive_rewrite_source_bytes() != 0 {
        println!(
            "archive rewrite working-space proxy: {} of source archives (additional headroom may be required)",
            froe::format_byte_size(plan.estimated_archive_rewrite_source_bytes())
        );
    }
}

pub(crate) fn print_cleanup_plan(plan: &CompactionPlan, purge_skipped: Option<SkipReason>) {
    println!(
        "compaction plan for {} (verified head {}):",
        crate::output::sanitize_terminal_path(plan.directory()),
        plan.current_head()
    );
    // The convergence verdict comes before the actions, because it explains
    // an absence: an operator who selected a compaction and sees no copy
    // line must learn it was proven unnecessary, not lost.
    if plan.already_fully_compacted() {
        if plan.effective_compaction_kind().is_some() {
            println!(
                "  the head is already fully compacted; --always-copy forces the copy regardless"
            );
        } else {
            println!(
                "  the head is already fully compacted; the selected copy would replace this generation with an identical one and was dropped from the plan"
            );
        }
    }
    if plan.actions().is_empty() {
        println!("  no mutations");
    }
    for action in plan.actions() {
        print_planned_action(action);
    }
    for removal in plan.journal_line_removals() {
        let preview = crate::output::escape_terminal_bytes(removal.preview_bytes());
        let truncated = if removal.preview_truncated() {
            "…"
        } else {
            ""
        };
        if let Some(identifier) = removal.record_identifier() {
            println!(
                "    journal line {}: {} ({identifier}); b\"{preview}\"{truncated}",
                removal.line_number(),
                removal.reason(),
            );
        } else {
            println!(
                "    journal line {}: {}; b\"{preview}\"{truncated}",
                removal.line_number(),
                removal.reason(),
            );
        }
    }
    for warning in plan.warnings() {
        eprintln!(
            "froe: warning: {}",
            crate::output::sanitize_terminal_text(warning)
        );
    }
    // Every other estimate here describes space the run gives back. Repair is
    // the one action that takes space and keeps it, so it is stated
    // separately rather than netted against a reclaim figure it would
    // silently contradict.
    print_plan_totals(plan, purge_skipped);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report's last line says what this run does, never advising a
    /// flag for something that is already selected.
    #[test]
    fn the_orphan_disposition_states_this_runs_answer() {
        let selected = PurgeFacts {
            histories: 5,
            nodes: 10,
            retained_checkpoints: 0,
        };
        assert_eq!(
            orphan_report_disposition(5, Some(&selected), None),
            "  this run purges all of them, as listed in the plan above"
        );
        let partial = orphan_report_disposition(8, Some(&selected), None);
        assert!(partial.contains("purges 5 of them"), "{partial}");
        assert!(
            orphan_report_disposition(3, None, Some(SkipReason::Flag))
                .contains("--skip-purging-orphaned-version-histories")
        );
        assert!(
            orphan_report_disposition(3, None, Some(SkipReason::Declined))
                .contains("declined at the prompt")
        );
        assert!(
            orphan_report_disposition(3, None, Some(SkipReason::TailCompaction))
                .contains("a full compaction purges them")
        );
        assert!(orphan_report_disposition(3, None, None).contains("none can be purged this run"));
    }
}
