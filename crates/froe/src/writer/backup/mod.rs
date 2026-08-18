//! Backup, restore, and journal recovery.
//!
//! * **Backup** deep-copies the source repository's head — the super-root,
//!   so the content root *and* every checkpoint — into a target store,
//!   read-only on the source (no lock) and read-write on the target.
//! * **Restore** does the same with the roles swapped: the backup is the
//!   read-only source, and its head is copied into an existing store and
//!   becomes the new head.
//! * **Recover-journal** rebuilds `journal.log` when it is missing or
//!   unusable by scanning every data segment for candidate super-roots
//!   (nodes with both a `root` and a `checkpoints` child), verifying the
//!   newest one is fully traversable, and rewriting the journal with the
//!   surviving candidates oldest first — ordered by the timestamp in
//!   each segment's info record — so the consistent head is the last
//!   line.
//!
//! All three run under the target's repository lock, so they can never
//! race a running AEM instance, and all leave the store in a state a
//! subsequent AEM start consumes cleanly.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::cache::BoundedCache;
use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::store::{ArchiveSet, Repository};
use crate::writer::compaction::deep_copy_tree_across_stores_with_progress;
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::WritableRepository;

mod consistency;
mod recover;
mod scan;
#[cfg(test)]
mod test_support;

pub(crate) use consistency::*;
pub use recover::*;
pub(crate) use scan::*;

/// Copies the source repository's head into `target_directory`,
/// overwriting the target's head with the copied super-root. The target
/// is created when absent.
pub fn backup(source_directory: &Path, target_directory: &Path) -> Result<()> {
    backup_with_progress(source_directory, target_directory, &mut DiscardedProgress)
}

/// Backs up exactly like [`backup`], reporting the source scan and the
/// node copy to `observer`.
pub fn backup_with_progress(
    source_directory: &Path,
    target_directory: &Path,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let source = Repository::open_with_progress(source_directory, observer)?;
    let target = WritableRepository::open_with_progress(target_directory, observer)?;
    copy_head_between(
        &source,
        source.head_record_identifier(),
        &target,
        "b",
        observer,
    )?;
    target.close()
}

/// Restores a backup into an existing store: the backup's head is copied
/// into `target_directory` and becomes the new head. The target's earlier
/// revisions remain in the journal until later compaction reclaims them.
pub fn restore(backup_directory: &Path, target_directory: &Path) -> Result<()> {
    restore_with_progress(backup_directory, target_directory, &mut DiscardedProgress)
}

/// Restores exactly like [`restore`], reporting the backup scan and the
/// node copy to `observer`.
pub fn restore_with_progress(
    backup_directory: &Path,
    target_directory: &Path,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let backup = Repository::open_with_progress(backup_directory, observer)?;
    let target = WritableRepository::open_with_progress(target_directory, observer)?;
    copy_head_between(
        &backup,
        backup.head_record_identifier(),
        &target,
        "r",
        observer,
    )?;
    target.close()
}

/// Deep-copies `source_head` from `source` into `target`, advances the
/// target head, and flushes. Copied segments carry the source head's
/// exact garbage collection generation triple, never the target's — Oak
/// stamps `sourceHead.getSegmentId().getGcGeneration()` verbatim, and a
/// later Java cleanup pass reclaims by generation distance, so an
/// invented triple could make it delete segments the head still needs.
///
/// Both callers cross a store boundary, so the copy must carry every
/// binary block rather than re-linking bulk segments where they lie. That
/// re-linking is correct only within one store: it is what keeps a bulk
/// segment reachable across a compaction, and it is meaningless when the
/// target is a different directory, where the reference resolves to
/// nothing. Getting it wrong is silent — the target opens, serves its
/// whole content tree, and holds no binary content.
pub(crate) fn copy_head_between(
    source: &Repository,
    source_head: RecordIdentifier,
    target: &WritableRepository,
    writer_identifier: &str,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let generation = source_head_generation(source, source_head)?;
    let mut writer = target.record_writer_with_identifier(generation, writer_identifier);
    let (new_head, _) = crate::progress::observe(
        observer,
        &Step::new("copying nodes", WorkUnit::Nodes),
        |observer| {
            deep_copy_tree_across_stores_with_progress(source, &mut writer, source_head, observer)
        },
    )?;
    writer.finish()?;
    target.replace_head(new_head);
    target.flush()
}

/// The garbage collection generation triple of the source head's segment,
/// from the archive index or — for a recovered archive without index
/// metadata — the segment header itself.
pub(crate) fn source_head_generation(
    source: &Repository,
    source_head: RecordIdentifier,
) -> Result<GarbageCollectionGeneration> {
    for archive in source.archives() {
        if let Some(entry) = archive.index_entry(source_head.segment) {
            return Ok(GarbageCollectionGeneration {
                generation: entry.generation,
                full_generation: entry.full_generation,
                is_compacted: entry.is_compacted,
            });
        }
    }
    let view = source.segment(source_head.segment)?;
    Ok(GarbageCollectionGeneration {
        generation: view.structure.generation,
        full_generation: view.structure.full_generation,
        is_compacted: view.structure.is_compacted,
    })
}

