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

/// Proves every segment in one finalized session archive against what the
/// session recorded for it, and rebuilds the graph and binary-reference
/// trailers those segments imply.
///
/// The payload is checked against the CRC in the segment's own tar entry
/// name and then against what the session actually wrote. Together those
/// two establish what comparing against a retained copy of every byte used
/// to, without the session holding its whole output to say it.
fn certify_archive_segments(
    provider: &crate::store::Repository,
    archive: &TarArchiveReader,
    expected_segments: &HashMap<SegmentIdentifier, SessionSegment>,
    seen: &mut std::collections::HashSet<SegmentIdentifier>,
) -> Result<(ExpectedGraph, ExpectedBinaryReferences)> {
    let mut expected_graph = ExpectedGraph::new();
    let mut expected_binary_references = ExpectedBinaryReferences::new();
    for identifier in archive.segment_identifiers() {
        let Some(expected_session) = expected_segments.get(&identifier) else {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} contains non-session segment {identifier}",
                    archive.file_name()
                ),
            });
        };
        if !seen.insert(identifier) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "session segment {identifier} occurs more than once in finalized session archives"
                ),
            });
        }
        // Proves the archive's payload against the CRC in its own
        // tar entry name.
        archive.validate_indexed_segment_entry(identifier)?;
        // And this closes the loop to what the session actually
        // wrote. Together the two are what comparing against a
        // retained copy of every byte used to establish, without the
        // session holding its whole output to say it.
        let actual_crc =
            archive
                .segment_entry_checksum(identifier)
                .ok_or(Error::SegmentNotFound {
                    segment_identifier: identifier,
                })?;
        if actual_crc != expected_session.payload_crc {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} changed the payload of segment {identifier}",
                    archive.file_name()
                ),
            });
        }
        let disk_segment = provider.segment(identifier)?;
        let actual = archive
            .index_entry(identifier)
            .ok_or(Error::SegmentNotFound {
                segment_identifier: identifier,
            })?;
        let expected_generation = stored_segment_generation(identifier, &disk_segment.structure);
        let actual_generation = GarbageCollectionGeneration {
            generation: actual.generation,
            full_generation: actual.full_generation,
            is_compacted: actual.is_compacted,
        };
        if actual_generation != expected_generation {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} indexes segment {identifier} as {actual_generation:?}, but its header/session generation is {expected_generation:?}",
                    archive.file_name()
                ),
            });
        }
        if !disk_segment.structure.referenced_segments.is_empty() {
            expected_graph
                .entry(identifier)
                .or_default()
                .extend(disk_segment.structure.referenced_segments.iter().copied());
        }
        let binary_references =
            read_blob_identifiers(provider, &disk_segment).map_err(|error| {
                Error::InvalidFormat {
                    details: format!(
                        "cannot reconstruct binary references for finalized session segment {identifier}: {error}"
                    ),
                }
            })?;
        if !binary_references.is_empty() {
            expected_binary_references
                .entry((
                    expected_generation.generation,
                    expected_generation.full_generation,
                    expected_generation.is_compacted,
                ))
                .or_default()
                .entry(identifier)
                .or_default()
                .extend(binary_references);
        }
    }
    Ok((expected_graph, expected_binary_references))
}

/// Walks every archive this session wrote and proves each of its segments
/// is exactly what the session recorded: right archive, right position,
/// right payload, right generation, and trailers that agree.
///
/// Returns the segments and archives actually seen, so the caller can name
/// anything the session expected but the disk does not hold.
fn certify_session_archives<'archives>(
    provider: &crate::store::Repository,
    archives: &'archives [TarArchiveReader],
    base_names: &std::collections::HashSet<&str>,
    expected_segments: &HashMap<SegmentIdentifier, SessionSegment>,
    expected_archive_order: &HashMap<String, Vec<SegmentIdentifier>>,
) -> Result<(
    std::collections::HashSet<SegmentIdentifier>,
    std::collections::HashSet<&'archives str>,
)> {
    let mut seen = std::collections::HashSet::new();
    let mut seen_archives = std::collections::HashSet::new();
    for archive in archives
        .iter()
        .filter(|archive| !base_names.contains(archive.file_name()))
    {
        if archive.is_recovered() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} has no valid index",
                    archive.file_name()
                ),
            });
        }
        if archive.segment_count() == 0 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} contains no session segments",
                    archive.file_name()
                ),
            });
        }
        let expected_order = expected_archive_order
            .get(archive.file_name())
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "finalized archive {} was not created by the current write session",
                    archive.file_name()
                ),
            })?;
        let mut actual_in_file_order = archive
            .index()
            .expect("a non-recovered session archive has an index")
            .entries()
            .to_vec();
        actual_in_file_order.sort_by_key(|entry| entry.position);
        let actual_order: Vec<_> = actual_in_file_order
            .iter()
            .map(|entry| entry.segment_identifier)
            .collect();
        if actual_order.as_slice() != expected_order.as_slice() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} changed the physical write order or archive boundary of its session segments",
                    archive.file_name()
                ),
            });
        }
        seen_archives.insert(archive.file_name());
        let (expected_graph, expected_binary_references) =
            certify_archive_segments(provider, archive, expected_segments, &mut seen)?;
        validate_exact_archive_trailers(
            archive,
            archive.file_name(),
            &expected_graph,
            &expected_binary_references,
        )?;
    }
    Ok((seen, seen_archives))
}

