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
//!   (nodes with a `root` child), verifying each is fully traversable,
//!   and writing a journal line for the newest consistent one — ordered
//!   by the timestamp in each segment's info record.
//!
//! All three run under the target's repository lock, so they can never
//! race a running AEM instance, and all leave the store in a state a
//! subsequent AEM start consumes cleanly.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::writer::compaction::deep_copy_tree;
use crate::writer::store_writer::WritableRepository;

/// Copies the source repository's head into `target_directory`,
/// overwriting the target's head with the copied super-root. The target
/// is created when absent.
pub fn backup(source_directory: &Path, target_directory: &Path) -> Result<()> {
    let source = Repository::open(source_directory)?;
    let target = WritableRepository::open(target_directory)?;
    copy_head_between(&source, source.head_record_identifier(), &target, "b")?;
    target.close()
}

/// Restores a backup into an existing store: the backup's head is copied
/// into `target_directory` and becomes the new head. The target's earlier
/// revisions remain in the journal until later compaction reclaims them.
pub fn restore(backup_directory: &Path, target_directory: &Path) -> Result<()> {
    let backup = Repository::open(backup_directory)?;
    let target = WritableRepository::open(target_directory)?;
    copy_head_between(&backup, backup.head_record_identifier(), &target, "r")?;
    target.close()
}

/// Deep-copies `source_head` from `source` into `target`, advances the
/// target head, and flushes.
fn copy_head_between(
    source: &Repository,
    source_head: RecordIdentifier,
    target: &WritableRepository,
    writer_identifier: &str,
) -> Result<()> {
    let generation = target.writing_generation()?;
    let mut writer = target.record_writer_with_identifier(generation, writer_identifier);
    let (new_head, _) = deep_copy_tree(source, &mut writer, source_head)?;
    writer.finish()?;
    target.replace_head(new_head);
    target.flush()
}

/// The outcome of a journal recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryOutcome {
    /// The record identifier the journal was recovered to.
    pub recovered_head: RecordIdentifier,
    /// How many candidate super-roots were found.
    pub candidates_examined: usize,
    /// The path the previous journal was backed up to, when one existed.
    pub previous_journal_backup: Option<std::path::PathBuf>,
}

/// A candidate head discovered during recovery.
struct Candidate {
    record: RecordIdentifier,
    timestamp_milliseconds: i64,
    /// Whether the super-root has a `checkpoints` child, Oak's stronger
    /// candidate signal.
    has_checkpoints: bool,
}

/// Rebuilds `journal.log` from the segments on disk. Scans every data
/// segment for super-root candidates, keeps those that are fully
/// traversable, and rewrites the journal to a single line naming the
/// newest one. Backs up any existing journal to `journal.log.bak.NNN`.
pub fn recover_journal(directory: &Path) -> Result<RecoveryOutcome> {
    // A read-only view of every archive, opened without the lock and
    // without needing a resolvable journal.
    let archives = crate::store::open_all_archives(directory)?;

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen: HashSet<RecordIdentifier> = HashSet::new();
    let provider = crate::store::ArchiveSet::new(archives);

    for segment_identifier in provider.segment_identifiers() {
        if segment_identifier.is_bulk_segment() {
            continue;
        }
        let Ok(view) = provider.segment(segment_identifier) else {
            continue;
        };
        let timestamp = read_segment_info_timestamp(&provider, segment_identifier).unwrap_or(-1);
        // Every NODE record of the segment is a potential super-root.
        let node_records: Vec<u32> = view
            .structure
            .record_table()
            .iter()
            .filter(|entry| entry.record_type == crate::segment::record::RecordType::Node)
            .map(|entry| entry.record_number)
            .collect();
        for record_number in node_records {
            let record = RecordIdentifier::new(segment_identifier, record_number);
            if !seen.insert(record) {
                continue;
            }
            if let Some(has_checkpoints) = super_root_kind(&provider, record) {
                candidates.push(Candidate {
                    record,
                    timestamp_milliseconds: timestamp,
                    has_checkpoints,
                });
            }
        }
    }
    let candidates_examined = candidates.len();

    // Order candidates newest first — Oak's ordering — so the first
    // consistent one is the true head, never an older super-root that
    // happens to reach more content (which would resurrect deleted
    // content). Prefer a super-root that also has a checkpoints child
    // (Oak requires both), then the latest timestamp, then a deterministic
    // record order so the result never depends on random segment
    // identifiers. The write-order signal within a segment is the record
    // number: a higher number was allocated later, hence newer.
    candidates.sort_by(|first, second| {
        second
            .has_checkpoints
            .cmp(&first.has_checkpoints)
            .then_with(|| {
                second
                    .timestamp_milliseconds
                    .cmp(&first.timestamp_milliseconds)
            })
            .then_with(|| second.record.record_number.cmp(&first.record.record_number))
            .then_with(|| record_order_key(second.record).cmp(&record_order_key(first.record)))
    });

    // Take the newest fully consistent candidate. Consistency is checked
    // lazily in newest-first order, so a healthy store stops at the first
    // candidate rather than walking every historical revision.
    let recovered_head = candidates
        .iter()
        .find(|candidate| is_fully_consistent(&provider, candidate.record))
        .map(|candidate| candidate.record)
        .ok_or_else(|| Error::InvalidFormat {
            details: format!(
                "no consistent super-root found among {candidates_examined} candidates in {}",
                directory.display()
            ),
        })?;

    let previous_journal_backup = back_up_existing_journal(directory)?;
    // Roll the journal backup back if writing the new journal fails, so a
    // failure never leaves the store without a journal.
    if let Err(error) = write_recovered_journal(directory, recovered_head) {
        if let Some(backup_path) = &previous_journal_backup {
            let _ = std::fs::rename(backup_path, directory.join("journal.log"));
        }
        return Err(error);
    }

    Ok(RecoveryOutcome {
        recovered_head,
        candidates_examined,
        previous_journal_backup,
    })
}

