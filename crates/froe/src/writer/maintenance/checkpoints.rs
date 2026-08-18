//! Planning checkpoint removal, including the asynchronous references
//! a checkpoint's own subtree still holds.

use super::options::{CompactionOptions, MaintenanceTask};
use super::planning::CheckpointPlan;
use crate::content::node::PropertyValues;
use crate::content::property::PropertyValue;
use crate::error::Result;
use crate::store::Repository;
use std::collections::{BTreeSet, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn plan_checkpoints(
    repository: &Repository,
    options: &CompactionOptions,
    now: SystemTime,
    warnings: &mut Vec<String>,
) -> Result<CheckpointPlan> {
    if !options.contains(MaintenanceTask::ExpiredCheckpoints)
        && !options.contains(MaintenanceTask::UnreferencedCheckpoints)
    {
        return Ok(CheckpointPlan::default());
    }
    let now_milliseconds = now.duration_since(UNIX_EPOCH).map_or(0, |duration| {
        duration.as_millis().min(i64::MAX as u128) as i64
    });
    let checkpoints = repository.checkpoints()?;
    let referenced = if options.contains(MaintenanceTask::UnreferencedCheckpoints) {
        async_checkpoint_references(repository)?
    } else {
        HashSet::new()
    };
    let mut expired_names = BTreeSet::new();
    let mut unreferenced_names = BTreeSet::new();
    for (name, checkpoint) in checkpoints {
        if options.contains(MaintenanceTask::ExpiredCheckpoints) {
            match checkpoint.property("timestamp")? {
                Some(property) => match property.values {
                    PropertyValues::Single(PropertyValue::Long(timestamp)) => {
                        if now_milliseconds > timestamp {
                            expired_names.insert(name.clone());
                        }
                    }
                    _ => warnings.push(format!(
                        "checkpoint {name} has a malformed timestamp and was not selected by expiry"
                    )),
                },
                None => warnings.push(format!(
                    "checkpoint {name} has no timestamp and was not selected by expiry"
                )),
            }
        }
        if options.contains(MaintenanceTask::UnreferencedCheckpoints)
            && !referenced.contains(&name)
            && !expired_names.contains(&name)
        {
            unreferenced_names.insert(name);
        }
    }
    let expired = expired_names.len();
    let unreferenced = unreferenced_names.len();
    expired_names.extend(unreferenced_names);
    Ok(CheckpointPlan {
        names: expired_names.into_iter().collect(),
        expired,
        unreferenced,
    })
}

pub(super) fn async_checkpoint_references(repository: &Repository) -> Result<HashSet<String>> {
    let mut referenced = HashSet::new();
    if let Some(async_state) = repository.content_root()?.child_node(":async")? {
        for property in async_state.properties()? {
            match property.values {
                PropertyValues::Single(PropertyValue::String(value)) => {
                    referenced.insert(value);
                }
                PropertyValues::Multiple(values) => {
                    referenced.extend(values.into_iter().filter_map(|value| match value {
                        PropertyValue::String(value) => Some(value),
                        _ => None,
                    }));
                }
                PropertyValues::Single(_) => {}
            }
        }
    }
    Ok(referenced)
}

#[cfg(test)]
mod tests {
    use crate::content::provider::SegmentProvider as _;
    use crate::store::Repository;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::prepared::*;
    use crate::writer::maintenance::test_support::*;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::store_writer::WritableRepository;
    use std::num::NonZeroUsize;

    #[test]
    fn checkpoint_without_a_snapshot_root_fails_the_health_gate() {
        let directory = TestDirectory::repository("malformed-checkpoint");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        let content_root = store
            .head_node()
            .child_node("root")
            .expect("read root")
            .expect("root exists")
            .record_identifier();
        let mut writer = store.record_writer(store.writing_generation().expect("generation"));
        let malformed_checkpoint = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("malformed checkpoint");
        let checkpoints = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "broken".to_owned(),
                    node: malformed_checkpoint,
                },
                &[],
            )
            .expect("checkpoint container");
        let malformed_head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("checkpoints".to_owned(), checkpoints),
                    ("root".to_owned(), content_root),
                ]),
                &[],
            )
            .expect("malformed super-root");
        writer.finish().expect("finish");
        assert!(store.compare_and_set_head(store.head(), malformed_head));
        store.close().expect("close");

        let result = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([]),
        );
        assert!(
            result.is_err(),
            "cleanup must not bless a checkpoint without its snapshot root"
        );
    }

    #[test]
    fn expired_checkpoints_are_removed_in_one_healthy_head_update() {
        let directory = TestDirectory::repository("expired-checkpoint");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let outcome = compact(&directory.path, options).expect("cleanup");

        assert_eq!(outcome.removed_checkpoints, 1);
        let repository = Repository::open(&directory.path).expect("healthy repository");
        assert!(repository.checkpoints().expect("checkpoints").is_empty());
    }

    #[test]
    fn a_retention_bound_beside_a_checkpoint_head_update_is_refused_while_planning() {
        let directory = TestDirectory::repository("retention-with-checkpoint");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Checkpoint removal installs a new head and appends its journal
        // line, which moves the newest resolvable revision. The bound would
        // then retire the very head the plan retained, and the run would
        // abort its own apply — after the checkpoint removal had already
        // been committed. Refuse while planning, where nothing has moved.
        let options = CompactionOptions::default()
            .with_tasks([
                MaintenanceTask::ExpiredCheckpoints,
                MaintenanceTask::Journal,
            ])
            .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision"));
        let error = plan_compaction(&directory.path, &options).expect_err("must refuse");
        assert!(
            error.to_string().contains("checkpoint"),
            "unexpected refusal: {error}"
        );

        // Nothing was touched: the checkpoint is still there to be removed
        // by a run that does not also bound the journal.
        let repository = Repository::open(&directory.path).expect("healthy repository");
        assert_eq!(repository.checkpoints().expect("checkpoints").len(), 1);
    }

    #[test]
    fn checkpoint_planning_rejects_a_physically_exhausted_archive_namespace() {
        let directory = TestDirectory::repository("checkpoint-archive-number-exhausted");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Zero-byte files are skipped by archive discovery, but their exact
        // Oak archive names still occupy the physical namespace.
        std::fs::write(directory.path.join("data4294967295z.tar"), b"")
            .expect("install maximum-number residue");
        let before = file_bytes(&directory.path);
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("planning must reject u32::MAX before checkpoint mutation");

        assert!(
            error.to_string().contains("namespace is exhausted"),
            "{error}"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "namespace preflight must remain byte-exact"
        );
    }

    #[test]
    fn checkpoint_only_cleanup_rejects_current_index_generation_mismatch() {
        let directory = TestDirectory::repository("checkpoint-index-generation-mismatch");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let repository = Repository::open(&directory.path).expect("open checkpoint repository");
        let head = repository.head_record_identifier();
        let archive_name = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(head.segment))
            .expect("active archive contains head")
            .file_name()
            .to_owned();
        let header_generation = repository
            .segment(head.segment)
            .expect("read head segment")
            .structure
            .generation;
        drop(repository);
        change_index_generation(
            &directory.path.join(archive_name),
            head.segment,
            header_generation.saturating_add(1),
        );
        let before = file_bytes(&directory.path);
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("checkpoint head update must reject corrupt index generation");

        assert!(error.to_string().contains("index generation"), "{error}");
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning must not mutate"
        );
    }

    #[test]
    fn checkpoint_only_cleanup_rejects_duplicate_head_segments_before_generation_validation() {
        let directory = TestDirectory::repository("checkpoint-duplicate-head-generation");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));

        let repository = Repository::open(&directory.path).expect("open checkpoint repository");
        let head = repository.head_record_identifier();
        let source_name = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(head.segment))
            .expect("active archive contains head")
            .file_name()
            .to_owned();
        let next_number = repository
            .archives()
            .iter()
            .filter_map(|archive| {
                crate::tar_archive::file_name::ArchiveFileName::parse(archive.file_name())
                    .map(|name| name.archive_number)
            })
            .max()
            .expect("fixture has archives")
            .checked_add(1)
            .expect("fixture archive namespace");
        let duplicate_name = format!("data{next_number:05}a.tar");
        let header_generation = repository
            .segment(head.segment)
            .expect("read head segment")
            .structure
            .generation;
        drop(repository);

        let duplicate_path = directory.path.join(&duplicate_name);
        std::fs::copy(directory.path.join(source_name), &duplicate_path)
            .expect("copy head archive under a newer number");
        change_index_generation(
            &duplicate_path,
            head.segment,
            header_generation.saturating_add(1),
        );
        let before = file_bytes(&directory.path);
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

        let error = plan_compaction(&directory.path, &options)
            .expect_err("checkpoint write must reject ambiguous duplicate segment locations");

        assert!(
            error.to_string().contains("occurs in active archives"),
            "{error}"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning must not mutate"
        );
    }
}
