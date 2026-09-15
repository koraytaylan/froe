//! The native Lucene reindex's durability boundaries.
//!
//! Two of the three are **outside the store**, which is what this arm adds
//! to plan 0007's: the segment is assembled in the run's own subdirectory
//! under the work directory, and not a byte of it reaches `:data` until the
//! copy. So a fault before that copy leaves the store it started from,
//! exactly, plus whatever the run left in its subdirectory — which a
//! returned error removes and a dead process does not.
//!
//! The third is inside the copy, where an exhausted filesystem stops one.
//! Its claim is the import's: records appended for the bytes already read,
//! and not one byte of the store changed.
//!
//! The publication and verification boundaries are `apply.rs`'s, armed by
//! task 0708 under the `index-reindex.` prefix; the wiring test in
//! `writer/index/apply.rs` proves a Lucene selection reaches them.

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::writer::fault_injection::lucene_fixture::{
        LUCENE_REINDEX_SCENARIO, write_lucene_reindex_fixture,
    };
    use crate::writer::fault_injection::test_support::{
        TestDirectory, reindex_work_directory, run_crash_child, run_error_child,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;

    const AFTER_LAST_DOCUMENT: &str = "lucene-reindex.after-last-document";
    const AFTER_SEGMENT_FINISHED: &str = "lucene-reindex.after-segment-finished";
    const MID_FILE_COPY: &str = "lucene-reindex.mid-file-copy";

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

    /// The digest lines under `path`.
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

    /// The original store, byte for byte, plus at most new files — and the
    /// definition exactly as the run found it.
    fn assert_the_original_store_survived(
        store: &Path,
        before: &BTreeMap<OsString, Vec<u8>>,
        definition_before: &[String],
    ) {
        let after = store_files(store);
        for (name, bytes) in before {
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
            "the definition must be exactly as the run found it"
        );
    }

    /// What the run left in its own subdirectory, by name.
    fn work_leftovers(store: &Path) -> Vec<OsString> {
        let work = reindex_work_directory(store);
        let Ok(entries) = std::fs::read_dir(&work) else {
            return Vec::new();
        };
        entries
            .map(|entry| entry.expect("entry").file_name())
            .collect()
    }

    /// Every file under the run's subdirectories, so a case can say whether
    /// a whole segment was left behind.
    fn work_files(store: &Path) -> Vec<String> {
        fn walk(directory: &Path, found: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(directory) else {
                return;
            };
            for entry in entries {
                let entry = entry.expect("entry");
                if entry.file_type().expect("file type").is_dir() {
                    walk(&entry.path(), found);
                } else {
                    found.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        let mut found = Vec::new();
        walk(&reindex_work_directory(store), &mut found);
        found.sort();
        found
    }

    /// The retry after a dead run: the plan refuses the residue an
    /// operator-named work directory holds, and once it is cleared the
    /// retry publishes the post-state the interrupted run would have.
    ///
    /// The refusal is plan 0007's rule, and it is what keeps a dead run's
    /// complete segment from being mistaken for a live one's.
    fn assert_the_retry_refuses_the_residue_then_rebuilds(store: &Path) {
        use crate::index::lucene::documents::binaries::{BinaryTextFallback, BinaryTextPolicy};
        use crate::writer::index::{ReindexOptions, WorkDirectory, plan_reindex, reindex};

        let work = reindex_work_directory(store);
        let residue: Vec<OsString> = std::fs::read_dir(&work)
            .expect("read the work directory")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert!(
            !residue.is_empty(),
            "the death left no run subdirectory to retry against"
        );

        let options = ReindexOptions::new()
            .with_work_directory(WorkDirectory::OperatorNamed(work.clone()))
            .with_binary_text_policy(BinaryTextPolicy::new(BinaryTextFallback::Marker))
            .with_sort_budget_bytes(1024);
        let error = plan_reindex(store, &options)
            .expect_err("an operator-named directory holding residue must be refused");
        assert!(
            error
                .to_string()
                .contains("left over from an earlier froe reindex"),
            "the refusal names what it found: {error}"
        );

        for name in residue {
            std::fs::remove_dir_all(work.join(name)).expect("clear the residue");
        }
        let outcome = reindex(store, options).expect("the retry runs");
        assert!(outcome.moved_the_head(), "the retry published nothing");
        assert!(
            !digest_under(store, "/oak:index/lucene/:data").is_empty(),
            "the retry did not rebuild the index"
        );
    }

    /// An injected error once the documents are made and the runs have
    /// spilled leaves the store untouched and takes the subdirectory with
    /// it.
    #[test]
    fn an_error_after_the_last_document_leaves_the_store_unchanged() {
        let directory = TestDirectory::new("lucene-reindex-documents-error");
        let store = write_lucene_reindex_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");

        run_error_child(&store, LUCENE_REINDEX_SCENARIO, AFTER_LAST_DOCUMENT);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert!(
            work_leftovers(&store).is_empty(),
            "a returned error removes the run's subdirectory: {:?}",
            work_leftovers(&store)
        );
    }

    /// A death at the same boundary leaves the spill files and the partial
    /// stored-field file behind, outside the store.
    #[test]
    fn a_death_after_the_last_document_leaves_files_outside_the_store() {
        let directory = TestDirectory::new("lucene-reindex-documents-crash");
        let store = write_lucene_reindex_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");

        run_crash_child(&store, LUCENE_REINDEX_SCENARIO, AFTER_LAST_DOCUMENT);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert!(
            !work_files(&store).is_empty(),
            "the dead run left nothing behind, so the claim about where it left it is untested"
        );
        assert_the_retry_refuses_the_residue_then_rebuilds(&store);
    }

    /// An injected error once the segment is complete and before the first
    /// `:data` record leaves the store untouched.
    #[test]
    fn an_error_after_the_segment_is_finished_leaves_the_store_unchanged() {
        let directory = TestDirectory::new("lucene-reindex-segment-error");
        let store = write_lucene_reindex_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");

        run_error_child(&store, LUCENE_REINDEX_SCENARIO, AFTER_SEGMENT_FINISHED);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert!(work_leftovers(&store).is_empty());
    }

    /// A death there leaves a **complete** segment in the run's
    /// subdirectory — and the next attempt assembles into a fresh one, so
    /// it is never mistaken for this attempt's.
    #[test]
    fn a_death_after_the_segment_is_finished_leaves_a_whole_segment_outside_the_store() {
        let directory = TestDirectory::new("lucene-reindex-segment-crash");
        let store = write_lucene_reindex_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");

        run_crash_child(&store, LUCENE_REINDEX_SCENARIO, AFTER_SEGMENT_FINISHED);

        assert_the_original_store_survived(&store, &before, &definition_before);
        let left = work_files(&store);
        assert!(
            left.iter().any(|name| name == "segments_1"),
            "a complete segment must be what the death left: {left:?}"
        );
        assert_the_retry_refuses_the_residue_then_rebuilds(&store);
    }

    /// A failure inside the copy appends records for the bytes already
    /// read and changes no byte of the store.
    #[test]
    fn an_error_in_the_middle_of_the_copy_changes_no_byte_of_the_store() {
        let directory = TestDirectory::new("lucene-reindex-copy-error");
        let store = write_lucene_reindex_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");

        run_error_child(&store, LUCENE_REINDEX_SCENARIO, MID_FILE_COPY);

        assert_the_original_store_survived(&store, &before, &definition_before);
    }

    /// And a death there leaves the same prefix, the records being
    /// unreachable from the head either way.
    #[test]
    fn a_death_in_the_middle_of_the_copy_leaves_the_head_where_it_was() {
        let directory = TestDirectory::new("lucene-reindex-copy-crash");
        let store = write_lucene_reindex_fixture(&directory.path);
        let before = store_files(&store);
        let definition_before = digest_under(&store, "/oak:index/lucene");

        run_crash_child(&store, LUCENE_REINDEX_SCENARIO, MID_FILE_COPY);

        assert_the_original_store_survived(&store, &before, &definition_before);
        assert_the_retry_refuses_the_residue_then_rebuilds(&store);
    }
}