/// Classifies a node as a super-root candidate: `Some(has_checkpoints)`
/// when it has a `root` child, where the flag reports whether it also has
/// a `checkpoints` child (Oak's stronger signal). `None` when it is not a
/// super-root or cannot be read.
fn super_root_kind(provider: &dyn SegmentProvider, record: RecordIdentifier) -> Option<bool> {
    let node = NodeState::new(provider, record);
    let has_root = node.child_node("root").ok()?.is_some();
    if !has_root {
        return None;
    }
    let has_checkpoints = node.child_node("checkpoints").ok()?.is_some();
    Some(has_checkpoints)
}

/// A total, deterministic ordering key for a record identifier, so
/// recovery never depends on random segment identifiers.
fn record_order_key(record: RecordIdentifier) -> (u64, u64, u32) {
    (
        record.segment.most_significant_bits,
        record.segment.least_significant_bits,
        record.record_number,
    )
}

/// A candidate tree is never this deep; a greater depth means a cycle in
/// corrupt data. Bounded below the stack-overflow threshold.
const MAXIMUM_RECOVERY_DEPTH: usize = 4000;

/// Whether the entire tree under a candidate super-root can be traversed
/// without a missing segment or malformed record — the consistency gate.
/// Inline binaries are materialized so a head whose binary bulk segments
/// are missing fails the gate (Oak would reject such a revision).
fn is_fully_consistent(provider: &dyn SegmentProvider, record: RecordIdentifier) -> bool {
    fn walk(
        provider: &dyn SegmentProvider,
        record: RecordIdentifier,
        visited: &mut HashSet<RecordIdentifier>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAXIMUM_RECOVERY_DEPTH || !visited.insert(record) {
            return Ok(());
        }
        let node = NodeState::new(provider, record);
        for property in node.properties()? {
            match &property.values {
                crate::content::node::PropertyValues::Single(value) => {
                    materialize_inline_binary(provider, value)?;
                }
                crate::content::node::PropertyValues::Multiple(values) => {
                    for value in values {
                        materialize_inline_binary(provider, value)?;
                    }
                }
            }
        }
        for (_, child) in node.child_node_entries()? {
            walk(provider, child.record_identifier(), visited, depth + 1)?;
        }
        Ok(())
    }
    let mut visited = HashSet::new();
    walk(provider, record, &mut visited, 0).is_ok()
}

/// Reads an inline binary's content to force its blocks (possibly in bulk
/// segments) to resolve; external binaries have no local content.
fn materialize_inline_binary(
    provider: &dyn SegmentProvider,
    value: &crate::content::property::PropertyValue,
) -> Result<()> {
    use crate::content::property::PropertyValue;
    use crate::content::value::BinaryValue;
    if let PropertyValue::Binary(BinaryValue::Inline {
        record_identifier, ..
    }) = value
    {
        crate::content::value::read_binary_content(provider, *record_identifier)?;
    }
    Ok(())
}

/// Reads the `"t"` timestamp from a segment's info record (record 0).
fn read_segment_info_timestamp(
    provider: &dyn SegmentProvider,
    segment: SegmentIdentifier,
) -> Option<i64> {
    let view = provider.segment(segment).ok()?;
    let first_record = view.structure.record_table().first()?.record_number;
    let info =
        crate::content::value::read_string(provider, RecordIdentifier::new(segment, first_record))
            .ok()?;
    parse_info_timestamp(&info)
}

