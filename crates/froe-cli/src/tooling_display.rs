//! Rendering for the read-only diagnostic commands: check, diff,
//! history, search, segment dumps, and archive attribution.

use std::fmt::Write as _;
use std::io;
use std::io::Write as _;
use std::path::Path;

use froe::content::node::PropertyValues;
use froe::segment::identifier::SegmentIdentifier;
use froe::store::Repository;
use froe::tooling::archive_debug::{
    ArchiveDebugState, ArchiveGraphReferences, ArchivePathReference, debug_archive,
};
use froe::tooling::diff::{NodeDifference, PropertyChange};
use froe::tooling::digest::{DigestSummary, compare_digests, digest_repository_excluding};
use froe::tooling::search::SearchQuery;
use froe::tooling::{
    BinaryCheck, check_consistency_with_progress, diff_revisions_with_progress, dump_segment,
    node_history_with_progress, search_nodes_visiting,
};

use froe_export::json::append_json_values;

use crate::output::{format_timestamp, sanitize_terminal_text};
use crate::progress::Reporter;

/// `froe segment --hex`: Oak-compatible `SegmentDump` output.
pub(crate) fn print_segment_dump(
    repository: &Repository,
    identifier: SegmentIdentifier,
) -> froe::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    write_segment_dump(&mut output, repository, identifier)
}

fn write_segment_dump(
    output: &mut dyn io::Write,
    repository: &Repository,
    identifier: SegmentIdentifier,
) -> froe::Result<()> {
    write_diagnostic_handling_observed_broken_pipe(output, |output| {
        let dump = dump_segment(repository, identifier)?;
        output.write_all(dump.as_bytes())?;
        Ok(())
    })
}

/// `froe debug PATH file.tar...`: current-head record attribution and the
/// archive graph. The narrower spelling is deliberate: unlike oak-run's
/// overloaded `debug`, segment dumping remains under `froe segment --hex`.
pub(crate) fn print_archive_debug(
    repository: &Repository,
    archive_file_names: &[String],
) -> froe::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    write_archive_debug(&mut output, repository, archive_file_names)
}

fn write_archive_debug(
    output: &mut dyn io::Write,
    repository: &Repository,
    archive_file_names: &[String],
) -> froe::Result<()> {
    write_diagnostic_handling_observed_broken_pipe(output, |output| {
        render_archive_debug(output, repository, archive_file_names)
    })
}

fn render_archive_debug(
    output: &mut dyn io::Write,
    repository: &Repository,
    archive_file_names: &[String],
) -> froe::Result<()> {
    for archive_file_name in archive_file_names {
        let report = debug_archive(repository, archive_file_name)?;
        match report.state {
            ArchiveDebugState::Missing => {
                writeln!(
                    output,
                    "file doesn't exist, skipping {}",
                    report.archive_file_name
                )?;
                continue;
            }
            ArchiveDebugState::Inactive => {
                writeln!(
                    output,
                    "archive exists but is not active, skipping {}",
                    report.archive_file_name
                )?;
                continue;
            }
            ArchiveDebugState::Active => {}
            _ => {
                writeln!(
                    output,
                    "archive has an unsupported state, skipping {}",
                    report.archive_file_name
                )?;
                continue;
            }
        }

        writeln!(
            output,
            "Debug file {}({})",
            sanitize_terminal_text(
                &repository
                    .directory()
                    .join(&report.archive_file_name)
                    .to_string_lossy()
            ),
            report.file_size.unwrap_or(0)
        )?;
        writeln!(
            output,
            "SegmentNodeState references to {}",
            report.archive_file_name
        )?;
        for reference in &report.references {
            write_archive_reference(output, reference)?;
        }
        writeln!(output)?;
        writeln!(output, "Tar graph:")?;
        match &report.graph {
            Some(graph) => {
                for row in &graph.rows {
                    match &row.references {
                        ArchiveGraphReferences::Available(targets) => {
                            write_available_graph_row(output, row.segment_identifier, targets)?;
                        }
                        ArchiveGraphReferences::Unavailable { details } => writeln!(
                            output,
                            "{}=unavailable ({})",
                            row.segment_identifier,
                            sanitize_terminal_text(details)
                        )?,
                        _ => writeln!(output, "{}=unavailable", row.segment_identifier)?,
                    }
                }
            }
            None => writeln!(output, "unavailable (archive is not active)")?,
        }
    }
    Ok(())
}

