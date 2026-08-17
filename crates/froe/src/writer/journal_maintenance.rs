//! Byte-preserving journal inspection and replacement for offline cleanup.
//!
//! The ordinary journal reader intentionally presents a tolerant, semantic
//! view. Cleanup needs a second view: it must be able to identify lines that
//! the reader ignores without normalising any line that it keeps. This module
//! therefore treats `journal.log` as bytes and carries each physical line,
//! including its original line ending, through to an atomic replacement.

use std::collections::BTreeSet;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::journal::parse_record_identifier_text;
use crate::segment::record::RecordIdentifier;
use crate::writer::store_writer::preserve_file_metadata;

/// A byte-exact snapshot of an existing `journal.log`.
#[derive(Debug)]
pub(crate) struct RawJournal {
    path: PathBuf,
    source_bytes: Vec<u8>,
    metadata: Metadata,
    lines: Vec<RawJournalLine>,
}

impl RawJournal {
    /// Returns the physical lines in their original, oldest-first order.
    pub(crate) fn lines(&self) -> &[RawJournalLine] {
        &self.lines
    }

    /// Returns every source byte exactly as it appeared on disk.
    pub(crate) fn source_bytes(&self) -> &[u8] {
        &self.source_bytes
    }
}

/// One physical journal line, including its original line terminator.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RawJournalLine {
    raw_bytes: Vec<u8>,
    content_length: usize,
    classification: RawJournalLineClassification,
}

impl RawJournalLine {
    /// Returns the complete line, including `LF`, `CRLF`, or bare `CR`.
    #[allow(
        dead_code,
        reason = "the byte-exact line accessor is part of the cleanup scanner's internal contract"
    )]
    pub(crate) fn raw_bytes(&self) -> &[u8] {
        &self.raw_bytes
    }

    /// Returns the line without its terminator.
    #[allow(
        dead_code,
        reason = "callers inspecting unusual journal syntax need the un-terminated bytes"
    )]
    pub(crate) fn content_bytes(&self) -> &[u8] {
        &self.raw_bytes[..self.content_length]
    }

    /// Returns the reader-compatible classification of this line.
    pub(crate) fn classification(&self) -> &RawJournalLineClassification {
        &self.classification
    }
}

/// Why the ordinary journal reader would retain or ignore a physical line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum RawJournalLineClassification {
    /// The line contains no ASCII space and is skipped before field parsing.
    ParserSkippedNoSpace,
    /// The line has fields, but its first field is not a record identifier.
    InvalidRecordIdentifier {
        /// The exact, possibly non-UTF-8 first field.
        revision_text: Vec<u8>,
    },
    /// The line is a syntactic journal record, even if its timestamp is bad.
    Record(RawJournalRecord),
}

/// Parsed fields needed by cleanup while the enclosing line retains the bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RawJournalRecord {
    /// The validated record identifier from the first field.
    pub(crate) record_identifier: RecordIdentifier,
    /// The exact bytes of the first field.
    pub(crate) revision_text: Vec<u8>,
    /// The exact bytes of the second (historical tag) field.
    pub(crate) tag: Vec<u8>,
    /// The third-field timestamp classification.
    pub(crate) timestamp: RawJournalTimestamp,
}

/// Timestamp parsing that preserves the distinction between absent and bad.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum RawJournalTimestamp {
    /// The line has no third space-separated field.
    Missing,
    /// A third field exists but is not an `i64` timestamp.
    Malformed {
        /// The exact third-field bytes.
        raw: Vec<u8>,
    },
    /// The third field is a valid millisecond timestamp.
    Milliseconds {
        /// The exact third-field bytes, before numeric parsing.
        raw: Vec<u8>,
        /// The parsed signed millisecond value.
        value: i64,
    },
}

/// The durable result of replacing a journal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct JournalRewriteOutcome {
    /// Whether the installed journal bytes changed.
    pub(crate) changed: bool,
    /// The recovery backup created for a changed journal.
    pub(crate) backup_path: Option<PathBuf>,
    /// Number of syntactic record lines retained.
    pub(crate) retained_record_count: usize,
    /// Number of physical input lines not retained.
    pub(crate) removed_line_count: usize,
    /// Number of bytes in the installed journal.
    pub(crate) bytes_written: usize,
}

