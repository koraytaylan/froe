//! Filesystem snapshots used to prove that repository reads are strictly
//! read-only, including content-preserving rewrites and metadata changes.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, PartialEq, Eq)]
enum SnapshotFileType {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, PartialEq, Eq)]
struct MetadataSnapshot {
    file_type: SnapshotFileType,
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    unix: UnixMetadataSnapshot,
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct UnixMetadataSnapshot {
    device: u64,
    inode: u64,
    mode: u32,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Debug, PartialEq, Eq)]
struct EntrySnapshot {
    bytes: Option<Vec<u8>>,
    metadata: MetadataSnapshot,
}

/// Captures repository entries and the containing directory without observing
/// access times, which can change merely because the snapshot reads a file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DirectorySnapshot {
    directory_metadata: MetadataSnapshot,
    entries: BTreeMap<OsString, EntrySnapshot>,
}

fn metadata_snapshot(metadata: &std::fs::Metadata) -> MetadataSnapshot {
    let file_type = metadata.file_type();
    let file_type = if file_type.is_file() {
        SnapshotFileType::File
    } else if file_type.is_dir() {
        SnapshotFileType::Directory
    } else if file_type.is_symlink() {
        SnapshotFileType::Symlink
    } else {
        SnapshotFileType::Other
    };

    MetadataSnapshot {
        file_type,
        len: metadata.len(),
        modified: metadata.modified().ok(),
        readonly: metadata.permissions().readonly(),
        #[cfg(unix)]
        unix: unix_metadata_snapshot(metadata),
    }
}

#[cfg(unix)]
fn unix_metadata_snapshot(metadata: &std::fs::Metadata) -> UnixMetadataSnapshot {
    use std::os::unix::fs::MetadataExt as _;

    UnixMetadataSnapshot {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

/// Takes a content-and-metadata snapshot of one repository directory.
pub(crate) fn directory_snapshot(path: &Path) -> DirectorySnapshot {
    let directory_metadata =
        metadata_snapshot(&std::fs::symlink_metadata(path).expect("read directory metadata"));
    let entries = std::fs::read_dir(path)
        .expect("read directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let entry_path = entry.path();
            let metadata =
                std::fs::symlink_metadata(&entry_path).expect("read repository entry metadata");
            let bytes = metadata
                .file_type()
                .is_file()
                .then(|| std::fs::read(&entry_path).expect("read repository file"));
            (
                entry.file_name(),
                EntrySnapshot {
                    bytes,
                    metadata: metadata_snapshot(&metadata),
                },
            )
        })
        .collect();
    DirectorySnapshot {
        directory_metadata,
        entries,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use super::{DirectorySnapshot, SnapshotFileType, directory_snapshot};

    struct SnapshotTestDirectory(PathBuf);

    impl SnapshotTestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("froe-filesystem-snapshot-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create snapshot test directory");
            Self(path)
        }
    }

    impl Drop for SnapshotTestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry<'a>(snapshot: &'a DirectorySnapshot, name: &str) -> &'a super::EntrySnapshot {
        snapshot
            .entries
            .get(OsStr::new(name))
            .expect("snapshot entry")
    }

    fn set_modified(path: &Path, timestamp: SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open snapshot file")
            .set_modified(timestamp)
            .expect("set snapshot file mtime");
    }

    #[test]
    fn equality_detects_content_preserving_filesystem_mutations() {
        const FILE_NAME: &str = "repository-file";
        const CONTENT: &[u8] = b"unchanged repository bytes";

        let directory = SnapshotTestDirectory::new();
        let file_path = directory.0.join(FILE_NAME);
        std::fs::write(&file_path, CONTENT).expect("write snapshot fixture");
        set_modified(
            &file_path,
            SystemTime::UNIX_EPOCH + Duration::from_secs(946_684_800),
        );

        let before_rewrite = directory_snapshot(&directory.0);
        std::fs::write(&file_path, CONTENT).expect("rewrite identical bytes");
        let after_rewrite = directory_snapshot(&directory.0);
        assert_eq!(
            entry(&after_rewrite, FILE_NAME).bytes,
            entry(&before_rewrite, FILE_NAME).bytes
        );
        assert_ne!(
            entry(&after_rewrite, FILE_NAME).metadata.modified,
            entry(&before_rewrite, FILE_NAME).metadata.modified
        );
        assert_ne!(after_rewrite, before_rewrite);

        let before_mtime_change = directory_snapshot(&directory.0);
        set_modified(
            &file_path,
            SystemTime::UNIX_EPOCH + Duration::from_secs(978_307_200),
        );
        let after_mtime_change = directory_snapshot(&directory.0);
        assert_ne!(
            entry(&after_mtime_change, FILE_NAME).metadata.modified,
            entry(&before_mtime_change, FILE_NAME).metadata.modified
        );
        assert_ne!(after_mtime_change, before_mtime_change);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let before_mode_change = directory_snapshot(&directory.0);
            let permissions = std::fs::metadata(&file_path)
                .expect("snapshot file metadata")
                .permissions();
            let changed_mode = permissions.mode() ^ 0o100;
            std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(changed_mode))
                .expect("change snapshot file mode");
            let after_mode_change = directory_snapshot(&directory.0);
            assert_ne!(
                entry(&after_mode_change, FILE_NAME).metadata.unix.mode,
                entry(&before_mode_change, FILE_NAME).metadata.unix.mode
            );
            assert_ne!(after_mode_change, before_mode_change);

            let before_replacement = directory_snapshot(&directory.0);
            let replacement_path = directory.0.join("replacement");
            std::fs::write(&replacement_path, CONTENT).expect("write replacement file");
            let replaced_metadata = std::fs::metadata(&file_path).expect("replaced file metadata");
            std::fs::set_permissions(&replacement_path, replaced_metadata.permissions())
                .expect("copy replacement mode");
            set_modified(
                &replacement_path,
                replaced_metadata.modified().expect("replaced file mtime"),
            );
            std::fs::rename(&replacement_path, &file_path).expect("replace snapshot file");
            let after_replacement = directory_snapshot(&directory.0);
            assert_eq!(
                entry(&after_replacement, FILE_NAME).bytes,
                entry(&before_replacement, FILE_NAME).bytes
            );
            assert_ne!(
                entry(&after_replacement, FILE_NAME).metadata.unix.inode,
                entry(&before_replacement, FILE_NAME).metadata.unix.inode
            );
            assert_ne!(after_replacement, before_replacement);
        }

        let before_type_change = directory_snapshot(&directory.0);
        std::fs::remove_file(&file_path).expect("remove snapshot file");
        std::fs::create_dir(&file_path).expect("replace snapshot file with directory");
        let after_type_change = directory_snapshot(&directory.0);
        assert_eq!(
            entry(&before_type_change, FILE_NAME).metadata.file_type,
            SnapshotFileType::File
        );
        assert_eq!(
            entry(&after_type_change, FILE_NAME).metadata.file_type,
            SnapshotFileType::Directory
        );
        assert_ne!(after_type_change, before_type_change);
    }
}