/// Converts a `BrokenPipe` returned to Rust into a quiet diagnostic exit.
///
/// Unix normally terminates the CLI with `SIGPIPE` before this fallback sees
/// an error because `main` restores the conventional default disposition.
/// Platforms that report the closed pipe to [`io::Write`] instead use this
/// path. Other output errors remain failures.
fn write_diagnostic_handling_observed_broken_pipe(
    output: &mut dyn io::Write,
    write_output: impl FnOnce(&mut dyn io::Write) -> froe::Result<()>,
) -> froe::Result<()> {
    match write_output(output) {
        Err(froe::Error::InputOutput(error)) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
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

fn write_archive_reference(
    output: &mut dyn io::Write,
    reference: &ArchivePathReference,
) -> io::Result<()> {
    match reference {
        ArchivePathReference::Node {
            path,
            record_identifier,
        } => writeln!(
            output,
            "  {} [SegmentNodeState@{record_identifier}]",
            sanitize_terminal_text(path)
        ),
        ArchivePathReference::Template {
            path,
            record_identifier,
        } => writeln!(
            output,
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
            writeln!(
                output,
                "  {}{} = {display} [SegmentPropertyState<{}>@{record_identifier}]",
                sanitize_terminal_text(path),
                sanitize_terminal_text(name),
                oak_property_type_name(*property_type, *is_multiple),
            )
        }
        _ => writeln!(output, "  unsupported archive reference"),
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
    binary_check: BinaryCheck,
    revision_limit: usize,
    reporter: &Reporter,
) -> froe::Result<bool> {
    let report = check_consistency_with_progress(
        repository,
        paths,
        binary_check,
        revision_limit,
        &mut reporter.clone(),
    )?;
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

/// `froe digest`: the canonical content rendering, optionally compared
/// against one taken earlier.
///
/// Returns whether the run was clean. Three separate things can make it
/// not clean, and the caller only needs the one answer: the store
/// disagreed with its baseline, a child or property was unreachable by
/// lookup, or an index lane still references a checkpoint that is gone.
///
/// The digest is data and standard output carries only data, so the
/// summary always goes to standard error — including when `--output`
/// redirects the digest to a file. Splitting on the destination would make
/// `froe digest store | diff - before.digest` work and
/// `froe digest store --output after.digest` print to a different stream,
/// which is exactly the kind of inconsistency the stream contract exists
/// to prevent.
pub(crate) fn print_digest(
    repository_path: &Path,
    output_path: Option<&Path>,
    baseline_path: Option<&Path>,
    exclude_subtrees: &[String],
) -> froe::Result<bool> {
    let repository = Repository::open(repository_path)?;

    // Streamed straight to its destination unless it has to be compared.
    // A digest is roughly 200 bytes per node, so a large production
    // repository produces one far too big to hold in memory for no reason
    // — and the common case, taking a digest before a maintenance run, has
    // no baseline to compare against yet.
    let Some(baseline_path) = baseline_path else {
        let Some(summary) = stream_digest(&repository, output_path, exclude_subtrees)? else {
            // The consumer closed the pipe partway through, so there is no
            // complete digest and no verdict to give.
            return Ok(true);
        };
        eprint!("{}", format_digest_summary(&summary));
        return Ok(summary.is_clean());
    };

    let mut rendered = Vec::new();
    let summary = digest_repository_excluding(&repository, exclude_subtrees, &mut rendered)?;
    let digest = String::from_utf8(rendered).map_err(|error| froe::Error::InvalidFormat {
        details: format!("the digest is not valid UTF-8: {error}"),
    })?;

    if let Some(path) = output_path {
        std::fs::write(path, digest.as_bytes())?;
    } else {
        let stdout = io::stdout();
        let mut locked = stdout.lock();
        write_diagnostic_handling_observed_broken_pipe(&mut locked, |output| {
            output.write_all(digest.as_bytes())?;
            Ok(())
        })?;
    }

    let mut clean = summary.is_clean();
    let mut report = format_digest_summary(&summary);

    {
        let baseline = std::fs::read_to_string(baseline_path)?;
        let difference = compare_digests(&baseline, &digest);
        if difference.is_empty() {
            report.push_str("content is identical to the baseline\n");
        } else {
            clean = false;
            let _ = writeln!(
                report,
                "content differs from the baseline: {} changed, {} removed, {} added",
                difference.changed.len(),
                difference.removed.len(),
                difference.added.len()
            );
            for (label, paths) in [
                ("changed", &difference.changed),
                ("removed", &difference.removed),
                ("added", &difference.added),
            ] {
                for node_path in paths.iter().take(DIGEST_DIFFERENCE_REPORT_LIMIT) {
                    let _ = writeln!(report, "  {label} {}", sanitize_terminal_text(node_path));
                }
                if paths.len() > DIGEST_DIFFERENCE_REPORT_LIMIT {
                    let _ = writeln!(
                        report,
                        "  ... and {} more {label}",
                        paths.len() - DIGEST_DIFFERENCE_REPORT_LIMIT
                    );
                }
            }
        }
    }

    eprint!("{report}");
    Ok(clean)
}

/// Renders the digest straight to its destination, returning the summary,
/// or `None` when a downstream consumer closed the pipe first.
fn stream_digest(
    repository: &Repository,
    output_path: Option<&Path>,
    exclude_subtrees: &[String],
) -> froe::Result<Option<DigestSummary>> {
    if let Some(path) = output_path {
        let mut file = io::BufWriter::new(std::fs::File::create(path)?);
        let summary = digest_repository_excluding(repository, exclude_subtrees, &mut file)?;
        file.flush()?;
        return Ok(Some(summary));
    }
    let stdout = io::stdout();
    let mut locked = io::BufWriter::new(stdout.lock());
    let mut summary = None;
    write_diagnostic_handling_observed_broken_pipe(&mut locked, |output| {
        summary = Some(digest_repository_excluding(
            repository,
            exclude_subtrees,
            output,
        )?);
        Ok(())
    })?;
    Ok(summary)
}

/// How many differing paths are named before the report summarizes the
/// rest. A whole-store change would otherwise print a line per node.
const DIGEST_DIFFERENCE_REPORT_LIMIT: usize = 20;

fn format_digest_summary(summary: &DigestSummary) -> String {
    let mut report = format!(
        "digested {} nodes, {} properties, {} binaries ({} bytes) and {} checkpoints\n",
        summary.nodes,
        summary.properties,
        summary.binaries,
        summary.binary_bytes,
        summary.checkpoints
    );
    if summary.lookup_failures > 0 {
        let _ = writeln!(
            report,
            "{} children or properties are present when enumerated but not reachable by \
             lookup, so an application resolving those paths finds nothing:",
            summary.lookup_failures
        );
        for detail in &summary.reported_lookup_failures {
            let _ = writeln!(report, "  {}", sanitize_terminal_text(detail));
        }
        let reported = summary.reported_lookup_failures.len() as u64;
        if summary.lookup_failures > reported {
            let _ = writeln!(
                report,
                "  ... and {} more",
                summary.lookup_failures - reported
            );
        }
    }
    if !summary.dangling_async_checkpoints.is_empty() {
        report.push_str(
            "asynchronous index lanes reference checkpoints that no longer exist, so Oak \
             will reindex from scratch rather than resume:\n",
        );
        for name in &summary.dangling_async_checkpoints {
            let _ = writeln!(report, "  {}", sanitize_terminal_text(name));
        }
    }
    report
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
    reporter: &Reporter,
) -> froe::Result<()> {
    let differences =
        diff_revisions_with_progress(repository, before, after, path, &mut reporter.clone())?;
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
pub(crate) fn print_history(
    repository: &Path,
    path: &str,
    reporter: &Reporter,
) -> froe::Result<()> {
    for entry in node_history_with_progress(repository, path, &mut reporter.clone())? {
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
    reporter: &Reporter,
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
    // Streamed, not collected: the scan visits every node record in the
    // store, so a broad predicate would otherwise buffer every match before
    // printing the first. Printing as they are found also means the operator
    // sees results during the scan rather than after it.
    let mut found = 0usize;
    let mut lines = Vec::new();
    let unreadable_nodes = search_nodes_visiting(
        repository,
        &query,
        &mut reporter.clone(),
        &mut |node_match| {
            found += 1;
            lines.push(format!(
                "{}  stable {}",
                node_match.record, node_match.stable_identifier
            ));
            if limit != 0 && found >= limit {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        },
    )?;
    // The reporter owns the terminal until the scan ends, so the lines are
    // held for the moment it releases it rather than interleaved with the
    // progress it is drawing.
    reporter.finish();
    for line in lines {
        println!("{line}");
    }
    println!("{found} matching nodes");
    if limit != 0 && found >= limit {
        eprintln!(
            "froe: stopped at the --limit of {limit}; raise it or pass --limit 0 for every match"
        );
    }
    if unreadable_nodes > 0 {
        eprintln!(
            "froe: {unreadable_nodes} record table entries could not be read and were skipped"
        );
    }
    Ok(())
}

#[cfg(test)]
mod archive_rendering_tests {
    use super::{
        oak_property_type_name, write_archive_debug, write_available_graph_row, write_segment_dump,
    };
    use froe::segment::identifier::SegmentIdentifier;
    use froe::tooling::ArchivePropertyDisplay;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("froe-cli-tooling-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    struct FailingWriter(std::io::ErrorKind);

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn independent_crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let low_bit_mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & low_bit_mask);
            }
        }
        !crc
    }

    fn independently_encoded_info_segment() -> Vec<u8> {
        let info = b"Oak";
        let record_position = 44usize;
        let record_length = 1 + info.len();
        let size = (record_position + record_length.div_ceil(4) * 4).div_ceil(16) * 16;
        let mut bytes = vec![0u8; size];
        bytes[0..3].copy_from_slice(b"0aK");
        bytes[3] = 13;
        bytes[4..8].copy_from_slice(&0x8000_0001u32.to_be_bytes());
        bytes[10..14].copy_from_slice(&1u32.to_be_bytes());
        bytes[18..22].copy_from_slice(&1u32.to_be_bytes());
        bytes[36] = 4;
        let virtual_offset = (262_144 - (size - record_position)) as u32;
        bytes[37..41].copy_from_slice(&virtual_offset.to_be_bytes());
        bytes[record_position] = info.len() as u8;
        bytes[record_position + 1..record_position + record_length].copy_from_slice(info);
        bytes
    }

    fn write_minimal_repository(directory: &std::path::Path) -> SegmentIdentifier {
        let segment_bytes = independently_encoded_info_segment();
        let identifier = SegmentIdentifier::new(0x1234, 0xa000_0000_0000_5678);
        let entry_name = format!("{identifier}.{:08x}", independent_crc32(&segment_bytes));

        let mut header = vec![0u8; 512];
        header[..entry_name.len()].copy_from_slice(entry_name.as_bytes());
        header[100..107].copy_from_slice(b"0000400");
        header[108..115].copy_from_slice(b"0000000");
        header[116..123].copy_from_slice(b"0000000");
        header[124..135].copy_from_slice(format!("{:011o}", segment_bytes.len()).as_bytes());
        header[136..147].copy_from_slice(b"00000000000");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        let header_checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..156].copy_from_slice(format!("{header_checksum:06o}\0 ").as_bytes());

        let mut archive = header;
        archive.extend_from_slice(&segment_bytes);
        archive.resize(512 + segment_bytes.len().div_ceil(512) * 512, 0);
        archive.extend_from_slice(&[0u8; 1024]);
        std::fs::write(directory.join("data00000a.tar"), archive).expect("write archive");
        std::fs::write(
            directory.join("journal.log"),
            format!("{identifier}:0 root 1\n"),
        )
        .expect("write journal");
        std::fs::write(directory.join("manifest"), "store.version=2\n").expect("write manifest");
        identifier
    }

    #[test]
    fn diagnostic_writer_fallback_quiets_only_an_observed_broken_pipe() {
        let directory = TestDirectory::new("broken-pipe");
        let identifier = write_minimal_repository(&directory.path);
        let repository = froe::Repository::open(&directory.path).expect("open repository");

        // This models platforms where the standard-output write returns an
        // error. Unix's real process-level SIGPIPE contract is covered by
        // spawned CLI integration tests instead.
        let mut broken_pipe = FailingWriter(std::io::ErrorKind::BrokenPipe);
        write_segment_dump(&mut broken_pipe, &repository, identifier)
            .expect("segment dump must stop quietly at a closed pipe");

        let mut broken_pipe = FailingWriter(std::io::ErrorKind::BrokenPipe);
        write_archive_debug(
            &mut broken_pipe,
            &repository,
            &["data99999a.tar".to_owned()],
        )
        .expect("archive debug must stop quietly at a closed pipe");

        let mut other_failure = FailingWriter(std::io::ErrorKind::PermissionDenied);
        let error = write_segment_dump(&mut other_failure, &repository, identifier)
            .expect_err("non-pipe output errors must remain visible");
        assert!(matches!(
            error,
            froe::Error::InputOutput(source)
                if source.kind() == std::io::ErrorKind::PermissionDenied
        ));

        drop(repository);
    }

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
