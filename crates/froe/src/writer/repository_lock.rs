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

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};

/// The repository directories this process currently holds locked.
/// Classic process-associated POSIX record locks do not conflict within
/// one process, so same-process double opens must be refused here — Java
/// throws `OverlappingFileLockException` for exactly this case. The
/// registry also gives open-file-description platforms the identical
/// same-process behavior.
static LOCKED_DIRECTORIES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn locked_directories() -> std::sync::MutexGuard<'static, HashSet<PathBuf>> {
    LOCKED_DIRECTORIES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An exclusive lock on a repository, released on drop.
pub struct RepositoryLock {
    /// Held for the lifetime of the lock; closing the file releases the
    /// operating system lock. An `Option` so [`Drop`] can close it
    /// *before* unregistering the directory, inside one registry
    /// critical section.
    lock_file: Option<File>,
    /// The canonical directory registered in [`LOCKED_DIRECTORIES`].
    registered_directory: PathBuf,
}

impl RepositoryLock {
    /// Acquires the exclusive repository lock, creating `repo.lock` when
    /// absent. Fails immediately — with a message pointing at a possibly
    /// running AEM instance — when another process holds the lock, and
    /// equally when *this* process already holds it (Java's
    /// `OverlappingFileLockException`).
    pub fn acquire(repository_directory: &Path) -> Result<Self> {
        let in_use = |detail: &str| Error::InvalidFormat {
            details: format!(
                "the repository at {} is locked by {detail} — \
                 is an AEM or Oak instance still running?",
                repository_directory.display()
            ),
        };
        let canonical_directory = std::fs::canonicalize(repository_directory)?;
        if !locked_directories().insert(canonical_directory.clone()) {
            return Err(in_use("this process"));
        }
        let unregister = |directory: &PathBuf| {
            locked_directories().remove(directory);
        };

        let lock_path = repository_directory.join("repo.lock");
        let lock_file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) => {
                unregister(&canonical_directory);
                return Err(error.into());
            }
        };
        if let Err(source) = lock_exclusively(&lock_file) {
            unregister(&canonical_directory);
            return Err(if source.kind() == std::io::ErrorKind::WouldBlock {
                in_use("another process")
            } else {
                Error::InputOutput(source)
            });
        }
        Ok(Self {
            lock_file: Some(lock_file),
            registered_directory: canonical_directory,
        })
    }
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        // The operating system lock is released *before* the directory is
        // unregistered, and both happen inside one registry critical
        // section. The reverse order would open a race on classic
        // process-associated POSIX locks: a same-process acquire slipping
        // in between unregister and close would take a lock that this
        // descriptor's close then silently releases (closing any
        // descriptor of a file drops all of the process's record locks
        // on it).
        let mut registry = locked_directories();
        drop(self.lock_file.take());
        registry.remove(&self.registered_directory);
    }
}

/// Takes an exclusive whole-file lock without blocking.
#[cfg(unix)]
fn lock_exclusively(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // An fcntl record lock over the entire file: the same lock space as
    // Java's FileChannel.lock(), so a running Oak instance is genuinely
    // excluded. Where the operating system offers open-file-description
    // locks (Linux, Android, macOS, iOS), ownership is tied to the file
    // handle; other Unixes fall back to a classic process-associated
    // POSIX lock — the same lock space, safe here because this process
    // opens `repo.lock` exactly once and holds that handle for the
    // lock's lifetime.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    const SET_LOCK_COMMAND: libc::c_int = libc::F_OFD_SETLK;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    const SET_LOCK_COMMAND: libc::c_int = libc::F_SETLK;

    // Zero-initialized rather than a struct literal: `libc::flock`
    // carries platform-specific extra fields (for example `l_sysid` on
    // the BSDs), and zero is the correct value for every one of them —
    // including `l_start`/`l_len`, where zero length means "to the end
    // of the file, however it grows".
    // SAFETY: `flock` is plain old data; the all-zero bit pattern is a
    // valid value.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    // SAFETY: fcntl with a record-lock command reads the flock structure
    // we own and has no other memory effects.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), SET_LOCK_COMMAND, &raw mut lock) };
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
    fn concurrent_acquisitions_are_excluded() {
        // A second acquire in the same process is refused by the
        // process-local registry — on every platform, including those
        // whose classic POSIX locks would not conflict in-process — the
        // same behavior as Java's OverlappingFileLockException. (A second
        // *process* is excluded by the operating system lock itself.)
        let directory = TestDirectory::new("exclusion");
        let held = RepositoryLock::acquire(&directory.path).expect("acquire");
        let second = RepositoryLock::acquire(&directory.path);
        assert!(second.is_err(), "the second acquisition must fail");
        let message = second.err().expect("error").to_string();
        assert!(message.contains("locked by this process"), "{message}");
        // Releasing makes the directory acquirable again.
        drop(held);
        RepositoryLock::acquire(&directory.path).expect("reacquire after release");
    }
}
