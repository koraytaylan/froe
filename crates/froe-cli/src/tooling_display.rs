//! Rendering for the read-only diagnostic commands: check, diff,
//! history, search, segment dumps, and archive attribution.

use std::io;
use std::path::Path;

use froe::content::node::PropertyValues;
use froe::segment::identifier::SegmentIdentifier;
use froe::store::Repository;
use froe::tooling::archive_debug::{
    ArchiveDebugState, ArchiveGraphReferences, ArchivePathReference, debug_archive,
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
            _ => {
                println!(
                    "archive has an unsupported state, skipping {}",
                    report.archive_file_name
                );
                continue;
            }
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
                            let stdout = io::stdout();
                            let mut output = stdout.lock();
                            write_available_graph_row(
                                &mut output,
                                row.segment_identifier,
                                targets,
                            )?;
                        }
                        ArchiveGraphReferences::Unavailable { details } => println!(
                            "{}=unavailable ({})",
                            row.segment_identifier,
                            sanitize_terminal_text(details)
                        ),
                        _ => println!("{}=unavailable", row.segment_identifier),
                    }
                }
            }
            None => println!("unavailable (archive is not active)"),
        }
    }
    Ok(())
}

fn write_available_graph_row(
    output: &mut dyn io::Write,
    source: SegmentIdentifier,
    targets: &[SegmentIdentifier],
) -> io::Result<()> {
    write!(output, "{source}=[")?;
    for (position, target) in targets.iter().enumerate() {
        if position > 0 {
            write!(output, ", ")?;
        }
        write!(output, "{target}")?;
    }
    writeln!(output, "]")
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
            let display = sanitize_terminal_text(&display.oak_rendered_value());
            println!(
                "  {}{} = {display} [SegmentPropertyState<{}>@{record_identifier}]",
                sanitize_terminal_text(path),
                sanitize_terminal_text(name),
                oak_property_type_name(*property_type, *is_multiple),
            );
        }
        _ => println!("  unsupported archive reference"),
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
    use super::{oak_property_type_name, write_available_graph_row};
    use froe::segment::identifier::SegmentIdentifier;
    use froe::tooling::ArchivePropertyDisplay;

    #[test]
    fn string_display_uses_java_escaping_and_utf16_truncation() {
        let escaped_units: Vec<u16> = "quote \" slash \\ line\n caf\u{e9}"
            .encode_utf16()
            .collect();
        assert_eq!(
            ArchivePropertyDisplay::String {
                preview_utf16: escaped_units.clone(),
                utf16_length: escaped_units.len() as u64,
            }
            .oak_rendered_value(),
            "\"quote \\\" slash \\\\ line\\n caf\\u00E9\""
        );
        assert_eq!(
            ArchivePropertyDisplay::String {
                preview_utf16: format!("{}\u{1f600}", "x".repeat(59))
                    .encode_utf16()
                    .take(60)
                    .collect::<Vec<_>>(),
                utf16_length: 61,
            }
            .oak_rendered_value(),
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

    #[test]
    fn high_degree_graph_rows_stream_without_collecting_target_strings() {
        struct CountingWriter(usize);
        impl std::io::Write for CountingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let identifier = SegmentIdentifier::new(1, 0xa000_0000_0000_0001);
        let targets = vec![identifier; 10_000];
        let mut output = CountingWriter(0);
        write_available_graph_row(&mut output, identifier, &targets).expect("render row");
        assert_eq!(
            output.0,
            identifier.to_string().len()
                + "=[".len()
                + targets.len() * identifier.to_string().len()
                + (targets.len() - 1) * ", ".len()
                + "]\n".len()
        );
    }
}