/// Oak's mark phase (§3 of the cleanup specification).
///
/// Archives are walked newest first and entries within each archive in
/// reverse file order, with one references set shared across all archives
/// so a kept data segment in a newer archive protects bulk segments in
/// older ones. The seed set is otherwise empty — sanctioned for an offline
/// tool on a quiescent store, which the exclusive repository lock
/// guarantees — and the dangling-future rule runs with a null compacted
/// root, i.e. disabled, which the specification calls always safe.
fn mark_reclaimable_segments(
    session_archives: &[TarArchiveReader],
    base_archives: &[TarArchiveReader],
    rule: ReclaimRule,
) -> Result<std::collections::HashSet<SegmentIdentifier>> {
    // Oak's mark phase (§3 of the cleanup specification): archives
    // newest first, entries within each archive in reverse file
    // order, one references set shared across all archives so a kept
    // data segment in a newer archive protects bulk segments in
    // older ones. The seed set is otherwise empty — sanctioned for an
    // offline tool on a quiescent store, which the exclusive
    // repository lock guarantees — and the dangling-future rule runs
    // with a null compacted root, i.e. disabled, which the
    // specification calls always safe.
    let mut references: std::collections::HashSet<SegmentIdentifier> =
        std::collections::HashSet::new();
    for archive in session_archives {
        seed_references_from_archive(archive, &mut references)?;
    }
    let protected_data_segments = std::collections::HashSet::new();
    let mut reclaimable = std::collections::HashSet::new();
    // Post-compaction cleanup has no dangling-future root: the caller
    // just committed the newly compacted head, so every compacted
    // segment written by that run belongs at or before that head.
    let mut ahead_of_root = None;
    for archive in base_archives {
        mark_one_archive(
            archive,
            ReclaimPolicy {
                rule,
                protected_data_segments: &protected_data_segments,
            },
            &mut references,
            &mut reclaimable,
            &mut ahead_of_root,
        )?;
    }

    Ok(reclaimable)
}

impl SegmentSweepOutcome {
    /// Folds one archive's sweep into the run's totals, returning the
    /// segments that sweep made unavailable.
    fn record_swept_archive(
        &mut self,
        outcome: ArchiveSweepOutcome,
    ) -> std::collections::HashSet<SegmentIdentifier> {
        match outcome.disposition {
            ArchiveSweepDisposition::Removed => self.removed_archives += 1,
            ArchiveSweepDisposition::Rewritten => self.rewritten_archives += 1,
            ArchiveSweepDisposition::Unchanged => {}
        }
        self.removed_segments += outcome.newly_unavailable.len();
        self.deletion_failures.extend(outcome.deletion_failures);
        outcome.newly_unavailable
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

    /// The current in-memory head record.
    #[must_use]
    pub fn head(&self) -> RecordIdentifier {
        self.lock_write_state().head
    }

    /// Compare-and-set of the head. Returns whether the head moved.
    pub fn compare_and_set_head(
        &self,
        expected: RecordIdentifier,
        new_head: RecordIdentifier,
    ) -> bool {
        let mut state = self.lock_write_state();
        if state.head == expected {
            state.head = new_head;
            true
        } else {
            false
        }
    }

    /// Replaces the head unconditionally (the compaction primitive).
    pub fn replace_head(&self, new_head: RecordIdentifier) {
        self.lock_write_state().head = new_head;
    }

    /// Marks `head` as the persisted head after an out-of-band journal
    /// rewrite (compaction), so the next flush does not re-append a line —
    /// and reopens the journal handle onto the freshly written file, so a
    /// later head-moving flush in the same session appends to the live
    /// journal rather than the unlinked old inode.
    pub fn reset_persisted_head(&self, head: RecordIdentifier) -> Result<()> {
        let mut state = self.lock_write_state();
        state.head = head;
        state.persisted_head = Some(head);
        state.journal_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("journal.log"))?;
        Ok(())
    }

