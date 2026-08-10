//! Safe creation of export output files.

use std::path::Path;
/// Creates the export output directory, parents included, refusing a
/// directory inside the repository — even a stray *directory* in the
/// segment store is a surprise the next administrator must investigate.
/// An already existing directory outside the repository is fine; only
/// files are guarded against reuse, by [`create_export_output`].
pub fn create_export_directory(repository_path: &Path, directory: &Path) -> froe::Result<()> {
    let repository_directory = std::fs::canonicalize(repository_path)?;
    // The directory may not exist yet, and only existing paths
    // canonicalize; check the nearest existing ancestor, so symlinks on
    // the way in cannot smuggle the directory into the repository.
    let mut existing_ancestor = directory;
    while !existing_ancestor.exists() {
        existing_ancestor = match existing_ancestor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
    }
    if std::fs::canonicalize(existing_ancestor)?.starts_with(&repository_directory) {
        return Err(froe::Error::InvalidFormat {
            details: format!(
                "output directory {} is inside the repository directory; a stray entry \
                 there could be mistaken for damage at the next open",
                directory.display()
            ),
        });
    }
    std::fs::create_dir_all(directory)?;
    Ok(())
}

/// Creates an export output file. Never an existing file — an output
/// path aimed at the repository itself (`journal.log`, an archive, or an
/// alias of one) must never be truncated, and unrelated files must never
/// be silently overwritten — and never a fresh file *inside* the
/// repository directory, where the next open would mistake it for a
/// damaged archive. On Unix the file is created with mode 0600 before
/// the umask applies — never *broader* than owner-only, though a
/// restrictive umask can narrow it further.
///
/// The file is opened for reading and writing: sinks that re-open the
/// file through the descriptor (the `SQLite` sink on Unix) need both.
/// On Windows the file is created without delete-share, so the pathname
/// cannot be repointed (renamed or deleted away) while the export holds
/// the file — the identity checks the `SQLite` sink performs then always
/// refer to the created file.
pub fn create_export_output(
    repository_path: &Path,
    output_path: &Path,
) -> froe::Result<std::fs::File> {
    // Canonical paths, so symlinks and relative forms cannot smuggle the
    // output into the repository directory. A nonexistent parent skips
    // the check and fails file creation below instead.
    let repository_directory = std::fs::canonicalize(repository_path)?;
    let output_parent = match output_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if let Ok(canonical_parent) = std::fs::canonicalize(output_parent)
        && canonical_parent.starts_with(&repository_directory)
    {
        return Err(froe::Error::InvalidFormat {
            details: format!(
                "output file {} is inside the repository directory; a stray file there \
                     could be mistaken for a damaged archive at the next open",
                output_path.display()
            ),
        });
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE, deliberately without
        // FILE_SHARE_DELETE: while the export holds the file, nobody can
        // rename or delete it out from under the pathname.
        options.share_mode(0x0001 | 0x0002);
    }
    options.open(output_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            froe::Error::InvalidFormat {
                details: format!(
                    "output file {} already exists; the export never overwrites — choose a \
                     fresh path or remove the file first",
                    output_path.display()
                ),
            }
        } else {
            froe::Error::InputOutput(error)
        }
    })
}

/// The file name an export writes into before [`replace_export_output`]
/// moves it over the real output: the output's own name wrapped in dots
/// and tagged with the process identifier, for example
/// `.nodes.parquet.4173.tmp`. Hidden, unmistakably transient, and unique
/// per process — a crashed run's leftover never blocks the next run,
/// which sweeps same-pattern files before starting (see
/// [`sweep_temporary_outputs`]). Cross-process safety comes from the
/// export directory lock ([`lock_export_directory`]): with it held, no
/// live writer owns a file the sweep can match.
#[must_use]
pub fn temporary_output_name(file_name: &str) -> String {
    format!(".{file_name}.{}.tmp", std::process::id())
}

