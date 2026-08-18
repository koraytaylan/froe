//! Everything maintenance does with the journal.
//!
//! [`analysis`] decides which of its lines a run removes and where the
//! retention boundary falls; the rest of this module carries out the
//! rewrite, byte for byte.
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

mod analysis;
mod file_identity;
mod scan;
#[cfg(test)]
mod test_support;

pub(in crate::writer::maintenance) use analysis::*;
pub(crate) use file_identity::*;
pub(crate) use scan::*;

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

/// Atomically installs only the selected syntactic record lines.
///
/// Indexes may be supplied in any order; output always follows original file
/// order. Duplicate, out-of-range, or non-record indexes are rejected. At
/// least one record must survive. An originally unterminated final retained
/// record receives one `LF`; existing `LF`, `CRLF`, and bare-`CR` terminators
/// remain byte-exact. Before replacement, the original bytes are copied to the
/// first unused numbered backup, and both files are durably synced.
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

    let staged = stage_journal_replacement(snapshot, &output)?;
    publish_journal_replacement(
        snapshot,
        &output,
        source_certificate,
        staged,
        JournalRewriteCounts {
            retained_record_count: retained.len(),
            removed_line_count,
        },
    )
}

/// What the caller already counted, carried through publication into the
/// outcome it reports.
#[derive(Clone, Copy)]
pub(crate) struct JournalRewriteCounts {
    pub(crate) retained_record_count: usize,
    pub(crate) removed_line_count: usize,
}

/// The replacement and the recovery backup, written and certified but not
/// yet published.
pub(crate) struct StagedJournal {
    pub(crate) directory: PathBuf,
    pub(crate) temporary_path: PathBuf,
    pub(crate) temporary_certificate: JournalFileCertificate,
    pub(crate) temporary_guard: UncommittedFile,
    pub(crate) backup_path: PathBuf,
    pub(crate) backup_certificate: JournalFileCertificate,
    pub(crate) backup_guard: UncommittedFile,
}

/// Writes the replacement and the recovery backup beside the journal,
/// certifying each, without touching the journal itself.
pub(crate) fn stage_journal_replacement(
    snapshot: &RawJournal,
    output: &[u8],
) -> Result<StagedJournal> {
    let directory = snapshot.path.parent().ok_or_else(|| Error::InvalidFormat {
        details: format!("journal path {} has no parent", snapshot.path.display()),
    })?;
    let (temporary_path, mut temporary, temporary_guard) =
        create_numbered_file(directory, "journal.log.cleaning")?;
    temporary.write_all(output)?;
    preserve_file_metadata(&temporary, &snapshot.metadata)?;
    let temporary_identity = temporary.metadata()?;
    drop(temporary);
    let temporary_certificate = certify_journal_file(
        &temporary_path,
        &temporary_identity,
        output,
        StagingAccess::ReadAppend,
        "staged journal replacement",
    )?;
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.temporary-durable")?;
    #[cfg(test)]
    crate::writer::fault_injection::crash_if_armed("journal.temporary-durable");

    // Preparing the replacement before reserving a backup name means an
    // exhausted temporary namespace cannot leave an unnecessary backup.
    let (backup_path, mut backup, backup_guard) =
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
    Ok(StagedJournal {
        directory: directory.to_path_buf(),
        temporary_path,
        temporary_certificate,
        temporary_guard,
        backup_path,
        backup_certificate,
        backup_guard,
    })
}

/// Renames the staged replacement over the journal and makes that durable.
///
/// The order below is the safety argument and reads straight down: every
/// fault boundary is bracketed by a proof that the staged bytes and the
/// backup are still exactly what staging certified.
/// The two certificates publication re-proves at every fault boundary.
///
/// Named as a pair because the proof must not drift: the pre-rename path is
/// the temporary and the post-rename path is the journal itself, and the
/// recovery backup is checked alongside both.
pub(crate) struct PublicationProofs<'proof> {
    pub(crate) snapshot: &'proof RawJournal,
    pub(crate) output: &'proof [u8],
    pub(crate) temporary_path: &'proof Path,
    pub(crate) temporary_certificate: &'proof JournalFileCertificate,
    pub(crate) backup_path: &'proof Path,
    pub(crate) backup_certificate: &'proof JournalFileCertificate,
}