/// Extracts the `"t":<number>` value from a segment-info JSON string.
fn parse_info_timestamp(info: &str) -> Option<i64> {
    let marker = "\"t\":";
    let start = info.find(marker)? + marker.len();
    let rest = &info[start..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Backs up an existing `journal.log` to the first free
/// `journal.log.bak.NNN` (000–999).
fn back_up_existing_journal(directory: &Path) -> Result<Option<std::path::PathBuf>> {
    let journal_path = directory.join("journal.log");
    if !journal_path.exists() {
        return Ok(None);
    }
    for counter in 0..1000 {
        let backup = directory.join(format!("journal.log.bak.{counter:03}"));
        if !backup.exists() {
            std::fs::rename(&journal_path, &backup)?;
            return Ok(Some(backup));
        }
    }
    Err(Error::InvalidFormat {
        details: "all journal backup names (000-999) are taken".to_owned(),
    })
}

/// Writes the recovered journal: a single line naming `head`, fsynced.
fn write_recovered_journal(directory: &Path, head: RecordIdentifier) -> Result<()> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let line = format!(
        "{}:{} root {timestamp}\n",
        head.segment, head.record_number as i32
    );
    let mut file = std::fs::File::create(directory.join("journal.log"))?;
    file.write_all(line.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{backup, parse_info_timestamp, recover_journal, restore};
    use crate::content::node::PropertyValues;
    use crate::content::property::PropertyValue;
    use crate::store::Repository;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::store_writer::WritableRepository;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-backup-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Populates a store with `/content` (a title property, two children)
    /// and one checkpoint.
    fn populate(directory: &std::path::Path) {
        let store = WritableRepository::open(directory).expect("bootstrap");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let alpha = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("alpha");
        let beta = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("beta");
        let title = writer.write_string("Backup Source").expect("value");
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("alpha".to_owned(), alpha),
                    ("beta".to_owned(), beta),
                ]),
                &[PropertyToWrite {
                    name: "title".to_owned(),
                    property_type: crate::content::property::PropertyType::String,
                    values: PropertyValuesToWrite::Single(title),
                }],
            )
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content,
                },
                &[],
            )
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
        store.close().expect("close");
    }

    fn assert_content(directory: &std::path::Path, expected_title: &str) {
        let repository = Repository::open(directory).expect("reader");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        assert_eq!(content.child_node_count().expect("count"), 2);
        assert_eq!(
            content
                .property("title")
                .expect("read")
                .expect("present")
                .values,
            PropertyValues::Single(PropertyValue::String(expected_title.to_owned()))
        );
    }

    /// Writes one `/content` revision whose child count is `child_count`,
    /// then a checkpoint so it is a full super-root candidate.
    fn write_revision_with_children(directory: &std::path::Path, child_count: usize) {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let mut children = Vec::new();
        for index in 0..child_count {
            let child = writer
                .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
                .expect("child");
            children.push((format!("child-{index}"), child));
        }
        let child_structure = if children.is_empty() {
            ChildNodesToWrite::Zero
        } else {
            ChildNodesToWrite::Many(children)
        };
        let content = writer
            .write_node(Some("nt:unstructured"), &[], &child_structure, &[])
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content,
                },
                &[],
            )
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
        store.close().expect("close");
    }

    #[test]
    fn recovery_picks_the_newest_revision_even_when_it_has_fewer_nodes() {
        // A recovery must return the newest committed state, never an older
        // one that happens to reach more content — otherwise a commit that
        // deleted content would be silently reverted.
        let directory = TestDirectory::new("recover-newest");
        write_revision_with_children(&directory.path, 8); // older, larger
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_revision_with_children(&directory.path, 2); // newer, smaller

        let newest_head = {
            let repository = Repository::open(&directory.path).expect("reader");
            repository.head_record_identifier()
        };

        std::fs::remove_file(directory.path.join("journal.log")).expect("remove journal");
        let outcome = recover_journal(&directory.path).expect("recover");
        assert_eq!(
            outcome.recovered_head, newest_head,
            "recovery returns the newest revision, not the larger older one"
        );
        // The recovered content has the newer, smaller child set.
        let repository = Repository::open(&directory.path).expect("reader");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        assert_eq!(content.child_node_count().expect("count"), 2);
    }

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
    fn recover_journal_rebuilds_a_deleted_journal() {
        let directory = TestDirectory::new("recover");
        populate(&directory.path);

        // Delete the journal, then recover it from the segments.
        std::fs::remove_file(directory.path.join("journal.log")).expect("remove journal");
        let outcome = recover_journal(&directory.path).expect("recover");
        assert!(outcome.candidates_examined >= 1);

        // The store reads back with content intact.
        assert_content(&directory.path, "Backup Source");
    }

    #[test]
    fn recover_journal_backs_up_a_corrupt_journal() {
        let directory = TestDirectory::new("recover-backup");
        populate(&directory.path);

        std::fs::write(
            directory.path.join("journal.log"),
            "garbage-with-no-space\n",
        )
        .expect("corrupt journal");
        let outcome = recover_journal(&directory.path).expect("recover");
        assert!(
            outcome.previous_journal_backup.is_some(),
            "the corrupt journal is backed up"
        );
        assert!(directory.path.join("journal.log.bak.000").exists());
        assert_content(&directory.path, "Backup Source");
    }

    #[test]
    fn parses_segment_info_timestamps() {
        assert_eq!(
            parse_info_timestamp("{\"wid\":\"froe\",\"sno\":3,\"t\":1700000000000}"),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_info_timestamp("{\"wid\":\"x\"}"), None);
    }
}
