//! Claiming the output file exclusively and proving it is still the one
//! this export created, so a swap mid-run neither modifies nor deletes
//! someone else's file.

use super::{Path, PathBuf};

/// Where `SQLite` should open the database. On Unix, the guard file's
/// own descriptor: reopening `/proc/self/fd/N` (Linux) or `/dev/fd/N`
/// yields a new descriptor for the same inode, so what `SQLite` writes
/// is anchored to the file the export created, no matter how the
/// pathname is renamed or swapped meanwhile. Elsewhere the pathname
/// itself, which the guard's share mode keeps pinned (Windows) — see
/// [`crate::create_export_output`].
#[cfg(target_os = "linux")]
pub(crate) fn open_target(path: &Path, guard: &std::fs::File) -> PathBuf {
    use std::os::unix::io::AsRawFd;
    let _ = path;
    PathBuf::from(format!("/proc/self/fd/{}", guard.as_raw_fd()))
}

/// The `/dev/fd` variant of [`open_target`] for non-Linux Unixes.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn open_target(path: &Path, guard: &std::fs::File) -> PathBuf {
    use std::os::unix::io::AsRawFd;
    let _ = path;
    PathBuf::from(format!("/dev/fd/{}", guard.as_raw_fd()))
}

/// The pathname variant of [`open_target`]: the guard created by
/// [`crate::create_export_output`] denies delete-share on Windows, so
/// the pathname cannot be repointed while the sink holds the file.
#[cfg(not(unix))]
pub(crate) fn open_target(path: &Path, guard: &std::fs::File) -> PathBuf {
    let _ = guard;
    path.to_path_buf()
}

/// Whether `path` still names `file`: same device and inode. A vanished
/// or unreadable path counts as "not ours".
#[cfg(unix)]
pub(crate) fn path_still_names(path: &Path, file: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(created), Ok(named)) = (file.metadata(), std::fs::metadata(path)) else {
        return false;
    };
    created.dev() == named.dev() && created.ino() == named.ino()
}

/// Whether `path` still names `file`. On Windows the guard's missing
/// delete-share makes this trivially true for as long as the sink holds
/// the file; the metadata comparison is a sanity read. Other non-Unix
/// platforms get a best-effort size comparison — the export's identity
/// guarantee is Unix- and Windows-grade.
#[cfg(not(unix))]
pub(crate) fn path_still_names(path: &Path, file: &std::fs::File) -> bool {
    let (Ok(created), Ok(named)) = (file.metadata(), std::fs::metadata(path)) else {
        return false;
    };
    named.is_file() && named.len() == created.len()
}

#[cfg(test)]
mod tests {
    use crate::export::export_subtree;
    use crate::sqlite::test_support::{TestDirectory, populate};
    use crate::sqlite::{SqliteExportOptions, SqliteSink};
    use froe::store::Repository;
    use rusqlite::Connection;

    #[test]
    fn the_export_never_overwrites_an_existing_file() {
        let directory = TestDirectory::new("never-overwrites");
        populate(&directory.store());
        let database = directory.database();
        std::fs::write(&database, b"someone else's data").expect("seed");

        let result = SqliteSink::create(
            &directory.store(),
            &database,
            SqliteExportOptions::default(),
        );
        assert!(result.is_err(), "an existing file must be refused");
        assert_eq!(
            std::fs::read(&database).expect("read"),
            b"someone else's data",
            "the existing file must be untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_identity_check_detects_a_replaced_file() {
        let directory = TestDirectory::new("identity");
        let first = directory.path.join("first.db");
        let second = directory.path.join("second.db");
        let guard = std::fs::File::create_new(&first).expect("create first");
        std::fs::File::create_new(&second).expect("create second");

        assert!(
            crate::sqlite::path_still_names(&first, &guard),
            "the created file is named by its path"
        );
        assert!(
            !crate::sqlite::path_still_names(&second, &guard),
            "a different file at the path must be detected"
        );
    }

    /// The ABA attack from the design review: the exclusively created
    /// file is renamed away, a victim takes the pathname, and the
    /// export runs to its (failing) end. The victim must be neither
    /// modified nor deleted, and our file must have received the data
    /// through the descriptor.
    #[cfg(unix)]
    #[test]
    fn a_mid_export_pathname_swap_neither_modifies_nor_deletes_the_replacement() {
        let directory = TestDirectory::new("aba");
        populate(&directory.store());
        let database = directory.database();
        let aside = directory.path.join("aside.db");
        let victim = directory.path.join("victim.db");

        let repository = Repository::open(&directory.store()).expect("open");
        let mut sink = SqliteSink::create(
            &directory.store(),
            &database,
            SqliteExportOptions::default(),
        )
        .expect("sink");

        // The swap: our freshly created file aside, a victim at the
        // pathname.
        std::fs::rename(&database, &aside).expect("aside");
        std::fs::write(&victim, b"precious original content").expect("victim seed");
        std::fs::rename(&victim, &database).expect("victim into place");

        let result = export_subtree(&repository, "/", None, &mut sink);
        assert!(result.is_err(), "finish must report the displacement");
        assert_eq!(
            std::fs::read(&database).expect("victim read"),
            b"precious original content",
            "the replacement must be unmodified"
        );
        drop(sink);
        assert_eq!(
            std::fs::read(&database).expect("victim read"),
            b"precious original content",
            "and must not be deleted by cleanup either"
        );

        // Our file kept receiving the export through the descriptor.
        let connection = Connection::open(&aside).expect("aside");
        let nodes: i64 = connection
            .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
            .expect("nodes");
        assert_eq!(nodes, 3, "the export wrote into the file it created");
    }

    #[test]
    fn an_abandoned_export_leaves_no_file_behind() {
        let directory = TestDirectory::new("abandoned");
        populate(&directory.store());
        let database = directory.database();
        {
            let _sink = SqliteSink::create(
                &directory.store(),
                &database,
                SqliteExportOptions::default(),
            )
            .expect("sink");
            // No export, no finish: the sink is dropped uncompleted.
        }
        assert!(!database.exists(), "an abandoned export must not linger");
    }
}