/// Scans `journal.log` without creating, changing, or following any file.
///
/// The returned line indexes are stable for the lifetime of the snapshot and
/// are the indexes accepted by [`rewrite_journal_atomically`].
pub(crate) fn scan_raw_journal(directory: &Path) -> Result<RawJournal> {
    scan_raw_journal_file(&directory.join("journal.log"))
}

/// Scans an explicitly named journal-shaped staging file without following
/// links. Cleanup uses this to prove that every physical staging line is
/// already represented by the canonical journal before deleting it.
pub(crate) fn scan_raw_journal_file(path: &Path) -> Result<RawJournal> {
    let path = path.to_owned();
    let (mut file, metadata) = open_regular_journal(&path)?;
    let mut source_bytes = Vec::new();
    file.read_to_end(&mut source_bytes)?;
    let lines = split_and_classify_lines(&source_bytes);
    Ok(RawJournal {
        path,
        source_bytes,
        metadata,
        lines,
    })
}

/// Atomically installs only the selected syntactic record lines.
///
/// Indexes may be supplied in any order; output always follows original file
/// order. Duplicate, out-of-range, or non-record indexes are rejected. At
/// least one record must survive. An originally unterminated final retained
/// record receives one `LF`; existing `LF`, `CRLF`, and bare-`CR` terminators
/// remain byte-exact. Before replacement, the original bytes are copied to the
/// first unused numbered backup, and both files are durably synced.
#[allow(
    clippy::too_many_lines,
    reason = "source, staging, backup, rename, and durability recertification form one ordered publication protocol"
)]
pub(crate) fn rewrite_journal_atomically(
    snapshot: &RawJournal,
    retained_record_line_indexes: &[usize],
) -> Result<JournalRewriteOutcome> {
    let retained = validate_retained_indexes(snapshot, retained_record_line_indexes)?;
    let output = assemble_replacement(snapshot, &retained);

    // Do not turn a stale plan into a write. The cleanup session also guards
    // broader repository state, but this local check makes the primitive safe
    // to use on its own while the repository lock is held.
    let source_certificate = certify_journal_file(
        &snapshot.path,
        &snapshot.metadata,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "source journal",
    )?;

    let removed_line_count = snapshot.lines.len() - retained.len();
    if output == snapshot.source_bytes {
        return Ok(JournalRewriteOutcome {
            changed: false,
            backup_path: None,
            retained_record_count: retained.len(),
            removed_line_count,
            bytes_written: output.len(),
        });
    }

    let directory = snapshot.path.parent().ok_or_else(|| Error::InvalidFormat {
        details: format!("journal path {} has no parent", snapshot.path.display()),
    })?;
    let (temporary_path, mut temporary, mut temporary_guard) =
        create_numbered_file(directory, "journal.log.cleaning")?;
    temporary.write_all(&output)?;
    preserve_file_metadata(&temporary, &snapshot.metadata)?;
    let temporary_identity = temporary.metadata()?;
    drop(temporary);
    let temporary_certificate = certify_journal_file(
        &temporary_path,
        &temporary_identity,
        &output,
        StagingAccess::ReadAppend,
        "staged journal replacement",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("journal.temporary-durable")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("journal.temporary-durable");

    // Preparing the replacement before reserving a backup name means an
    // exhausted temporary namespace cannot leave an unnecessary backup.
    let (backup_path, mut backup, mut backup_guard) =
        create_numbered_file(directory, "journal.log.bak")?;
    backup.write_all(&snapshot.source_bytes)?;
    preserve_file_metadata(&backup, &snapshot.metadata)?;
    let backup_identity = backup.metadata()?;
    drop(backup);
    let backup_certificate = certify_journal_file(
        &backup_path,
        &backup_identity,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "journal recovery backup",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("journal.backup-file-durable")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("journal.backup-file-durable");
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "journal.before-pre-rename-directory-sync",
    )?;
    sync_directory_strict(directory)?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "journal.after-pre-rename-directory-sync",
    )?;
    temporary_certificate.recertify(
        &temporary_path,
        &output,
        StagingAccess::ReadAppend,
        "staged journal replacement",
    )?;
    backup_certificate.recertify(
        &backup_path,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "journal recovery backup",
    )?;
    backup_guard.commit();
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "journal.pre-rename-directory-durable",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed(
        "journal.pre-rename-directory-durable",
    );

    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("journal.before-rename")?;
    source_certificate.recertify(
        &snapshot.path,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "source journal",
    )?;
    temporary_certificate.recertify(
        &temporary_path,
        &output,
        StagingAccess::ReadAppend,
        "staged journal replacement",
    )?;
    backup_certificate.recertify(
        &backup_path,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "journal recovery backup",
    )?;
    std::fs::rename(&temporary_path, &snapshot.path)?;
    temporary_certificate.recertify(
        &snapshot.path,
        &output,
        StagingAccess::ReadAppend,
        "installed journal replacement",
    )?;
    backup_certificate.recertify(
        &backup_path,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "journal recovery backup",
    )?;
    drop(source_certificate);
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed("journal.after-rename")?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed(
        "journal.renamed-before-directory-sync",
    );
    temporary_guard.commit();
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "journal.before-post-rename-directory-sync",
    )?;
    temporary_certificate.recertify(
        &snapshot.path,
        &output,
        StagingAccess::ReadAppend,
        "installed journal replacement",
    )?;
    backup_certificate.recertify(
        &backup_path,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "journal recovery backup",
    )?;
    sync_directory_strict(directory)?;
    temporary_certificate.recertify(
        &snapshot.path,
        &output,
        StagingAccess::ReadAppend,
        "installed journal replacement",
    )?;
    backup_certificate.recertify(
        &backup_path,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "journal recovery backup",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::fail_if_armed(
        "journal.after-post-rename-directory-sync",
    )?;
    temporary_certificate.recertify(
        &snapshot.path,
        &output,
        StagingAccess::ReadAppend,
        "installed journal replacement",
    )?;
    backup_certificate.recertify(
        &backup_path,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "journal recovery backup",
    )?;
    #[cfg(test)]
    crate::writer::maintenance_fault_injection::crash_if_armed("journal.rename-durable");

    Ok(JournalRewriteOutcome {
        changed: true,
        backup_path: Some(backup_path),
        retained_record_count: retained.len(),
        removed_line_count,
        bytes_written: output.len(),
    })
}

