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
/// [`sweep_temporary_outputs`]). Concurrent exports into one directory
/// still race on the final rename, last writer winning — exactly like
/// two concurrent exports into two fresh names would.
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

/// Moves a fully written temporary output over `target`. On POSIX, and
/// on Windows through Rust's `MoveFileEx`-based `rename`, replacing an
/// existing target is atomic: a concurrent reader sees either the old
/// or the new file, never a mixture. The remove-and-retry branch covers
/// filesystems where renaming over an existing file fails outright —
/// there a crash can leave the target missing, which the Parquet
/// refresh's provenance validation tolerates: an interrupted
/// replacement simply fails the next run's validation and is rebuilt.
pub fn replace_export_output(temporary: &Path, target: &Path) -> froe::Result<()> {
    match std::fs::rename(temporary, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            if !target.exists() {
                return Err(froe::Error::InputOutput(error));
            }
            std::fs::remove_file(target)?;
            std::fs::rename(temporary, target)?;
            Ok(())
        }
    }
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
}
