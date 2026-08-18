//! Every boundary of an archive sweep: a killed run leaves a healthy
//! prefix, a substituted path is never published, and a certified source
//! is never unlinked under a different inode.

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::maintenance::{CompactionAction, plan_compaction};
    use crate::writer::maintenance_fault_injection::test_support::{
        POSTCOMPACTION_SWEEP_SCENARIO, REMOVAL_SCENARIO, RepositorySnapshot,
        STALE_ARCHIVE_SCENARIO, SWEEP_SCENARIO, TestDirectory, assert_exact_snapshot_reopens,
        readable_journal_roots, run_absence_child, run_crash_child, run_error_child,
        run_substitution_child, scenario_options, snapshot_repository,
    };
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::repository_lock::RepositoryLock;
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::store_writer::WritableRepository;
    use std::path::Path;

    fn create_partial_sweep_fixture(
        directory: &TestDirectory,
    ) -> (RepositorySnapshot, String, String) {
        let store = WritableRepository::open(&directory.path).expect("open sweep fixture writer");
        let obsolete_generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        for _ in 0..3 {
            let mut writer = store.record_writer(obsolete_generation);
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("write orphan node");
            writer.finish().expect("finish orphan segment");
        }

        let current_generation = GarbageCollectionGeneration {
            generation: 3,
            full_generation: 3,
            is_compacted: false,
        };
        let old_head = store.head();
        let mut writer = store.record_writer(current_generation);
        let content_root = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("write current content root");
        let new_head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("write current super-root");
        writer.finish().expect("finish current head segment");
        assert!(store.compare_and_set_head(old_head, new_head));
        store.close().expect("close sweep fixture writer");

        let options = scenario_options(SWEEP_SCENARIO);
        let plan = plan_compaction(&directory.path, &options).expect("plan partial sweep fixture");
        let (source, replacement) = plan
            .actions()
            .iter()
            .find_map(|action| match action {
                CompactionAction::RewriteArchive {
                    file_name,
                    replacement_name,
                    ..
                } => Some((file_name.clone(), replacement_name.clone())),
                _ => None,
            })
            .expect("fixture must require a partial archive rewrite");
        (snapshot_repository(&directory.path), source, replacement)
    }

    fn create_whole_archive_sweep_fixture(
        directory: &TestDirectory,
    ) -> (RepositorySnapshot, String, Vec<u8>) {
        {
            let store = WritableRepository::open(&directory.path).expect("open head writer");
            let old_head = store.head();
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 3,
                full_generation: 3,
                is_compacted: false,
            });
            let content_root = writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("write current content root");
            let new_head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: content_root,
                    },
                    &[],
                )
                .expect("write current super-root");
            writer.finish().expect("finish current head segment");
            assert!(store.compare_and_set_head(old_head, new_head));
            store.close().expect("close head writer");
        }
        {
            let store = WritableRepository::open(&directory.path).expect("open orphan writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("write whole-archive orphan");
            writer.finish().expect("finish whole-archive orphan");
            store.close().expect("close orphan writer");
        }

        let options = scenario_options(SWEEP_SCENARIO);
        let plan = plan_compaction(&directory.path, &options).expect("plan whole-archive sweep");
        let source = plan
            .actions()
            .iter()
            .find_map(|action| match action {
                CompactionAction::RemoveReclaimableArchive { file_name, .. } => {
                    Some(file_name.clone())
                }
                _ => None,
            })
            .expect("fixture must remove a wholly reclaimable archive");
        let source_bytes =
            std::fs::read(directory.path.join(&source)).expect("read whole-removal source");
        (snapshot_repository(&directory.path), source, source_bytes)
    }

    fn create_removal_then_rewrite_fixture(
        directory: &TestDirectory,
    ) -> (RepositorySnapshot, String, String, Vec<u8>, String) {
        let (_, rewrite_source, replacement) = create_partial_sweep_fixture(directory);
        {
            let store = WritableRepository::open(&directory.path)
                .expect("open whole-removal orphan writer");
            let mut writer = store.record_writer(GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("write whole-archive orphan");
            writer.finish().expect("finish whole-archive orphan");
            store.close().expect("close whole-removal orphan writer");
        }

        let options = scenario_options(SWEEP_SCENARIO);
        let plan = plan_compaction(&directory.path, &options)
            .expect("plan removal followed by rewrite fixture");
        let removal_source = plan
            .actions()
            .iter()
            .find_map(|action| match action {
                CompactionAction::RemoveReclaimableArchive { file_name, .. } => {
                    Some(file_name.clone())
                }
                _ => None,
            })
            .expect("fixture must contain a whole-archive removal");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RewriteArchive {
                file_name,
                replacement_name,
                ..
            } if file_name == &rewrite_source && replacement_name == &replacement
        )));
        let rewrite_bytes = std::fs::read(directory.path.join(&rewrite_source))
            .expect("read rewrite source before boundary fault");
        (
            snapshot_repository(&directory.path),
            removal_source,
            rewrite_source,
            rewrite_bytes,
            replacement,
        )
    }

    fn assert_removal_then_rewrite_prefix(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        removal_source: &str,
        rewrite_source: &str,
        rewrite_bytes: &[u8],
        replacement: &str,
    ) {
        assert_exact_snapshot_reopens(directory, snapshot);
        assert_eq!(
            std::fs::read(directory.join("journal.log"))
                .expect("read journal after phase-boundary fault"),
            snapshot.journal_bytes,
            "segment sweep prefix cannot alter retained journal history"
        );
        assert!(
            !directory.join(removal_source).exists(),
            "the boundary must be reached after the whole-removal phase"
        );
        assert_eq!(
            std::fs::read(directory.join(rewrite_source))
                .expect("rewrite source remains at the phase boundary"),
            rewrite_bytes,
            "the rewrite phase must not have begun"
        );
        assert!(
            !directory.join(replacement).exists(),
            "no replacement may publish before the rewrite phase begins"
        );
    }

    fn assert_postcomp_removal_then_rewrite_prefix(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        removal_source: &str,
        rewrite_source: &str,
        rewrite_bytes: &[u8],
        replacement: &str,
    ) {
        // Post-compaction reclamation is allowed to retire older journal
        // roots before the journal itself is compacted. Its healthy-prefix
        // oracle is therefore the still-durable current head, not byte-for-
        // byte preservation of every historical root.
        let repository = Repository::open(directory)
            .expect("fresh read-only reopen after post-compaction boundary fault");
        assert_eq!(repository.head_record_identifier(), snapshot.head);
        repository
            .content_root()
            .expect("the durable current head remains traversable");
        let readable_roots = readable_journal_roots(&repository);
        assert!(
            readable_roots.contains(&snapshot.head),
            "the durable current head must remain among the readable journal roots"
        );
        drop(repository);
        drop(RepositoryLock::acquire(directory).expect("child releases repository lock"));

        assert_eq!(
            std::fs::read(directory.join("journal.log"))
                .expect("read journal after post-compaction boundary fault"),
            snapshot.journal_bytes,
            "post-compaction archive sweeping cannot rewrite the journal"
        );
        assert!(
            !directory.join(removal_source).exists(),
            "the post-compaction boundary follows the whole-removal phase"
        );
        assert_eq!(
            std::fs::read(directory.join(rewrite_source))
                .expect("post-compaction rewrite source remains at the boundary"),
            rewrite_bytes,
            "the post-compaction rewrite phase must not have begun"
        );
        assert!(
            !directory.join(replacement).exists(),
            "no post-compaction replacement may publish before its rewrite phase"
        );
    }

    #[test]
    fn removal_to_rewrite_error_and_process_crash_leave_a_healthy_prefix() {
        const CUTPOINT: &str = "sweep.removals-complete-before-rewrites";
        for crash in [false, true] {
            let name = if crash {
                "removal-rewrite-boundary-crash"
            } else {
                "removal-rewrite-boundary-error"
            };
            let directory = TestDirectory::repository(name);
            let (snapshot, removal_source, rewrite_source, rewrite_bytes, replacement) =
                create_removal_then_rewrite_fixture(&directory);

            if crash {
                run_crash_child(&directory.path, SWEEP_SCENARIO, CUTPOINT);
            } else {
                run_error_child(&directory.path, SWEEP_SCENARIO, CUTPOINT);
            }

            assert_removal_then_rewrite_prefix(
                &directory.path,
                &snapshot,
                &removal_source,
                &rewrite_source,
                &rewrite_bytes,
                &replacement,
            );
        }
    }

    #[test]
    fn postcomp_removal_to_rewrite_error_and_process_crash_leave_a_healthy_prefix() {
        const CUTPOINT: &str = "postcomp-sweep.removals-complete-before-rewrites";
        for crash in [false, true] {
            let name = if crash {
                "postcomp-removal-rewrite-boundary-crash"
            } else {
                "postcomp-removal-rewrite-boundary-error"
            };
            let directory = TestDirectory::repository(name);
            let (snapshot, removal_source, rewrite_source, rewrite_bytes, replacement) =
                create_removal_then_rewrite_fixture(&directory);

            if crash {
                run_crash_child(&directory.path, POSTCOMPACTION_SWEEP_SCENARIO, CUTPOINT);
            } else {
                run_error_child(&directory.path, POSTCOMPACTION_SWEEP_SCENARIO, CUTPOINT);
            }

            assert_postcomp_removal_then_rewrite_prefix(
                &directory.path,
                &snapshot,
                &removal_source,
                &rewrite_source,
                &rewrite_bytes,
                &replacement,
            );
        }
    }

    #[test]
    fn segment_unlink_enoent_is_reported_with_a_typed_already_absent_disposition() {
        const CUTPOINT: &str = "sweep.remove-before-source-unlink-not-found";
        let directory = TestDirectory::repository("segment-unlink-already-absent");
        let (snapshot, source, _) = create_whole_archive_sweep_fixture(&directory);

        run_absence_child(&directory.path, SWEEP_SCENARIO, CUTPOINT);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert!(
            !directory.path.join(source).exists(),
            "the simulated competing unlink must leave the planned orphan absent"
        );
    }

    #[test]
    fn published_sweep_survives_process_death_before_source_unlink() {
        let directory = TestDirectory::repository("sweep-before-unlink");
        let (snapshot, source, replacement) = create_partial_sweep_fixture(&directory);

        run_crash_child(
            &directory.path,
            SWEEP_SCENARIO,
            "sweep.published-before-source-unlink",
        );

        assert_sweep_residue(
            &directory.path,
            &snapshot,
            &source,
            &replacement,
            (true, true, true),
        );
    }

    fn assert_sweep_residue(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        source: &str,
        replacement: &str,
        expected: (bool, bool, bool),
    ) {
        let (source_exists, replacement_exists, staging_exists) = expected;
        assert_exact_snapshot_reopens(directory, snapshot);
        assert_eq!(
            std::fs::read(directory.join("journal.log")).expect("read journal after fault"),
            snapshot.journal_bytes
        );
        let source_path = directory.join(source);
        let replacement_path = directory.join(replacement);
        let staging_path = directory.join(format!("{replacement}.cleaning.000"));
        assert_eq!(source_path.exists(), source_exists, "source archive state");
        assert_eq!(
            replacement_path.exists(),
            replacement_exists,
            "replacement archive state"
        );
        assert_eq!(
            staging_path.exists(),
            staging_exists,
            "staging archive state"
        );
        if source_exists {
            TarArchiveReader::open(&source_path).expect("old source residue remains readable");
        }
        if replacement_exists {
            let swept = TarArchiveReader::open(&replacement_path)
                .expect("published replacement is never a corrupt active winner");
            assert!(!swept.is_recovered());
            assert!(swept.contains_segment(snapshot.head.segment));
        }
        if staging_exists {
            let staged = TarArchiveReader::open(&staging_path)
                .expect("ignored staging residue contains a complete validated archive");
            assert!(!staged.is_recovered());
            assert!(staged.contains_segment(snapshot.head.segment));
        }
        if replacement_exists && staging_exists {
            assert_eq!(
                std::fs::read(&replacement_path).expect("read published replacement"),
                std::fs::read(&staging_path).expect("read staging hard link"),
                "the active winner and ignored staging name must expose the same validated bytes"
            );
        }
    }

    #[test]
    fn published_sweep_survives_process_death_after_source_unlink() {
        let directory = TestDirectory::repository("sweep-after-unlink");
        let (snapshot, source, replacement) = create_partial_sweep_fixture(&directory);

        run_crash_child(&directory.path, SWEEP_SCENARIO, "sweep.source-unlinked");

        assert_sweep_residue(
            &directory.path,
            &snapshot,
            &source,
            &replacement,
            (false, true, false),
        );
    }

    #[test]
    fn sweep_syscall_failures_leave_only_an_old_or_valid_new_winner() {
        let cutpoints = [
            ("sweep.staging-write-after-create", true, false, false),
            ("sweep.staging-close-before-trailers", true, false, false),
            ("sweep.staging-before-validation-open", true, false, false),
            ("sweep.before-publish-link", true, false, true),
            ("sweep.after-publish-link", true, true, true),
            ("sweep.before-publish-directory-sync", true, true, true),
            ("sweep.after-publish-directory-sync", true, true, true),
            ("sweep.before-staging-unlink", true, true, true),
            ("sweep.after-staging-unlink", true, true, false),
            ("sweep.before-source-unlink", true, true, false),
            ("sweep.after-source-unlink", false, true, false),
        ];

        for (cutpoint, source_exists, replacement_exists, staging_exists) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, source, replacement) = create_partial_sweep_fixture(&directory);

            run_error_child(&directory.path, SWEEP_SCENARIO, cutpoint);

            assert_sweep_residue(
                &directory.path,
                &snapshot,
                &source,
                &replacement,
                (source_exists, replacement_exists, staging_exists),
            );
        }
    }

    #[test]
    fn sweep_rejects_a_staging_path_substituted_after_validation() {
        let cutpoint = "sweep.staging-validated-before-publish";
        let directory = TestDirectory::repository("sweep-staging-substitution");
        let (snapshot, source, replacement) = create_partial_sweep_fixture(&directory);
        let source_path = directory.path.join(&source);
        let source_bytes = std::fs::read(&source_path).expect("read certified source archive");

        run_substitution_child(&directory.path, SWEEP_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(&source_path).expect("source archive survives substitution"),
            source_bytes,
            "descriptor/path mismatch must be rejected before source removal"
        );
        assert!(
            !directory.path.join(&replacement).exists(),
            "a substituted staging pathname must never become an active higher archive letter"
        );
        assert_eq!(
            std::fs::read(directory.path.join(format!("{replacement}.cleaning.000")))
                .expect("substituted non-active staging residue remains"),
            b"substituted pathname\n"
        );
    }

    #[test]
    fn sweep_does_not_unlink_a_source_path_substituted_after_certification() {
        let cutpoint = "sweep.remove-before-source-identity";
        let directory = TestDirectory::repository("sweep-source-substitution");
        let (snapshot, source, source_bytes) = create_whole_archive_sweep_fixture(&directory);

        run_substitution_child(&directory.path, SWEEP_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join(format!("{source}.validated-inode")))
                .expect("certified source inode remains available"),
            source_bytes,
            "the certified source bytes must not be lost"
        );
        assert_eq!(
            std::fs::read(directory.path.join(&source))
                .expect("substituted source pathname was not unlinked"),
            b"substituted pathname\n",
            "cleanup must not unlink an inode it did not certify"
        );
    }

    #[test]
    fn prospective_sweep_refuses_an_injected_retained_root_in_a_removed_segment() {
        let cutpoint = "cleanup.before-prospective-retained-root-verification";
        let directory = TestDirectory::repository("prospective-retained-root-verification");
        let (snapshot, source, source_bytes) = create_whole_archive_sweep_fixture(&directory);

        run_substitution_child(&directory.path, SWEEP_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join(source)).expect("planned source remains"),
            source_bytes,
            "prospective validation must refuse before any archive mutation"
        );
    }

    #[test]
    fn planned_file_removal_does_not_unlink_a_substituted_staging_inode() {
        let cutpoint = "remove-planned-file.before-final-identity";
        let directory = TestDirectory::repository("planned-removal-substitution");
        let snapshot = snapshot_repository(&directory.path);
        let staged_path = directory.path.join("journal.log.cleaning.000");
        let staged_bytes = std::fs::read(directory.path.join("journal.log"))
            .expect("read canonical journal for redundant staging fixture");
        std::fs::write(&staged_path, &staged_bytes).expect("write redundant journal staging file");
        let plan = plan_compaction(&directory.path, &scenario_options(REMOVAL_SCENARIO))
            .expect("plan redundant staging removal");
        assert!(plan.actions().iter().any(|action| {
            matches!(
                action,
                CompactionAction::RemoveTemporary { file_name, .. }
                    if file_name == "journal.log.cleaning.000"
            )
        }));

        run_substitution_child(&directory.path, REMOVAL_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(&staged_path).expect("substituted staging inode was not unlinked"),
            b"substituted pathname\n"
        );
        assert_eq!(
            std::fs::read(
                directory
                    .path
                    .join("journal.log.cleaning.000.validated-inode")
            )
            .expect("the planned staging inode remains available"),
            staged_bytes
        );
    }

    #[test]
    fn stale_archive_identity_refusal_is_fatal_before_checkpoint_mutation() {
        let cutpoint = "remove-planned-file.before-final-identity";
        let directory = TestDirectory::repository("strict-stale-archive-removal");
        {
            let store = WritableRepository::open(&directory.path)
                .expect("open expired-checkpoint fixture writer");
            create_checkpoint(&store, 1, &[]).expect("create expiring checkpoint");
            store
                .close()
                .expect("close expired-checkpoint fixture writer");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::copy(
            directory.path.join("data00000a.tar"),
            directory.path.join("data00000b.tar"),
        )
        .expect("create semantically identical higher archive generation");
        let options = scenario_options(STALE_ARCHIVE_SCENARIO);
        let plan = plan_compaction(&directory.path, &options).expect("plan combined cleanup");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveStaleArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )));
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveCheckpoints { expired: 1, .. }
        )));
        let snapshot = snapshot_repository(&directory.path);
        let checkpoints_before = Repository::open(&directory.path)
            .expect("repository before strict refusal")
            .checkpoints()
            .expect("checkpoints before strict refusal")
            .len();
        assert_eq!(checkpoints_before, 1);

        run_substitution_child(&directory.path, STALE_ARCHIVE_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log"))
                .expect("journal after strict stale refusal"),
            snapshot.journal_bytes,
            "checkpoint cleanup must not append a head after stale archive identity changed"
        );
        assert_eq!(
            Repository::open(&directory.path)
                .expect("repository after strict refusal")
                .checkpoints()
                .expect("checkpoints after strict refusal")
                .len(),
            checkpoints_before,
            "stale archive refusal must precede checkpoint mutation"
        );
    }
}
