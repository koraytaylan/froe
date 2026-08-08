//! Rendering for the read-only diagnostic commands: check, diff,
//! history, and search.

use std::path::Path;

use froe::content::node::PropertyValues;
use froe::tooling::diff::{NodeDifference, PropertyChange};
use froe::tooling::search::SearchQuery;
use froe::tooling::{check_consistency, diff_revisions, node_history, search_nodes};

use crate::output::{append_json_values, format_timestamp};

/// `froe check`: report the newest consistent revision.
pub(crate) fn print_check(
    repository: &Path,
    paths: &[String],
    check_binaries: bool,
    revision_limit: usize,
) -> froe::Result<bool> {
    let report = check_consistency(repository, paths, check_binaries, revision_limit)?;
    for revision in &report.revisions {
        println!("revision {}", revision.revision);
        for path in &revision.consistent_paths {
            println!("  consistent    {path}");
        }
        for (path, reason) in &revision.inconsistent_paths {
            println!("  inconsistent  {path}: {reason}");
        }
    }
    if let Some(revision) = &report.good_revision {
        println!("newest consistent revision: {revision}");
        Ok(true)
    } else {
        println!("no fully consistent revision found");
        Ok(false)
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
            NodeDifference::NodeAdded { path } => println!("+ {path}"),
            NodeDifference::NodeRemoved { path } => println!("- {path}"),
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
                "  + {path}/{} = {}",
                property.name,
                render(&property.values)
            );
        }
        PropertyChange::Removed(property) => {
            println!(
                "  - {path}/{} = {}",
                property.name,
                render(&property.values)
            );
        }
        PropertyChange::Changed {
            name,
            before,
            after,
        } => {
            println!("  ^ {path}/{name}");
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
        eprintln!("froe: search-nodes needs at least one predicate");
        return Ok(());
    }
    let matches = search_nodes(repository, &query, limit)?;
    for node_match in &matches {
        println!(
            "{}  stable {}",
            node_match.record, node_match.stable_identifier
        );
    }
    println!("{} matching nodes", matches.len());
    Ok(())
}
