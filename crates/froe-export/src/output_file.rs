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