/// Whether a file name matches the [`temporary_output_name`] pattern of
/// `file_name`, whatever process wrote it: the name wrapped in dots,
/// a numeric process identifier, and the `.tmp` suffix.
fn is_temporary_output(file_name: &str, candidate: &str) -> bool {
    candidate
        .strip_prefix(format!(".{file_name}.").as_str())
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .is_some_and(|process| {
            !process.is_empty() && process.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Removes leftover [`temporary_output_name`] files of `file_name` from
/// `directory` — residue of crashed runs. Only our own hidden, clearly
/// transient pattern matches; real output files and foreign files stay.
/// Call only with the export directory lock held: without it a live
/// concurrent writer's in-flight temporary would be swept away.
pub fn sweep_temporary_outputs(directory: &Path, file_name: &str) -> froe::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(froe::Error::InputOutput(error)),
    };
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| is_temporary_output(file_name, name))
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The lock file serializing exports into one output directory.
const EXPORT_LOCK_FILE_NAME: &str = ".froe-export.lock";

/// A held exclusive lock on an export output directory. The lock is
/// advisory — it serializes froe processes, not other software — and
/// releases itself when the guard drops. The empty lock file stays in
/// the directory; deleting it after release would race the next locker.
pub struct ExportDirectoryLock {
    _file: std::fs::File,
}

/// Takes the exclusive lock on the export output `directory`, refusing
/// to wait: a contended lock means another export is writing there, and
/// proceeding would let concurrent runs sweep each other's temporaries
/// and rename over each other's results.
///
/// # Errors
///
/// Fails with [`std::io::ErrorKind::WouldBlock`] while another export
/// holds the lock.
pub fn lock_export_directory(directory: &Path) -> froe::Result<ExportDirectoryLock> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(directory.join(EXPORT_LOCK_FILE_NAME))?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => froe::Error::InputOutput(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "another export is writing to {}; wait for it to finish",
                directory.display()
            ),
        )),
        std::fs::TryLockError::Error(error) => froe::Error::InputOutput(error),
    })?;
    Ok(ExportDirectoryLock { _file: file })
}

/// Moves a fully written temporary output over `target`, atomically:
/// POSIX `rename` replaces by definition, and Rust's `MoveFileEx`-based
/// `rename` does the same on Windows, so a concurrent reader sees
/// either the old or the new file, never a mixture. A platform or
/// filesystem that refuses to replace reports the error instead — the
/// target is never unlinked to make room, so a failed replacement
/// always leaves it intact.
///
/// The temporary's bytes are forced to disk before the rename and the
/// directory entry after, so the replacement survives a power loss. On
/// Unix an existing target's permission bits carry over to the
/// replacement (ownership and ACLs cannot, portably); the permission
/// transfer runs after the sync, since a target mode without owner-read
/// would otherwise make the temporary unreadable before it is synced.
pub fn replace_export_output(temporary: &Path, target: &Path) -> froe::Result<()> {
    std::fs::File::open(temporary)?.sync_all()?;
    preserve_permissions(temporary, target)?;
    std::fs::rename(temporary, target).map_err(froe::Error::InputOutput)?;
    sync_parent_directory(target)
}

/// Carries an existing target's permission bits over to its replacement
/// on Unix; a missing target keeps the temporary's fresh mode.
#[cfg(unix)]
fn preserve_permissions(temporary: &Path, target: &Path) -> froe::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(target) {
        Ok(metadata) => {
            std::fs::set_permissions(
                temporary,
                std::fs::Permissions::from_mode(metadata.permissions().mode()),
            )?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(froe::Error::InputOutput(error)),
    }
}

/// No permission model to translate on other platforms.
#[cfg(not(unix))]
fn preserve_permissions(_temporary: &Path, _target: &Path) -> froe::Result<()> {
    Ok(())
}

