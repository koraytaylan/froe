//! Filesystem snapshots used to prove that repository reads are strictly
//! read-only, including content-preserving rewrites and metadata changes.

#![allow(
    dead_code,
    reason = "this module is compiled into every test binary that declares `support`, \
              but only the snapshot tests call it"
)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
#[cfg(unix)]
use std::time::Duration;
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

struct SnapshotTestDirectory(std::path::PathBuf);

impl SnapshotTestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
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

fn entry<'a>(snapshot: &'a DirectorySnapshot, name: &str) -> &'a EntrySnapshot {
    snapshot
        .entries
        .get(std::ffi::OsStr::new(name))
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

/// Verifies that every metadata component relied on by read-only tests is
/// independently capable of changing snapshot equality: change one field and
/// the snapshot must differ, so none is silently ignored.
#[allow(
    dead_code,
    reason = "called only by the package's dedicated snapshot integration target"
)]
#[cfg(unix)]
fn assert_every_metadata_field_is_observed(
    directory: &SnapshotTestDirectory,
    file_path: &Path,
    file_name: &str,
    content: &[u8],
) {
    let ctime_baseline = directory_snapshot(&directory.0);
    let before_changed = (
        entry(&ctime_baseline, file_name)
            .metadata
            .unix
            .changed_seconds,
        entry(&ctime_baseline, file_name)
            .metadata
            .unix
            .changed_nanoseconds,
    );
    let stable_mtime = std::fs::metadata(file_path)
        .expect("snapshot file metadata")
        .modified()
        .expect("snapshot file mtime");
    let mut attempts = 0;
    let ctime_result = loop {
        std::fs::write(file_path, content).expect("rewrite identical bytes for ctime");
        set_modified(file_path, stable_mtime);
        let candidate = directory_snapshot(&directory.0);
        let candidate_changed = (
            entry(&candidate, file_name).metadata.unix.changed_seconds,
            entry(&candidate, file_name)
                .metadata
                .unix
                .changed_nanoseconds,
        );
        if candidate_changed != before_changed {
            break candidate;
        }
        attempts += 1;
        assert!(
            attempts < 200,
            "filesystem ctime did not advance after repeated metadata changes"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        ctime_result.directory_metadata,
        ctime_baseline.directory_metadata
    );
    assert_eq!(
        entry(&ctime_result, file_name).bytes,
        entry(&ctime_baseline, file_name).bytes
    );
    assert_eq!(
        entry(&ctime_result, file_name).metadata.file_type,
        entry(&ctime_baseline, file_name).metadata.file_type
    );
    assert_eq!(
        entry(&ctime_result, file_name).metadata.len,
        entry(&ctime_baseline, file_name).metadata.len
    );
    assert_eq!(
        entry(&ctime_result, file_name).metadata.modified,
        entry(&ctime_baseline, file_name).metadata.modified
    );
    assert_eq!(
        entry(&ctime_result, file_name).metadata.readonly,
        entry(&ctime_baseline, file_name).metadata.readonly
    );
    assert_eq!(
        entry(&ctime_result, file_name).metadata.unix.device,
        entry(&ctime_baseline, file_name).metadata.unix.device
    );
    assert_eq!(
        entry(&ctime_result, file_name).metadata.unix.inode,
        entry(&ctime_baseline, file_name).metadata.unix.inode
    );
    assert_eq!(
        entry(&ctime_result, file_name).metadata.unix.mode,
        entry(&ctime_baseline, file_name).metadata.unix.mode
    );
    assert_eq!(
        entry(&ctime_result, file_name)
            .metadata
            .unix
            .modified_seconds,
        entry(&ctime_baseline, file_name)
            .metadata
            .unix
            .modified_seconds
    );
    assert_eq!(
        entry(&ctime_result, file_name)
            .metadata
            .unix
            .modified_nanoseconds,
        entry(&ctime_baseline, file_name)
            .metadata
            .unix
            .modified_nanoseconds
    );
    assert_ne!(ctime_result, ctime_baseline);
}

/// Unix-only: mode changes, inode replacement, and every timestamp or
/// ownership field the snapshot records must each show up on their own.
fn assert_unix_metadata_is_observed(
    directory: &SnapshotTestDirectory,
    file_path: &Path,
    file_name: &str,
    content: &[u8],
) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let before_mode_change = directory_snapshot(&directory.0);
        let permissions = std::fs::metadata(file_path)
            .expect("snapshot file metadata")
            .permissions();
        let changed_mode = permissions.mode() ^ 0o100;
        std::fs::set_permissions(file_path, std::fs::Permissions::from_mode(changed_mode))
            .expect("change snapshot file mode");
        let after_mode_change = directory_snapshot(&directory.0);
        assert_ne!(
            entry(&after_mode_change, file_name).metadata.unix.mode,
            entry(&before_mode_change, file_name).metadata.unix.mode
        );
        assert_ne!(after_mode_change, before_mode_change);

        let before_replacement = directory_snapshot(&directory.0);
        let replacement_path = directory.0.join("replacement");
        std::fs::write(&replacement_path, content).expect("write replacement file");
        let replaced_metadata = std::fs::metadata(file_path).expect("replaced file metadata");
        std::fs::set_permissions(&replacement_path, replaced_metadata.permissions())
            .expect("copy replacement mode");
        set_modified(
            &replacement_path,
            replaced_metadata.modified().expect("replaced file mtime"),
        );
        std::fs::rename(&replacement_path, file_path).expect("replace snapshot file");
        let after_replacement = directory_snapshot(&directory.0);
        assert_eq!(
            entry(&after_replacement, file_name).bytes,
            entry(&before_replacement, file_name).bytes
        );
        assert_ne!(
            entry(&after_replacement, file_name).metadata.unix.inode,
            entry(&before_replacement, file_name).metadata.unix.inode
        );
        assert_ne!(after_replacement, before_replacement);

        // Restore mtime after an in-place same-byte rewrite. The inode, mode,
        // contents, directory metadata, and final mtime remain equal, leaving
        // ctime as the only snapshot field that can detect the rewrite.
        assert_every_metadata_field_is_observed(directory, file_path, file_name, content);
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, file_path, file_name, content);
    }
}

