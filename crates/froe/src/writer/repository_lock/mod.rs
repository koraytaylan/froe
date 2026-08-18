//! The repository lock: exclusive access to a segment store.
//!
//! Oak's writable file store takes an exclusive `FileChannel` lock over
//! the whole of `repo.lock` before touching anything else, and a running
//! AEM instance holds that lock for its lifetime. On Linux,
//! `FileChannel.lock()` maps to POSIX record locks (`fcntl`), which do
//! *not* conflict with `flock`-style locks — so this module uses `fcntl`
//! process-associated record locks, matching Java's ownership and fork
//! behavior and genuinely excluding a running Oak process.
//!
//! One deliberate deviation from Oak: acquisition does not block. Oak
//! waits indefinitely for the lock; a command-line tool is better served
//! by failing immediately with a clear message that the repository is in
//! use. The canonical lock file's content is never written and the file is
//! never deleted — only the advisory lock matters. On Unix, an absent lock
//! is first created and hardened under a non-active staging name, then
//! published with an absent-only hard link. Thus `repo.lock` is never
//! observable with a restrictive-umask mode that its creator has not yet
//! repaired.

use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

mod identity;
mod publication;
#[cfg(test)]
mod test_support;

pub(crate) use identity::*;
pub(crate) use publication::*;

/// An exclusive lock on a repository, released on drop.
pub struct RepositoryLock {
    /// Held for the lifetime of the lock; closing the file releases the
    /// operating system lock. An `Option` so [`Drop`] can close it
    /// *before* unregistering the identity, inside one registry critical
    /// section.
    pub(crate) lock_file: Option<File>,
    /// The identity registered in [`LOCKED_IDENTITIES`].
    pub(crate) registered_identity: LockIdentity,
}

impl RepositoryLock {
    /// Acquires the exclusive repository lock, creating `repo.lock` when
    /// absent. Fails immediately — with a message pointing at a possibly
    /// running AEM instance — when another process holds the lock, and
    /// equally when *this* process already holds it (Java's
    /// `OverlappingFileLockException`), regardless of the path the
    /// repository is reached through.
    /// A newly created lock is hardened to retain owner read/write access
    /// even under a restrictive umask; an existing inode's mode is never
    /// changed.
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
        let (lock_file, identity, creation_stage) =
            open_or_publish_lock_file(repository_directory, &lock_path, &registry)?;
        #[cfg(not(unix))]
        let (lock_file, identity, created) = {
            let (file, created) = open_lock_file(&lock_path)?;
            let identity = std::fs::canonicalize(&lock_path)?;
            if registry.contains(&identity) {
                return Err(in_use("this process"));
            }
            (file, identity, created)
        };

