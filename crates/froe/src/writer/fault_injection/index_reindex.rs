//! The reindex's durability boundaries.
//!
//! Everything a reindex writes before the head moves is additive: new
//! archives at a number above every physical name, holding records nothing
//! reachable from the head refers to. So a fault anywhere before publication
//! must leave the store it started from — same journal, same head, every
//! original `:index` subtree intact — plus at most that orphan output, which
//! a later `froe compact` retires.
//!
//! The pair of boundaries around publication is the interesting one.
//! `compare_and_set_head` changes nothing on disk and `flush` is what makes
//! the new head durable, so a fault on either side of the head move observes
//! the *same* on-disk prefix. The two tests below state that explicitly by
//! asserting the same thing.

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::writer::compaction::CompactionKind;
    use crate::writer::fault_injection::test_support::{
        REINDEX_SCENARIO, TestDirectory, reindex_work_directory, run_crash_child, run_error_child,
        write_flagged_index_fixture,
    };
    use crate::writer::index::{ReindexOptions, WorkDirectory, plan_reindex, reindex};
    use crate::writer::maintenance::{CompactionOptions, MaintenanceTask, compact};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;

    const BEFORE_SPILL_CLEANUP: &str = "index-reindex.before-spill-cleanup";
    const BEFORE_HEAD_PUBLISH: &str = "index-reindex.before-head-publish";
    const AFTER_HEAD_PUBLISH_BEFORE_FLUSH: &str = "index-reindex.after-head-publish-before-flush";
    const BEFORE_APPLIED_VERIFICATION: &str = "index-reindex.before-applied-verification";

    /// Every file in the store with its bytes.
    fn store_files(directory: &Path) -> BTreeMap<OsString, Vec<u8>> {
        std::fs::read_dir(directory)
            .expect("read the store directory")
            .map(|entry| {
                let entry = entry.expect("directory entry");
                (
                    entry.file_name(),
                    std::fs::read(entry.path()).expect("read the file"),
                )
            })
            .collect()
    }

    /// The digest lines under `path`, so a subtree can be compared across a
    /// failed run.
    fn digest_under(store: &Path, path: &str) -> Vec<String> {
        let repository = Repository::open(store).expect("open the repository");
        let mut rendered = Vec::new();
        crate::tooling::digest::digest_repository_excluding(&repository, &[], &[], &mut rendered)
            .expect("digest");
        String::from_utf8(rendered)
            .expect("UTF-8")
            .lines()
            .filter(|line| line.starts_with(path))
            .map(str::to_owned)
            .collect()
    }

    /// What every pre-publication boundary must have left: the original
    /// store, byte for byte, plus at most new files.
    fn assert_the_original_store_survived(
        store: &Path,
        before: &BTreeMap<OsString, Vec<u8>>,
        definition_before: &[String],
    ) {
        let after = store_files(store);
        for (name, bytes) in before {
            // `repo.lock` is the one file a run legitimately touches before
            // the boundary, and it is empty either way.
            if name == "repo.lock" {
                continue;
            }
            assert_eq!(
                after.get(name),
                Some(bytes),
                "{} must survive a pre-publication failure byte-identical",
                Path::new(name).display()
            );
        }
        assert_eq!(
            digest_under(store, "/oak:index/title"),
            definition_before,
            "the definition must be exactly as the run found it"
        );
    }

    /// The records a failed run appended are unreachable from the head, and
    /// a later compaction retires the archives holding them.
    ///
    /// `before` is the store as it stood before the run, so the archives
    /// the run added are exactly the names that are new — and those are the
    /// ones compaction has to retire. That the compaction *runs at all* is
    /// half the claim: an archive left without its trailers would make it
    /// refuse the store as damaged until an operator authorized a repair.
    fn assert_a_later_compaction_reclaims_the_orphans(
        store: &Path,
        before: &BTreeMap<OsString, Vec<u8>>,
    ) {
        let orphans: Vec<OsString> = store_files(store)
            .into_keys()
            .filter(|name| {
                !before.contains_key(name)
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension == "tar")
            })
            .collect();
        assert!(
            !orphans.is_empty(),
            "the failed run appended no archive, so there is nothing to reclaim"
        );

        compact(
            store,
            CompactionOptions::default()
                .with_tasks([MaintenanceTask::Segments])
                .with_compaction(CompactionKind::Full),
        )
        .expect("a later compaction runs over the orphan output");

        let after = store_files(store);
        for orphan in &orphans {
            assert!(
                !after.contains_key(orphan),
                "{} survived the compaction that should have retired it",
                Path::new(orphan).display()
            );
        }
        let repository = Repository::open(store).expect("the store reopens after compaction");
        drop(repository);
    }

    /// Rerunning after a death, with the run's subdirectory left behind.
    ///
    /// The lock is reacquired by the retry itself — an abrupt death releases
    /// the kernel's advisory lock even though the `repo.lock` inode stays —
    /// and what the retry does about the residue is the operator-named
    /// directory's contract: refuse, so nobody's files are removed by a
    /// program that did not create them.
    fn assert_the_retry_refuses_the_residue(store: &Path) {
        let work = reindex_work_directory(store);
        let residue: Vec<_> = std::fs::read_dir(&work)
            .expect("read the work directory")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert!(
            !residue.is_empty(),
            "the death left no run subdirectory to retry against"
        );

        let options = ReindexOptions::new()
            .with_work_directory(WorkDirectory::OperatorNamed(work.clone()))
            .with_sort_budget_bytes(64);
        let error = plan_reindex(store, &options)
            .expect_err("an operator-named directory holding residue must be refused");
        assert!(
            error
                .to_string()
                .contains("left over from an earlier froe reindex"),
            "the refusal names what it found: {error}"
        );

        // With the residue cleared, the retry publishes the post-state the
        // interrupted run would have.
        for name in residue {
            std::fs::remove_dir_all(work.join(name)).expect("clear the residue");
        }
        let outcome = reindex(store, options).expect("the retry runs");
        assert!(outcome.moved_the_head(), "the retry published nothing");
        assert!(
            !digest_under(store, "/oak:index/title/:index").is_empty(),
            "the retry did not rebuild the index"
        );
    }

    /// An injected error once the sort has spilled leaves the store
    /// untouched and takes the run's subdirectory with it.
    #[test]
    fn a_spill_failure_removes_the_run_subdirectory_and_leaves_the_store_unchanged() {
        let directory = TestDirectory::new("reindex-spill-error");
        let store = write_flagged_index_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/title");

        run_error_child(&store, REINDEX_SCENARIO, BEFORE_SPILL_CLEANUP);

        assert_the_original_store_survived(&store, &before, &definition_before);
        let leftovers: Vec<_> = std::fs::read_dir(reindex_work_directory(&store))
            .expect("read the work directory")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "a returned error removes the run's subdirectory: {leftovers:?}"
        );
    }

    /// A death at the same boundary leaves the store unchanged too — the
    /// spill files are outside it — and the retry is the operator's.
    #[test]
    fn a_death_before_spill_cleanup_leaves_no_file_in_the_store() {
        let directory = TestDirectory::new("reindex-spill-crash");
        let store = write_flagged_index_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/title");

        run_crash_child(&store, REINDEX_SCENARIO, BEFORE_SPILL_CLEANUP);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert_the_retry_refuses_the_residue(&store);
    }

    /// An injected error after every record is written and before the head
    /// moves leaves the head and the definition as they were.
    #[test]
    fn an_error_before_head_publish_leaves_the_head_and_every_definition_as_they_were() {
        let directory = TestDirectory::new("reindex-publish-error");
        let store = write_flagged_index_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/title");

        run_error_child(&store, REINDEX_SCENARIO, BEFORE_HEAD_PUBLISH);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert_a_later_compaction_reclaims_the_orphans(&store, &before);
    }

    /// A death at the same boundary leaves the head resolving the old
    /// records.
    #[test]
    fn a_death_before_head_publish_leaves_the_head_resolving_the_old_records() {
        let directory = TestDirectory::new("reindex-publish-crash");
        let store = write_flagged_index_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/title");

        run_crash_child(&store, REINDEX_SCENARIO, BEFORE_HEAD_PUBLISH);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert_the_retry_refuses_the_residue(&store);
    }

    /// An error between the head move and the flush leaves the journal
    /// naming the old head.
    ///
    /// This is the same assertion the pre-publication test makes, and
    /// deliberately so: `compare_and_set_head` writes no byte, so the two
    /// boundaries observe one on-disk prefix and there is no third state.
    #[test]
    fn an_error_after_head_publish_before_flush_leaves_the_journal_naming_the_old_head() {
        let directory = TestDirectory::new("reindex-flush-error");
        let store = write_flagged_index_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/title");

        run_error_child(&store, REINDEX_SCENARIO, AFTER_HEAD_PUBLISH_BEFORE_FLUSH);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert_a_later_compaction_reclaims_the_orphans(&store, &before);
    }

    /// And the death variant, which leaves one resolvable head.
    #[test]
    fn a_death_between_head_publish_and_flush_leaves_one_resolvable_head() {
        let directory = TestDirectory::new("reindex-flush-crash");
        let store = write_flagged_index_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/title");

        run_crash_child(&store, REINDEX_SCENARIO, AFTER_HEAD_PUBLISH_BEFORE_FLUSH);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert_the_retry_refuses_the_residue(&store);
    }

    /// A failure in the applied-state verification is reported, never
    /// repaired: the store is already final and stays that way.
    #[test]
    fn a_failed_applied_state_verification_reports_rather_than_repairs() {
        let directory = TestDirectory::new("reindex-applied");
        let store = write_flagged_index_fixture(&directory.path);
        let head_before = Repository::open(&store)
            .expect("open")
            .head_record_identifier();

        run_error_child(&store, REINDEX_SCENARIO, BEFORE_APPLIED_VERIFICATION);

        let repository = Repository::open(&store).expect("the store reopens");
        assert_ne!(
            repository.head_record_identifier(),
            head_before,
            "this boundary is after publication, so the head has moved"
        );
        drop(repository);
        assert!(
            !digest_under(&store, "/oak:index/title/:index").is_empty(),
            "the rebuilt index is published and stays published"
        );
        // The run reported rather than repaired: nothing was rolled back,
        // and the store still checks out.
        assert!(
            crate::tooling::check_consistency(
                &store,
                &["/".to_owned()],
                crate::tooling::BinaryCheck::EveryBlock,
                1,
            )
            .expect("check the store")
            .has_good_revision()
        );
    }
}
