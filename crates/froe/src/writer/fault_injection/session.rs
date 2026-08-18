//! Killing or failing a run at a session boundary, and proving the store
//! the child left behind reopens to exactly what it should.

#[cfg(test)]
mod tests {
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::fault_injection::test_support::{
        CHECKPOINT_SCENARIO, TestDirectory, archive_file_names, assert_exact_snapshot_reopens,
        run_crash_child, run_substitution_child, snapshot_repository,
    };
    use crate::writer::store_writer::WritableRepository;

    #[test]
    fn checkpoint_tar_is_readable_when_process_dies_before_journal_append() {
        let directory = TestDirectory::repository("checkpoint-before-journal");
        {
            let store = WritableRepository::open(&directory.path).expect("open checkpoint writer");
            create_checkpoint(&store, 60_000, &[]).expect("create retained checkpoint fixture");
            store.close().expect("close checkpoint writer");
        }
        let snapshot = snapshot_repository(&directory.path);
        assert_eq!(snapshot.readable_journal_roots.len(), 2);
        let archives_before = archive_file_names(&directory.path);

        run_crash_child(
            &directory.path,
            CHECKPOINT_SCENARIO,
            "checkpoint.tar-durable-before-journal",
        );

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("read journal after crash"),
            snapshot.journal_bytes,
            "the old journal must be byte-exact before the head append begins"
        );
        let archives_after = archive_file_names(&directory.path);
        assert_eq!(archives_after.len(), archives_before.len() + 1);
        let new_archives: Vec<_> = archives_after
            .iter()
            .filter(|name| !archives_before.contains(name))
            .collect();
        assert_eq!(new_archives.len(), 1);
        let orphan = TarArchiveReader::open(&directory.path.join(new_archives[0]))
            .expect("pre-journal checkpoint TAR residue is fully finalized and readable");
        assert!(!orphan.is_recovered());
    }

    #[test]
    fn checkpoint_rejects_a_session_tar_substituted_before_journal_append() {
        let cutpoint = "checkpoint.tar-durable-before-journal";
        let directory = TestDirectory::repository("checkpoint-tar-substitution");
        {
            let store = WritableRepository::open(&directory.path).expect("open checkpoint writer");
            create_checkpoint(&store, 60_000, &[])
                .expect("create deterministically unreferenced checkpoint");
            store.close().expect("close checkpoint writer");
        }
        let snapshot = snapshot_repository(&directory.path);
        let archives_before = archive_file_names(&directory.path);

        run_substitution_child(&directory.path, CHECKPOINT_SCENARIO, cutpoint);

        assert_exact_snapshot_reopens(&directory.path, &snapshot);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log"))
                .expect("journal survives session TAR substitution"),
            snapshot.journal_bytes,
            "a substituted checkpoint TAR must be rejected before its head is journal-visible"
        );
        let new_archives: Vec<_> = archive_file_names(&directory.path)
            .into_iter()
            .filter(|name| !archives_before.contains(name))
            .collect();
        assert_eq!(new_archives.len(), 1);
        assert_eq!(
            std::fs::read(directory.path.join(&new_archives[0]))
                .expect("substituted active path remains for diagnosis"),
            b"substituted pathname\n"
        );
        let retained = directory
            .path
            .join(format!("{}.validated-inode", new_archives[0]));
        let validated = TarArchiveReader::open(&retained)
            .expect("descriptor-certified session inode remains readable");
        assert!(!validated.is_recovered());
    }
}
