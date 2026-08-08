//! The mutating commands: compaction, backup, restore, journal recovery,
//! and checkpoint management.
//!
//! Every one of these takes the exclusive repository lock, so it can never
//! run against a live AEM instance. Because they change the store on disk,
//! each requires explicit confirmation — either an interactive `yes` or
//! the `--yes` flag — before proceeding.

use std::io::Write;
use std::path::Path;

use froe::writer::commit::{
    create_checkpoint, list_checkpoints, release_checkpoint, remove_all_checkpoints,
    remove_unreferenced_checkpoints,
};
use froe::writer::compaction::CompactionKind;
use froe::writer::store_writer::WritableRepository;
use froe::{backup, compact, recover_journal, restore};

use crate::output::format_timestamp;

/// Asks for confirmation before a mutating operation, unless `assume_yes`.
fn confirm(action: &str, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    eprint!("froe: {action} — this modifies the repository. Continue? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
}

/// `froe compact`: offline full or tail compaction.
pub(crate) fn run_compact(repository: &Path, tail: bool, assume_yes: bool) -> froe::Result<bool> {
    let kind = if tail {
        CompactionKind::Tail
    } else {
        CompactionKind::Full
    };
    if !confirm(
        &format!(
            "about to run {} compaction on {}",
            kind_name(kind),
            repository.display()
        ),
        assume_yes,
    ) {
        eprintln!("froe: compaction cancelled");
        return Ok(false);
    }
    let mut store = WritableRepository::open(repository)?;
    let outcome = compact(&mut store, kind)?;
    store.close()?;
    println!(
        "compacted {} nodes; {} bytes -> {} bytes ({} reclaimed)",
        outcome.compacted_nodes,
        outcome.size_before,
        outcome.size_after,
        outcome.size_before.saturating_sub(outcome.size_after),
    );
    Ok(true)
}

fn kind_name(kind: CompactionKind) -> &'static str {
    match kind {
        CompactionKind::Full => "full",
        CompactionKind::Tail => "tail",
    }
}

/// `froe backup`: copy the source repository's head into a target.
pub(crate) fn run_backup(source: &Path, target: &Path, assume_yes: bool) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to back up {} into {}",
            source.display(),
            target.display()
        ),
        assume_yes,
    ) {
        eprintln!("froe: backup cancelled");
        return Ok(false);
    }
    backup(source, target)?;
    println!(
        "backup complete: {} -> {}",
        source.display(),
        target.display()
    );
    Ok(true)
}

/// `froe restore`: copy a backup's head into an existing store.
pub(crate) fn run_restore(
    backup_directory: &Path,
    target: &Path,
    assume_yes: bool,
) -> froe::Result<bool> {
    if !confirm(
        &format!(
            "about to restore {} into {} (overwriting its head)",
            backup_directory.display(),
            target.display()
        ),
        assume_yes,
    ) {
        eprintln!("froe: restore cancelled");
        return Ok(false);
    }
    restore(backup_directory, target)?;
    println!(
        "restore complete: {} -> {}",
        backup_directory.display(),
        target.display()
    );
    Ok(true)
}

/// `froe recover-journal`: rebuild journal.log from the segments.
pub(crate) fn run_recover_journal(repository: &Path, assume_yes: bool) -> froe::Result<bool> {
    if !confirm(
        &format!("about to rebuild the journal of {}", repository.display()),
        assume_yes,
    ) {
        eprintln!("froe: recovery cancelled");
        return Ok(false);
    }
    let outcome = recover_journal(repository)?;
    println!(
        "recovered head {} from {} candidates",
        outcome.recovered_head, outcome.candidates_examined
    );
    if let Some(backup_path) = outcome.previous_journal_backup {
        println!("previous journal backed up at {}", backup_path.display());
    }
    Ok(true)
}

/// `froe checkpoint list`: the store's checkpoints.
pub(crate) fn run_checkpoint_list(repository: &Path) -> froe::Result<()> {
    let store = WritableRepository::open(repository)?;
    let checkpoints = list_checkpoints(&store)?;
    if checkpoints.is_empty() {
        println!("no checkpoints");
    }
    for checkpoint in &checkpoints {
        let created = checkpoint
            .created_milliseconds
            .map_or_else(|| "unknown".to_owned(), format_timestamp);
        let expires = checkpoint
            .expires_milliseconds
            .map_or_else(|| "unknown".to_owned(), format_timestamp);
        println!("{}  created {created}  expires {expires}", checkpoint.name);
    }
    store.close()
}

/// `froe checkpoint create`: create a checkpoint.
pub(crate) fn run_checkpoint_create(
    repository: &Path,
    lifetime_milliseconds: i64,
    assume_yes: bool,
) -> froe::Result<bool> {
    if !confirm(
        &format!("about to create a checkpoint in {}", repository.display()),
        assume_yes,
    ) {
        eprintln!("froe: checkpoint creation cancelled");
        return Ok(false);
    }
    let store = WritableRepository::open(repository)?;
    let name = create_checkpoint(&store, lifetime_milliseconds, &[])?;
    store.close()?;
    println!("created checkpoint {name}");
    Ok(true)
}

/// `froe checkpoint remove`: remove a checkpoint by name, all, or
/// unreferenced ones.
pub(crate) fn run_checkpoint_remove(
    repository: &Path,
    target: &CheckpointRemoval,
    assume_yes: bool,
) -> froe::Result<bool> {
    if !confirm(
        &format!("about to remove checkpoints from {}", repository.display()),
        assume_yes,
    ) {
        eprintln!("froe: checkpoint removal cancelled");
        return Ok(false);
    }
    let store = WritableRepository::open(repository)?;
    match target {
        CheckpointRemoval::Named(name) => {
            if release_checkpoint(&store, name)? {
                println!("removed checkpoint {name}");
            } else {
                println!("no checkpoint named {name}");
            }
        }
        CheckpointRemoval::All => {
            let removed = remove_all_checkpoints(&store)?;
            println!("removed {removed} checkpoints");
        }
        CheckpointRemoval::Unreferenced => {
            let removed = remove_unreferenced_checkpoints(&store)?;
            println!("removed {removed} unreferenced checkpoints");
        }
    }
    store.close()?;
    Ok(true)
}

/// Which checkpoints a removal targets.
pub(crate) enum CheckpointRemoval {
    /// One checkpoint by name.
    Named(String),
    /// Every checkpoint.
    All,
    /// Checkpoints not referenced by the asynchronous indexer.
    Unreferenced,
}
