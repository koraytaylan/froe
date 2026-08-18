//! Proving the journal, its staging file, and its backup are each still
//! the file this run certified, and the numbered names it may claim.

use super::{Error, File, Metadata, OpenOptions, Path, PathBuf, Read, Result};

pub(crate) fn open_regular_journal(path: &Path) -> Result<(File, Metadata)> {
    let link_metadata = std::fs::symlink_metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::InvalidFormat {
                details: format!("journal file {} does not exist", path.display()),
            }
        } else {
            Error::InputOutput(source)
        }
    })?;
    if !link_metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!("journal {} is not a regular file", path.display()),
        });
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::InvalidFormat {
            details: format!("journal {} is not a regular file", path.display()),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if (link_metadata.dev(), link_metadata.ino()) != (metadata.dev(), metadata.ino()) {
            return Err(Error::InvalidFormat {
                details: format!("journal {} changed identity while opening", path.display()),
            });
        }
    }
    Ok((file, metadata))
}

#[derive(Clone, Copy)]
pub(crate) enum StagingAccess {
    Read,
    ReadAppend,
}

pub(crate) struct JournalFileCertificate {
    pub(crate) held: File,
    #[cfg(unix)]
    pub(crate) device: u64,
    #[cfg(unix)]
    pub(crate) inode: u64,
}

impl JournalFileCertificate {
    pub(in crate::writer) fn recertify(
        &self,
        path: &Path,
        expected_bytes: &[u8],
        access: StagingAccess,
        label: &str,
    ) -> Result<()> {
        let held_metadata = self.held.metadata()?;
        if !held_metadata.is_file() {
            return Err(Error::InvalidFormat {
                details: format!("held {label} is no longer regular"),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if (held_metadata.dev(), held_metadata.ino()) != (self.device, self.inode) {
                return Err(Error::InvalidFormat {
                    details: format!("held {label} changed identity"),
                });
            }
        }
        let recertified =
            open_verified_journal_file(path, &held_metadata, expected_bytes, access, label)?;
        drop(recertified);
        Ok(())
    }
}

/// Reopens a prepared journal file through its pathname and proves that the
/// service identity can use it after publication. Matching uid/gid/mode is not
/// sufficient when the source relies on ACLs that are not copied.
pub(crate) fn certify_journal_file(
    path: &Path,
    expected_identity: &Metadata,
    expected_bytes: &[u8],
    access: StagingAccess,
    label: &str,
) -> Result<JournalFileCertificate> {
    let held = open_verified_journal_file(path, expected_identity, expected_bytes, access, label)?;
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt as _;
        let held_metadata = held.metadata()?;
        (held_metadata.dev(), held_metadata.ino())
    };
    Ok(JournalFileCertificate {
        held,
        #[cfg(unix)]
        device,
        #[cfg(unix)]
        inode,
    })
}

pub(crate) fn open_verified_journal_file(
    path: &Path,
    expected_identity: &Metadata,
    expected_bytes: &[u8],
    access: StagingAccess,
    label: &str,
) -> Result<File> {
    #[cfg(not(unix))]
    let _ = expected_identity;
    let link_metadata = std::fs::symlink_metadata(path)?;
    if !link_metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!("{label} {} is not regular", path.display()),
        });
    }

    let mut options = OpenOptions::new();
    options.read(true);
    if matches!(access, StagingAccess::ReadAppend) {
        options.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut reopened = options.open(path)?;
    let reopened_metadata = reopened.metadata()?;
    if !reopened_metadata.is_file() {
        return Err(Error::InvalidFormat {
            details: format!("reopened {label} {} is not regular", path.display()),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let expected = (expected_identity.dev(), expected_identity.ino());
        let path_identity = (link_metadata.dev(), link_metadata.ino());
        let reopened_identity = (reopened_metadata.dev(), reopened_metadata.ino());
        if path_identity != expected || reopened_identity != expected {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{label} {} changed identity after it was prepared",
                    path.display()
                ),
            });
        }
    }

    let mut actual_bytes = Vec::new();
    reopened.read_to_end(&mut actual_bytes)?;
    if actual_bytes != expected_bytes {
        return Err(Error::InvalidFormat {
            details: format!(
                "{label} {} differs from the exact prepared bytes",
                path.display()
            ),
        });
    }
    Ok(reopened)
}

