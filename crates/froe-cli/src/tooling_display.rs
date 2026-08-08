//! Rendering for the read-only diagnostic commands: check, diff,
//! history, and search.

use std::path::Path;

use froe::content::node::PropertyValues;
use froe::tooling::diff::{NodeDifference, PropertyChange};
use froe::tooling::search::SearchQuery;
use froe::tooling::{check_consistency, diff_revisions, node_history, search_nodes};

use crate::output::{append_json_values, format_timestamp, sanitize_terminal_text};

/// `froe check`: each path's latest good revision, Oak-style. Succeeds
/// (exit 0) when any path found a good revision — Java's default,
/// fail-fast off.
pub(crate) fn print_check(
    repository: &Path,
    paths: &[String],
    check_binaries: bool,
    revision_limit: usize,
) -> froe::Result<bool> {
    let report = check_consistency(repository, paths, check_binaries, revision_limit)?;
    println!(
        "searched through {} revisions and {} checkpoints",
        report.checked_revisions,
        report.checkpoints.len()
    );
    println!("head");
    for verdict in &report.head_paths {
        print_path_verdict(verdict, "  ");
    }
    if !report.checkpoints.is_empty() {
        println!("checkpoints");
        for (checkpoint, verdicts) in &report.checkpoint_paths {
            println!("- {}", sanitize_terminal_text(checkpoint));
            for verdict in verdicts {
                print_path_verdict(verdict, "    ");
            }
        }
    }
    println!("overall");
    match &report.overall_revision {
        Some(revision) => println!("  latest good revision for all checked paths is {revision}"),
        None => println!("  latest good revision for all checked paths is none"),
    }
    if report.has_good_revision() {
        Ok(true)
    } else {
        println!("no good revision found");
        Ok(false)
    }
}

fn print_path_verdict(verdict: &froe::tooling::PathVerdict, indent: &str) {
    if let Some(revision) = &verdict.latest_good_revision {
        let timestamp = verdict
            .latest_good_timestamp_milliseconds
            .map_or_else(|| "unknown time".to_owned(), format_timestamp);
        println!(
            "{indent}latest good revision for path {} is {revision} from {timestamp}",
            verdict.path
        );
    } else {
        let reason = verdict.newest_failure.as_deref().unwrap_or("never checked");
        println!(
            "{indent}latest good revision for path {} is none ({})",
            verdict.path,
            sanitize_terminal_text(reason)
        );
    }
}

/// `froe difference`: print the changes between two revisions.
pub(crate) fn print_difference(
    repository: &Path,
    before: &str,
    after: &str,
    path: &str,
) -> froe::Result<()> {
    let differences = diff_revisions(repository, before, after, path)?;
    if differences.is_empty() {
        println!("no differences");
        return Ok(());
    }
    for difference in &differences {
        match difference {
            NodeDifference::NodeAdded { path } => {
                println!("+ {}", sanitize_terminal_text(path));
            }
            NodeDifference::NodeRemoved { path } => {
                println!("- {}", sanitize_terminal_text(path));
            }
            NodeDifference::PropertyChanged { path, change } => {
                print_property_change(path, change);
            }
        }
    }
    Ok(())
}

fn print_property_change(path: &str, change: &PropertyChange) {
    match change {
        PropertyChange::Added(property) => {
            println!(
                "  + {}/{} = {}",
                sanitize_terminal_text(path),
                sanitize_terminal_text(&property.name),
                render(&property.values)
            );
        }
        PropertyChange::Removed(property) => {
            println!(
                "  - {}/{} = {}",
                sanitize_terminal_text(path),
                sanitize_terminal_text(&property.name),
                render(&property.values)
            );
        }
        PropertyChange::Changed {
            name,
            before,
            after,
        } => {
            println!(
                "  ^ {}/{}",
                sanitize_terminal_text(path),
                sanitize_terminal_text(name)
            );
            println!("      - {}", render(before));
            println!("      + {}", render(after));
        }
    }
}

fn render(values: &PropertyValues) -> String {
    let mut buffer = String::new();
    append_json_values(&mut buffer, values);
    buffer
}

/// `froe history`: print a node's states across revisions.
pub(crate) fn print_history(repository: &Path, path: &str) -> froe::Result<()> {
    for entry in node_history(repository, path)? {
        let record = entry
            .record
            .map_or_else(|| "absent".to_owned(), |record| record.to_string());
        println!(
            "{}  {}  {record}",
            format_timestamp(entry.timestamp_milliseconds),
            entry.revision,
        );
    }
    Ok(())
}

/// `froe search-nodes`: print nodes matching the given predicates.
pub(crate) fn print_search(
    repository: &Path,
    has_properties: &[String],
    has_children: &[String],
    property_values: &[(String, String)],
    limit: usize,
) -> froe::Result<()> {
    let query = SearchQuery {
        has_properties: has_properties.to_vec(),
        has_children: has_children.to_vec(),
        property_values: property_values.to_vec(),
    };
    if query.is_empty() {
        // An error, not a quiet return: scripts must see a failure exit
        // code when the search never ran.
        return Err(froe::Error::InvalidFormat {
            details: "search-nodes needs at least one predicate".to_owned(),
        });
    }
    let outcome = search_nodes(repository, &query, limit)?;
    for node_match in &outcome.matches {
        println!(
            "{}  stable {}",
            node_match.record, node_match.stable_identifier
        );
    }
    println!("{} matching nodes", outcome.matches.len());
    if outcome.unreadable_nodes > 0 {
        eprintln!(
            "froe: {} record table entries could not be read and were skipped",
            outcome.unreadable_nodes
        );
    }
    Ok(())
}