/// Windows-only: the read-only attribute is the metadata bit the
/// snapshot must notice.
fn assert_windows_readonly_is_observed(
    directory: &SnapshotTestDirectory,
    file_path: &Path,
    file_name: &str,
    content: &[u8],
) {
    #[cfg(windows)]
    {
        let before_readonly_change = directory_snapshot(&directory.0);
        let original_permissions = std::fs::metadata(&file_path)
            .expect("snapshot file metadata")
            .permissions();
        let mut changed_permissions = original_permissions.clone();
        changed_permissions.set_readonly(!original_permissions.readonly());
        std::fs::set_permissions(&file_path, changed_permissions)
            .expect("change snapshot file readonly flag");
        let after_readonly_change = directory_snapshot(&directory.0);
        std::fs::set_permissions(&file_path, original_permissions)
            .expect("restore snapshot file readonly flag");
        assert_eq!(
            entry(&after_readonly_change, file_name).bytes,
            entry(&before_readonly_change, file_name).bytes
        );
        assert_eq!(
            after_readonly_change.directory_metadata,
            before_readonly_change.directory_metadata
        );
        assert_eq!(
            entry(&after_readonly_change, file_name).metadata.file_type,
            entry(&before_readonly_change, file_name).metadata.file_type
        );
        assert_eq!(
            entry(&after_readonly_change, file_name).metadata.len,
            entry(&before_readonly_change, file_name).metadata.len
        );
        assert_eq!(
            entry(&after_readonly_change, file_name).metadata.modified,
            entry(&before_readonly_change, file_name).metadata.modified
        );
        assert_ne!(
            entry(&after_readonly_change, file_name).metadata.readonly,
            entry(&before_readonly_change, file_name).metadata.readonly
        );
        assert_ne!(after_readonly_change, before_readonly_change);
        // The read-only attribute moves without the bytes moving, so the
        // fixture content is not read here. The parameter stays for symmetry
        // with the Unix assertion beside it.
        let _ = content;
    }
    #[cfg(not(windows))]
    {
        let _ = (directory, file_path, file_name, content);
    }
}

pub(crate) fn assert_snapshot_mutation_matrix(test_name: &str) {
    const FILE_NAME: &str = "repository-file";
    const CONTENT: &[u8] = b"unchanged repository bytes";
    use std::time::Duration;

    let directory = SnapshotTestDirectory::new(test_name);
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

    assert_unix_metadata_is_observed(&directory, &file_path, FILE_NAME, CONTENT);

    assert_windows_readonly_is_observed(&directory, &file_path, FILE_NAME, CONTENT);

    // A transient lock-like file leaves the final entry map unchanged. Only
    // containing-directory metadata can expose that create/delete cycle.
    let before_transient_entry = directory_snapshot(&directory.0);
    let transient_path = directory.0.join("transient-lock");
    let mut attempts = 0;
    let after_transient_entry = loop {
        std::fs::write(&transient_path, b"transient").expect("create transient entry");
        std::fs::remove_file(&transient_path).expect("remove transient entry");
        let candidate = directory_snapshot(&directory.0);
        if candidate.directory_metadata != before_transient_entry.directory_metadata {
            break candidate;
        }
        attempts += 1;
        assert!(
            attempts < 200,
            "directory metadata did not advance after repeated create/delete cycles"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        after_transient_entry.entries,
        before_transient_entry.entries
    );
    assert_ne!(
        after_transient_entry.directory_metadata,
        before_transient_entry.directory_metadata
    );
    assert_ne!(after_transient_entry, before_transient_entry);

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
