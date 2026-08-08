//! Inspection commands: repository summary, journal, archives, segments,
//! and checkpoints.

use froe::content::PropertyValues;
use froe::content::property::PropertyValue;
use froe::segment::identifier::SegmentIdentifier;
use froe::store::Repository;

use crate::output::format_timestamp;

/// `froe summary`: one screen of repository facts.
pub(crate) fn print_summary(repository: &Repository) -> froe::Result<()> {
    println!("repository        {}", repository.directory().display());
    println!("archives          {}", repository.archives().len());
    let total_size: u64 = repository
        .archives()
        .iter()
        .map(froe::tar_archive::TarArchiveReader::file_size)
        .sum();
    println!("archive bytes     {total_size}");
    let mut data_segments = 0usize;
    let mut bulk_segments = 0usize;
    for identifier in repository.segment_identifiers() {
        if identifier.is_bulk_segment() {
            bulk_segments += 1;
        } else {
            data_segments += 1;
        }
    }
    println!("segments          {data_segments} data, {bulk_segments} bulk");
    println!("journal entries   {}", repository.journal_entries().len());
    let head = repository.head_record_identifier();
    println!("head              {}:{}", head.segment, head.record_number);
    if let Some(newest) = repository.journal_entries().first() {
        println!(
            "head written      {}",
            format_timestamp(newest.timestamp_milliseconds)
        );
    }
    println!("checkpoints       {}", repository.checkpoints()?.len());
    Ok(())
}

/// `froe journal`: the revisions, newest first.
pub(crate) fn print_journal(repository: &Repository, limit: usize) {
    for entry in repository.journal_entries().iter().take(limit) {
        let validity = match entry.record_identifier() {
            Some(identifier) if repository.contains_segment(identifier.segment) => "",
            Some(_) => "  (segment missing)",
            None => "  (unparseable)",
        };
        println!(
            "{}  {}{validity}",
            format_timestamp(entry.timestamp_milliseconds),
            entry.revision_text,
        );
    }
}

/// `froe archives`: per-archive statistics.
pub(crate) fn print_archives(repository: &Repository) {
    for archive in repository.archives() {
        let index_state = if archive.is_recovered() {
            "recovered (no valid index)".to_owned()
        } else {
            format!(
                "index version {}",
                archive.index().map_or(0, |index| index.version)
            )
        };
        println!(
            "{}  {} bytes  {} segments  {index_state}",
            archive.file_name(),
            archive.file_size(),
            archive.segment_count(),
        );
    }
}

/// `froe segments`: every segment, in archive probe order.
pub(crate) fn print_segments(repository: &Repository) {
    for archive in repository.archives() {
        for identifier in archive.segment_identifiers() {
            let kind = if identifier.is_bulk_segment() {
                "bulk"
            } else {
                "data"
            };
            match archive.index_entry(identifier) {
                Some(entry) => println!(
                    "{identifier}  {kind}  {} bytes  generation {}  full {}  {}  {}",
                    entry.size,
                    entry.generation,
                    entry.full_generation,
                    if entry.is_compacted {
                        "compacted"
                    } else {
                        "not compacted"
                    },
                    archive.file_name(),
                ),
                None => println!("{identifier}  {kind}  (recovered)  {}", archive.file_name()),
            }
        }
    }
}

/// `froe segment`: one segment's structure.
pub(crate) fn print_segment(
    repository: &Repository,
    identifier: SegmentIdentifier,
) -> froe::Result<()> {
    use froe::content::provider::SegmentProvider;
    let view = repository.segment(identifier)?;
    let structure = &view.structure;
    println!("segment           {identifier}");
    println!("kind              {:?}", structure.kind);
    println!("size              {} bytes", structure.size);
    match structure.version {
        Some(version) => println!("format version    {version}"),
        None => println!("format version    none (bulk segments have no header)"),
    }
    println!("generation        {}", structure.generation);
    println!("full generation   {}", structure.full_generation);
    println!("compacted         {}", structure.is_compacted);
    println!("references        {}", structure.referenced_segments.len());
    for referenced in &structure.referenced_segments {
        println!("                  {referenced}");
    }
    println!("records           {}", structure.record_table().len());
    let mut counts_by_type: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for entry in structure.record_table() {
        let type_name = entry.record_type().map_or_else(
            || format!("UnknownType({})", entry.type_byte),
            |record_type| format!("{record_type:?}"),
        );
        *counts_by_type.entry(type_name).or_default() += 1;
    }
    for (record_type, count) in counts_by_type {
        println!("                  {count} x {record_type}");
    }
    Ok(())
}

/// `froe checkpoints`: name and metadata of every checkpoint.
pub(crate) fn print_checkpoints(repository: &Repository) -> froe::Result<()> {
    let checkpoints = repository.checkpoints()?;
    if checkpoints.is_empty() {
        println!("no checkpoints");
        return Ok(());
    }
    for (name, checkpoint) in checkpoints {
        let read_long = |property_name: &str| -> froe::Result<Option<i64>> {
            Ok(checkpoint
                .property(property_name)?
                .and_then(|property| match property.values {
                    PropertyValues::Single(PropertyValue::Long(value)) => Some(value),
                    _ => None,
                }))
        };
        let created = read_long("created")?.map_or_else(|| "unknown".to_owned(), format_timestamp);
        let expires =
            read_long("timestamp")?.map_or_else(|| "unknown".to_owned(), format_timestamp);
        println!(
            "{}  created {created}  expires {expires}",
            crate::output::sanitize_terminal_text(&name)
        );
    }
    Ok(())
}
