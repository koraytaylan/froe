//! `WritableRepository`: the lock-holding session a caller writes
//! records, segments, and journal entries through.

use super::archive_certificate::{
    ExpectedBinaryReferences, ExpectedGraph, certify_archives_in_parallel,
    stored_segment_generation, validate_exact_archive_trailers,
};
use super::archive_numbering::{next_physical_archive_number, validate_cleanup_archive_number};
use super::file_identity::{preserve_file_metadata, sync_directory_strict};
use super::providers::{
    ArchiveSegmentsProvider, BaseSourceCertification, CertifiedReclaimSources,
    read_blob_identifiers, seed_references_from_archive,
};
use super::reclaim::{
    ArchiveRewritePolicy, mark_one_archive, plan_archive_sweep, reject_duplicate_active_segments,
    unique_active_segment_locations,
};
use super::reclaim::{ReclaimPolicy, ReclaimRule};
use super::session::{
    DEFAULT_MAXIMUM_ARCHIVE_SIZE, FinalizedSessionCertificate, SessionSegment, SessionSegmentWrite,
    SharedSegment, WriteState, session_cache_budget_bytes,
};
use super::startup::{check_and_update_manifest, initialize_archives_for_writing};
#[cfg(test)]
use super::sweep::probe_archive_sweep_phase_boundary;
use super::sweep::sweep_one_archive;
use super::sweep_plan::ArchiveSweepOutcome;
use super::sweep_plan::{
    ArchiveSweepDisposition, GenerationReclaimRequest, PlannedArchiveSweep, RETAINED_GENERATIONS,
    SegmentSweepOutcome, sorted_sweep_plan,
};
use crate::cache::BoundedCache;
use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::journal::{JournalEntry, read_journal};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::segment::view::SegmentView;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::compaction::CompactionKind;
use crate::writer::record_writer::{ChildNodesToWrite, RecordWriter, SegmentSink};
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::segment_builder::{BuiltSegment, GarbageCollectionGeneration};
use crate::writer::tar_writer::TarArchiveWriter;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

mod head;
mod reclaim_session;
mod session;

pub(in crate::writer::store_writer) use head::*;
pub(crate) use reclaim_session::*;

/// A read-write segment store session holding the repository lock.
pub struct WritableRepository {
    pub(super) directory: PathBuf,
    pub(super) _repository_lock: Arc<RepositoryLock>,
    pub(super) maximum_archive_size: u64,
    /// Archives that existed before this session, newest first.
    pub(super) base_archives: Vec<TarArchiveReader>,
    /// Locators for segments written in this session — metadata only. The
    /// payloads live in the archives on disk; see [`SessionSegment`].
    pub(super) session_segments: RwLock<HashMap<SegmentIdentifier, SessionSegment>>,
    /// Recently written segments, kept for read-back under a byte budget.
    ///
    /// The budget is at least one archive's worth, so every segment in the
    /// archive still being written is resident and read-back never has to
    /// reach into the open writer. Rotated archives are served from their
    /// mappings in [`Self::session_archives`] instead.
    pub(super) session_segment_cache: RwLock<BoundedCache<SegmentIdentifier, SharedSegment>>,
    /// Archives this session finished and reopened, so their segments stay
    /// readable through a mapping rather than through retained buffers.
    pub(super) session_archives: RwLock<Vec<TarArchiveReader>>,
    /// Exact physical write order, including the archive rotation boundary
    /// for every session segment. Cleanup certification must preserve this
    /// order because later reverse-order marking is semantically significant.
    pub(super) session_segment_writes: RwLock<Vec<SessionSegmentWrite>>,
    /// Base segments parsed while this session reads them, under the same
    /// byte budget the read path uses.
    ///
    /// It was an unbounded `HashMap`, which made every base segment a
    /// compaction touched resident for the whole run: a deep copy reads
    /// every reachable segment, so the cache grew to a fraction of the live
    /// store. A miss re-parses from the archive mapping and touches no I/O.
    pub(super) parsed_segment_cache: RwLock<BoundedCache<SegmentIdentifier, Arc<ParsedSegment>>>,
    pub(super) write_state: Mutex<WriteState>,
    /// Cleanup checkpoint commits seal and validate their archive on disk
    /// before making the new head durable in the journal.
    pub(super) seal_archive_before_head: bool,
    /// Exact metadata inherited by every archive created by a prepared
    /// cleanup session. Normal write sessions retain their existing
    /// create-time behavior.
    pub(super) cleanup_archive_metadata: Option<std::fs::Metadata>,
    #[cfg(test)]
    pub(super) finalized_session_semantic_validations: std::sync::atomic::AtomicUsize,
}