pub(crate) fn create_numbered_file(
    directory: &Path,
    stem: &str,
) -> Result<(PathBuf, File, UncommittedFile)> {
    for counter in 0..1000 {
        let path = directory.join(format!("{stem}.{counter:03}"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                let guard = UncommittedFile::new(path.clone());
                return Ok((path, file, guard));
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(source.into()),
        }
    }
    Err(Error::InvalidFormat {
        details: format!("all numbered names for {stem} (000-999) are occupied"),
    })
}

pub(crate) fn sync_directory_strict(directory: &Path) -> Result<()> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

pub(crate) struct UncommittedFile {
    pub(crate) path: PathBuf,
    pub(crate) committed: bool,
}

impl UncommittedFile {
    pub(in crate::writer) fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    pub(in crate::writer) fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for UncommittedFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::maintenance::journal::rewrite_journal_atomically;
    #[cfg(unix)]
    use crate::writer::maintenance::journal::scan::scan_raw_journal;
    use crate::writer::maintenance::journal::test_support::{FIRST, TestDirectory};

    #[cfg(unix)]
    #[test]
    fn journal_certificates_reject_staging_and_backup_substitution() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = TestDirectory::new("certificate-substitution");
        let canonical_bytes = format!("{FIRST} root 1\nignored\n");
        directory.write_journal(canonical_bytes.as_bytes());
        let canonical = directory.path.join("journal.log");

        let expected = format!("{FIRST} root 1\n");
        let staged = directory.path.join("journal.log.cleaning.000");
        std::fs::write(&staged, expected.as_bytes()).expect("write staged journal");
        let staged_metadata = std::fs::symlink_metadata(&staged).expect("staged metadata");
        let staged_certificate = certify_journal_file(
            &staged,
            &staged_metadata,
            expected.as_bytes(),
            StagingAccess::ReadAppend,
            "staged journal replacement",
        )
        .expect("certify staged journal");

        let retained_inode = directory.path.join("retained-journal-inode");
        std::fs::rename(&staged, &retained_inode).expect("move certified inode aside");
        std::fs::write(&staged, expected.as_bytes()).expect("substitute same-byte staged journal");
        let substituted_metadata =
            std::fs::symlink_metadata(&staged).expect("substituted staged metadata");
        assert_ne!(
            (substituted_metadata.dev(), substituted_metadata.ino()),
            (staged_metadata.dev(), staged_metadata.ino()),
            "the fixture must isolate identity checking from byte checking"
        );
        staged_certificate
            .recertify(
                &staged,
                expected.as_bytes(),
                StagingAccess::ReadAppend,
                "staged journal replacement",
            )
            .expect_err("same bytes on a different inode must not be publishable");
        assert_eq!(
            std::fs::read(&canonical).expect("read canonical journal"),
            canonical_bytes.as_bytes(),
            "a rejected staging substitution must leave the source canonical"
        );

        std::fs::remove_file(&staged).expect("remove substituted staging file");
        std::fs::rename(&retained_inode, &staged).expect("restore certified inode");
        let installed = directory.path.join("installed-journal");
        std::fs::rename(&staged, &installed).expect("publish certified journal inode");
        staged_certificate
            .recertify(
                &installed,
                expected.as_bytes(),
                StagingAccess::ReadAppend,
                "installed journal replacement",
            )
            .expect("certificate follows the journal inode through rename");

        let backup = directory.path.join("journal.log.bak.000");
        std::fs::write(&backup, canonical_bytes.as_bytes()).expect("write journal backup");
        let backup_metadata = std::fs::symlink_metadata(&backup).expect("backup metadata");
        let backup_certificate = certify_journal_file(
            &backup,
            &backup_metadata,
            canonical_bytes.as_bytes(),
            StagingAccess::Read,
            "journal recovery backup",
        )
        .expect("certify journal backup");
        std::fs::write(&backup, b"tampered backup bytes\n")
            .expect("mutate backup without changing its inode");
        let mutated_backup_metadata =
            std::fs::symlink_metadata(&backup).expect("mutated backup metadata");
        assert_eq!(
            (mutated_backup_metadata.dev(), mutated_backup_metadata.ino()),
            (backup_metadata.dev(), backup_metadata.ino()),
            "the fixture must isolate byte checking from identity checking"
        );
        backup_certificate
            .recertify(
                &backup,
                canonical_bytes.as_bytes(),
                StagingAccess::Read,
                "journal recovery backup",
            )
            .expect_err("same-inode backup byte mutation must be detected");

        let displaced = directory.path.join("displaced-installed-journal");
        std::fs::rename(&installed, &displaced).expect("displace installed journal inode");
        std::fs::write(&installed, expected.as_bytes())
            .expect("substitute installed journal with the same bytes");
        let installed_substitute =
            std::fs::symlink_metadata(&installed).expect("installed substitute metadata");
        assert_ne!(
            (installed_substitute.dev(), installed_substitute.ino()),
            (staged_metadata.dev(), staged_metadata.ino()),
            "the post-publication fixture must install a different inode"
        );
        staged_certificate
            .recertify(
                &installed,
                expected.as_bytes(),
                StagingAccess::ReadAppend,
                "installed journal replacement",
            )
            .expect_err("post-rename same-byte inode substitution must be detected");
    }

    #[test]
    fn occupied_temporary_name_is_never_truncated() {
        let directory = TestDirectory::new("occupied-temp");
        let bytes = format!("{FIRST} root 1\nignored\n");
        directory.write_journal(bytes.as_bytes());
        let occupied = directory.path.join("journal.log.cleaning.000");
        std::fs::write(&occupied, b"do not truncate").expect("occupy temporary name");
        let occupied_backup = directory.path.join("journal.log.bak.000");
        std::fs::write(&occupied_backup, b"do not overwrite").expect("occupy backup name");

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        let outcome = rewrite_journal_atomically(&journal, &[0]).expect("rewrite journal");

        assert_eq!(
            std::fs::read(occupied).expect("read occupied temporary"),
            b"do not truncate"
        );
        assert_eq!(
            std::fs::read(occupied_backup).expect("read occupied backup"),
            b"do not overwrite"
        );
        assert_eq!(
            outcome.backup_path.as_deref(),
            Some(directory.path.join("journal.log.bak.001").as_path())
        );
        assert!(!directory.path.join("journal.log.cleaning.001").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_refuses_a_replacement_the_service_user_cannot_reopen_for_append() {
        use std::os::unix::fs::PermissionsExt;

        // Root can bypass ordinary mode checks, so this access regression is
        // meaningful only for the service-user execution cleanup requires.
        // SAFETY: geteuid has no preconditions and does not access memory.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let directory = TestDirectory::new("replacement-reopen-access");
        let bytes = format!("{FIRST} root 1\nignored\n");
        directory.write_journal(bytes.as_bytes());
        let journal_path = directory.path.join("journal.log");
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o400))
            .expect("make source readable but not appendable");
        let journal = scan_raw_journal(&directory.path).expect("scan readable source journal");

        rewrite_journal_atomically(&journal, &[0])
            .expect_err("a staged canonical journal must be reopenable for append");

        assert_eq!(
            std::fs::read(&journal_path).expect("read unchanged canonical journal"),
            bytes.as_bytes(),
            "the inaccessible replacement must fail before canonical rename"
        );
        assert!(
            !std::fs::read_dir(&directory.path)
                .expect("list journal directory")
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                .any(|name| name.starts_with("journal.log.cleaning.")
                    || name.starts_with("journal.log.bak.")),
            "the uncommitted staging guard removes the rejected replacement"
        );
    }
}
