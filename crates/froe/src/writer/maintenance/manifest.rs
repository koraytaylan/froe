//! Upgrading the store manifest to version 2 atomically, with a
//! certificate proving the file never changed underneath the write.

use crate::error::{Error, Result};
use crate::writer::store_writer::{preserve_file_metadata, sync_directory_strict};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[allow(
    clippy::too_many_lines,
    reason = "source/staging certification and every durability cutpoint form one atomic publication protocol"
)]
pub(super) fn upgrade_manifest_atomically(directory: &Path) -> Result<()> {
    let manifest_path = directory.join("manifest");
    let metadata = std::fs::symlink_metadata(&manifest_path)?;
    if !metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!("{} is not a regular file", manifest_path.display()),
        });
    }
    if crate::store::read_manifest_store_version(&manifest_path)? >= 2 {
        return Ok(());
    }
    let source = std::fs::read(&manifest_path)?;
    let source_certificate = certify_manifest_file(
        &manifest_path,
        &metadata,
        &source,
        ManifestFileAccess::Read,
        "source manifest",
    )?;
    let output = manifest_upgrade_bytes(&source);

    let (temporary_path, mut temporary) =
        create_exclusive_numbered_file(directory, "manifest.cleaning")?;
    let mut guard = UncommittedFile::new(temporary_path.clone());
    temporary.write_all(&output)?;
    preserve_file_metadata(&temporary, &metadata)?;
    let temporary_identity = temporary.metadata()?;
    drop(temporary);
    let temporary_certificate = certify_manifest_file(
        &temporary_path,
        &temporary_identity,
        &output,
        ManifestFileAccess::ReadWrite,
        "staged manifest replacement",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.temporary-durable")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.temporary-durable");
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.before-rename")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.before-rename");
    source_certificate.recertify(
        &manifest_path,
        &source,
        ManifestFileAccess::Read,
        "source manifest",
    )?;
    temporary_certificate.recertify(
        &temporary_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "staged manifest replacement",
    )?;
    std::fs::rename(&temporary_path, &manifest_path)?;
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    drop(source_certificate);
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "manifest.renamed-before-directory-sync",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed(
        "manifest.renamed-before-directory-sync",
    );
    guard.commit();
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "manifest.before-post-rename-directory-sync",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed(
        "manifest.before-post-rename-directory-sync",
    );
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    sync_directory_strict(directory)?;
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.rename-durable")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.rename-durable");
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    if crate::store::read_manifest_store_version(&manifest_path)? != 2 {
        return Err(Error::InvalidFormat {
            details: "atomic manifest upgrade did not install store.version=2".to_owned(),
        });
    }
    temporary_certificate.recertify(
        &manifest_path,
        &output,
        ManifestFileAccess::ReadWrite,
        "installed manifest replacement",
    )?;
    Ok(())
}

pub(super) fn manifest_upgrade_bytes(source: &[u8]) -> Vec<u8> {
    let mut output = source.to_vec();
    if !output.is_empty() {
        // Properties.load joins a physical line ending in an odd number of
        // backslashes with the next natural line. Install an empty natural
        // line before our comment so a trailing continuation cannot consume
        // it and alter the preceding custom property's value.
        match output.last() {
            Some(b'\n') => output.push(b'\n'),
            Some(_) => output.extend_from_slice(b"\n\n"),
            None => unreachable!("non-empty output has a final byte"),
        }
    }
    output.extend_from_slice(b"# upgraded atomically by froe cleanup\nstore.version=2\n");
    output
}

#[derive(Clone, Copy)]
pub(super) enum ManifestFileAccess {
    Read,
    ReadWrite,
}

pub(super) struct ManifestFileCertificate {
    pub(super) held: File,
    #[cfg(unix)]
    pub(super) device: u64,
    #[cfg(unix)]
    pub(super) inode: u64,
}

impl ManifestFileCertificate {
    pub(super) fn recertify(
        &self,
        path: &Path,
        expected: &[u8],
        access: ManifestFileAccess,
        label: &str,
    ) -> Result<()> {
        let held_metadata = self.held.metadata()?;
        if !held_metadata.file_type().is_file() {
            return Err(Error::InvalidFormat {
                details: format!("held {label} is no longer regular"),
            });
        }
        #[cfg(unix)]
        if (held_metadata.dev(), held_metadata.ino()) != (self.device, self.inode) {
            return Err(Error::InvalidFormat {
                details: format!("held {label} changed identity before publication"),
            });
        }
        let recertified =
            open_verified_manifest_file(path, &held_metadata, expected, access, label)?;
        drop(recertified);
        Ok(())
    }
}

pub(super) fn certify_manifest_file(
    path: &Path,
    expected_identity: &Metadata,
    expected: &[u8],
    access: ManifestFileAccess,
    label: &str,
) -> Result<ManifestFileCertificate> {
    let held = open_verified_manifest_file(path, expected_identity, expected, access, label)?;
    #[cfg(unix)]
    let held_metadata = held.metadata()?;
    Ok(ManifestFileCertificate {
        held,
        #[cfg(unix)]
        device: held_metadata.dev(),
        #[cfg(unix)]
        inode: held_metadata.ino(),
    })
}

pub(super) fn open_verified_manifest_file(
    path: &Path,
    expected_identity: &Metadata,
    expected: &[u8],
    access: ManifestFileAccess,
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
    if matches!(access, ManifestFileAccess::ReadWrite) {
        options.write(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let reopened_metadata = file.metadata()?;
    if !reopened_metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!("{label} {} is not regular", path.display()),
        });
    }
    #[cfg(unix)]
    if (link_metadata.dev(), link_metadata.ino())
        != (expected_identity.dev(), expected_identity.ino())
        || (reopened_metadata.dev(), reopened_metadata.ino())
            != (expected_identity.dev(), expected_identity.ino())
    {
        return Err(Error::InvalidFormat {
            details: format!(
                "{label} {} changed identity before publication",
                path.display()
            ),
        });
    }
    let mut actual = Vec::new();
    file.read_to_end(&mut actual)?;
    if actual != expected {
        return Err(Error::InvalidFormat {
            details: format!("{label} {} changed before publication", path.display()),
        });
    }
    Ok(file)
}

pub(super) fn create_exclusive_numbered_file(
    directory: &Path,
    stem: &str,
) -> Result<(PathBuf, File)> {
    for counter in 0..1000u16 {
        let path = directory.join(format!("{stem}.{counter:03}"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::InvalidFormat {
        details: format!("all numbered names for {stem} (000-999) are occupied"),
    })
}

pub(super) struct UncommittedFile {
    pub(super) path: PathBuf,
    pub(super) committed: bool,
}

impl UncommittedFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn commit(&mut self) {
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