fn validate_retained_indexes(snapshot: &RawJournal, indexes: &[usize]) -> Result<BTreeSet<usize>> {
    if indexes.is_empty() {
        return Err(Error::InvalidFormat {
            details: "journal cleanup would remove every syntactic record".to_owned(),
        });
    }
    let mut retained = BTreeSet::new();
    for &index in indexes {
        let Some(line) = snapshot.lines.get(index) else {
            return Err(Error::InvalidFormat {
                details: format!("journal cleanup selected nonexistent line index {index}"),
            });
        };
        if !matches!(line.classification, RawJournalLineClassification::Record(_)) {
            return Err(Error::InvalidFormat {
                details: format!("journal cleanup selected non-record line index {index}"),
            });
        }
        if !retained.insert(index) {
            return Err(Error::InvalidFormat {
                details: format!("journal cleanup selected line index {index} more than once"),
            });
        }
    }
    Ok(retained)
}

fn assemble_replacement(snapshot: &RawJournal, retained: &BTreeSet<usize>) -> Vec<u8> {
    let retained_bytes = snapshot
        .lines
        .iter()
        .enumerate()
        .filter(|(index, _)| retained.contains(index))
        .map(|(_, line)| line.raw_bytes.len())
        .sum::<usize>();
    let mut output = Vec::with_capacity(retained_bytes.saturating_add(1));
    for (index, line) in snapshot.lines.iter().enumerate() {
        if retained.contains(&index) {
            output.extend_from_slice(&line.raw_bytes);
        }
    }
    if !matches!(output.last(), Some(b'\n' | b'\r')) {
        output.push(b'\n');
    }
    output
}

