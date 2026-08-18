//! Proving a path is still the regular file the caller certified, and
//! the safe file operations that rely on that proof.

use crate::error::{Error, Result};
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::tar_writer::TarArchiveWriter;
use std::fs::File;
use std::path::{Path, PathBuf};

pub(super) fn archive_file_bytes(directory: &Path) -> Result<u64> {
    let mut total = 0u64;
    for file_name in crate::store::list_archive_file_names(directory)? {
        if ArchiveFileName::parse(&file_name).is_some() {
            total = total
                .checked_add(std::fs::symlink_metadata(directory.join(file_name))?.len())
                .ok_or_else(|| Error::InvalidFormat {
                    details: "archive byte accounting overflow".to_owned(),
                })?;
        }
    }
    Ok(total)
}

pub(crate) fn sync_directory_strict(directory: &Path) -> Result<()> {
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

/// Stable identity of a regular file used in a destructive maintenance step.
/// The mutating writer is Unix-only, where `(device, inode)` binds a held file
/// descriptor to directory entries without trusting a replaceable pathname.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RegularFileIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RegularFileIdentity;

pub(super) fn regular_file_identity(metadata: &std::fs::Metadata) -> Result<RegularFileIdentity> {
    if !metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: "destructive maintenance target is not a regular file".to_owned(),
        });
    }
    filesystem_object_identity(metadata)
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "the non-Unix implementation is an intentional runtime refusal while Unix returns a verified identity"
)]
pub(super) fn filesystem_object_identity(
    metadata: &std::fs::Metadata,
) -> Result<RegularFileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(RegularFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(Error::InvalidFormat {
            details: "destructive archive maintenance requires Unix file-identity checks"
                .to_owned(),
        })
    }
}

/// Whether an opened handle may write through to the file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FileAccess {
    /// Read only, so the handle cannot modify what it inspects.
    ReadOnly,
    /// Read and write.
    ReadWrite,
}

pub(super) fn open_regular_file_no_follow(path: &Path, access: FileAccess) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(access == FileAccess::ReadWrite);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    regular_file_identity(&file.metadata()?)?;
    Ok(file)
}

pub(super) fn held_file_identity(file: &File) -> Result<RegularFileIdentity> {
    regular_file_identity(&file.metadata()?)
}

pub(super) fn path_file_identity(path: &Path) -> Result<RegularFileIdentity> {
    let metadata = std::fs::symlink_metadata(path)?;
    regular_file_identity(&metadata).map_err(|error| Error::InvalidFormat {
        details: format!(
            "{} is not the regular file required for destructive maintenance ({error})",
            path.display()
        ),
    })
}

pub(super) fn path_object_identity(path: &Path) -> Result<RegularFileIdentity> {
    filesystem_object_identity(&std::fs::symlink_metadata(path)?)
}

pub(super) fn require_path_file_identity(
    path: &Path,
    expected: RegularFileIdentity,
    description: &str,
) -> Result<()> {
    let actual = path_file_identity(path)?;
    if actual != expected {
        return Err(Error::InvalidFormat {
            details: format!(
                "{description} {} changed file identity before destructive maintenance",
                path.display()
            ),
        });
    }
    Ok(())
}

pub(super) fn require_held_file_identity(
    file: &File,
    expected: RegularFileIdentity,
    description: &str,
) -> Result<()> {
    if held_file_identity(file)? != expected {
        return Err(Error::InvalidFormat {
            details: format!("{description} held file descriptor changed identity"),
        });
    }
    Ok(())
}

/// Removes a publication only when its pathname still names the inode that
/// this process proved it had just linked. A concurrent replacement is never
/// unlinked on the strength of an older observation.
pub(super) fn remove_published_link_if_same(
    directory: &Path,
    path: &Path,
    published_identity: Option<RegularFileIdentity>,
) -> Result<()> {
    if let Some(published_identity) = published_identity
        && path_object_identity(path).ok() == Some(published_identity)
    {
        std::fs::remove_file(path)?;
    }
    sync_directory_strict(directory)
}

/// Owns cleanup of an archive staging inode until that inode has passed full
/// validation. The held descriptor is captured immediately after lazy
/// `create_new` succeeds, including when the first write itself returns an
/// error. Drop never trusts the pathname alone: a substituted object is left
/// untouched for diagnosis.
pub(super) struct UncommittedArchiveStaging {
    pub(super) directory: PathBuf,
    pub(super) path: PathBuf,
    pub(super) held: Option<(File, RegularFileIdentity)>,
    pub(super) armed: bool,
}

impl UncommittedArchiveStaging {
    pub(super) fn new(directory: &Path, path: PathBuf) -> Self {
        Self {
            directory: directory.to_owned(),
            path,
            held: None,
            armed: true,
        }
    }

    pub(super) fn capture_created_file(&mut self, writer: &TarArchiveWriter) -> Result<()> {
        if self.held.is_some() {
            return Ok(());
        }
        let Some(file) = writer.created_file() else {
            return Ok(());
        };
        let held = file.try_clone()?;
        let identity = held_file_identity(&held)?;
        require_path_file_identity(&self.path, identity, "new archive staging file")?;
        self.held = Some((held, identity));
        Ok(())
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
        self.held = None;
    }
}

impl Drop for UncommittedArchiveStaging {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some((held, identity)) = &self.held else {
            return;
        };
        if held_file_identity(held).ok() == Some(*identity)
            && path_object_identity(&self.path).ok() == Some(*identity)
            && std::fs::remove_file(&self.path).is_ok()
        {
            let _ = sync_directory_strict(&self.directory);
        }
    }
}

/// Copies durability-relevant filesystem metadata from `source` onto an open
/// replacement file and proves the result before publication.
///
/// Unix cleanup may run as an administrator, so relying on create-time owner
/// or umask can publish a root-owned or unreadable repository file. Ownership
/// and permission mismatches therefore fail closed rather than being warned.
pub(crate) fn preserve_file_metadata(target: &File, source: &std::fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let current = target.metadata()?;
        if current.uid() != source.uid() || current.gid() != source.gid() {
            // SAFETY: `target` owns a live file descriptor for the staged
            // regular file, and uid/gid values come directly from stat(2).
            if unsafe { libc::fchown(target.as_raw_fd(), source.uid(), source.gid()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        target.set_permissions(std::fs::Permissions::from_mode(source.mode()))?;
        target.sync_all()?;
        let installed = target.metadata()?;
        if installed.uid() != source.uid()
            || installed.gid() != source.gid()
            || installed.mode() & 0o7777 != source.mode() & 0o7777
        {
            return Err(Error::InvalidFormat {
                details: "replacement file ownership or permissions differ from the source after preservation"
                    .to_owned(),
            });
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        target.set_permissions(source.permissions())?;
        target.sync_all()?;
        Ok(())
    }
}