#[cfg(test)]
mod tests {
    use super::{backup, restore};
    use crate::content::node::PropertyValues;
    use crate::content::property::PropertyValue;
    use crate::store::Repository;
    use crate::writer::backup::test_support::{
        TestDirectory, assert_content, populate, populate_with_bulk_binary,
    };
    use crate::writer::store_writer::WritableRepository;

    #[test]
    fn backup_copies_content_and_checkpoints() {
        let source = TestDirectory::new("backup-source");
        let target = TestDirectory::new("backup-target");
        populate(&source.path);

        backup(&source.path, &target.path).expect("backup");

        assert_content(&target.path, "Backup Source");
        let repository = Repository::open(&target.path).expect("reader");
        assert_eq!(
            repository.checkpoints().expect("checkpoints").len(),
            1,
            "checkpoints are copied with the head"
        );
        // The source is untouched and still readable.
        assert_content(&source.path, "Backup Source");
    }

    #[test]
    fn a_backup_carries_binary_content_that_lived_in_a_bulk_segment() {
        // Compaction shares bulk-segment blocks by reference rather than
        // copying them, because within one store a reference from the new
        // generation is exactly what keeps a bulk segment alive. Backup
        // used the same copy, and a reference into the *source's* bulk
        // segments resolves to nothing in the target — so the backup came
        // out holding the whole content tree and none of the binary.
        //
        // Nothing caught it, because the damage is invisible to everything
        // short of reading the bytes back: the target opens, its node and
        // property structure is complete, and a consistency check that
        // only resolves binary records rather than reading them passes.
        let source = TestDirectory::new("backup-bulk-source");
        let target = TestDirectory::new("backup-bulk-target");
        let content = populate_with_bulk_binary(&source.path);

        backup(&source.path, &target.path).expect("backup");

        // Read the binary back out of the target *alone*. Opening the
        // source anywhere in this assertion would let the missing blocks
        // resolve through it and hide the defect.
        let repository = Repository::open(&target.path).expect("open the backup");
        let resource = repository
            .content_root()
            .expect("content root")
            .child_node("file")
            .expect("read")
            .expect("the file node is present");
        let data = resource
            .property("jcr:data")
            .expect("read")
            .expect("jcr:data is present");
        let PropertyValues::Single(PropertyValue::Binary(binary)) = &data.values else {
            panic!("jcr:data did not decode as a single binary: {data:?}");
        };
        let crate::content::value::BinaryValue::Inline {
            record_identifier, ..
        } = binary
        else {
            panic!("expected an inline binary, got {binary:?}");
        };
        let copied = crate::content::value::read_binary_content(&repository, *record_identifier)
            .expect("the backup holds every block of the binary");
        assert_eq!(
            copied.len(),
            content.len(),
            "the backup holds the whole binary, not a prefix of it"
        );
        assert!(
            copied == content,
            "the binary in the backup is byte-identical to the source's"
        );
    }

    #[test]
    fn restore_overwrites_the_target_head() {
        let backup_store = TestDirectory::new("restore-backup");
        let target = TestDirectory::new("restore-target");
        populate(&backup_store.path);

        // The target starts as a different, bootstrapped store.
        {
            let store = WritableRepository::open(&target.path).expect("bootstrap target");
            store.close().expect("close");
        }
        restore(&backup_store.path, &target.path).expect("restore");
        assert_content(&target.path, "Backup Source");
    }

    #[test]
    fn backup_stamps_the_source_head_generation_verbatim() {
        let source = TestDirectory::new("backup-generation-source");
        let target = TestDirectory::new("backup-generation-target");
        populate(&source.path);
        // Advance the source's generation so the stamp is distinguishable
        // from a fresh target's (0, 0, false).
        {
            let mut store = WritableRepository::open(&source.path).expect("open");
            crate::writer::compaction::compact(
                &mut store,
                crate::writer::compaction::CompactionKind::Full,
            )
            .expect("compact");
            store.close().expect("close");
        }
        let source_head_generation = {
            let repository = Repository::open(&source.path).expect("reader");
            let head_segment = repository.head_record_identifier().segment;
            repository
                .archives()
                .iter()
                .find_map(|archive| archive.index_entry(head_segment))
                .map(|entry| (entry.generation, entry.full_generation, entry.is_compacted))
                .expect("head segment is indexed")
        };
        assert_ne!(
            source_head_generation.0, 0,
            "compaction advanced the source generation"
        );

        backup(&source.path, &target.path).expect("backup");

        let repository = Repository::open(&target.path).expect("target reader");
        let head_segment = repository.head_record_identifier().segment;
        let stamped = repository
            .archives()
            .iter()
            .find_map(|archive| archive.index_entry(head_segment))
            .map(|entry| (entry.generation, entry.full_generation, entry.is_compacted))
            .expect("target head segment is indexed");
        assert_eq!(
            stamped, source_head_generation,
            "the source head's generation triple is stamped verbatim"
        );
    }
}
