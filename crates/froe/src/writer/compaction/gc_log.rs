//! The `gc.log` entry a compaction leaves behind, in the form Oak writes
//! and reads. Maintenance appends one too, so this is not private to the
//! compactor.

use super::{GarbageCollectionGeneration, RecordIdentifier, Result};

/// Oak's seven-field `gc.log` line for one completed compaction cycle:
/// `repoSize,reclaimedSize,timestamp,generation,fullGeneration,nodes,root`.
///
/// Built separately from the append so a caller can hold the exact bytes it
/// wrote and prove afterwards that the file grew by those and nothing else.
/// The timestamp makes the line unreproducible, which is why proving it after
/// the fact means remembering it rather than recomputing it.
pub(crate) fn garbage_collection_log_entry(
    repository_size: u64,
    reclaimed_size: u64,
    generation: GarbageCollectionGeneration,
    compacted_nodes: u64,
    root: RecordIdentifier,
) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    format!(
        "{repository_size},{reclaimed_size},{timestamp},{},{},{compacted_nodes},{}:{}\n",
        generation.generation, generation.full_generation, root.segment, root.record_number as i32,
    )
}

/// Appends one already-built entry to `gc.log`, durably.
pub(crate) fn append_garbage_collection_log_entry(
    directory: &std::path::Path,
    line: &str,
) -> Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("gc.log"))?;
    file.write_all(line.as_bytes())?;
    file.sync_data()?;
    Ok(())
}