fn split_and_classify_lines(source: &[u8]) -> Vec<RawJournalLine> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    while cursor < source.len() {
        let terminator_length = match source[cursor] {
            b'\r' if source.get(cursor + 1) == Some(&b'\n') => 2,
            b'\n' | b'\r' => 1,
            _ => {
                cursor += 1;
                continue;
            }
        };
        let end = cursor + terminator_length;
        lines.push(make_line(&source[start..end], cursor - start));
        start = end;
        cursor = end;
    }
    if start < source.len() {
        lines.push(make_line(&source[start..], source.len() - start));
    }
    lines
}

fn make_line(raw: &[u8], content_length: usize) -> RawJournalLine {
    let content = &raw[..content_length];
    RawJournalLine {
        raw_bytes: raw.to_vec(),
        content_length,
        classification: classify_line(content),
    }
}

fn classify_line(content: &[u8]) -> RawJournalLineClassification {
    if !content.contains(&b' ') {
        return RawJournalLineClassification::ParserSkippedNoSpace;
    }
    let mut fields = content.split(|byte| *byte == b' ');
    let revision_text = fields.next().unwrap_or_default().to_vec();
    let tag = fields.next().unwrap_or_default().to_vec();
    let record_identifier = std::str::from_utf8(&revision_text)
        .ok()
        .and_then(parse_record_identifier_text);
    let Some(record_identifier) = record_identifier else {
        return RawJournalLineClassification::InvalidRecordIdentifier { revision_text };
    };
    let timestamp = match fields.next() {
        None => RawJournalTimestamp::Missing,
        Some(raw) => match std::str::from_utf8(raw)
            .ok()
            .and_then(|text| text.parse::<i64>().ok())
        {
            Some(value) => RawJournalTimestamp::Milliseconds {
                raw: raw.to_vec(),
                value,
            },
            None => RawJournalTimestamp::Malformed { raw: raw.to_vec() },
        },
    };
    RawJournalLineClassification::Record(RawJournalRecord {
        record_identifier,
        revision_text,
        tag,
        timestamp,
    })
}