/// Forces the directory entry of a renamed file to disk. Directory
/// handles open like files only on Unix; elsewhere the file's own sync
/// is what we have.
#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> froe::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// See the Unix variant.
#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> froe::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        is_temporary_output, replace_export_output, sweep_temporary_outputs, temporary_output_name,
    };

    #[test]
    fn temporary_names_match_their_pattern() {
        let name = temporary_output_name("nodes.parquet");
        assert!(is_temporary_output("nodes.parquet", &name));
        assert!(!is_temporary_output("properties.parquet", &name));
        assert!(!is_temporary_output("nodes.parquet", "nodes.parquet"));
        assert!(!is_temporary_output("nodes.parquet", ".nodes.parquet.tmp"));
        assert!(!is_temporary_output(
            "nodes.parquet",
            ".nodes.parquet.abc.tmp"
        ));
        assert!(!is_temporary_output(
            "nodes.parquet",
            ".nodes.parquet.42.tmp.bak"
        ));
        assert!(!is_temporary_output("nodes.parquet", ".nodes.parquet..tmp"));
        // The delta files' names never collide with the tables'.
        assert!(!is_temporary_output(
            "nodes.parquet",
            &temporary_output_name("nodes.delta.parquet")
        ));
    }

    #[test]
    fn the_sweep_removes_only_temporary_leftovers() {
        let directory =
            std::env::temp_dir().join(format!("froe-output-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create");
        std::fs::write(directory.join("nodes.parquet"), b"data").expect("write");
        std::fs::write(directory.join(".nodes.parquet.1.tmp"), b"partial").expect("write");
        std::fs::write(directory.join(".nodes.delta.parquet.1.tmp"), b"partial").expect("write");
        std::fs::write(directory.join("notes.txt"), b"foreign").expect("write");

        let remaining = || {
            let mut names: Vec<String> = std::fs::read_dir(&directory)
                .expect("read dir")
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        };
        sweep_temporary_outputs(&directory, "nodes.parquet").expect("sweep");
        assert_eq!(
            remaining(),
            vec![
                ".nodes.delta.parquet.1.tmp".to_owned(),
                "nodes.parquet".to_owned(),
                "notes.txt".to_owned(),
            ],
            "only the table's own temp files are swept; real and foreign files stay"
        );
        // The delta pattern sweeps independently.
        sweep_temporary_outputs(&directory, "nodes.delta.parquet").expect("sweep");
        assert_eq!(
            remaining(),
            vec!["nodes.parquet".to_owned(), "notes.txt".to_owned()]
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn replacement_moves_over_an_existing_target() {
        let directory =
            std::env::temp_dir().join(format!("froe-output-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create");
        let temporary = directory.join("temporary");
        let target = directory.join("target");
        std::fs::write(&target, b"old").expect("write");
        std::fs::write(&temporary, b"new").expect("write");

        replace_export_output(&temporary, &target).expect("replace");
        assert_eq!(std::fs::read(&target).expect("read"), b"new");
        assert!(!temporary.exists(), "the temporary moved away");

        // Replacing onto nothing simply moves.
        let fresh = directory.join("fresh");
        std::fs::write(&temporary, b"first").expect("write");
        replace_export_output(&temporary, &fresh).expect("replace");
        assert_eq!(std::fs::read(&fresh).expect("read"), b"first");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_failed_replacement_preserves_the_target() {
        let directory =
            std::env::temp_dir().join(format!("froe-output-preserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create");
        let target = directory.join("target");
        std::fs::write(&target, b"good data").expect("write");

        // The temporary is missing: the rename fails, and the target
        // must survive untouched.
        let missing = directory.join("missing-temporary");
        assert!(replace_export_output(&missing, &target).is_err());
        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"good data",
            "a failed rename never costs the good target"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn the_replacement_keeps_the_targets_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("froe-output-permissions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create");
        let temporary = directory.join("temporary");
        let target = directory.join("target");
        std::fs::write(&temporary, b"new").expect("write");
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
        std::fs::write(&target, b"old").expect("write");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o440)).expect("chmod");

        replace_export_output(&temporary, &target).expect("replace");
        assert_eq!(
            std::fs::metadata(&target)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o440,
            "the replacement carries the target's permissions, not the temporary's"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_export_directory_lock_serializes_writers() {
        let directory =
            std::env::temp_dir().join(format!("froe-output-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create");

        let lock = super::lock_export_directory(&directory).expect("lock");
        let contention = super::lock_export_directory(&directory);
        assert!(
            contention.is_err(),
            "a held lock refuses a second writer, even in-process"
        );
        drop(lock);
        super::lock_export_directory(&directory).expect("the released lock relocks");
        let _ = std::fs::remove_dir_all(&directory);
    }
}
