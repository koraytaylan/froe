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
/// registry gives every supported platform the identical same-process
/// behavior.
static LOCKED_IDENTITIES: OnceLock<Mutex<HashSet<LockIdentity>>> = OnceLock::new();

/// Makes concurrent absent-lock creators choose distinct staging names. The
/// timestamp and process identifier make stale names from earlier processes
/// unlikely to collide; `create_new` remains the authoritative collision
/// check.
#[cfg(unix)]
static NEXT_LOCK_STAGE: AtomicU64 = AtomicU64::new(0);

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

/// Opens `repo.lock`, or publishes it when absent, returning the descriptor
/// with the identity it was proved to have.
///
/// Retries because both halves race an outside actor: an existing inode can
/// be replaced between the stat and the open, and an absent-only
/// publication can be lost to a competing creator.
#[cfg(unix)]
fn open_or_publish_lock_file(
    repository_directory: &Path,
    lock_path: &Path,
    registry: &HashSet<LockIdentity>,
) -> Result<(std::fs::File, LockIdentity, Option<LockCreationStage>)> {
    let in_use = |detail: &str| Error::InvalidFormat {
        details: format!(
            "the repository at {} is locked by {detail} — \
             is an AEM or Oak instance still running?",
            repository_directory.display()
        ),
    };
    // Stat *before* open: on classic process-associated POSIX
    // locks, merely opening and closing a descriptor of a lock
    // file another guard in this process holds would release
    // that guard's locks — so the existing file is opened only
    // once the registry proves no in-process guard holds this
    // identity. An absent file is never created at `repo.lock`
    // directly; competing creators race at an absent-only link.
    let mut attempts = 0u16;
    loop {
        attempts += 1;
        if attempts > 1000 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} changed repeatedly while the repository lock was being opened",
                    lock_path.display()
                ),
            });
        }
        match std::fs::symlink_metadata(lock_path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(invalid_lock_file_type(lock_path));
                }
                let observed_identity = lock_identity(&metadata);
                if registry.contains(&observed_identity) {
                    return Err(in_use("this process"));
                }
                let file = match open_existing_lock_file(lock_path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                let identity = lock_identity(&file.metadata()?);
                if registry.contains(&identity) {
                    // The lock file changed identity between the stat
                    // and open and this process already holds the new
                    // inode. Closing our descriptor would release that
                    // guard's classic record locks, so leak this one
                    // descriptor in the pathological race.
                    std::mem::forget(file);
                    return Err(in_use("this process"));
                }
                match std::fs::symlink_metadata(lock_path) {
                    Ok(current)
                        if current.file_type().is_file() && lock_identity(&current) == identity =>
                    {
                        break Ok((file, identity, None));
                    }
                    Ok(current) if !current.file_type().is_file() => {
                        drop(file);
                        return Err(invalid_lock_file_type(lock_path));
                    }
                    Ok(_) => {
                        // The path was replaced after open. This inode
                        // is not registered in-process, so closing it
                        // is safe; retry against the current name.
                        drop(file);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        drop(file);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some((file, stage)) = publish_new_lock(repository_directory, lock_path)? {
                    let identity = lock_identity(&file.metadata()?);
                    break Ok((file, identity, Some(stage)));
                }
                // Another creator won the absent-only publication.
                // Its distinct stage was not opened or replaced; retry
                // through the ordinary existing-inode path.
            }
            Err(error) => return Err(error.into()),
        }
    }
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

