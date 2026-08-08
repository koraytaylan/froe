//! The repository lock: exclusive access to a segment store.
//!
//! Oak's writable file store takes an exclusive `FileChannel` lock over
//! the whole of `repo.lock` before touching anything else, and a running
//! AEM instance holds that lock for its lifetime. On Linux,
//! `FileChannel.lock()` maps to POSIX record locks (`fcntl`), which do
//! *not* conflict with `flock`-style locks — so this module uses `fcntl`
//! open-file-description locks, which share the POSIX lock space and
//! therefore genuinely exclude a running Oak process.
//!
//! One deliberate deviation from Oak: acquisition does not block. Oak
//! waits indefinitely for the lock; a command-line tool is better served
//! by failing immediately with a clear message that the repository is in
//! use. The lock file's content is never written and the file is never
//! deleted — only the advisory lock matters.

use std::fs::File;
use std::path::Path;

use crate::error::{Error, Result};

/// An exclusive lock on a repository, released on drop.
pub struct RepositoryLock {
    /// Held for the lifetime of the lock; dropping the file releases the
    /// open-file-description lock.
    _lock_file: File,
}

impl RepositoryLock {
    /// Acquires the exclusive repository lock, creating `repo.lock` when
    /// absent. Fails immediately — with a message pointing at a possibly
    /// running AEM instance — when another process holds the lock.
    pub fn acquire(repository_directory: &Path) -> Result<Self> {
        let lock_path = repository_directory.join("repo.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        lock_exclusively(&lock_file).map_err(|source| {
            if source.kind() == std::io::ErrorKind::WouldBlock {
                Error::InvalidFormat {
                    details: format!(
                        "the repository at {} is locked by another process — \
                         is an AEM or Oak instance still running?",
                        repository_directory.display()
                    ),
                }
            } else {
                Error::InputOutput(source)
            }
        })?;
        Ok(Self {
            _lock_file: lock_file,
        })
    }
}

/// Takes an exclusive whole-file lock without blocking.
#[cfg(unix)]
fn lock_exclusively(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // An open-file-description record lock over the entire file: the same
    // lock space as Java's FileChannel.lock(), with ownership tied to the
    // file handle (released when the handle closes) rather than the
    // process.
    let mut lock = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0, // zero length means "to the end of the file, however it grows"
        l_pid: 0,
    };
    // SAFETY: fcntl with F_OFD_SETLK reads the flock structure we own and
    // has no other memory effects.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &raw mut lock) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        // Contended fcntl locks report EAGAIN or EACCES; normalize both
        // to WouldBlock for the caller's message.
        if matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) {
            Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, error))
        } else {
            Err(error)
        }
    }
}

/// Takes an exclusive whole-file lock without blocking. On Windows,
/// `File::try_lock` (Rust 1.89) uses `LockFileEx` — the same lock space
/// as Java's `FileChannel.lock()` there, so a running Oak instance is
/// genuinely excluded.
#[cfg(not(unix))]
fn lock_exclusively(file: &File) -> std::io::Result<()> {
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => std::io::Error::from(std::io::ErrorKind::WouldBlock),
        std::fs::TryLockError::Error(error) => error,
    })
}

#[cfg(test)]
mod tests {
    use super::RepositoryLock;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-lock-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn lock_creates_the_lock_file_and_releases_on_drop() {
        let directory = TestDirectory::new("release");
        let lock = RepositoryLock::acquire(&directory.path).expect("acquire");
        assert!(directory.path.join("repo.lock").exists());
        drop(lock);
        // Reacquirable after release.
        let _second = RepositoryLock::acquire(&directory.path).expect("reacquire");
        assert!(
            directory.path.join("repo.lock").exists(),
            "the lock file is never deleted"
        );
    }

    #[test]
    fn concurrent_processes_are_excluded() {
        // Open-file-description locks conflict across file handles even in
        // one process, so a second acquire in this test observes exactly
        // what a second process would.
        let directory = TestDirectory::new("exclusion");
        let _held = RepositoryLock::acquire(&directory.path).expect("acquire");
        let second = RepositoryLock::acquire(&directory.path);
        assert!(second.is_err(), "the second acquisition must fail");
        let message = second.err().expect("error").to_string();
        assert!(message.contains("locked by another process"), "{message}");
    }
}
