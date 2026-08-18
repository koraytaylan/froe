//! Which inode a lock is held on, so a directory renamed under a live
//! session cannot buy a second writer the same repository.

#[cfg(unix)]
use super::{Error, File, Path};
use super::{HashSet, Mutex, OnceLock};

/// How a locked repository is identified in the process-local registry.
/// On Unix the lock *file's* `(device, inode)` identity, so directory
/// renames and mount aliases cannot smuggle a second same-process
/// acquisition past the registry; elsewhere the canonical lock-file
/// path (Windows `LockFileEx` already conflicts across handles within
/// one process, so the registry is belt and braces there).
#[cfg(unix)]
pub(crate) type LockIdentity = (u64, u64);

#[cfg(not(unix))]
pub(crate) type LockIdentity = std::path::PathBuf;

#[cfg(unix)]
pub(crate) fn lock_identity(metadata: &std::fs::Metadata) -> LockIdentity {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

/// The lock identities this process currently holds. Classic
/// process-associated POSIX record locks do not conflict within one
/// process, so same-process double opens must be refused here — Java
/// throws `OverlappingFileLockException` for exactly this case. The
/// registry gives every supported platform the identical same-process
/// behavior.
pub(crate) static LOCKED_IDENTITIES: OnceLock<Mutex<HashSet<LockIdentity>>> = OnceLock::new();

pub(crate) fn locked_identities() -> std::sync::MutexGuard<'static, HashSet<LockIdentity>> {
    LOCKED_IDENTITIES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(unix)]
pub(crate) fn invalid_lock_file_type(lock_path: &Path) -> Error {
    Error::InvalidFormat {
        details: format!(
            "repository lock {} is not a regular file; refusing to follow or replace it",
            lock_path.display()
        ),
    }
}

/// Opens an existing canonical lock exactly once and never follows a final
/// symlink. The caller performs the pre-open registry check needed by classic
/// process-associated record locks.
#[cfg(unix)]
pub(crate) fn open_existing_lock_file(lock_path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other(format!(
            "repository lock {} is not a regular file",
            lock_path.display()
        )));
    }
    Ok(file)
}

#[cfg(all(test, unix))]
mod tests {
    use crate::writer::repository_lock::RepositoryLock;
    use crate::writer::repository_lock::test_support::TestDirectory;

    #[cfg(unix)]
    #[test]
    fn existing_lock_symlink_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("lock-symlink");
        let target = directory.path.join("target");
        std::fs::write(&target, b"unchanged").expect("write symlink target");
        symlink("target", directory.path.join("repo.lock")).expect("create lock symlink");

        let error = RepositoryLock::acquire(&directory.path)
            .err()
            .expect("lock symlink must be rejected");

        assert!(error.to_string().contains("not a regular file"), "{error}");
        assert_eq!(std::fs::read(target).expect("read target"), b"unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn renaming_the_directory_does_not_defeat_same_process_exclusion() {
        // The registry keys on the lock file's (device, inode) identity,
        // so reaching the same repository through a renamed path is
        // still refused while the lock is held.
        let directory = TestDirectory::new("rename-alias");
        let renamed = directory.path.with_extension("renamed");
        let _ = std::fs::remove_dir_all(&renamed);

        let held = RepositoryLock::acquire(&directory.path).expect("acquire");
        std::fs::rename(&directory.path, &renamed).expect("rename directory");
        let through_new_path = RepositoryLock::acquire(&renamed);
        assert!(
            through_new_path.is_err(),
            "the renamed path is the same lock file and must be refused"
        );
        let message = through_new_path.err().expect("error").to_string();
        assert!(message.contains("locked by this process"), "{message}");

        drop(held);
        RepositoryLock::acquire(&renamed).expect("reacquire after release");
        let _ = std::fs::remove_dir_all(&renamed);
    }
}