fn open_regular_journal(path: &Path) -> Result<(File, Metadata)> {
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
enum StagingAccess {
    Read,
    ReadAppend,
}

struct JournalFileCertificate {
    held: File,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl JournalFileCertificate {
    fn recertify(
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
fn certify_journal_file(
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

fn open_verified_journal_file(
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

fn create_numbered_file(directory: &Path, stem: &str) -> Result<(PathBuf, File, UncommittedFile)> {
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

fn sync_directory_strict(directory: &Path) -> Result<()> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

struct UncommittedFile {
    path: PathBuf,
    committed: bool,
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
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        RawJournalLineClassification, RawJournalTimestamp, rewrite_journal_atomically,
        scan_raw_journal,
    };
    #[cfg(unix)]
    use super::{StagingAccess, certify_journal_file};

    const FIRST: &str = "11111111-1111-4111-a111-111111111111:1";
    const SECOND: &str = "22222222-2222-4222-a222-222222222222:2";

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "froe-journal-maintenance-{name}-{}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("create test directory");
            Self { path }
        }

        fn write_journal(&self, bytes: &[u8]) {
            std::fs::write(self.path.join("journal.log"), bytes).expect("write journal");
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn scanner_preserves_every_raw_byte_and_classifies_ignored_lines() {
        let directory = TestDirectory::new("raw");
        let bytes = format!("{FIRST} root 1\nno-space\rnot-a-record root 2\n{SECOND} custom 3");
        directory.write_journal(bytes.as_bytes());

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        assert_eq!(journal.source_bytes(), bytes.as_bytes());
        assert_eq!(
            journal
                .lines()
                .iter()
                .flat_map(super::RawJournalLine::raw_bytes)
                .copied()
                .collect::<Vec<_>>(),
            bytes.as_bytes()
        );
        assert!(matches!(
            journal.lines()[0].classification(),
            RawJournalLineClassification::Record(_)
        ));
        assert!(matches!(
            journal.lines()[1].classification(),
            RawJournalLineClassification::ParserSkippedNoSpace
        ));
        assert!(matches!(
            journal.lines()[2].classification(),
            RawJournalLineClassification::InvalidRecordIdentifier { .. }
        ));
        assert_eq!(
            journal.lines()[3].content_bytes(),
            format!("{SECOND} custom 3").as_bytes()
        );
    }

    #[test]
    fn malformed_or_missing_timestamp_does_not_invalidate_a_record() {
        let directory = TestDirectory::new("timestamp");
        let bytes = format!("{FIRST} tag not-a-number\n{SECOND} tag\n");
        directory.write_journal(bytes.as_bytes());

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        let RawJournalLineClassification::Record(first) = journal.lines()[0].classification()
        else {
            panic!("malformed timestamp must remain a record");
        };
        assert_eq!(first.tag, b"tag");
        assert_eq!(
            first.timestamp,
            RawJournalTimestamp::Malformed {
                raw: b"not-a-number".to_vec()
            }
        );
        let RawJournalLineClassification::Record(second) = journal.lines()[1].classification()
        else {
            panic!("missing timestamp must remain a record");
        };
        assert_eq!(second.timestamp, RawJournalTimestamp::Missing);
    }

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
    fn invalid_identifiers_are_distinct_from_parser_skipped_lines() {
        let directory = TestDirectory::new("invalid-id");
        directory.write_journal(b"garbage\ngarbage root 12\n");

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        assert!(matches!(
            journal.lines()[0].classification(),
            RawJournalLineClassification::ParserSkippedNoSpace
        ));
        assert_eq!(
            journal.lines()[1].classification(),
            &RawJournalLineClassification::InvalidRecordIdentifier {
                revision_text: b"garbage".to_vec()
            }
        );
    }

    #[test]
    fn crlf_and_duplicate_records_survive_byte_for_byte() {
        let directory = TestDirectory::new("crlf-duplicates");
        let bytes = format!("{FIRST} root +01\r\n{FIRST} root +01\r\n");
        directory.write_journal(bytes.as_bytes());

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        assert_eq!(journal.lines().len(), 2);
        assert_eq!(journal.lines()[0], journal.lines()[1]);
        let outcome = rewrite_journal_atomically(&journal, &[1, 0]).expect("no-op rewrite");
        assert!(!outcome.changed);
        assert_eq!(outcome.backup_path, None);
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("read journal"),
            bytes.as_bytes()
        );
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

    #[test]
    fn rewrite_is_atomic_byte_preserving_and_creates_a_synced_backup() {
        let directory = TestDirectory::new("rewrite");
        let bytes = format!("ignored\r\n{FIRST} unusual bad\r\n{SECOND} root 2");
        directory.write_journal(bytes.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                directory.path.join("journal.log"),
                std::fs::Permissions::from_mode(0o640),
            )
            .expect("set journal permissions");
        }
        #[cfg(unix)]
        let source_identity = {
            use std::os::unix::fs::MetadataExt;
            let metadata =
                std::fs::metadata(directory.path.join("journal.log")).expect("source metadata");
            (metadata.uid(), metadata.gid())
        };

        let journal = scan_raw_journal(&directory.path).expect("scan journal");
        let outcome = rewrite_journal_atomically(&journal, &[1, 2]).expect("rewrite journal");

        assert!(outcome.changed);
        assert_eq!(outcome.retained_record_count, 2);
        assert_eq!(outcome.removed_line_count, 1);
        assert_eq!(
            outcome.backup_path.as_deref(),
            Some(directory.path.join("journal.log.bak.000").as_path())
        );
        assert_eq!(
            std::fs::read(directory.path.join("journal.log.bak.000")).expect("read backup"),
            bytes.as_bytes()
        );
        let expected = format!("{FIRST} unusual bad\r\n{SECOND} root 2\n");
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("read replacement"),
            expected.as_bytes()
        );
        assert_eq!(outcome.bytes_written, expected.len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            for path in [
                directory.path.join("journal.log"),
                directory.path.join("journal.log.bak.000"),
            ] {
                let metadata = std::fs::metadata(path).expect("replacement metadata");
                assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
                assert_eq!((metadata.uid(), metadata.gid()), source_identity);
            }
        }
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

    #[cfg(unix)]
    #[test]
    fn scanner_rejects_a_symlinked_journal() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("symlink");
        std::fs::write(directory.path.join("target"), b"not the journal").expect("write target");
        symlink("target", directory.path.join("journal.log")).expect("create symlink");

        assert!(scan_raw_journal(&directory.path).is_err());
    }
}