/// The segment sink wiring a [`RecordWriter`] into a store.
pub struct StoreSink<'store> {
    pub(super) store: &'store WritableRepository,
}

impl SegmentSink for StoreSink<'_> {
    fn write_segment(&mut self, segment: BuiltSegment) -> Result<()> {
        self.store.persist_segment(segment)
    }
}

impl SegmentProvider for WritableRepository {
    fn segment(&self, segment_identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        if let Some((structure, bytes)) = self
            .session_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&segment_identifier)
        {
            return Ok(SegmentView {
                structure,
                bytes: crate::segment::view::SegmentBytes::Shared(bytes),
            });
        }
        // A session segment the cache no longer holds. It is on disk in one
        // of this session's archives, so re-read it there rather than keeping
        // every written byte resident against the chance of this lookup.
        if self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&segment_identifier)
        {
            return self.reread_session_segment(segment_identifier);
        }
        for archive in &self.base_archives {
            if let Some(bytes) = archive.segment_data(segment_identifier) {
                let cached = self
                    .parsed_segment_cache
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&segment_identifier);
                let structure = if let Some(structure) = cached {
                    structure
                } else {
                    let structure = Arc::new(ParsedSegment::parse(segment_identifier, bytes)?);
                    self.parsed_segment_cache
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(segment_identifier, Arc::clone(&structure));
                    structure
                };
                return Ok(SegmentView {
                    structure,
                    bytes: bytes.into(),
                });
            }
        }
        Err(Error::SegmentNotFound { segment_identifier })
    }

    fn string(&self, record_identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, record_identifier).map(Arc::from)
    }

    fn template(&self, record_identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, record_identifier).map(Arc::new)
    }
}

impl WritableRepository {
    /// Opens (or bootstraps) a segment store for writing.
    pub fn open(directory: &Path) -> Result<Self> {
        Self::open_with_progress(directory, &mut crate::progress::DiscardedProgress)
    }

    /// Opens exactly like [`WritableRepository::open`], reporting the
    /// archive scan to `observer`. Every mutating command goes through
    /// this open before it can do anything, and on a large store the scan
    /// is the first thing that makes the operator wait.
    pub fn open_with_progress(
        directory: &Path,
        observer: &mut dyn crate::progress::ProgressObserver,
    ) -> Result<Self> {
        std::fs::create_dir_all(directory)?;

        // Deliberate deviation from Java's open order, which opens the
        // journal writer (creating `journal.log`) *before* taking the
        // lock: the lock comes first here, so an open that loses the
        // lock race leaves nothing behind but the directory and
        // `repo.lock` itself — and the documented lock-first contract
        // holds. The ordering has no on-disk consequence in the success
        // path.
        let repository_lock = Arc::new(RepositoryLock::acquire(directory)?);
        let journal_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("journal.log"))?;

        // Writing needs collision-safe segment identifiers; without the
        // operating system entropy source there is no safe way to write.
        crate::writer::identifier_generator::verify_entropy_source()?;

        check_and_update_manifest(directory)?;

        let base_archives = initialize_archives_for_writing(directory, observer)?;
        let next_archive_number = next_physical_archive_number(directory, &base_archives)?;

        let store = Self {
            directory: directory.to_owned(),
            _repository_lock: repository_lock,
            maximum_archive_size: DEFAULT_MAXIMUM_ARCHIVE_SIZE,
            base_archives,
            session_segments: RwLock::new(HashMap::new()),
            session_segment_cache: RwLock::new(BoundedCache::new(session_cache_budget_bytes(
                DEFAULT_MAXIMUM_ARCHIVE_SIZE,
            ))),
            session_archives: RwLock::new(Vec::new()),
            session_segment_writes: RwLock::new(Vec::new()),
            parsed_segment_cache: RwLock::new(BoundedCache::new(
                crate::store::SEGMENT_CACHE_BUDGET_BYTES,
            )),
            seal_archive_before_head: false,
            cleanup_archive_metadata: None,
            #[cfg(test)]
            finalized_session_semantic_validations: std::sync::atomic::AtomicUsize::new(0),
            write_state: Mutex::new(WriteState {
                journal_file,
                tar_writer: None,
                next_archive_number,
                // Placeholder until binding below.
                head: RecordIdentifier::new(SegmentIdentifier::new(0, 0), 0),
                persisted_head: None,
            }),
        };

