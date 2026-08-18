//! Creating an absent lock file so that whichever way a run dies, the
//! directory holds either no lock or one fully hardened, reopenable inode
//! that exactly one writer holds.

use super::File;
#[cfg(unix)]
use super::{
    AtomicU64, Error, HashSet, LockIdentity, Ordering, Path, PathBuf, Result,
    invalid_lock_file_type, lock_identity, open_existing_lock_file,
};

/// Makes concurrent absent-lock creators choose distinct staging names. The
/// timestamp and process identifier make stale names from earlier processes
/// unlikely to collide; `create_new` remains the authoritative collision
/// check.
#[cfg(unix)]
pub(crate) static NEXT_LOCK_STAGE: AtomicU64 = AtomicU64::new(0);

/// Opens `repo.lock`, or publishes it when absent, returning the descriptor
/// with the identity it was proved to have.
///
/// Retries because both halves race an outside actor: an existing inode can
/// be replaced between the stat and the open, and an absent-only
/// publication can be lost to a competing creator.
#[cfg(unix)]
pub(crate) fn open_or_publish_lock_file(
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

/// A same-directory, non-active name for an inode being prepared as a new
/// canonical lock. Normal unwinding removes it; abrupt process death may
/// leave the harmless name for forensic/manual cleanup.
#[cfg(unix)]
pub(crate) struct LockCreationStage {
    pub(crate) path: PathBuf,
    pub(crate) armed: bool,
}

#[cfg(unix)]
impl LockCreationStage {
    pub(in crate::writer) fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub(in crate::writer) fn remove(mut self) -> std::io::Result<()> {
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
pub(crate) fn publish_new_lock(
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
pub(crate) fn create_lock_stage(repository_directory: &Path) -> Result<(File, LockCreationStage)> {
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
pub(crate) fn harden_new_lock_stage(file: &File) -> std::io::Result<()> {
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
pub(crate) fn verify_published_lock(
    file: &File,
    stage_path: &Path,
    lock_path: &Path,
) -> Result<()> {
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

#[cfg(test)]
#[cfg(unix)]
pub(crate) fn crash_at_lock_creation_cutpoint(point: &str) {
    const ENVIRONMENT: &str = "FROE_LOCK_CREATION_CRASH_POINT";
    if std::env::var_os(ENVIRONMENT).is_some_and(|armed| armed == point) {
        // SAFETY: this test-only cutpoint intentionally simulates abrupt
        // process death without running destructors.
        unsafe { libc::_exit(86) };
    }
}

#[cfg(test)]
#[cfg(unix)]
pub(crate) fn wait_at_lock_creation_race_barrier() {
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

/// A newly created non-Unix lock must be durable before acquisition returns.
#[cfg(not(unix))]
pub(crate) fn make_new_lock_reopenable(file: &File) -> std::io::Result<()> {
    file.sync_all()
}

#[cfg(all(test, unix))]
mod tests {
    use crate::writer::repository_lock::RepositoryLock;
    use crate::writer::repository_lock::test_support::TestDirectory;

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
            "writer::repository_lock::publication::tests::a_new_lock_remains_reopenable_under_a_restrictive_umask",
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
        const TEST_NAME: &str = "writer::repository_lock::publication::tests::absent_lock_publication_survives_every_process_crash_cutpoint";
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
        const TEST_NAME: &str = "writer::repository_lock::publication::tests::two_absent_lock_creators_publish_one_inode_and_one_lock_wins";
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
}
