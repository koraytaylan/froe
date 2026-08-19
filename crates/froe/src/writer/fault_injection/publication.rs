//! The boundary between a verified compacted copy and its publication:
//! everything before it is additive and disposable, so a failure there must
//! leave the store it started from — same journal, same head, every source
//! archive byte-identical — plus at most some orphan output a later run
//! retires as residue.

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::writer::fault_injection::test_support::{
        COMPACTION_PUBLICATION_SCENARIO, PURGING_PUBLICATION_SCENARIO, TestDirectory,
        run_crash_child, run_error_child, write_orphaned_history_fixture,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;

    const CUTPOINT: &str = "cleanup.before-compacted-head-publication";

    /// Every file in the store with its bytes, so additivity can be proven:
    /// after a pre-publication failure each of these must still exist
    /// byte-identical, whatever else the failed run appended.
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

    fn assert_failure_was_additive_only(directory: &Path, before: &BTreeMap<OsString, Vec<u8>>) {
        let after = store_files(directory);
        for (name, bytes) in before {
            // `repo.lock` is the one file a run legitimately touches before
            // the boundary: the child held it and its content is empty
            // either way.
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
        let repository = Repository::open(directory)
            .expect("the store reopens at its original head after a pre-publication failure");
        drop(repository);
    }

    /// An injected error between the copy's verification and the head's
    /// publication refuses the run with the original store intact: the
    /// journal never names the copy, and nothing was unlinked.
    #[test]
    fn an_error_before_publication_leaves_the_original_store_intact() {
        let directory = TestDirectory::repository("publication-error");
        let before = store_files(&directory.path);

        run_error_child(&directory.path, COMPACTION_PUBLICATION_SCENARIO, CUTPOINT);

        assert_failure_was_additive_only(&directory.path, &before);
    }

    /// A process death at the same boundary leaves the same recoverable
    /// state: the copy's orphan output is on disk, and everything that was
    /// the store still is.
    #[test]
    fn a_crash_before_publication_leaves_the_original_store_intact() {
        let directory = TestDirectory::repository("publication-crash");
        let before = store_files(&directory.path);

        run_crash_child(&directory.path, COMPACTION_PUBLICATION_SCENARIO, CUTPOINT);

        assert_failure_was_additive_only(&directory.path, &before);
    }

    /// A purging run interrupted at the same boundary loses nothing: the
    /// journal still names the old head, and the history the copy was
    /// about to omit still resolves through it.
    #[test]
    fn an_interrupted_purge_leaves_every_history_at_the_old_head() {
        let directory = TestDirectory::new("publication-purge-crash");
        write_orphaned_history_fixture(&directory.path);
        let before = store_files(&directory.path);

        run_crash_child(&directory.path, PURGING_PUBLICATION_SCENARIO, CUTPOINT);

        assert_failure_was_additive_only(&directory.path, &before);
        let repository = Repository::open(&directory.path)
            .expect("the store reopens at the old head after the interrupted purge");
        assert!(
            repository
                .node_at_path("/jcr:system/jcr:versionStorage/bbbbbbbb-2222-4222-8222-222222222222")
                .expect("resolve the history")
                .is_some(),
            "an interrupted purge must not have removed anything"
        );
    }
}
