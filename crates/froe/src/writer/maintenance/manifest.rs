//! Upgrading the store manifest to version 2 atomically, with a
//! certificate proving the file never changed underneath the write.

use crate::error::{Error, Result};
use crate::writer::store_writer::{preserve_file_metadata, sync_directory_strict};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A staged upgrade: the certified source, the bytes that will replace it,
/// and the temporary holding them until the rename publishes it.
struct StagedManifestUpgrade {
    source: Vec<u8>,
    source_certificate: ManifestFileCertificate,
    output: Vec<u8>,
    temporary_path: PathBuf,
    temporary_certificate: ManifestFileCertificate,
    guard: UncommittedFile,
}

pub(super) fn upgrade_manifest_atomically(directory: &Path) -> Result<()> {
    let manifest_path = directory.join("manifest");
    let Some(staged) = stage_manifest_upgrade(directory, &manifest_path)? else {
        return Ok(());
    };
    publish_manifest_upgrade(directory, &manifest_path, staged)
}

/// Certifies the current manifest and writes its replacement to a
/// temporary, without touching the manifest itself.
///
/// Returns `None` when the store is already at version 2, which is the
/// only case in which no file is created at all.
fn stage_manifest_upgrade(
    directory: &Path,
    manifest_path: &Path,
) -> Result<Option<StagedManifestUpgrade>> {
    let metadata = std::fs::symlink_metadata(manifest_path)?;
    if !metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: format!("{} is not a regular file", manifest_path.display()),
        });
    }
    if crate::store::read_manifest_store_version(manifest_path)? >= 2 {
        return Ok(None);
    }
    let source = std::fs::read(manifest_path)?;
    let source_certificate = certify_manifest_file(
        manifest_path,
        &metadata,
        &source,
        ManifestFileAccess::Read,
        "source manifest",
    )?;
    let output = manifest_upgrade_bytes(&source);

    let (temporary_path, mut temporary) =
        create_exclusive_numbered_file(directory, "manifest.cleaning")?;
    let guard = UncommittedFile::new(temporary_path.clone());
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
    Ok(Some(StagedManifestUpgrade {
        source,
        source_certificate,
        output,
        temporary_path,
        temporary_certificate,
        guard,
    }))
}

