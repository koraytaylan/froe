//! Every boundary of the journal rewrite: the readable roots and the
//! byte-exact retained lines a reopen must still find.

#[cfg(test)]
mod tests {
    use crate::segment::identifier::SegmentIdentifier;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::fault_injection::test_support::{
        JOURNAL_SCENARIO, REMOVAL_SCENARIO, RepositorySnapshot, TestDirectory,
        assert_exact_snapshot_reopens, run_crash_child, run_error_child, run_substitution_child,
        snapshot_repository,
    };
    use crate::writer::store_writer::WritableRepository;
    use std::io::Write as _;
    use std::path::Path;

    #[test]
    fn final_reopen_refuses_a_missing_previously_readable_journal_root() {
        let cutpoint = "cleanup.before-final-retained-root-verification";
        let directory = TestDirectory::repository("final-retained-root-verification");
        let snapshot = snapshot_repository(&directory.path);
        let staged_path = directory.path.join("journal.log.cleaning.000");
        std::fs::write(&staged_path, &snapshot.journal_bytes)
            .expect("write redundant staging fixture");

        run_substitution_child(&directory.path, REMOVAL_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("journal after refusal"),
            snapshot.journal_bytes,
            "the in-memory verifier probe must not alter canonical journal bytes"
        );
        assert!(
            staged_path.exists(),
            "deferred deletion must remain pending when final root verification refuses"
        );
    }

    #[test]
    fn final_reopen_refuses_a_missing_byte_exact_retained_journal_line() {
        let cutpoint = "cleanup.before-final-retained-line-verification";
        let directory = TestDirectory::repository("final-retained-line-verification");
        let snapshot = snapshot_repository(&directory.path);
        let staged_path = directory.path.join("journal.log.cleaning.000");
        std::fs::write(&staged_path, &snapshot.journal_bytes)
            .expect("write redundant staging fixture");

        run_substitution_child(&directory.path, REMOVAL_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("journal after refusal"),
            snapshot.journal_bytes,
            "the in-memory verifier probe must not alter canonical journal bytes"
        );
        assert!(
            staged_path.exists(),
            "deferred deletion must remain pending when byte-exact verification refuses"
        );
    }

    fn create_journal_rewrite_fixture(directory: &TestDirectory) -> (RepositorySnapshot, Vec<u8>) {
        {
            let store = WritableRepository::open(&directory.path).expect("open journal fixture");
            create_checkpoint(&store, 60_000, &[]).expect("create second readable revision");
            store.close().expect("close journal fixture writer");
        }
        let expected_replacement =
            std::fs::read(directory.path.join("journal.log")).expect("read retained journal");
        let missing = SegmentIdentifier::new(7, 0xA000_0000_0000_0007);
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(directory.path.join("journal.log"))
                .expect("open journal for dangling line"),
            "{missing}:0 root 123"
        )
        .expect("append dangling journal line");
        let snapshot = snapshot_repository(&directory.path);
        assert_eq!(snapshot.readable_journal_roots.len(), 2);
        (snapshot, expected_replacement)
    }

    fn assert_journal_residue(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        expected_replacement: &[u8],
        cutpoint: &str,
        expected: (bool, bool, bool),
    ) {
        let (replacement_installed, temporary_exists, backup_exists) = expected;
        assert_exact_snapshot_reopens(directory, snapshot);
        let canonical = std::fs::read(directory.join("journal.log"))
            .expect("read canonical journal after fault");
        assert_eq!(
            canonical.as_slice(),
            if replacement_installed {
                expected_replacement
            } else {
                snapshot.journal_bytes.as_slice()
            },
            "canonical journal must be exactly the old or replacement byte sequence at {cutpoint}"
        );

        let temporary = directory.join("journal.log.cleaning.000");
        let backup = directory.join("journal.log.bak.000");
        assert_eq!(temporary.exists(), temporary_exists, "{cutpoint}");
        assert_eq!(backup.exists(), backup_exists, "{cutpoint}");
        if temporary_exists {
            assert_eq!(
                std::fs::read(temporary).expect("read staged journal"),
                expected_replacement,
                "staging residue must contain the exact replacement"
            );
        }
        if backup_exists {
            assert_eq!(
                std::fs::read(backup).expect("read journal backup"),
                snapshot.journal_bytes,
                "backup residue must contain the exact old journal"
            );
        }
    }

    #[test]
    fn journal_rewrite_crash_boundaries_preserve_exact_readable_roots() {
        let cutpoints = [
            ("journal.temporary-durable", false, true, false),
            ("journal.backup-file-durable", false, true, true),
            ("journal.pre-rename-directory-durable", false, true, true),
            ("journal.renamed-before-directory-sync", true, false, true),
            ("journal.rename-durable", true, false, true),
        ];

        for (cutpoint, replacement_installed, temporary_exists, backup_exists) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, expected_replacement) = create_journal_rewrite_fixture(&directory);

            run_crash_child(&directory.path, JOURNAL_SCENARIO, cutpoint);

            assert_journal_residue(
                &directory.path,
                &snapshot,
                &expected_replacement,
                cutpoint,
                (replacement_installed, temporary_exists, backup_exists),
            );
        }
    }

    #[test]
    fn journal_syscall_failures_leave_an_exact_old_or_new_canonical_file() {
        let cutpoints = [
            ("journal.temporary-durable", false, false, false),
            ("journal.backup-file-durable", false, false, false),
            (
                "journal.before-pre-rename-directory-sync",
                false,
                false,
                false,
            ),
            (
                "journal.after-pre-rename-directory-sync",
                false,
                false,
                false,
            ),
            ("journal.pre-rename-directory-durable", false, false, true),
            ("journal.before-rename", false, false, true),
            ("journal.after-rename", true, false, true),
            (
                "journal.before-post-rename-directory-sync",
                true,
                false,
                true,
            ),
            (
                "journal.after-post-rename-directory-sync",
                true,
                false,
                true,
            ),
        ];

        for (cutpoint, replacement_installed, temporary_exists, backup_exists) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, expected_replacement) = create_journal_rewrite_fixture(&directory);

            run_error_child(&directory.path, JOURNAL_SCENARIO, cutpoint);

            assert_journal_residue(
                &directory.path,
                &snapshot,
                &expected_replacement,
                cutpoint,
                (replacement_installed, temporary_exists, backup_exists),
            );
        }
    }
}
