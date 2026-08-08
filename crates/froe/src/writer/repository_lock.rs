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
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};

/// How a locked repository is identified in the process-local registry.
/// On Unix the lock *file's* `(device, inode)` identity, so directory
/// renames and mount aliases cannot smuggle a second same-process
/// acquisition past the registry; elsewhere the canonical lock-file
/// path (Windows `LockFileEx` already conflicts across handles within
/// one process, so the registry is belt and braces there).
#[cfg(unix)]
type LockIdentity = (u64, u64);
#[cfg(not(unix))]
type LockIdentity = std::path::PathBuf;

#[cfg(unix)]
fn lock_identity(metadata: &std::fs::Metadata) -> LockIdentity {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

/// The lock identities this process currently holds. Classic
/// process-associated POSIX record locks do not conflict within one
/// process, so same-process double opens must be refused here — Java
/// throws `OverlappingFileLockException` for exactly this case. The
/// registry also gives open-file-description platforms the identical
/// same-process behavior.
static LOCKED_IDENTITIES: OnceLock<Mutex<HashSet<LockIdentity>>> = OnceLock::new();

fn locked_identities() -> std::sync::MutexGuard<'static, HashSet<LockIdentity>> {
    LOCKED_IDENTITIES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An exclusive lock on a repository, released on drop.
pub struct RepositoryLock {
    /// Held for the lifetime of the lock; closing the file releases the
    /// operating system lock. An `Option` so [`Drop`] can close it
    /// *before* unregistering the identity, inside one registry critical
    /// section.
    lock_file: Option<File>,
    /// The identity registered in [`LOCKED_IDENTITIES`].
    registered_identity: LockIdentity,
}

impl RepositoryLock {
    /// Acquires the exclusive repository lock, creating `repo.lock` when
    /// absent. Fails immediately — with a message pointing at a possibly
    /// running AEM instance — when another process holds the lock, and
    /// equally when *this* process already holds it (Java's
    /// `OverlappingFileLockException`), regardless of the path the
    /// repository is reached through.
    pub fn acquire(repository_directory: &Path) -> Result<Self> {
        let in_use = |detail: &str| Error::InvalidFormat {
            details: format!(
                "the repository at {} is locked by {detail} — \
                 is an AEM or Oak instance still running?",
                repository_directory.display()
            ),
        };
        let lock_path = repository_directory.join("repo.lock");

        // One critical section covers identity resolution, registration,
        // and the operating system lock, so no interleaving same-process
        // acquire can observe a half-registered state.
        let mut registry = locked_identities();

        #[cfg(unix)]
        let (lock_file, identity) = {
            // Stat *before* open: on classic process-associated POSIX
            // locks, merely opening and closing a descriptor of a lock
            // file another guard in this process holds would release
            // that guard's locks — so the file is opened only once the
            // registry proves no in-process guard holds this identity.
            match std::fs::metadata(&lock_path) {
                Ok(metadata) => {
                    let identity = lock_identity(&metadata);
                    if registry.contains(&identity) {
                        return Err(in_use("this process"));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let file = open_lock_file(&lock_path)?;
            let identity = lock_identity(&file.metadata()?);
            if registry.contains(&identity) {
                // The lock file changed identity between the stat and the
                // open (replaced, or freshly created by a racing
                // acquire) and this process already holds the new
                // identity. Closing our descriptor would release that
                // guard's record locks, so the descriptor is deliberately
                // leaked — one descriptor, in a pathological race.
                std::mem::forget(file);
                return Err(in_use("this process"));
            }
            (file, identity)
        };
        #[cfg(not(unix))]
        let (lock_file, identity) = {
            let file = open_lock_file(&lock_path)?;
            let identity = std::fs::canonicalize(&lock_path)?;
            if registry.contains(&identity) {
                return Err(in_use("this process"));
            }
            (file, identity)
        };

        #[allow(
            clippy::clone_on_copy,
            reason = "the identity is a PathBuf on non-Unix targets"
        )]
        registry.insert(identity.clone());
        if let Err(source) = lock_exclusively(&lock_file) {
            registry.remove(&identity);
            // Closing this descriptor is safe: the registry proved no
            // other in-process guard holds this identity, so no foreign
            // locks hang off it.
            return Err(if source.kind() == std::io::ErrorKind::WouldBlock {
                in_use("another process")
            } else {
                Error::InputOutput(source)
            });
        }
        Ok(Self {
            lock_file: Some(lock_file),
            registered_identity: identity,
        })
    }
}

/// Opens (creating when absent) the lock file for locking.
fn open_lock_file(lock_path: &Path) -> Result<File> {
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?)
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        // The operating system lock is released *before* the identity is
        // unregistered, and both happen inside one registry critical
        // section. The reverse order would open a race on classic
        // process-associated POSIX locks: a same-process acquire slipping
        // in between unregister and close would take a lock that this
        // descriptor's close then silently releases (closing any
        // descriptor of a file drops all of the process's record locks
        // on it).
        let mut registry = locked_identities();
        drop(self.lock_file.take());
        registry.remove(&self.registered_identity);
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