    /// The head node state (the super-root).
    #[must_use]
    pub fn head_node(&self) -> NodeState<'_> {
        NodeState::new(self, self.head())
    }

    /// The total size of the store's archive files on disk.
    pub fn archive_size_on_disk(&self) -> Result<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && ArchiveFileName::parse(name).is_some()
            {
                total += entry.metadata()?.len();
            }
        }
        Ok(total)
    }

    /// The file names of the archives that existed when this session opened.
    pub(super) fn base_archive_names(&self) -> std::collections::HashSet<String> {
        self.base_archives
            .iter()
            .map(|archive| archive.file_name().to_owned())
            .collect()
    }

    /// Proves that every archive which a subsequent compaction cleanup may
    /// mutate has complete, self-consistent payloads and trailers.
    ///
    /// Compaction calls this before writing its deep copy, so a pre-existing
    /// defect is refused before the run appends a full copy that a retry
    /// would then append again. Each source is certified once more through a
    /// fresh no-follow descriptor immediately before it is mutated, because
    /// an out-of-process pathname or byte change must still fail closed even
    /// while froe holds its advisory repository lock.
    ///
    /// The pass parses every data segment of every base archive, so
    /// compaction would otherwise begin with a long silence before its first
    /// reported step. That cost is also why the returned proof exists: see
    /// [`CertifiedReclaimSources`].
    pub(crate) fn preflight_reclaim_sources_with_progress(
        &self,
        observer: &mut dyn crate::progress::ProgressObserver,
    ) -> Result<CertifiedReclaimSources> {
        drop(self.open_base_repository_with_progress(BaseSourceCertification::Derive, observer)?);
        Ok(CertifiedReclaimSources {
            base_names: self.base_archive_names(),
        })
    }

    /// Opens a fresh, lazy provider over this session's base archives,
    /// deriving the full certificate for each one unless `certification`
    /// says the caller already holds it.
    ///
    /// The base-name check below runs either way. It is the cheap half — it
    /// proves the fresh open still sees every archive the session is about
    /// to reclaim from — and nothing may skip it.
    pub(super) fn open_base_repository_with_progress(
        &self,
        certification: BaseSourceCertification,
        observer: &mut dyn crate::progress::ProgressObserver,
    ) -> Result<crate::store::Repository> {
        let base_names = self.base_archive_names();
        let repository = crate::store::Repository::open_with_progress(&self.directory, observer)?;
        reject_duplicate_active_segments(repository.archives())?;
        let base_archives: Vec<&TarArchiveReader> = repository
            .archives()
            .iter()
            .filter(|archive| base_names.contains(archive.file_name()))
            .collect();
        // Before certifying, not after: an archive that has gone missing is
        // the cheaper refusal, and there is no reason to prove the ones that
        // remain first.
        let opened_base_names: std::collections::HashSet<String> = base_archives
            .iter()
            .map(|archive| archive.file_name().to_owned())
            .collect();
        if opened_base_names != base_names {
            let mut missing: Vec<_> = base_names.difference(&opened_base_names).cloned().collect();
            missing.sort();
            return Err(Error::InvalidFormat {
                details: format!(
                    "fresh reclamation source provider omitted active base archive(s) {missing:?}"
                ),
            });
        }
        if matches!(certification, BaseSourceCertification::Derive) {
            certify_archives_in_parallel(&repository, &base_archives, observer)?;
        }
        Ok(repository)
    }

    /// Reclaims segments older than `reference_generation` after a
    /// compaction: Oak's mark phase decides what goes, then each base
    /// archive is swept. Data segments are retained purely by the
    /// generation predicate with a single retained generation, selected
    /// by `kind`; bulk segments are
    /// retained purely by reachability from kept data segments, through
    /// a references set shared across all archives. A base archive whose
    /// segments all reclaim is deleted; one with survivors is rewritten
    /// to the next generation letter with only the survivors.
    ///
    /// This is safe only when every record reachable from the current
    /// head lives in `reference_generation` — which compaction's deep
    /// copy guarantees.
    ///
    /// Scope: only the archives that existed when this session opened are
    /// swept. Archives written during this session participate in the
    /// *mark* — their retained data segments protect the bulk segments
    /// they reference, wherever those live — but are never swept
    /// themselves; the next compaction run sees them as base archives.
    pub fn reclaim_old_generations(
        &mut self,
        reference_generation: GarbageCollectionGeneration,
        kind: CompactionKind,
    ) -> Result<()> {
        self.reclaim_old_generations_with(GenerationReclaimRequest {
            rule: ReclaimRule {
                reference: reference_generation,
                kind,
                retained_generations: RETAINED_GENERATIONS,
            },
            rewrite_policy: ArchiveRewritePolicy::EveryReclaimableArchive,
            certified_sources: None,
            expected: None,
        })
        .map(|_| ())
    }

    /// Refuses a store in which a segment this session wrote also occurs in
    /// an active base archive.
    ///
    /// The mark result is one store-wide UUID set, so an old-generation
    /// occurrence could put that UUID in the set even though a newer
    /// occurrence must stay, and sweep or trailer filtering would then
    /// remove the authoritative copy. Refusing here — before the current
    /// writer is closed or any base reader is taken — keeps the preflight
    /// fail-closed and non-mutating.
    ///
    /// The location map is scoped, not held: it is a store-wide identifier
    /// map built for a preflight that ends in milliseconds, and leaving it
    /// bound for the rest of the reclaim pinned hundreds of megabytes
    /// across the expensive phase for no reader.
    fn reject_session_segments_already_in_base_archives(&self) -> Result<()> {
        let base_locations = unique_active_segment_locations(&self.base_archives)?;
        let session_segments = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((identifier, previous)) = session_segments.keys().find_map(|identifier| {
            base_locations
                .get(identifier)
                .map(|name| (*identifier, *name))
        }) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "segment {identifier} occurs in active base archive {previous} and the current write session; refusing global reclamation"
                ),
            });
        }
        Ok(())
    }

    /// Opens the archives this session wrote: everything active that is not
    /// a base archive.
    ///
    /// Sorted newest number first, because the mark phase walks archives in
    /// that order. Only names matching the Oak archive pattern participate;
    /// unrelated files are ignored exactly as the write open ignores them.
    fn open_session_archives(
        &self,
        base_names: &std::collections::HashSet<String>,
    ) -> Result<Vec<TarArchiveReader>> {
        let mut session_archives = Vec::new();
        for file_name in crate::store::list_archive_file_names(&self.directory)? {
            if ArchiveFileName::parse(&file_name).is_none() || base_names.contains(&file_name) {
                continue;
            }
            let path = self.directory.join(&file_name);
            // A zero-length archive is not something this session wrote: it
            // is the residue of a writer killed inside its own lazy
            // next-archive creation, which the write open deliberately
            // serves no archive for. Opening it would fail outright, so the
            // skip has to hold here too or compaction inherits the failure
            // that opening was fixed to avoid.
            if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == 0) {
                continue;
            }
            session_archives.push(TarArchiveReader::open(&path)?);
        }
        session_archives.sort_by_key(|archive| {
            std::cmp::Reverse(
                ArchiveFileName::parse(archive.file_name())
                    .map_or(0, |parsed| parsed.archive_number),
            )
        });

        Ok(session_archives)
    }

    /// Plans the sweep of every base archive against what the mark phase
    /// found reclaimable.
    ///
    /// Compaction has already paid for a full deep copy of the live tree.
    /// Declining to move the survivors of an archive whose data segments all
    /// died would hand the operator a store the very next maintenance run
    /// reports as dirty and equally cannot clean, which is the field report
    /// this default policy fixes.
    fn plan_base_archive_sweeps(
        &self,
        reclaimable: &std::collections::HashSet<SegmentIdentifier>,
        rewrite_policy: ArchiveRewritePolicy,
    ) -> Result<HashMap<String, PlannedArchiveSweep>> {
        let mut planned_base_sweeps = HashMap::new();
        for archive in &self.base_archives {
            // Compaction has already paid for a full deep copy of the live
            // tree. Declining to move the survivors of an archive whose data
            // segments all died would hand the operator a store the very next
            // maintenance run reports as dirty and equally cannot clean, which
            // is the field report this default policy fixes.
            if let Some(planned) = plan_archive_sweep(
                &self.directory,
                archive,
                reclaimable,
                rewrite_policy,
                &std::collections::HashSet::new(),
            )? {
                planned_base_sweeps.insert(archive.file_name().to_owned(), planned);
            }
        }
        Ok(planned_base_sweeps)
    }

    /// Closes this session's archive, makes it durable, and certifies what
    /// it wrote before any base archive may be removed.
    ///
    /// The compacted head is already journal-visible at this point, so its
    /// finalized TAR link and trailers must be durable and independently
    /// traversable before deleting any base archive it may replace.
    fn finalize_and_certify_session(&mut self) -> Result<FinalizedSessionCertificate> {
        // Finalize the session archive so its new-generation segments are
        // complete on disk before old archives are removed.
        {
            let mut state = self.lock_write_state();
            if let Some(tar_writer) = state.tar_writer.take() {
                drop(state);
                self.close_archive_writer(tar_writer)?;
            }
        }
        // The compacted head is already journal-visible at this point. Its
        // finalized TAR link and trailers must be durable and independently
        // traversable before deleting any base archive it may replace.
        sync_directory_strict(&self.directory)?;
        let head = self.head();
        let head_is_in_session = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&head.segment);
        let finalized_session_certificate =
            self.validate_finalized_session(head_is_in_session.then_some(head))?;

        Ok(finalized_session_certificate)
    }

    /// Releases the base archives and their parsed segments.
    ///
    /// Called only after every immediate source certificate and sweep has
    /// completed: keeping `self` intact until here lets the mark and sweep
    /// phases retain their original immutable source views.
    #[cfg_attr(not(test), allow(clippy::unnecessary_wraps))]
    fn retire_base_archives(
        &mut self,
        #[cfg(test)] parsed_cache_entries_before_reclaim: usize,
    ) -> Result<()> {
        #[cfg(test)]
        if self
            .parsed_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
            > parsed_cache_entries_before_reclaim
        {
            return Err(Error::InvalidFormat {
                details: "post-compaction certification and sweeping grew the writable base-segment cache"
                    .to_owned(),
            });
        }
        let base_archives = std::mem::take(&mut self.base_archives);
        self.parsed_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        drop(base_archives);
        Ok(())
    }

    /// Reclaims exactly like [`Self::reclaim_old_generations`], accepting a
    /// proof that the caller already certified these sources under the
    /// currently held lock.
    pub(crate) fn reclaim_old_generations_with(
        &mut self,
        request: GenerationReclaimRequest<'_>,
    ) -> Result<SegmentSweepOutcome> {
        let GenerationReclaimRequest {
            rule,
            rewrite_policy,
            certified_sources,
            expected,
        } = request;
        let mut sweep_outcome = SegmentSweepOutcome::default();
        #[cfg(test)]
        let parsed_cache_entries_before_reclaim = self
            .parsed_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();

        // The mark result is one store-wide UUID set. If two active base
        // archives contain the same UUID, an old-generation occurrence can
        // put that UUID in the set even though a newer occurrence must stay,
        // causing sweep/trailer filtering to remove the authoritative copy.
        // Refuse before closing the current writer or taking base readers so
        // the caller observes a true fail-closed, non-mutating preflight.
        // Scoped, not held: this is a store-wide identifier map built for a
        // preflight that ends in milliseconds, and leaving it bound for the
        // rest of the reclaim pinned hundreds of megabytes across the
        // expensive phase for no reader.
        self.reject_session_segments_already_in_base_archives()?;

        let finalized_session_certificate = self.finalize_and_certify_session()?;

        // Use one fresh read-only repository for every base-source
        // certificate in this reclaim pass. Its parsed-segment cache is
        // bounded, unlike the writable store's session cache: certifying all
        // base archives through `self` would otherwise pin the parsed record
        // table of every live and garbage segment until sweeping completed.
        // Keeping this provider alive also gives each immediate reopened-source
        // certificate a complete, stable cross-archive fallback without
        // repopulating `self.parsed_segment_cache`.
        //
        // Deriving the certificate here is what a caller's proof can excuse,
        // and only that: the provider is still opened fresh, still rejects
        // duplicate active segments, and still proves it sees every base
        // archive. What the proof stands in for is re-reading bytes this same
        // locked run already read, which for compaction is a second full
        // parse and CRC of the whole store between its preflight and its
        // sweeps. The certificate that guards each mutation is neither of
        // these: it is the per-archive one `sweep_one_archive` derives
        // through a fresh no-follow descriptor, immediately before acting.
        let base_names = self.base_archive_names();
        let certification =
            if certified_sources.is_some_and(|proof| proof.certifies_exactly(&base_names)) {
                BaseSourceCertification::AlreadyProven
            } else {
                BaseSourceCertification::Derive
            };
        let certification_repository = self.open_base_repository_with_progress(
            certification,
            &mut crate::progress::DiscardedProgress,
        )?;

        // Archives this session wrote (now closed and complete on disk):
        // newer than every base archive. They are never swept, so every
        // data segment they hold stays on disk regardless of generation —
        // and each one therefore seeds the references set with the bulk
        // segments it points at, including pre-existing bulk segments in
        // base archives, which the empty seed alone would miss. Only
        // names matching the Oak archive pattern participate; unrelated
        // `*.tar` files in the directory are ignored, exactly as the
        // write open ignores them.
        let session_archives = self.open_session_archives(&base_names)?;

        let reclaimable = mark_reclaimable_segments(&session_archives, &self.base_archives, rule)?;

        // Store-wide fallback provider for catalog reconstruction, built
        // only if some swept archive turns out to have no readable
        // catalog. Newest first — session archives before base archives —
        // so a duplicated segment resolves to the copy live lookups
        // serve.
        let provider_order: Vec<&TarArchiveReader> = session_archives
            .iter()
            .chain(self.base_archives.iter())
            .collect();
        let mut fallback_provider: Option<ArchiveSegmentsProvider<'_>> = None;
        let planned_base_sweeps = self.plan_base_archive_sweeps(&reclaimable, rewrite_policy)?;
        // Nothing has been unlinked yet. This is the last instant at which a
        // disagreement between what the operator confirmed and what the store
        // now says can be answered by refusing rather than by explaining, so
        // it is where the comparison belongs — the same position the
        // directory-level engine puts it in.
        if let Some(expected) = expected {
            let replanned = sorted_sweep_plan(&planned_base_sweeps, &reclaimable);
            if replanned != *expected {
                return Err(Error::InvalidFormat {
                    details: "the archive sweep changed after confirmation; refusing to apply an \
                              unconfirmed archive mutation"
                        .to_owned(),
                });
            }
        }
        let mut actually_unavailable = std::collections::HashSet::new();
        finalized_session_certificate.recertify()?;
        // Whole removals run before rewrites. Only a removal that actually
        // unlinked its source contributes graph-filter targets; a failed
        // unlink leaves the edge conservatively intact. Each rewrite adds its
        // own removed entries while it is built, then makes them unavailable
        // through the published higher generation before the next rewrite.
        for rewrite_phase in [false, true] {
            for archive in &self.base_archives {
                let Some(planned) = planned_base_sweeps.get(archive.file_name()) else {
                    continue;
                };
                let is_rewrite = matches!(planned, PlannedArchiveSweep::Rewrite { .. });
                let is_remove = matches!(planned, PlannedArchiveSweep::Remove { .. });
                if (!rewrite_phase && !is_remove) || (rewrite_phase && !is_rewrite) {
                    continue;
                }
                finalized_session_certificate.recertify()?;
                let outcome = sweep_one_archive(
                    &self.directory,
                    archive,
                    &reclaimable,
                    &actually_unavailable,
                    &provider_order,
                    &mut fallback_provider,
                    Some(&certification_repository),
                    rewrite_policy,
                )?;
                finalized_session_certificate.recertify()?;
                actually_unavailable.extend(sweep_outcome.record_swept_archive(outcome));
            }
            #[cfg(test)]
            if !rewrite_phase
                && planned_base_sweeps
                    .values()
                    .any(|planned| matches!(planned, PlannedArchiveSweep::Rewrite { .. }))
            {
                probe_archive_sweep_phase_boundary(
                    "postcomp-sweep.removals-complete-before-rewrites",
                )?;
            }
        }
        drop(fallback_provider);
        drop(provider_order);
        drop(session_archives);
        drop(certification_repository);
        self.retire_base_archives(
            #[cfg(test)]
            parsed_cache_entries_before_reclaim,
        )?;
        finalized_session_certificate.recertify()?;
        // Make the archive deletions and any swept replacements durable
        // before the caller proceeds to the journal rewrite.
        sync_directory_strict(&self.directory)?;
        Ok(sweep_outcome)
    }

    /// Appends one entry to the session's physical write-order ledger.
    ///
    /// The archive name is shared with the other writes to the same archive
    /// rather than allocated per segment: writes go to one archive until it
    /// rotates, so the previous entry almost always already holds it.
    pub(super) fn record_session_write(
        &self,
        archive_file_name: &str,
        identifier: SegmentIdentifier,
    ) {
        let mut writes = self
            .session_segment_writes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shared_name = match writes.last() {
            Some(previous) if *previous.archive_file_name == *archive_file_name => {
                Arc::clone(&previous.archive_file_name)
            }
            _ => Arc::from(archive_file_name),
        };
        writes.push(SessionSegmentWrite {
            archive_file_name: shared_name,
            identifier,
        });
    }

    /// Re-reads a session segment from the archive it was written to.
    ///
    /// Rotated archives are reopened mappings and answer directly. The
    /// archive still being written answers through the writer's positional
    /// read-back; the cache budget keeps that archive resident, so this is
    /// the path a smaller-than-default budget would take rather than the
    /// ordinary one. Nothing here holds the write-state lock across a
    /// provider call, and no caller reaches a provider read while holding it.
    pub(super) fn reread_session_segment(
        &self,
        segment_identifier: SegmentIdentifier,
    ) -> Result<SegmentView<'_>> {
        let bytes = {
            let session_archives = self
                .session_archives
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let rotated = session_archives
                .iter()
                .find_map(|archive| archive.segment_data(segment_identifier))
                .map(<[u8]>::to_vec);
            if let Some(bytes) = rotated {
                bytes
            } else {
                let state = self.lock_write_state();
                let open = state
                    .tar_writer
                    .as_ref()
                    .and_then(|writer| writer.read_segment(segment_identifier).transpose())
                    .transpose()?;
                open.ok_or(Error::SegmentNotFound { segment_identifier })?
            }
        };
        let structure = Arc::new(ParsedSegment::parse(segment_identifier, &bytes)?);
        let shared = (structure, Arc::new(bytes));
        self.session_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(segment_identifier, shared.clone());
        Ok(SegmentView {
            structure: shared.0,
            bytes: crate::segment::view::SegmentBytes::Shared(shared.1),
        })
    }

    /// Whether any source of this store holds the segment.
    #[must_use]
    pub fn contains_segment(&self, segment_identifier: SegmentIdentifier) -> bool {
        self.session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&segment_identifier)
            || self
                .base_archives
                .iter()
                .any(|archive| archive.contains_segment(segment_identifier))
    }

    /// The garbage collection generation of an existing segment, from the
    /// archive index or the session state.
    #[must_use]
    pub fn segment_generation(
        &self,
        segment_identifier: SegmentIdentifier,
    ) -> Option<GarbageCollectionGeneration> {
        if let Some(session) = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&segment_identifier)
        {
            return Some(session.generation);
        }
        for archive in &self.base_archives {
            if let Some(entry) = archive.index_entry(segment_identifier) {
                return Some(GarbageCollectionGeneration {
                    generation: entry.generation,
                    full_generation: entry.full_generation,
                    is_compacted: entry.is_compacted,
                });
            }
            if archive.contains_segment(segment_identifier) {
                // Recovered archive without index metadata: parse the header.
                return self.segment(segment_identifier).ok().map(|view| {
                    GarbageCollectionGeneration {
                        generation: view.structure.generation,
                        full_generation: view.structure.full_generation,
                        is_compacted: view.structure.is_compacted,
                    }
                });
            }
        }
        None
    }

    /// The generation new, non-compacting writes must use: the head
    /// segment's generation with the compacted flag cleared.
    pub fn writing_generation(&self) -> Result<GarbageCollectionGeneration> {
        let head = self.head();
        let generation = self
            .segment_generation(head.segment)
            .ok_or(Error::SegmentNotFound {
                segment_identifier: head.segment,
            })?;
        Ok(GarbageCollectionGeneration {
            is_compacted: false,
            ..generation
        })
    }

    /// Persists one built segment: appends it to the current archive
    /// (rotating past the size threshold) and makes it readable.
    pub fn persist_segment(&self, segment: BuiltSegment) -> Result<()> {
        let structure = Arc::new(ParsedSegment::parse(segment.identifier, &segment.bytes)?);

        let mut state = self.lock_write_state();
        if self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&segment.identifier)
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "segment {} was written more than once in the current session",
                    segment.identifier
                ),
            });
        }
        if state.tar_writer.is_none() {
            let archive_number = state.next_archive_number.take().ok_or_else(|| {
                Error::InvalidFormat {
                    details: "the archive-number namespace is exhausted at u32::MAX; refusing to wrap to data00000a.tar"
                        .to_owned(),
                }
            })?;
            state.next_archive_number = archive_number.checked_add(1);
            let file_name = format!("data{archive_number:05}a.tar");
            state.tar_writer = Some(if self.seal_archive_before_head {
                // Prepared cleanup must never truncate unexplained residue
                // that appeared after planning, even at the otherwise-next
                // archive number.
                TarArchiveWriter::new_exclusive(&self.directory, &file_name)
            } else {
                TarArchiveWriter::new(&self.directory, &file_name)
            });
        }
        let tar_writer = state
            .tar_writer
            .as_mut()
            .ok_or_else(|| Error::InvalidFormat {
                details: "the archive writer disappeared while locked".to_owned(),
            })?;
        let archive_file_name = tar_writer
            .path()
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "session archive path {} has no UTF-8 file name",
                    tar_writer.path().display()
                ),
            })?
            .to_owned();
        let tar_generation = if segment.identifier.is_data_segment() {
            segment.generation
        } else {
            GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            }
        };
        let length = tar_writer.write_segment(
            segment.identifier,
            &segment.bytes,
            tar_generation,
            &segment.referenced_segments,
            &segment.binary_reference_identifiers,
        )?;
        let finished = (length >= self.maximum_archive_size)
            .then(|| state.tar_writer.take())
            .flatten();
        if let Some(finished) = finished {
            self.close_archive_writer(finished)?;
        }

        self.session_segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                segment.identifier,
                SessionSegment {
                    generation: stored_segment_generation(segment.identifier, &structure),
                    payload_crc: crate::checksum::crc32(&segment.bytes),
                },
            );
        // The payload goes to the read-back cache, not to permanent session
        // state: it is already durable in the archive this call just appended
        // it to, and the cache is sized to keep the open archive resident.
        self.session_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(segment.identifier, (structure, Arc::new(segment.bytes)));
        self.record_session_write(&archive_file_name, segment.identifier);
        drop(state);
        Ok(())
    }

    /// Flushes with Oak's durability ordering: archive fsync first, then
    /// — only when the head moved since the last flush — one appended
    /// journal line, fdatasynced. Pending segment bytes are forced to
    /// disk even when the head is unchanged, exactly like Java's
    /// `flush()`; only the journal line is conditional.
    pub fn flush(&self) -> Result<()> {
        let mut state = self.lock_write_state();
        let head_moved = state.persisted_head != Some(state.head);
        let finalized_session_certificate = if self.seal_archive_before_head && head_moved {
            // Cleanup is an offline maintenance transaction. Its newly
            // checkpoint-free head must never become durable while its TAR
            // still lacks graph/catalog/index trailers. Consume and fully
            // close the writer, persist the directory entry, then traverse
            // the exact head through fresh on-disk archive readers.
            if let Some(tar_writer) = state.tar_writer.take() {
                self.close_archive_writer(tar_writer)?;
            }
            sync_directory_strict(&self.directory)?;
            let certificate = self.validate_finalized_session(Some(state.head))?;
            #[cfg(test)]
            certificate.substitute_first_path_if_armed("checkpoint.tar-durable-before-journal")?;
            #[cfg(test)]
            crate::writer::maintenance_fault_injection::crash_if_armed(
                "checkpoint.tar-durable-before-journal",
            );
            Some(certificate)
        } else if let Some(tar_writer) = &mut state.tar_writer {
            tar_writer.flush()?;
            None
        } else {
            None
        };
        if !head_moved {
            return Ok(());
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let head = state.head;
        if let Some(certificate) = &finalized_session_certificate {
            certificate.recertify()?;
        }
        let line = format!(
            "{}:{} root {timestamp}\n",
            head.segment, head.record_number as i32
        );
        // A crash may leave the previous journal append without its line
        // terminator. Appending the next head directly would concatenate the
        // two revisions and make the newly committed head invisible to every
        // Oak-compatible reader. Segment bytes are already durable above, so
        // inserting a separator here preserves the write-order contract; a
        // crash after the separator merely turns the torn tail into a line
        // the tolerant reader skips.
        if journal_needs_separator(&self.directory.join("journal.log"))? {
            state.journal_file.write_all(b"\n")?;
        }
        if let Some(certificate) = &finalized_session_certificate {
            certificate.recertify()?;
        }
        state.journal_file.write_all(line.as_bytes())?;
        state.journal_file.sync_data()?;
        if let Some(certificate) = &finalized_session_certificate {
            certificate.recertify()?;
        }
        state.persisted_head = Some(head);
        Ok(())
    }

    /// Certifies every finalized session archive through freshly opened disk
    /// readers before a prepared head can reach the journal or
    /// post-compaction cleanup can mutate a base archive.
    ///
    /// A structurally valid index alone is insufficient: every session UUID
    /// must occur exactly once and in a session-created archive, every entry
    /// name/CRC/payload/generation must match the immutable in-memory write,
    /// no extra UUID may share a session archive, and graph/BRF trailers must
    /// equal a reconstruction through the complete fresh provider.
    pub(super) fn validate_finalized_session(
        &self,
        head: Option<RecordIdentifier>,
    ) -> Result<FinalizedSessionCertificate> {
        let expected_writes = self
            .session_segment_writes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let certificate = FinalizedSessionCertificate::capture(&self.directory, &expected_writes)?;
        self.validate_finalized_session_semantics(head)?;
        certificate.recertify()?;
        Ok(certificate)
    }

    /// The order this session recorded writing its segments, per archive,
    /// after proving the write-order ledger and the segment set agree.
    fn expected_session_archive_order(
        &self,
        expected_segments: &HashMap<SegmentIdentifier, SessionSegment>,
    ) -> Result<HashMap<String, Vec<SegmentIdentifier>>> {
        let expected_writes = self
            .session_segment_writes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if expected_writes.len() != expected_segments.len() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "session write-order ledger contains {} entries for {} distinct segments",
                    expected_writes.len(),
                    expected_segments.len()
                ),
            });
        }
        let mut expected_archive_order: HashMap<String, Vec<SegmentIdentifier>> = HashMap::new();
        let mut ordered_identifiers = std::collections::HashSet::new();
        for write in &expected_writes {
            if !expected_segments.contains_key(&write.identifier)
                || !ordered_identifiers.insert(write.identifier)
            {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "session write-order ledger contains an absent or repeated segment {}",
                        write.identifier
                    ),
                });
            }
            expected_archive_order
                .entry(write.archive_file_name.to_string())
                .or_default()
                .push(write.identifier);
        }
        Ok(expected_archive_order)
    }

    pub(super) fn validate_finalized_session_semantics(
        &self,
        head: Option<RecordIdentifier>,
    ) -> Result<()> {
        #[cfg(test)]
        self.finalized_session_semantic_validations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let expected_segments: HashMap<SegmentIdentifier, SessionSegment> = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let expected_archive_order = self.expected_session_archive_order(&expected_segments)?;

        if let Some(head) = head
            && !expected_segments.contains_key(&head.segment)
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "cleanup head {head} is not part of the current write session; refusing to append it to the journal"
                ),
            });
        }

        // A fresh read-only repository gives validation the exact active
        // archive set plus lazy, bounded segment parsing. Building the older
        // eager archive provider parsed every segment in every base archive
        // under the cleanup lock, even though certification normally touches
        // only the new session and its reachable dependencies. Repository
        // opening binds the journal head but indexes all active segments, so
        // finalized session segments remain addressable before their new head
        // is appended to the journal.
        let provider = crate::store::Repository::open(&self.directory)?;
        let archives = provider.archives();
        reject_duplicate_active_segments(archives)?;
        let base_names: std::collections::HashSet<&str> = self
            .base_archives
            .iter()
            .map(TarArchiveReader::file_name)
            .collect();
        let (seen, seen_archives) = certify_session_archives(
            &provider,
            archives,
            &base_names,
            &expected_segments,
            &expected_archive_order,
        )?;
        if seen_archives.len() != expected_archive_order.len() {
            let mut missing_archives: Vec<_> = expected_archive_order
                .keys()
                .filter(|name| !seen_archives.contains(name.as_str()))
                .cloned()
                .collect();
            missing_archives.sort();
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archives omit expected archive(s): {missing_archives:?}"
                ),
            });
        }
        if seen.len() != expected_segments.len() {
            let mut missing: Vec<_> = expected_segments
                .keys()
                .filter(|identifier| !seen.contains(identifier))
                .copied()
                .collect();
            missing.sort_by_key(|identifier| {
                (
                    identifier.most_significant_bits,
                    identifier.least_significant_bits,
                )
            });
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archives omit {} session segment(s): {missing:?}",
                    missing.len()
                ),
            });
        }

        if let Some(head) = head {
            let disk_head = provider.segment(head.segment)?;
            if disk_head.structure.record_type(head.record_number) != Some(RecordType::Node) {
                return Err(Error::InvalidFormat {
                    details: format!("cleanup head {head} is not a finalized node record"),
                });
            }
            crate::tooling::verify_node_tree(&provider, head).map_err(|error| {
                Error::InvalidFormat {
                    details: format!(
                        "finalized cleanup head {head} failed its pre-journal health traversal: {error}"
                    ),
                }
            })?;
        }
        Ok(())
    }

    /// Finalizes one session TAR and, for prepared cleanup sessions, copies
    /// and verifies the active repository archive's uid/gid/mode before the
    /// new archive can become journal-visible.
    pub(super) fn close_archive_writer(&self, tar_writer: TarArchiveWriter) -> Result<()> {
        let path = tar_writer.path().to_owned();
        if !tar_writer.close()? {
            return Ok(());
        }
        if let Some(source_metadata) = &self.cleanup_archive_metadata {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)?;
            preserve_file_metadata(&file, source_metadata)?;
        }
        // Reopen what was just finished. A rotated archive's segments must
        // stay readable for the rest of the session — a later record can
        // reference one — and a mapping is how they stay readable without
        // the session holding their bytes. Reopening also drops the file
        // descriptor: `TarArchiveReader` keeps only the mapping.
        let reopened = TarArchiveReader::open(&path)?;
        self.session_archives
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(reopened);
        Ok(())
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

/// Whether appending a journal entry first needs a line separator.
pub(super) fn journal_needs_separator(path: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(!matches!(last[0], b'\n' | b'\r'))
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

/// The segment sink wiring a [`RecordWriter`] into a store.
pub struct StoreSink<'store> {
    pub(super) store: &'store WritableRepository,
}

impl SegmentSink for StoreSink<'_> {
    fn write_segment(&mut self, segment: BuiltSegment) -> Result<()> {
        self.store.persist_segment(segment)
    }
}