        // Bind: newest journal revision whose segment exists, or the
        // initial-node bootstrap for a fresh store. A journal that cannot
        // be *read* is a loud failure — silently bootstrapping would
        // append a head line pointing at a fresh empty root on top of a
        // populated store.
        let journal_entries = read_journal(&directory.join("journal.log"))?;
        let persisted = journal_entries
            .iter()
            .filter_map(JournalEntry::record_identifier)
            .find(|identifier| store.contains_segment(identifier.segment));
        if let Some(head) = persisted {
            let mut state = store.lock_write_state();
            state.head = head;
            state.persisted_head = Some(head);
        } else {
            // Deliberate deviation from Java's TarRevisions.bind, which
            // bootstraps a fresh initial node even when the archives hold
            // segments — silently replacing all reachable content with an
            // empty tree at the next flush. A populated store whose
            // journal has no resolvable line needs journal recovery, not
            // a new empty head; refusing loses nothing.
            if store
                .base_archives
                .iter()
                .any(|archive| archive.segment_count() > 0)
            {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "{} has segment archives but no journal revision resolves; refusing to \
                         bootstrap an empty head over existing content — run recover-journal \
                         first",
                        directory.display()
                    ),
                });
            }
            let head = store.write_initial_node()?;
            let mut state = store.lock_write_state();
            state.head = head;
            state.persisted_head = None;
        }
        Ok(store)
    }

    /// Opens an existing, already-validated repository while sharing a lock
    /// held by a larger maintenance transaction.
    ///
    /// Unlike [`Self::open`], this strict path has no archive-normalization,
    /// manifest-rewrite, journal-creation, directory-creation, recovery, or
    /// bootstrap side effects. Cleanup uses it only after those operations
    /// have been independently planned and authorized. The caller must keep
    /// its own clone of `repository_lock` alive for the complete transaction.
    pub(crate) fn open_prepared(
        directory: &Path,
        repository_lock: Arc<RepositoryLock>,
        certified_next_archive_number: u32,
    ) -> Result<Self> {
        if !directory.is_dir() {
            return Err(Error::InvalidFormat {
                details: format!("{} is not a repository directory", directory.display()),
            });
        }

        validate_cleanup_archive_number(directory, certified_next_archive_number)?;
        let archive_file_names = crate::store::list_archive_file_names(directory)?;
        crate::store::check_manifest(
            directory,
            crate::store::ArchivePresence::of(&archive_file_names),
        )?;
        crate::writer::identifier_generator::verify_entropy_source()?;

        let journal_path = directory.join("journal.log");
        let journal_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    Error::InvalidFormat {
                        details: format!(
                            "journal file {} does not exist; cleanup never bootstraps a store",
                            journal_path.display()
                        ),
                    }
                } else {
                    Error::InputOutput(source)
                }
            })?;

        // Preserve every on-disk generation exactly as planned. The normal
        // writer open deliberately repairs/deletes here; doing that behind a
        // cleanup task's back would violate task isolation and dry-run.
        let base_archives = crate::store::open_all_archives(directory)?;
        let cleanup_archive_metadata = base_archives
            .first()
            .map(|archive| std::fs::metadata(archive.path()))
            .transpose()?
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "{} has no active source archive from which cleanup can inherit file metadata",
                    directory.display()
                ),
            })?;

        let journal_entries = read_journal(&journal_path)?;
        let persisted = journal_entries
            .iter()
            .filter_map(JournalEntry::record_identifier)
            .find(|identifier| {
                base_archives
                    .iter()
                    .any(|archive| archive.contains_segment(identifier.segment))
            })
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "no journal revision in {} resolves; cleanup never bootstraps or rolls back",
                    directory.display()
                ),
            })?;

        Ok(Self {
            directory: directory.to_owned(),
            _repository_lock: repository_lock,
            maximum_archive_size: DEFAULT_MAXIMUM_ARCHIVE_SIZE,
            base_archives,
            session_segments: RwLock::new(HashMap::new()),
            session_segment_cache: RwLock::new(BoundedCache::new(session_cache_budget_bytes(
                DEFAULT_MAXIMUM_ARCHIVE_SIZE,
            ))),
            session_archives: RwLock::new(Vec::new()),
            session_segment_writes: RwLock::new(Vec::new()),
            parsed_segment_cache: RwLock::new(BoundedCache::new(
                crate::store::SEGMENT_CACHE_BUDGET_BYTES,
            )),
            seal_archive_before_head: true,
            cleanup_archive_metadata: Some(cleanup_archive_metadata),
            #[cfg(test)]
            finalized_session_semantic_validations: std::sync::atomic::AtomicUsize::new(0),
            write_state: Mutex::new(WriteState {
                journal_file,
                tar_writer: None,
                next_archive_number: Some(certified_next_archive_number),
                head: persisted,
                persisted_head: Some(persisted),
            }),
        })
    }

    pub(super) fn lock_write_state(&self) -> std::sync::MutexGuard<'_, WriteState> {
        self.write_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The store directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Closes the session: flush (journal line before trailers, like
    /// Oak), then finalize the open archive with its trailer entries, and
    /// fsync the directory so newly created archive files and the journal
    /// are durably linked.
    pub fn close(self) -> Result<()> {
        self.flush()?;
        let mut state = self.lock_write_state();
        if let Some(tar_writer) = state.tar_writer.take() {
            drop(state);
            self.close_archive_writer(tar_writer)?;
        } else {
            drop(state);
        }
        if self.seal_archive_before_head {
            sync_directory_strict(&self.directory)?;
        } else {
            crate::writer::compaction::fsync_directory(&self.directory);
        }
        Ok(())
    }

    /// Bootstraps the initial head of a fresh store: a node whose only
    /// child is an empty node named `root`, in generation `(0, 0, false)`.
    pub(super) fn write_initial_node(&self) -> Result<RecordIdentifier> {
        let generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        let mut writer = RecordWriter::new(StoreSink { store: self }, generation);
        let empty_child = writer.write_node(None, &[], &ChildNodesToWrite::Zero, &[])?;
        let head = writer.write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: empty_child,
            },
            &[],
        )?;
        writer.finish()?;
        Ok(head)
    }

    /// A record writer whose segments persist into this store, stamped
    /// with the given generation.
    #[must_use]
    pub fn record_writer(
        &self,
        generation: GarbageCollectionGeneration,
    ) -> RecordWriter<StoreSink<'_>> {
        RecordWriter::new(StoreSink { store: self }, generation)
    }

    /// A record writer stamping its segments with the given writer
    /// identifier (recorded in each segment's info string): `sys` for
    /// commits, `c` for compaction, `b` for backup, `r` for restore.
    #[must_use]
    pub fn record_writer_with_identifier(
        &self,
        generation: GarbageCollectionGeneration,
        writer_identifier: &str,
    ) -> RecordWriter<StoreSink<'_>> {
        RecordWriter::with_writer_identifier(
            StoreSink { store: self },
            generation,
            writer_identifier,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::provider::SegmentProvider;
    use crate::store::Repository;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::store_writer::test_support::*;

    #[test]
    fn writes_survive_reopening_through_both_stores() {
        let directory = TestDirectory::new("reopen");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        {
            let store = WritableRepository::open(&directory.path).expect("reopen for write");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let child = writer
                .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
                .expect("child");
            let root = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "content".to_owned(),
                        node: child,
                    },
                    &[],
                )
                .expect("root");
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("super root");
            writer.finish().expect("finish");
            let previous = store.head();
            assert!(
                store.compare_and_set_head(previous, head),
                "compare and set succeeds"
            );
            store.close().expect("close");
        }
        let repository = Repository::open(&directory.path).expect("reader opens");
        let content = repository
            .node_at_path("/content")
            .expect("resolve")
            .expect("present");
        let template = content.template().expect("template");
        assert_eq!(template.primary_type.as_deref(), Some("nt:unstructured"));
        assert_eq!(
            repository.journal_entries().len(),
            2,
            "bootstrap plus one commit"
        );
    }

    #[test]
    fn every_written_segment_starts_with_a_segment_info_record() {
        let directory = TestDirectory::new("segment-info");
        let store = WritableRepository::open(&directory.path).expect("open fresh store");
        store.close().expect("close");

        let repository = Repository::open(&directory.path).expect("reader opens");
        let mut data_segments_seen = 0usize;
        for segment_identifier in repository.segment_identifiers() {
            if segment_identifier.is_bulk_segment() {
                continue;
            }
            data_segments_seen += 1;
            let view = repository.segment(segment_identifier).expect("segment");
            let first_record = view
                .structure
                .record_table()
                .first()
                .expect("a data segment has records")
                .record_number;
            let info = crate::content::value::read_string(
                &repository,
                crate::segment::record::RecordIdentifier::new(segment_identifier, first_record),
            )
            .expect("record 0 is a readable string");
            // The exact shape backup timestamp parsing and Java-side
            // diagnostics rely on: {"wid":"...","sno":N,"t":T}.
            assert!(
                info.starts_with("{\"wid\":\""),
                "unexpected info record {info:?}"
            );
            assert!(
                info.contains("\",\"sno\":"),
                "unexpected info record {info:?}"
            );
            assert!(info.contains(",\"t\":"), "unexpected info record {info:?}");
            assert!(info.ends_with('}'), "unexpected info record {info:?}");
        }
        assert!(
            data_segments_seen > 0,
            "a bootstrapped store must hold at least one data segment"
        );
    }

    #[test]
    fn the_lock_excludes_concurrent_writers() {
        let directory = TestDirectory::new("exclusion");
        let store = WritableRepository::open(&directory.path).expect("first open");
        assert!(
            WritableRepository::open(&directory.path).is_err(),
            "a second writable session must be refused"
        );
        store.close().expect("close");
    }
}
