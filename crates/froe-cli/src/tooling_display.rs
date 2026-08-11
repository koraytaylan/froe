//! Rendering for the read-only diagnostic commands: check, diff,
//! history, search, segment dumps, and archive attribution.

use std::fmt::Write as _;
use std::path::Path;

use froe::content::node::PropertyValues;
use froe::segment::identifier::SegmentIdentifier;
use froe::store::Repository;
use froe::tooling::archive_debug::{
    ArchiveDebugState, ArchiveGraphReferences, ArchivePathReference, ArchivePropertyDisplay,
    debug_archive,
};
use froe::tooling::diff::{NodeDifference, PropertyChange};
use froe::tooling::search::SearchQuery;
use froe::tooling::{check_consistency, diff_revisions, dump_segment, node_history, search_nodes};

use froe_export::json::append_json_values;

use crate::output::{format_timestamp, sanitize_terminal_text};

/// `froe segment --hex`: Oak-compatible `SegmentDump` output.
pub(crate) fn print_segment_dump(
    repository: &Repository,
    identifier: SegmentIdentifier,
) -> froe::Result<()> {
    print!("{}", dump_segment(repository, identifier)?);
    Ok(())
}

/// `froe debug PATH file.tar...`: current-head record attribution and the
/// archive graph. The narrower spelling is deliberate: unlike oak-run's
/// overloaded `debug`, segment dumping remains under `froe segment --hex`.
pub(crate) fn print_archive_debug(
    repository: &Repository,
    archive_file_names: &[String],
) -> froe::Result<()> {
    for archive_file_name in archive_file_names {
        let report = debug_archive(repository, archive_file_name)?;
        match report.state {
            ArchiveDebugState::Missing => {
                println!("file doesn't exist, skipping {}", report.archive_file_name);
                continue;
            }
            ArchiveDebugState::Inactive => {
                println!(
                    "archive exists but is not active, skipping {}",
                    report.archive_file_name
                );
                continue;
            }
            ArchiveDebugState::Active => {}
        }

        println!(
            "Debug file {}({})",
            sanitize_terminal_text(
                &repository
                    .directory()
                    .join(&report.archive_file_name)
                    .to_string_lossy()
            ),
            report.file_size.unwrap_or(0)
        );
        println!(
            "SegmentNodeState references to {}",
            report.archive_file_name
        );
        for reference in &report.references {
            print_archive_reference(reference);
        }
        println!();
        println!("Tar graph:");
        match &report.graph {
            Some(graph) => {
                for row in &graph.rows {
                    match &row.references {
                        ArchiveGraphReferences::Available(targets) => {
                            let targets = targets
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!("{}=[{targets}]", row.segment_identifier);
                        }
                        ArchiveGraphReferences::Unavailable { details } => println!(
                            "{}=unavailable ({})",
                            row.segment_identifier,
                            sanitize_terminal_text(details)
                        ),
                    }
                }
            }
            None => println!("unavailable (archive is not active)"),
        }
    }
    Ok(())
}

fn print_archive_reference(reference: &ArchivePathReference) {
    match reference {
        ArchivePathReference::Node {
            path,
            record_identifier,
        } => println!(
            "  {} [SegmentNodeState@{record_identifier}]",
            sanitize_terminal_text(path)
        ),
        ArchivePathReference::Template {
            path,
            record_identifier,
        } => println!(
            "  {}[Template@{record_identifier}]",
            sanitize_terminal_text(path)
        ),
        ArchivePathReference::Property {
            path,
            name,
            property_type,
            is_multiple,
            record_identifier,
            display,
            ..
        } => {
            let display = match display {
                ArchivePropertyDisplay::String(value) => java_string_display(value, 60),
                ArchivePropertyDisplay::EmptyStrings => String::new(),
                ArchivePropertyDisplay::Other(value) => sanitize_terminal_text(value),
            };
            println!(
                "  {}{} = {display} [SegmentPropertyState<{}>@{record_identifier}]",
                sanitize_terminal_text(path),
                sanitize_terminal_text(name),
                oak_property_type_name(*property_type, *is_multiple),
            );
        }
    }
}

