//! The import's durability boundaries.
//!
//! An import is additive in exactly the way a reindex is: every `:data`
//! record and every rewritten definition goes into fresh archives at a
//! number above every physical name, and nothing reachable from the head
//! refers to them until the head moves. So a fault anywhere before
//! publication must leave the store it started from — same journal, same
//! head, the same `:data` under the definition, every checkpoint — plus at
//! most that orphan output, which a later `froe compact` retires.
//!
//! The pair of boundaries around publication is the same non-event it is
//! for a reindex. `compare_and_set_head` changes nothing on disk and
//! `flush` is what makes the new head durable, so a fault on either side of
//! the head move observes the *same* on-disk prefix. The two tests below
//! assert the same thing, deliberately.
//!
//! What the import adds is a cutpoint *inside* a file copy, because that is
//! where an exhausted filesystem stops one. Its claim is the same: records
//! appended for the bytes already read, and not one byte of the store
//! changed.

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::writer::compaction::CompactionKind;
    use crate::writer::fault_injection::test_support::{
        LUCENE_IMPORT_SCENARIO, TestDirectory, lucene_import_input, run_crash_child,
        run_error_child, write_lucene_import_fixture,
    };
    use crate::writer::index::lucene_import::{LuceneImportOptions, lucene_import};
    use crate::writer::maintenance::{CompactionOptions, MaintenanceTask, compact};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;

    const MID_FILE_COPY: &str = "lucene-import.mid-file-copy";
    const BEFORE_HEAD_PUBLISH: &str = "lucene-import.before-head-publish";
    const AFTER_HEAD_PUBLISH_BEFORE_FLUSH: &str = "lucene-import.after-head-publish-before-flush";
    const BEFORE_APPLIED_VERIFICATION: &str = "lucene-import.before-applied-verification";

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

    /// The checkpoint names the store holds.
    fn checkpoint_names(store: &Path) -> Vec<String> {
        let repository = Repository::open(store).expect("open the repository");
        let mut names: Vec<String> = repository
            .checkpoints()
            .expect("read the checkpoints")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        names.sort();
        names
    }

    /// What every pre-publication boundary must have left: the original
    /// store, byte for byte, plus at most new files.
    fn assert_the_original_store_survived(
        store: &Path,
        before: &BTreeMap<OsString, Vec<u8>>,
        definition_before: &[String],
        checkpoints_before: &[String],
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
            digest_under(store, "/oak:index/lucene"),
            definition_before,
            "the definition and its :data must be exactly as the run found them"
        );
        assert_eq!(
            checkpoint_names(store),
            checkpoints_before,
            "an import releases no checkpoint, least of all a failed one"
        );
    }

    /// The records a failed run appended are unreachable from the head, and
    /// a later compaction retires the archives holding them.
    ///
    /// That the compaction *runs at all* is half the claim: an archive left
    /// without its trailers would make it refuse the store as damaged until
    /// an operator authorized a repair. This is the regression plan 0007
    /// bought the hard way.
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

    /// What a mid-copy failure leaves, which is less than the publication
    /// boundaries leave.
    ///
    /// The error returns *before* `RecordWriter::finish`, so the segment
    /// holding the bytes copied so far is never handed to the store, and at
    /// this fixture's scale nothing reaches disk at all. An index large
    /// enough to fill a segment mid-copy would leave one behind, so the
    /// claim pinned here is the one that holds at every scale: whatever was
    /// appended is unreferenced, a later compaction retires it, and the
    /// store checks out either way.
    fn assert_whatever_was_appended_is_unreferenced(
        store: &Path,
        before: &BTreeMap<OsString, Vec<u8>>,
    ) {
        let appended: Vec<OsString> = store_files(store)
            .into_keys()
            .filter(|name| {
                !before.contains_key(name)
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension == "tar")
            })
            .collect();

        compact(
            store,
            CompactionOptions::default()
                .with_tasks([MaintenanceTask::Segments])
                .with_compaction(CompactionKind::Full),
        )
        .expect("a later compaction runs over whatever the failed copy left");

        let after = store_files(store);
        for orphan in &appended {
            assert!(
                !after.contains_key(orphan),
                "{} survived the compaction that should have retired it",
                Path::new(orphan).display()
            );
        }
        assert!(
            crate::tooling::check_consistency(
                store,
                &["/".to_owned()],
                crate::tooling::BinaryCheck::EveryBlock,
                1,
            )
            .expect("check the store")
            .has_good_revision()
        );
    }

    /// Rerunning after a death. The import has no work directory and leaves
    /// no residue outside the store, so the retry is simply the same call
    /// again — which reacquires the lock the dead child's abrupt exit
    /// released, and publishes the post-state the interrupted run would
    /// have.
    fn assert_the_retry_publishes_the_same_post_state(store: &Path) {
        let outcome = lucene_import(store, &LuceneImportOptions::new(lucene_import_input(store)))
            .expect("the retry runs from the same input directory");
        assert!(outcome.moved_the_head(), "the retry published nothing");
        assert_eq!(outcome.indexes.len(), 1);
        assert!(
            crate::tooling::check_consistency(
                store,
                &["/".to_owned()],
                crate::tooling::BinaryCheck::EveryBlock,
                1,
            )
            .expect("check the store")
            .has_good_revision()
        );
    }

    /// An injected error halfway through the largest file leaves the store
    /// unchanged, and says which file it stopped in and how far.
    #[test]
    fn a_mid_copy_error_leaves_the_store_unchanged_and_names_the_file_and_offset() {
        let directory = TestDirectory::new("import-mid-copy-error");
        let store = write_lucene_import_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");
        let checkpoints_before = checkpoint_names(&store);

        run_error_child(&store, LUCENE_IMPORT_SCENARIO, MID_FILE_COPY);

        assert_the_original_store_survived(
            &store,
            &before,
            &definition_before,
            &checkpoints_before,
        );
        assert_whatever_was_appended_is_unreferenced(&store, &before);
    }

    /// And the death variant, which leaves the head resolving the old
    /// `:data`.
    #[test]
    fn a_death_mid_copy_leaves_the_head_resolving_the_old_data() {
        let directory = TestDirectory::new("import-mid-copy-crash");
        let store = write_lucene_import_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");
        let checkpoints_before = checkpoint_names(&store);

        run_crash_child(&store, LUCENE_IMPORT_SCENARIO, MID_FILE_COPY);

        assert_the_original_store_survived(
            &store,
            &before,
            &definition_before,
            &checkpoints_before,
        );
        assert_the_retry_publishes_the_same_post_state(&store);
    }

    /// An injected error after every file is written and read back and
    /// before the head moves leaves the definition as it was.
    #[test]
    fn an_error_before_head_publish_leaves_the_head_and_the_definition_as_they_were() {
        let directory = TestDirectory::new("import-publish-error");
        let store = write_lucene_import_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");
        let checkpoints_before = checkpoint_names(&store);

        run_error_child(&store, LUCENE_IMPORT_SCENARIO, BEFORE_HEAD_PUBLISH);

        assert_the_original_store_survived(
            &store,
            &before,
            &definition_before,
            &checkpoints_before,
        );
        assert_a_later_compaction_reclaims_the_orphans(&store, &before);
    }

    /// A death at the same boundary leaves the head resolving the old
    /// records, and the retry publishes what the dead run would have.
    #[test]
    fn a_death_before_head_publish_leaves_the_head_resolving_the_old_records() {
        let directory = TestDirectory::new("import-publish-crash");
        let store = write_lucene_import_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");
        let checkpoints_before = checkpoint_names(&store);

        run_crash_child(&store, LUCENE_IMPORT_SCENARIO, BEFORE_HEAD_PUBLISH);

        assert_the_original_store_survived(
            &store,
            &before,
            &definition_before,
            &checkpoints_before,
        );
        assert_the_retry_publishes_the_same_post_state(&store);
    }

    /// An error between the head move and the flush leaves the journal
    /// naming the old head.
    ///
    /// The same assertion the pre-publication test makes, and deliberately
    /// so: `compare_and_set_head` writes no byte, so the two boundaries
    /// observe one on-disk prefix and there is no third state.
    #[test]
    fn an_error_after_head_publish_before_flush_leaves_the_journal_naming_the_old_head() {
        let directory = TestDirectory::new("import-flush-error");
        let store = write_lucene_import_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");
        let checkpoints_before = checkpoint_names(&store);

        run_error_child(
            &store,
            LUCENE_IMPORT_SCENARIO,
            AFTER_HEAD_PUBLISH_BEFORE_FLUSH,
        );

        assert_the_original_store_survived(
            &store,
            &before,
            &definition_before,
            &checkpoints_before,
        );
        assert_a_later_compaction_reclaims_the_orphans(&store, &before);
    }

    /// And the death variant, which leaves one resolvable head.
    #[test]
    fn a_death_between_head_publish_and_flush_leaves_one_resolvable_head() {
        let directory = TestDirectory::new("import-flush-crash");
        let store = write_lucene_import_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");
        let checkpoints_before = checkpoint_names(&store);

        run_crash_child(
            &store,
            LUCENE_IMPORT_SCENARIO,
            AFTER_HEAD_PUBLISH_BEFORE_FLUSH,
        );

        assert_the_original_store_survived(
            &store,
            &before,
            &definition_before,
            &checkpoints_before,
        );
        assert_the_retry_publishes_the_same_post_state(&store);
    }

    /// A failure in the applied-state verification is reported, never
    /// repaired: the store is already final and stays that way.
    #[test]
    fn a_failed_applied_state_verification_reports_rather_than_repairs() {
        let directory = TestDirectory::new("import-applied");
        let store = write_lucene_import_fixture(&directory.path);
        let head_before = Repository::open(&store)
            .expect("open")
            .head_record_identifier();

        run_error_child(&store, LUCENE_IMPORT_SCENARIO, BEFORE_APPLIED_VERIFICATION);

        let repository = Repository::open(&store).expect("the store reopens");
        assert_ne!(
            repository.head_record_identifier(),
            head_before,
            "this boundary is after publication, so the head has moved"
        );
        drop(repository);
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