/// Renames the staged replacement over the manifest and makes that
/// durable.
///
/// The order here is the safety argument and is meant to be read straight
/// down: every fault boundary is bracketed by a proof that both files are
/// still exactly what staging certified, so a crash at any cutpoint leaves
/// either the old manifest or the new one, never a blend.
fn publish_manifest_upgrade(
    directory: &Path,
    manifest_path: &Path,
    staged: StagedManifestUpgrade,
) -> Result<()> {
    let StagedManifestUpgrade {
        source,
        source_certificate,
        output,
        temporary_path,
        temporary_certificate,
        mut guard,
    } = staged;
    // Re-proven at every boundary below; naming it keeps six occurrences
    // from drifting apart in path or access mode.
    let recertify_installed = || -> Result<()> {
        temporary_certificate.recertify(
            manifest_path,
            &output,
            ManifestFileAccess::ReadWrite,
            "installed manifest replacement",
        )
    };

    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.temporary-durable")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.temporary-durable");
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.before-rename")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.before-rename");
    source_certificate.recertify(
        manifest_path,
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
    std::fs::rename(&temporary_path, manifest_path)?;
    recertify_installed()?;
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
    recertify_installed()?;
    sync_directory_strict(directory)?;
    recertify_installed()?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("manifest.rename-durable")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("manifest.rename-durable");
    recertify_installed()?;
    if crate::store::read_manifest_store_version(manifest_path)? != 2 {
        return Err(Error::InvalidFormat {
            details: "atomic manifest upgrade did not install store.version=2".to_owned(),
        });
    }
    recertify_installed()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Repository;
    use crate::writer::commit::create_checkpoint;
    use crate::writer::store_writer::WritableRepository;

    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::plan::*;

    use crate::writer::maintenance::prepared::*;

    use crate::writer::maintenance::test_support::*;

    #[test]
    fn manifest_upgrade_separates_a_trailing_properties_continuation() {
        let suffix = b"# upgraded atomically by froe cleanup\nstore.version=2\n";
        for (source, expected_prefix) in [
            (
                &b"custom.property=kept\\"[..],
                &b"custom.property=kept\\\n\n"[..],
            ),
            (
                &b"custom.property=kept\\\n"[..],
                &b"custom.property=kept\\\n\n"[..],
            ),
            (
                &b"custom.property=kept\\\r"[..],
                &b"custom.property=kept\\\r\n\n"[..],
            ),
        ] {
            let upgraded = manifest_upgrade_bytes(source);
            assert!(upgraded.starts_with(expected_prefix), "{upgraded:?}");
            assert!(upgraded.ends_with(suffix), "{upgraded:?}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn manifest_certificate_rejects_path_substitution_around_publication() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = TestDirectory::new("manifest-certificate-substitution");
        let canonical = directory.path.join("manifest");
        let canonical_bytes = b"custom.property=kept\nstore.version=1\n";
        std::fs::write(&canonical, canonical_bytes).expect("write canonical manifest");

        let staged = directory.path.join("manifest.cleaning.000");
        let expected = b"custom.property=kept\nstore.version=2\n";
        std::fs::write(&staged, expected).expect("write staged manifest");
        let staged_metadata = std::fs::symlink_metadata(&staged).expect("staged metadata");
        let certificate = certify_manifest_file(
            &staged,
            &staged_metadata,
            expected,
            ManifestFileAccess::ReadWrite,
            "staged manifest replacement",
        )
        .expect("certify staged manifest");

        let retained_inode = directory.path.join("retained-manifest-inode");
        std::fs::rename(&staged, &retained_inode).expect("move certified inode aside");
        std::fs::write(&staged, expected).expect("substitute same-byte staged manifest");
        let substituted_metadata =
            std::fs::symlink_metadata(&staged).expect("substituted staged metadata");
        assert_ne!(
            (substituted_metadata.dev(), substituted_metadata.ino()),
            (staged_metadata.dev(), staged_metadata.ino()),
            "the fixture must isolate identity checking from byte checking"
        );
        certificate
            .recertify(
                &staged,
                expected,
                ManifestFileAccess::ReadWrite,
                "staged manifest replacement",
            )
            .expect_err("same bytes on a different inode must not be publishable");
        assert_eq!(
            std::fs::read(&canonical).expect("read canonical manifest"),
            canonical_bytes,
            "a rejected staging substitution must leave the source canonical"
        );

        std::fs::remove_file(&staged).expect("remove substituted staging file");
        std::fs::rename(&retained_inode, &staged).expect("restore certified inode");
        certificate
            .recertify(
                &staged,
                expected,
                ManifestFileAccess::ReadWrite,
                "staged manifest replacement",
            )
            .expect("restored certified inode");

        let installed = directory.path.join("installed-manifest");
        std::fs::rename(&staged, &installed).expect("publish certified inode");
        certificate
            .recertify(
                &installed,
                expected,
                ManifestFileAccess::ReadWrite,
                "installed manifest replacement",
            )
            .expect("certificate follows the inode through rename");

        let displaced = directory.path.join("displaced-installed-manifest");
        std::fs::rename(&installed, &displaced).expect("displace installed inode");
        std::fs::write(&installed, expected).expect("substitute installed manifest");
        let installed_substitute =
            std::fs::symlink_metadata(&installed).expect("installed substitute metadata");
        assert_ne!(
            (installed_substitute.dev(), installed_substitute.ino()),
            (staged_metadata.dev(), staged_metadata.ino()),
            "the post-publication fixture must install a different inode"
        );
        certificate
            .recertify(
                &installed,
                expected,
                ManifestFileAccess::ReadWrite,
                "installed manifest replacement",
            )
            .expect_err("post-rename same-byte inode substitution must be detected");
    }
    #[test]
    fn manifest_staging_requires_exact_canonical_or_upgrade_bytes() {
        let directory = TestDirectory::repository("manifest-staging-proof");
        let canonical = b"custom.property=kept\nstore.version=1\n";
        std::fs::write(directory.path.join("manifest"), canonical)
            .expect("write version-one manifest");
        let identical = directory.path.join("manifest.cleaning.000");
        std::fs::write(&identical, canonical).expect("write identical staging manifest");
        let exact_upgrade = directory.path.join("manifest.cleaning.001");
        std::fs::write(&exact_upgrade, manifest_upgrade_bytes(canonical))
            .expect("write exact upgrade staging manifest");
        let divergent = directory.path.join("manifest.cleaning.002");
        std::fs::write(&divergent, b"store.version=2\noperator.data=lost\n")
            .expect("write divergent staging manifest");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);

        let plan = plan_compaction(&directory.path, &options).expect("plan stale manifests");
        let planned_names: Vec<_> = plan
            .actions()
            .iter()
            .filter_map(|action| match action {
                CompactionAction::RemoveTemporary { file_name, .. } => Some(file_name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            planned_names,
            ["manifest.cleaning.000", "manifest.cleaning.001"]
        );
        assert!(plan.warnings().iter().any(|warning| {
            warning.contains("manifest.cleaning.002") && warning.contains("not provably redundant")
        }));

        compact(&directory.path, options).expect("remove proven manifest staging files");
        assert!(!identical.exists());
        assert!(!exact_upgrade.exists());
        assert!(divergent.exists());
        assert_eq!(
            std::fs::read(directory.path.join("manifest")).expect("read canonical manifest"),
            canonical
        );
        Repository::open(&directory.path).expect("healthy repository");
    }

    /// The repair used to run before any store-version gate, so froe would
    /// rewrite the archives of a store it then declared itself unable to
    /// read. The library API reaches `prepare` without the lockless preview
    /// that would otherwise have refused.
    #[test]
    fn a_store_from_a_newer_oak_is_refused_before_any_repair() {
        let directory = TestDirectory::new("repair-newer-store");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));
        std::fs::write(
            directory.path.join("manifest"),
            "#from a newer Oak\nstore.version=3\n",
        )
        .expect("v3 manifest");
        let before = file_bytes(&directory.path);

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        match PreparedCompaction::prepare(&directory.path, options) {
            Ok(_) => panic!("a store version this reader does not support must be refused"),
            Err(error) => assert!(
                error.to_string().contains("newer"),
                "the refusal names the store version: {error}"
            ),
        }
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "and it is refused before a single archive is rewritten"
        );
    }

    /// The one-way v1 to v2 transition is charged when a rebuilt archive is
    /// about to become visible, not when one is merely predicted. A repair
    /// can still fail per archive for reasons no survey models, and paying an
    /// irreversible format change for a run that rebuilds nothing would leave
    /// the store damaged AND closed to an older Oak.
    #[test]
    fn a_repair_that_installs_nothing_does_not_upgrade_the_manifest() {
        let directory = TestDirectory::new("repair-v1-no-install");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));
        // Repairable by the survey, but the rebuild refuses: a staging
        // residue is found per number, after the survey has had its say.
        std::fs::write(
            directory.path.join("data00000a.tar.recovering"),
            b"an interrupted rebuild",
        )
        .expect("staging residue");
        std::fs::write(
            directory.path.join("manifest"),
            "#a version one store\nstore.version=1\n",
        )
        .expect("v1 manifest");
        let before = file_bytes(&directory.path);

        let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
        assert!(
            PreparedCompaction::prepare(&directory.path, options).is_err(),
            "the residue must refuse the run"
        );
        assert_eq!(
            crate::store::read_manifest_store_version(&directory.path.join("manifest"))
                .expect("read manifest"),
            1,
            "a run that installed no rebuilt archive must not have raised the store version"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "and nothing else moved either"
        );
    }

    #[test]
    fn a_head_moving_cleanup_upgrades_a_version_one_manifest_atomically() {
        let directory = TestDirectory::repository("manifest-upgrade");
        let store = WritableRepository::open(&directory.path).expect("open writer");
        create_checkpoint(&store, 1, &[]).expect("checkpoint");
        store.close().expect("close writer");
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            directory.path.join("manifest"),
            b"custom.property=kept\nstore.version=\\\n 1\n",
        )
        .expect("install Java-continuation version-one manifest");
        #[cfg(unix)]
        let source_identity = {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            std::fs::set_permissions(
                directory.path.join("manifest"),
                std::fs::Permissions::from_mode(0o640),
            )
            .expect("set manifest permissions");
            let metadata = std::fs::metadata(directory.path.join("manifest"))
                .expect("source manifest metadata");
            (metadata.uid(), metadata.gid())
        };
        let options =
            CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);
        let plan = plan_compaction(&directory.path, &options).expect("plan");
        assert!(
            plan.actions()
                .iter()
                .any(|action| matches!(action, CompactionAction::UpgradeManifest))
        );

        compact(&directory.path, options).expect("cleanup");

        let manifest = std::fs::read_to_string(directory.path.join("manifest"))
            .expect("read upgraded manifest");
        assert!(manifest.contains("custom.property=kept"));
        assert!(manifest.ends_with("store.version=2\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let metadata = std::fs::metadata(directory.path.join("manifest"))
                .expect("upgraded manifest metadata");
            assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
            assert_eq!((metadata.uid(), metadata.gid()), source_identity);
        }
        assert!(
            !std::fs::read_dir(&directory.path)
                .expect("read directory")
                .any(|entry| entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("manifest.cleaning."))
        );
        Repository::open(&directory.path).expect("healthy v2 repository");
    }
}