fn oak_property_type_name(property_type: froe::PropertyType, is_multiple: bool) -> String {
    let singular = property_type.jcr_name().to_ascii_uppercase();
    if !is_multiple {
        singular
    } else if property_type == froe::PropertyType::Binary {
        "BINARIES".to_owned()
    } else {
        format!("{singular}S")
    }
}

/// Oak's default `max.char.display` is 60 Java `char`s. Commons Lang's
/// `escapeJava` then escapes controls and non-ASCII UTF-16 code units.
fn java_string_display(value: &str, maximum_characters: usize) -> String {
    let character_count = value.encode_utf16().count();
    let mut display = String::from("\"");
    for unit in value.encode_utf16().take(maximum_characters) {
        match unit {
            0x08 => display.push_str("\\b"),
            0x09 => display.push_str("\\t"),
            0x0a => display.push_str("\\n"),
            0x0c => display.push_str("\\f"),
            0x0d => display.push_str("\\r"),
            0x22 => display.push_str("\\\""),
            0x5c => display.push_str("\\\\"),
            0x20..=0x7e => display.push(char::from_u32(u32::from(unit)).expect("ASCII unit")),
            _ => write!(display, "\\u{unit:04X}").expect("writing to a String cannot fail"),
        }
    }
    if character_count > maximum_characters {
        write!(display, "... ({character_count} chars)").expect("writing to a String cannot fail");
    }
    display.push('"');
    display
}

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
        Some(revision) => println!(
            "  latest good revision for all checked paths is {}",
            sanitize_terminal_text(revision)
        ),
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
            "{indent}latest good revision for path {} is {} from {timestamp}",
            sanitize_terminal_text(&verdict.path),
            sanitize_terminal_text(revision)
        );
    } else {
        let reason = verdict.newest_failure.as_deref().unwrap_or("never checked");
        println!(
            "{indent}latest good revision for path {} is none ({})",
            sanitize_terminal_text(&verdict.path),
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
        PropertyChange::Changed { before, after } => {
            println!(
                "  ^ {}/{}",
                sanitize_terminal_text(path),
                sanitize_terminal_text(&before.name)
            );
            if before.property_type == after.property_type {
                println!("      - {}", render(&before.values));
                println!("      + {}", render(&after.values));
            } else {
                // A type-only change would otherwise print two
                // identical lines.
                println!(
                    "      - {} {}",
                    before.property_type.jcr_name(),
                    render(&before.values)
                );
                println!(
                    "      + {} {}",
                    after.property_type.jcr_name(),
                    render(&after.values)
                );
            }
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
            sanitize_terminal_text(&entry.revision),
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

#[cfg(test)]
mod archive_rendering_tests {
    use super::{java_string_display, oak_property_type_name};

    #[test]
    fn string_display_uses_java_escaping_and_utf16_truncation() {
        assert_eq!(
            java_string_display("quote \" slash \\ line\n caf\u{e9}", 60),
            "\"quote \\\" slash \\\\ line\\n caf\\u00E9\""
        );
        assert_eq!(
            java_string_display(&format!("{}\u{1f600}", "x".repeat(59)), 60),
            format!("\"{}\\uD83D... (61 chars)\"", "x".repeat(59)),
            "the sixty-character boundary follows Java UTF-16 units"
        );
    }

    #[test]
    fn property_type_names_use_oak_plural_spellings() {
        assert_eq!(
            oak_property_type_name(froe::PropertyType::String, false),
            "STRING"
        );
        assert_eq!(
            oak_property_type_name(froe::PropertyType::String, true),
            "STRINGS"
        );
        assert_eq!(
            oak_property_type_name(froe::PropertyType::Binary, true),
            "BINARIES"
        );
    }
}