        #[allow(
            clippy::clone_on_copy,
            reason = "the identity is a PathBuf on non-Unix targets"
        )]
        registry.insert(identity.clone());
        if let Err(source) = lock_exclusively(&lock_file) {
            registry.remove(&identity);
            #[cfg(unix)]
            if let Some(stage) = creation_stage {
                // The canonical hard link remains a valid lock target. The
                // staging alias is best-effort cleanup on this error path.
                let _ = stage.remove();
                let _ = sync_repository_directory(repository_directory);
            }
            // Closing this descriptor is safe: the registry proved no
            // other in-process guard holds this identity, so no foreign
            // locks hang off it.
            return Err(if source.kind() == std::io::ErrorKind::WouldBlock {
                in_use("another process")
            } else {
                Error::InputOutput(source)
            });
        }
        #[cfg(unix)]
        if let Some(stage) = creation_stage {
            #[cfg(test)]
            crash_at_lock_creation_cutpoint("after-lock-before-stage-unlink");
            if let Err(source) = stage.remove() {
                registry.remove(&identity);
                return Err(source.into());
            }
            #[cfg(test)]
            crash_at_lock_creation_cutpoint("after-stage-unlink-before-directory-sync");
            if let Err(source) = sync_repository_directory(repository_directory) {
                registry.remove(&identity);
                return Err(source.into());
            }
        }
        #[cfg(not(unix))]
        if created && let Err(source) = make_new_lock_reopenable(&lock_file) {
            registry.remove(&identity);
            return Err(source.into());
        }
        Ok(Self {
            lock_file: Some(lock_file),
            registered_identity: identity,
        })
    }

    /// Proves that the directory still names the exact inode/path whose lock
    /// this guard holds. Cleanup calls this between mutation phases so an
    /// uncooperative tool cannot replace `repo.lock` and acquire a disjoint
    /// lock while maintenance continues.
    pub(crate) fn validate_path_identity(&self, repository_directory: &Path) -> Result<()> {
        let lock_path = repository_directory.join("repo.lock");
        #[cfg(unix)]
        {
            let metadata = std::fs::symlink_metadata(&lock_path)?;
            if !metadata.file_type().is_file()
                || lock_identity(&metadata) != self.registered_identity
            {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "{} no longer names the repository lock inode held by cleanup; refusing further mutation",
                        lock_path.display()
                    ),
                });
            }
        }
        #[cfg(not(unix))]
        {
            if std::fs::canonicalize(&lock_path)? != self.registered_identity {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "{} no longer names the repository lock held by cleanup; refusing further mutation",
                        lock_path.display()
                    ),
                });
            }
        }
        Ok(())
    }
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

/// Opens (creating when absent) the lock file for locking on non-Unix
/// platforms. Unix must use staged absent-only publication above.
#[cfg(not(unix))]
pub(crate) fn open_lock_file(lock_path: &Path) -> Result<(File, bool)> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).read(true).write(true);
    match options.open(lock_path) {
        Ok(file) => Ok((file, true)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path)?;
            Ok((file, false))
        }
        Err(error) => Err(error.into()),
    }
}

/// Takes an exclusive whole-file lock without blocking.
#[cfg(unix)]
pub(crate) fn lock_exclusively(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // An fcntl record lock over the entire file: the same lock space as
    // Java's FileChannel.lock(), so a running Oak instance is genuinely
    // excluded. Classic process-associated ownership also means a forked
    // child does not transiently inherit the parent's lock while it execs;
    // the inode registry above supplies Java's same-process overlap refusal.
    // This process opens a held lock inode exactly once, because closing any
    // descriptor for that inode would release classic record locks.
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
pub(crate) fn lock_exclusively(file: &File) -> std::io::Result<()> {
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => std::io::Error::from(std::io::ErrorKind::WouldBlock),
        std::fs::TryLockError::Error(error) => error,
    })
}

#[cfg(unix)]
pub(crate) fn sync_repository_directory(repository_directory: &Path) -> std::io::Result<()> {
    File::open(repository_directory)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::RepositoryLock;
    use crate::writer::repository_lock::test_support::TestDirectory;

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
    fn classic_record_lock_excludes_a_separate_process_without_fork_inheritance() {
        const CHILD_DIRECTORY: &str = "FROE_LOCK_CROSS_PROCESS_TEST_DIRECTORY";
        if let Some(path) = std::env::var_os(CHILD_DIRECTORY) {
            let result = RepositoryLock::acquire(std::path::Path::new(&path));
            let error = result
                .err()
                .expect("the parent process must still hold the record lock");
            assert!(error.to_string().contains("locked by another process"));
            return;
        }

        let directory = TestDirectory::new("cross-process");
        let held = RepositoryLock::acquire(&directory.path).expect("parent acquire");
        let output = std::process::Command::new(
            std::env::current_exe().expect("current test executable"),
        )
        .args([
            "--exact",
            "writer::repository_lock::tests::classic_record_lock_excludes_a_separate_process_without_fork_inheritance",
            "--nocapture",
        ])
        .env(CHILD_DIRECTORY, &directory.path)
        .output()
        .expect("run lock contender child");
        assert!(
            output.status.success(),
            "child failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        drop(held);
        RepositoryLock::acquire(&directory.path).expect("lock releases after parent drop");
    }
}
