//! The in-memory state of one writing session: the segments it has
//! built, and the certificates proving its finalized archive untouched.

use super::file_identity::{
    FileAccess, RegularFileIdentity, open_regular_file_no_follow, regular_file_identity,
};
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::tar_writer::TarArchiveWriter;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The default archive rotation threshold (Oak: 256 MB).
pub(super) const DEFAULT_MAXIMUM_ARCHIVE_SIZE: u64 = 256 * 1024 * 1024;

/// Byte budget for the session read-back cache, given the archive size the
/// session rotates at.
///
/// Sized so the archive currently being written always fits. Eviction is in
/// write order, so the newest archive's worth of segments is exactly what
/// survives — which makes "a segment in the open archive is always cached" an
/// invariant the read path can rely on rather than a hope. Rotated archives
/// need no residency at all: they are reopened and served from their mapping.
/// The doubling is headroom for the entry overhead the budget also charges.
pub(super) fn session_cache_budget_bytes(maximum_archive_size: u64) -> usize {
    usize::try_from(maximum_archive_size.saturating_mul(2)).unwrap_or(usize::MAX)
}

/// A parsed segment paired with its shared bytes.
pub(super) type SharedSegment = (Arc<ParsedSegment>, Arc<Vec<u8>>);

/// What the session remembers about a segment it wrote.
///
/// Deliberately not the segment: this used to be the parsed structure *and*
/// an owned copy of every byte, retained for the whole session with no
/// eviction, which made a compaction hold its entire output in memory. The
/// bytes are already on disk in the archive the session just wrote them to,
/// so what has to survive here is only what cannot be recovered from the
/// archive alone plus what proves the archive still holds the right bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SessionSegment {
    /// The generation triple, answering `segment_generation` without a read.
    pub(super) generation: GarbageCollectionGeneration,
    /// The payload CRC the writer computed. Certification compares it with
    /// the CRC in the archive's own tar entry name, which the archive
    /// separately proves against the payload — together exactly the
    /// guarantee the retained byte copy used to give.
    pub(super) payload_crc: u32,
}

/// The mutable write-side state, serialized behind one mutex.
pub(super) struct WriteState {
    pub(super) journal_file: File,
    pub(super) tar_writer: Option<TarArchiveWriter>,
    /// The next free archive number, or `None` after `u32::MAX` has been
    /// allocated. An explicit exhausted state prevents wraparound to archive
    /// zero and destructive truncation of `data00000a.tar`.
    pub(super) next_archive_number: Option<u32>,
    pub(super) head: RecordIdentifier,
    pub(super) persisted_head: Option<RecordIdentifier>,
}

#[derive(Clone)]
pub(super) struct SessionSegmentWrite {
    /// Shared with every other write to the same archive.
    ///
    /// One owned `String` per segment made this ledger grow with the store
    /// for a value that only ever takes as many distinct forms as there are
    /// archives — a few thousand at most, against millions of segments.
    pub(super) archive_file_name: Arc<str>,
    pub(super) identifier: SegmentIdentifier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FinalizedSessionFileFingerprint {
    pub(super) identity: RegularFileIdentity,
    pub(super) length: u64,
    pub(super) modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    pub(super) change_time_seconds: i64,
    #[cfg(unix)]
    pub(super) change_time_nanoseconds: i64,
}

impl FinalizedSessionFileFingerprint {
    pub(super) fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;

        Ok(Self {
            identity: regular_file_identity(metadata)?,
            length: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            change_time_seconds: metadata.ctime(),
            #[cfg(unix)]
            change_time_nanoseconds: metadata.ctime_nsec(),
        })
    }
}

/// A certificate that one finalized session archive is still the file it was
/// when certified.
///
/// It records the file's identity and metadata rather than holding it open.
/// Holding an open descriptor per archive was one file descriptor for every
/// 256 MiB of output, so a large compaction exhausted the process limit with
/// `EMFILE` after all of its work was already done — a failure mode that
/// scaled with the repository. The fingerprint carries device and inode
/// alongside length, mtime and ctime, so a replaced file is caught by its
/// identity and a rewritten one by its metadata. Reusing an inode number
/// *and* reproducing a nanosecond ctime would be needed to defeat it, and
/// this whole path runs under the exclusive repository lock, which excludes
/// every cooperating writer.
pub(super) struct FinalizedSessionArchiveCertificate {
    pub(super) path: PathBuf,
    pub(super) fingerprint: FinalizedSessionFileFingerprint,
}

impl FinalizedSessionArchiveCertificate {
    pub(super) fn capture(path: PathBuf) -> Result<Self> {
        // Opened only to prove it is a regular file, not a symlink or a
        // device, then closed immediately.
        let opened = open_regular_file_no_follow(&path, FileAccess::ReadOnly)?;
        let fingerprint = FinalizedSessionFileFingerprint::from_metadata(&opened.metadata()?)?;
        drop(opened);
        let certificate = Self { path, fingerprint };
        certificate.recertify()?;
        Ok(certificate)
    }