impl PublicationProofs<'_> {
    /// Before the rename: the replacement is still at the temporary.
    pub(super) fn staged_replacement_holds(&self) -> Result<()> {
        self.temporary_certificate.recertify(
            self.temporary_path,
            self.output,
            StagingAccess::ReadAppend,
            "staged journal replacement",
        )?;
        self.recovery_backup_holds()
    }

    /// After the rename: the replacement is the journal.
    pub(super) fn installed_replacement_holds(&self) -> Result<()> {
        self.temporary_certificate.recertify(
            &self.snapshot.path,
            self.output,
            StagingAccess::ReadAppend,
            "installed journal replacement",
        )?;
        self.recovery_backup_holds()
    }

    pub(super) fn recovery_backup_holds(&self) -> Result<()> {
        self.backup_certificate.recertify(
            self.backup_path,
            &self.snapshot.source_bytes,
            StagingAccess::Read,
            "journal recovery backup",
        )
    }
}

pub(crate) fn publish_journal_replacement(
    snapshot: &RawJournal,
    output: &[u8],
    source_certificate: JournalFileCertificate,
    staged: StagedJournal,
    counts: JournalRewriteCounts,
) -> Result<JournalRewriteOutcome> {
    let StagedJournal {
        directory,
        temporary_path,
        temporary_certificate,
        mut temporary_guard,
        backup_path,
        backup_certificate,
        mut backup_guard,
    } = staged;
    let directory = directory.as_path();
    let proofs = PublicationProofs {
        snapshot,
        output,
        temporary_path: &temporary_path,
        temporary_certificate: &temporary_certificate,
        backup_path: &backup_path,
        backup_certificate: &backup_certificate,
    };
    let recertify_staged_replacement = || proofs.staged_replacement_holds();
    let recertify_installed_replacement = || proofs.installed_replacement_holds();
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.backup-file-durable")?;
    #[cfg(test)]
    crate::writer::fault_injection::crash_if_armed("journal.backup-file-durable");
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.before-pre-rename-directory-sync")?;
    sync_directory_strict(directory)?;
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.after-pre-rename-directory-sync")?;
    recertify_staged_replacement()?;
    backup_guard.commit();
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.pre-rename-directory-durable")?;
    #[cfg(test)]
    crate::writer::fault_injection::crash_if_armed("journal.pre-rename-directory-durable");

    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.before-rename")?;
    source_certificate.recertify(
        &snapshot.path,
        &snapshot.source_bytes,
        StagingAccess::Read,
        "source journal",
    )?;
    recertify_staged_replacement()?;
    std::fs::rename(&temporary_path, &snapshot.path)?;
    recertify_installed_replacement()?;
    drop(source_certificate);
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.after-rename")?;
    #[cfg(test)]
    crate::writer::fault_injection::crash_if_armed("journal.renamed-before-directory-sync");
    temporary_guard.commit();
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.before-post-rename-directory-sync")?;
    recertify_installed_replacement()?;
    sync_directory_strict(directory)?;
    recertify_installed_replacement()?;
    #[cfg(test)]
    crate::writer::fault_injection::fail_if_armed("journal.after-post-rename-directory-sync")?;
    recertify_installed_replacement()?;
    #[cfg(test)]
    crate::writer::fault_injection::crash_if_armed("journal.rename-durable");

    Ok(JournalRewriteOutcome {
        changed: true,
        backup_path: Some(backup_path),
        retained_record_count: counts.retained_record_count,
        removed_line_count: counts.removed_line_count,
        bytes_written: output.len(),
    })
}

pub(crate) fn validate_retained_indexes(
    snapshot: &RawJournal,
    indexes: &[usize],
) -> Result<BTreeSet<usize>> {
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

pub(crate) fn assemble_replacement(snapshot: &RawJournal, retained: &BTreeSet<usize>) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::rewrite_journal_atomically;
    #[cfg(unix)]
    use crate::writer::maintenance::journal::scan::scan_raw_journal;
    use crate::writer::maintenance::journal::test_support::{FIRST, SECOND, TestDirectory};

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
}