#[cfg(unix)]
fn invalid_lock_file_type(lock_path: &Path) -> Error {
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
fn open_existing_lock_file(lock_path: &Path) -> std::io::Result<File> {
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

/// A same-directory, non-active name for an inode being prepared as a new
/// canonical lock. Normal unwinding removes it; abrupt process death may
/// leave the harmless name for forensic/manual cleanup.
#[cfg(unix)]
struct LockCreationStage {
    path: PathBuf,
    armed: bool,
}

#[cfg(unix)]
impl LockCreationStage {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn remove(mut self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {
                self.armed = false;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.armed = false;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
impl Drop for LockCreationStage {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Creates, hardens, syncs, and absent-only publishes a new lock inode.
/// `None` means another creator won `repo.lock`; callers must retry through
/// the existing-inode path. Hard-link failures have no direct-create fallback.
#[cfg(unix)]
fn publish_new_lock(
    repository_directory: &Path,
    lock_path: &Path,
) -> Result<Option<(File, LockCreationStage)>> {
    let (file, stage) = create_lock_stage(repository_directory)?;
    harden_new_lock_stage(&file)?;
    #[cfg(test)]
    crash_at_lock_creation_cutpoint("after-stage-create");
    #[cfg(test)]
    wait_at_lock_creation_race_barrier();

    match std::fs::hard_link(&stage.path, lock_path) {
        Ok(()) => {
            #[cfg(test)]
            crash_at_lock_creation_cutpoint("after-publish");
            verify_published_lock(&file, &stage.path, lock_path)?;
            Ok(Some((file, stage)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            stage.remove()?;
            Ok(None)
        }
        Err(error) => {
            let cleanup_error = stage.remove().err();
            let cleanup_detail = cleanup_error.map_or_else(String::new, |cleanup| {
                format!("; staging cleanup also failed: {cleanup}")
            });
            Err(Error::InvalidFormat {
                details: format!(
                    "cannot atomically publish absent repository lock {}: same-directory hard-link creation is required and no direct-create fallback is permitted ({error}){cleanup_detail}",
                    lock_path.display()
                ),
            })
        }
    }
}

#[cfg(unix)]
fn create_lock_stage(repository_directory: &Path) -> Result<(File, LockCreationStage)> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..1000 {
        let sequence = NEXT_LOCK_STAGE.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            ".repo.lock.creating.{}.{timestamp:032x}.{sequence:016x}",
            std::process::id()
        );
        let path = repository_directory.join(name);
        match std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok((file, LockCreationStage::new(path))),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::InvalidFormat {
        details: format!(
            "all 1000 repository-lock staging names in {} are occupied",
            repository_directory.display()
        ),
    })
}

#[cfg(unix)]
fn harden_new_lock_stage(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::io::AsRawFd as _;

    // SAFETY: `file` owns a live descriptor and `fchmod` reads no memory.
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    file.sync_all()?;
    let metadata = file.metadata()?;
    let mode = metadata.mode() & 0o777;
    // SAFETY: geteuid has no preconditions and does not access memory.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file() || metadata.uid() != effective_uid || mode != 0o600 {
        return Err(std::io::Error::other(format!(
            "new repository lock staging inode failed verification (regular={}, uid={}, expected uid={effective_uid}, mode={mode:03o}, expected mode=600)",
            metadata.file_type().is_file(),
            metadata.uid()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_published_lock(file: &File, stage_path: &Path, lock_path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let opened = file.metadata()?;
    let stage = std::fs::symlink_metadata(stage_path)?;
    let canonical = std::fs::symlink_metadata(lock_path)?;
    let identity = lock_identity(&opened);
    // SAFETY: geteuid has no preconditions and does not access memory.
    let effective_uid = unsafe { libc::geteuid() };
    if !stage.file_type().is_file()
        || !canonical.file_type().is_file()
        || lock_identity(&stage) != identity
        || lock_identity(&canonical) != identity
        || canonical.uid() != effective_uid
        || canonical.mode() & 0o777 != 0o600
    {
        return Err(Error::InvalidFormat {
            details: format!(
                "new repository lock {} did not publish the verified staging inode with uid {effective_uid} and mode 600",
                lock_path.display()
            ),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn sync_repository_directory(repository_directory: &Path) -> std::io::Result<()> {
    File::open(repository_directory)?.sync_all()
}

#[cfg(test)]
#[cfg(unix)]
fn crash_at_lock_creation_cutpoint(point: &str) {
    const ENVIRONMENT: &str = "FROE_LOCK_CREATION_CRASH_POINT";
    if std::env::var_os(ENVIRONMENT).is_some_and(|armed| armed == point) {
        // SAFETY: this test-only cutpoint intentionally simulates abrupt
        // process death without running destructors.
        unsafe { libc::_exit(86) };
    }
}

#[cfg(test)]
#[cfg(unix)]
fn wait_at_lock_creation_race_barrier() {
    let Some(directory) = std::env::var_os("FROE_LOCK_CREATION_RACE_BARRIER") else {
        return;
    };
    let directory = PathBuf::from(directory);
    std::fs::write(directory.join(format!("ready-{}", std::process::id())), [])
        .expect("write lock-creation race readiness marker");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !directory.join("go").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out at lock-creation race barrier"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Opens (creating when absent) the lock file for locking on non-Unix
/// platforms. Unix must use staged absent-only publication above.
#[cfg(not(unix))]
fn open_lock_file(lock_path: &Path) -> Result<(File, bool)> {
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

/// A newly created non-Unix lock must be durable before acquisition returns.
#[cfg(not(unix))]
fn make_new_lock_reopenable(file: &File) -> std::io::Result<()> {
    file.sync_all()
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

    #[cfg(unix)]
    #[test]
    fn a_new_lock_remains_reopenable_under_a_restrictive_umask() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        const CHILD_DIRECTORY: &str = "FROE_LOCK_UMASK_TEST_DIRECTORY";
        if let Some(path) = std::env::var_os(CHILD_DIRECTORY) {
            let path = std::path::PathBuf::from(path);
            // SAFETY: this is an isolated child test process selected with
            // `--exact`; no other test or thread observes its process-global
            // umask before the process exits.
            unsafe { libc::umask(0o777) };
            let held = RepositoryLock::acquire(&path).expect("acquire absent lock");
            let first = std::fs::metadata(path.join("repo.lock")).expect("first metadata");
            assert_eq!(first.permissions().mode() & 0o600, 0o600);
            drop(held);

            let second = RepositoryLock::acquire(&path).expect("reopen hardened lock");
            let reopened = std::fs::metadata(path.join("repo.lock")).expect("second metadata");
            assert_eq!(reopened.ino(), first.ino());
            assert_eq!(reopened.permissions().mode() & 0o600, 0o600);
            drop(second);
            return;
        }

        let directory = TestDirectory::new("restrictive-umask");
        let output = std::process::Command::new(
            std::env::current_exe().expect("current test executable"),
        )
        .args([
            "--exact",
            "writer::repository_lock::tests::a_new_lock_remains_reopenable_under_a_restrictive_umask",
            "--nocapture",
        ])
        .env(CHILD_DIRECTORY, &directory.path)
        .output()
        .expect("run isolated umask test");
        assert!(
            output.status.success(),
            "child failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let mode = std::fs::metadata(directory.path.join("repo.lock"))
            .expect("lock remains")
            .permissions()
            .mode();
        assert_eq!(mode & 0o600, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn absent_lock_publication_survives_every_process_crash_cutpoint() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        const CHILD_DIRECTORY: &str = "FROE_LOCK_CRASH_TEST_DIRECTORY";
        const CRASH_POINT: &str = "FROE_LOCK_CREATION_CRASH_POINT";
        const TEST_NAME: &str = "writer::repository_lock::tests::absent_lock_publication_survives_every_process_crash_cutpoint";
        if let Some(path) = std::env::var_os(CHILD_DIRECTORY) {
            let _held = RepositoryLock::acquire(std::path::Path::new(&path))
                .expect("armed lock creation must reach its crash cutpoint");
            panic!("armed lock creation returned instead of crashing");
        }

        for point in [
            "after-stage-create",
            "after-publish",
            "after-lock-before-stage-unlink",
            "after-stage-unlink-before-directory-sync",
        ] {
            let directory = TestDirectory::new(&format!("crash-{point}"));
            let output = std::process::Command::new(
                std::env::current_exe().expect("current test executable"),
            )
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_DIRECTORY, &directory.path)
            .env(CRASH_POINT, point)
            .output()
            .expect("run crashing lock creator");
            assert_eq!(
                output.status.code(),
                Some(86),
                "unexpected child result at {point}:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            let canonical_path = directory.path.join("repo.lock");
            let mut stages: Vec<_> = std::fs::read_dir(&directory.path)
                .expect("read crash directory")
                .map(|entry| entry.expect("directory entry").path())
                .filter(|path| {
                    path.file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|name| name.starts_with(".repo.lock.creating."))
                })
                .collect();
            stages.sort();

            if point == "after-stage-create" {
                assert!(
                    !canonical_path.exists(),
                    "the canonical name must not precede publication"
                );
                assert_eq!(stages.len(), 1, "the interrupted stage is non-active");
                let stage = std::fs::symlink_metadata(&stages[0]).expect("stage metadata");
                assert_eq!(stage.permissions().mode() & 0o777, 0o600);
            } else {
                let canonical = std::fs::symlink_metadata(&canonical_path)
                    .expect("published canonical lock remains");
                assert!(canonical.file_type().is_file());
                assert_eq!(
                    canonical.permissions().mode() & 0o777,
                    0o600,
                    "the canonical lock is never published mode 000"
                );
                if matches!(point, "after-publish" | "after-lock-before-stage-unlink") {
                    assert_eq!(stages.len(), 1, "published stage alias remains");
                    let stage = std::fs::symlink_metadata(&stages[0]).expect("stage metadata");
                    assert_eq!(
                        (stage.dev(), stage.ino()),
                        (canonical.dev(), canonical.ino()),
                        "canonical and stage names must be hard links to one inode"
                    );
                    assert!(canonical.nlink() >= 2);
                } else {
                    assert!(
                        stages.is_empty(),
                        "the stage was unlinked before the cutpoint"
                    );
                }
            }

            let first = RepositoryLock::acquire(&directory.path)
                .expect("fresh process can acquire after creator crash");
            first
                .validate_path_identity(&directory.path)
                .expect("fresh guard holds canonical inode");
            drop(first);
            let second = RepositoryLock::acquire(&directory.path)
                .expect("canonical lock remains reopenable after crash");
            second
                .validate_path_identity(&directory.path)
                .expect("reopened guard holds canonical inode");
        }
    }

    #[cfg(unix)]
    #[test]
    fn two_absent_lock_creators_publish_one_inode_and_one_lock_wins() {
        use std::os::unix::fs::PermissionsExt as _;

        const CHILD_DIRECTORY: &str = "FROE_LOCK_RACE_TEST_DIRECTORY";
        const BARRIER_DIRECTORY: &str = "FROE_LOCK_CREATION_RACE_BARRIER";
        const TEST_NAME: &str = "writer::repository_lock::tests::two_absent_lock_creators_publish_one_inode_and_one_lock_wins";
        if let (Some(repository), Some(barrier)) = (
            std::env::var_os(CHILD_DIRECTORY),
            std::env::var_os(BARRIER_DIRECTORY),
        ) {
            let repository = std::path::PathBuf::from(repository);
            let barrier = std::path::PathBuf::from(barrier);
            match RepositoryLock::acquire(&repository) {
                Ok(held) => {
                    std::fs::write(barrier.join(format!("winner-{}", std::process::id())), [])
                        .expect("write winner marker");
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !barrier.join("release").exists() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "timed out waiting to release race winner"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    drop(held);
                }
                Err(error) => {
                    assert!(
                        error.to_string().contains("locked by another process"),
                        "unexpected race-loser error: {error}"
                    );
                    std::fs::write(barrier.join(format!("loser-{}", std::process::id())), [])
                        .expect("write loser marker");
                }
            }
            return;
        }

        let directory = TestDirectory::new("two-creator-race");
        let barrier = directory.path.join("race-barrier");
        std::fs::create_dir(&barrier).expect("create race barrier directory");
        let mut children = Vec::new();
        for _ in 0..2 {
            children.push(
                std::process::Command::new(
                    std::env::current_exe().expect("current test executable"),
                )
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(CHILD_DIRECTORY, &directory.path)
                .env(BARRIER_DIRECTORY, &barrier)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn absent-lock creator"),
            );
        }

        wait_for_marker_count(&barrier, "ready-", 2);
        std::fs::write(barrier.join("go"), []).expect("release creation barrier");
        wait_for_marker_count(&barrier, "winner-", 1);
        wait_for_marker_count(&barrier, "loser-", 1);
        std::fs::write(barrier.join("release"), []).expect("release winning lock holder");
        for child in children {
            let output = child.wait_with_output().expect("wait for lock creator");
            assert!(
                output.status.success(),
                "creator failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let canonical = std::fs::symlink_metadata(directory.path.join("repo.lock"))
            .expect("one canonical lock was published");
        assert_eq!(canonical.permissions().mode() & 0o777, 0o600);
        assert!(
            std::fs::read_dir(&directory.path)
                .expect("read race repository")
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .all(|name| !name.starts_with(".repo.lock.creating.")),
            "normal race completion removes both staging aliases"
        );
        RepositoryLock::acquire(&directory.path).expect("race result remains acquirable");
    }

    #[cfg(unix)]
    fn wait_for_marker_count(directory: &std::path::Path, prefix: &str, expected: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let count = std::fs::read_dir(directory)
                .expect("read marker directory")
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.starts_with(prefix))
                .count();
            if count == expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {expected} {prefix} markers; found {count}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

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