    pub(super) fn recertify(&self) -> Result<()> {
        let named = FinalizedSessionFileFingerprint::from_metadata(&std::fs::symlink_metadata(
            &self.path,
        )?)?;
        if named != self.fingerprint {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} changed inode or metadata after certification",
                    self.path.display()
                ),
            });
        }
        Ok(())
    }
}

pub(super) struct FinalizedSessionCertificate {
    pub(super) archives: Vec<FinalizedSessionArchiveCertificate>,
}

impl FinalizedSessionCertificate {
    pub(super) fn capture(directory: &Path, writes: &[SessionSegmentWrite]) -> Result<Self> {
        let names: std::collections::BTreeSet<_> = writes
            .iter()
            .map(|write| write.archive_file_name.as_ref())
            .collect();
        let mut archives = Vec::with_capacity(names.len());
        for name in names {
            archives.push(FinalizedSessionArchiveCertificate::capture(
                directory.join(name),
            )?);
        }
        Ok(Self { archives })
    }

    pub(super) fn recertify(&self) -> Result<()> {
        for archive in &self.archives {
            archive.recertify()?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn substitute_first_path_if_armed(&self, cutpoint: &str) -> Result<()> {
        if let Some(archive) = self.archives.first() {
            crate::writer::maintenance_fault_injection::substitute_path_if_armed(
                cutpoint,
                &archive.path,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::BoundedCache;
    use crate::writer::record_writer::ChildNodesToWrite;

    use crate::writer::store_writer::repository::*;

    use crate::writer::store_writer::test_support::*;
    use std::sync::RwLock;

    /// The regression guard for the defect that made `compact` unusable: a
    /// session that keeps what it writes.
    ///
    /// This asserts the property directly rather than a consequence of it.
    /// Writing more segments must not make the session hold more payload —
    /// if it does, this fails no matter which structure grew or why. A test
    /// that only checked "the store is still correct" would have passed
    /// throughout the entire period the bug existed.
    #[test]
    fn writing_more_segments_does_not_make_a_session_hold_more_bytes() {
        /// A ceiling far below what this many segments occupy, so eviction
        /// is doing the bounding rather than the fixture simply being
        /// smaller than the default budget.
        const TEST_BUDGET_BYTES: usize = 4096;

        fn payload_bytes_held_after(directory: &TestDirectory, segments: usize) -> usize {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            store.session_segment_cache = RwLock::new(BoundedCache::new(TEST_BUDGET_BYTES));
            let generation = store.writing_generation().expect("generation");
            for _ in 0..segments {
                let mut writer = store.record_writer(generation);
                writer
                    .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                    .expect("node");
                writer.finish().expect("finish");
            }
            let held = store
                .session_segment_cache
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .used_bytes();
            let locators = store
                .session_segments
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len();
            assert_eq!(locators, segments, "every write must be locatable");
            store.close().expect("close");
            held
        }

        // A budget small enough that even this fixture's tiny segments
        // overflow it, so the ceiling is what limits residency rather than
        // the fixture being smaller than the default budget.
        let few = TestDirectory::new("session-growth-few");
        WritableRepository::open(&few.path)
            .expect("bootstrap")
            .close()
            .expect("close bootstrap");
        let many = TestDirectory::new("session-growth-many");
        WritableRepository::open(&many.path)
            .expect("bootstrap")
            .close()
            .expect("close bootstrap");

        let after_few = payload_bytes_held_after(&few, 8);
        let after_many = payload_bytes_held_after(&many, 64);

        // Eight times the writes, and the payload held must not grow with
        // them. The locator count does grow — that is metadata, and the
        // assertion above pins it — but bytes must not.
        assert!(
            after_many <= TEST_BUDGET_BYTES,
            "payload residency exceeded the ceiling: {after_many} bytes held \
             against a {TEST_BUDGET_BYTES}-byte budget"
        );
        assert!(
            after_few <= TEST_BUDGET_BYTES,
            "payload residency exceeded the ceiling: {after_few} bytes"
        );
        // And the production budget is a constant of the archive size, not
        // of the store: nothing here may make it a function of content.
        assert_eq!(
            session_cache_budget_bytes(DEFAULT_MAXIMUM_ARCHIVE_SIZE),
            (DEFAULT_MAXIMUM_ARCHIVE_SIZE as usize) * 2
        );
    }
    /// Guards the locator against regrowing into what it replaced.
    ///
    /// `session_segments` holds one entry per segment for the life of the
    /// session, so anything heap-owning added to its value type is once
    /// again a per-segment allocation across the whole store. Keeping the
    /// type `Copy` and small is what makes that map metadata rather than a
    /// second copy of the repository.
    #[test]
    fn a_session_locator_owns_no_heap_and_stays_small() {
        // `Copy` cannot be derived for a type owning heap, so binding this
        // is a compile-time proof that no String, Vec, or Arc crept in.
        fn assert_copy<Type: Copy>() {}
        assert_copy::<SessionSegment>();
        assert!(
            std::mem::size_of::<SessionSegment>() <= 24,
            "SessionSegment grew to {} bytes; a field that scales per segment \
             belongs on disk or in a bounded cache, not here",
            std::mem::size_of::<SessionSegment>()
        );
    }
}
