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

#[cfg(test)]
mod tests {
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::repository_lock::RepositoryLock;
    use crate::writer::store_writer::repository::*;
    use crate::writer::store_writer::sweep_plan::*;
    use crate::writer::store_writer::test_support::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    #[cfg(unix)]
    #[test]
    fn swept_archive_preserves_source_owner_group_and_mode_before_publication() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = TestDirectory::new("sweep-file-metadata");
        let root = data_identifier(85);
        let old_one = data_identifier(86);
        let old_two = data_identifier(87);
        let reference = generation(4, 4, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(root, 64, reference),
                TestArchiveEntry::new(old_one, 64, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 64, generation(0, 0, false)),
            ],
        );
        let source_path = directory.path.join("data00000a.tar");
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o640))
            .expect("set distinctive source mode");
        let source_metadata = std::fs::metadata(&source_path).expect("source metadata");
        write_manifest(&directory);

        let plan = plan_cleanup_from_directory(&directory.path, reference, root, &HashSet::new())
            .expect("plan rewrite");
        assert!(matches!(
            plan.archives.as_slice(),
            [PlannedArchiveSweep::Rewrite { .. }]
        ));
        apply_cleanup_from_directory(
            &directory.path,
            reference,
            root,
            &HashSet::new(),
            Some(&plan),
        )
        .expect("publish metadata-preserving rewrite");

        let replacement_path = directory.path.join("data00000b.tar");
        let replacement_metadata =
            std::fs::metadata(&replacement_path).expect("replacement metadata");
        assert_eq!(replacement_metadata.uid(), source_metadata.uid());
        assert_eq!(replacement_metadata.gid(), source_metadata.gid());
        assert_eq!(
            replacement_metadata.mode() & 0o7777,
            source_metadata.mode() & 0o7777
        );
        assert_eq!(replacement_metadata.mode() & 0o7777, 0o640);
        assert!(
            std::fs::read_dir(&directory.path)
                .expect("list repository")
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().contains(".cleaning.")),
            "successful publication removes its non-active staging link"
        );
        let replacement = TarArchiveReader::open(&replacement_path).expect("open replacement");
        assert!(!replacement.is_recovered());
        assert!(replacement.contains_segment(root));
    }

    #[cfg(unix)]
    #[test]
    fn prepared_rotated_archives_inherit_active_archive_metadata_before_commit() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = TestDirectory::new("prepared-archive-metadata");
        let previous_head = {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            let head = store.head();
            store.close().expect("close bootstrap");
            head
        };
        let source_path = directory.path.join("data00000a.tar");
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o640))
            .expect("set source mode");
        let source_metadata = std::fs::metadata(&source_path).expect("source metadata");

        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let mut store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        store.maximum_archive_size = 1;
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let content_root = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("content root");
        let new_head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("rotation finalizes archive");
        assert!(
            store.lock_write_state().tar_writer.is_none(),
            "the tiny threshold exercises the rotation close path"
        );
        assert!(store.compare_and_set_head(previous_head, new_head));
        store.flush().expect("validate then commit prepared head");

        let created_metadata =
            std::fs::metadata(directory.path.join("data00001a.tar")).expect("created archive");
        assert_eq!(created_metadata.uid(), source_metadata.uid());
        assert_eq!(created_metadata.gid(), source_metadata.gid());
        assert_eq!(
            created_metadata.mode() & 0o7777,
            source_metadata.mode() & 0o7777
        );
        store.close().expect("close prepared writer");
        drop(repository_lock);

        let repository = Repository::open(&directory.path).expect("reopen committed store");
        assert_eq!(repository.head_record_identifier(), new_head);
        repository.content_root().expect("new root is traversable");
    }
}
