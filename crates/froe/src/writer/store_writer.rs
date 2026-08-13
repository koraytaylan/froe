//! The writable repository: Oak's read-write file store lifecycle.
//!
//! Opening takes the exclusive repository lock first (a documented,
//! strictly-safer deviation from Java, which creates `journal.log`
//! before locking — a contended open here leaves no trace), then opens
//! the journal handle, then the
//! manifest check-and-update (always rewriting `store.version=2`), then
//! archive initialization with *destructive* generation selection — the
//! newest valid generation letter of each archive number wins and stale
//! letters are deleted; archives without any valid index are backed up
//! to `.bak` names and regenerated from a raw scan — and finally journal
//! binding, bootstrapping the initial `{ "root": {} }` node into a fresh
//! store.
//!
//! Durability follows Oak's contract exactly: segment bytes are appended
//! and fsynced *before* the journal line referencing them is appended
//! and fdatasynced, and a journal line is written only when the head
//! actually moved.
//!
//! Segments written during the session are kept in memory (shared
//! buffers) so reads resolve them immediately; on disk they live in the
//! archives this writer produces.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

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
use crate::tar_archive::file_name::{ArchiveFileName, select_newest_file_generations};
use crate::writer::record_writer::{ChildNodesToWrite, RecordWriter, SegmentSink};
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::segment_builder::{BuiltSegment, GarbageCollectionGeneration};
use crate::writer::tar_writer::TarArchiveWriter;

/// The default archive rotation threshold (Oak: 256 MB).
const DEFAULT_MAXIMUM_ARCHIVE_SIZE: u64 = 256 * 1024 * 1024;

/// A parsed segment paired with its shared bytes.
type SharedSegment = (Arc<ParsedSegment>, Arc<Vec<u8>>);

/// Returns the next unused archive number. `None` is the explicit exhausted
/// state after an active `u32::MAX` archive; wrapping to zero is never valid.
fn next_archive_number(archives: &[TarArchiveReader]) -> Option<u32> {
    match archives
        .iter()
        .filter_map(|archive| ArchiveFileName::parse(archive.file_name()))
        .map(|name| name.archive_number)
        .max()
    {
        None => Some(0),
        Some(maximum) => maximum.checked_add(1),
    }
}

/// Computes the first archive number above every physical Oak archive name in
/// `directory`, without opening or selecting any of those archives. Prepared
/// cleanup uses this stronger namespace view so a zero-byte, invalid-index, or
/// otherwise unselected residue can never be reused as checkpoint output.
pub(crate) fn next_cleanup_archive_number(directory: &Path) -> Result<u32> {
    let maximum = physical_archive_names(directory)?
        .into_iter()
        .map(|name| name.archive_number)
        .max();
    match maximum {
        None => Ok(0),
        Some(maximum) => maximum.checked_add(1).ok_or_else(|| Error::InvalidFormat {
            details: "the physical archive-number namespace is exhausted at u32::MAX; cleanup cannot allocate a checkpoint output archive"
                .to_owned(),
        }),
    }
}

/// The next archive number a write session may allocate: above every
/// physical Oak archive name in `directory` *and* above every archive the
/// session actually opened. `None` is the explicit exhausted state.
///
/// Opening deliberately serves fewer archives than the directory holds — an
/// archive number whose every generation letter is empty contributes none —
/// so allocating out of the opened set alone would hand back a number a
/// residue file still claims. For the letterless spelling that collision is
/// unrecoverable rather than untidy: `data00007.tar` and a freshly written
/// `data00007a.tar` both parse as number 7 generation `'a'`, which
/// `group_file_generations_newest_first` refuses outright, so every later
/// open of the store fails. Cleanup allocates from the same stronger view;
/// see `next_cleanup_archive_number`.
fn next_physical_archive_number(
    directory: &Path,
    opened: &[TarArchiveReader],
) -> Result<Option<u32>> {
    let physical = physical_archive_names(directory)?
        .into_iter()
        .map(|name| name.archive_number)
        .max();
    // The opened set is a subset of the physical names, so this only ever
    // agrees with `physical`. It is consulted anyway so the two views can
    // never silently drift apart.
    let maximum = match (physical, next_archive_number(opened)) {
        (None, next) => return Ok(next),
        (Some(physical), _) => physical,
    };
    Ok(maximum.checked_add(1))
}

fn physical_archive_names(directory: &Path) -> Result<Vec<ArchiveFileName>> {
    let mut archives = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some(archive) = ArchiveFileName::parse(&file_name) {
            archives.push(archive);
        }
    }
    Ok(archives)
}

/// Rechecks a plan-certified checkpoint output number immediately before the
/// strict writer is opened. Earlier cleanup phases may remove physical names,
/// but none may introduce a number at or above the certificate. Both spellings
/// of generation `a` are checked explicitly because Oak treats a missing
/// letter as `a`.
fn validate_cleanup_archive_number(directory: &Path, certified: u32) -> Result<()> {
    for alias in [
        format!("data{certified:05}a.tar"),
        format!("data{certified:05}.tar"),
    ] {
        match std::fs::symlink_metadata(directory.join(&alias)) {
            Ok(_) => {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "certified checkpoint output alias {alias} is occupied; refusing prepared cleanup"
                    ),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    if let Some(conflict) = physical_archive_names(directory)?
        .into_iter()
        .filter(|name| name.archive_number >= certified)
        .max_by_key(|name| (name.archive_number, name.file_generation))
    {
        return Err(Error::InvalidFormat {
            details: format!(
                "physical archive {} has number {} at or above the certified checkpoint output number {certified}; refusing prepared cleanup",
                conflict.file_name, conflict.archive_number
            ),
        });
    }
    Ok(())
}

/// The mutable write-side state, serialized behind one mutex.
struct WriteState {
    journal_file: File,
    tar_writer: Option<TarArchiveWriter>,
    /// The next free archive number, or `None` after `u32::MAX` has been
    /// allocated. An explicit exhausted state prevents wraparound to archive
    /// zero and destructive truncation of `data00000a.tar`.
    next_archive_number: Option<u32>,
    head: RecordIdentifier,
    persisted_head: Option<RecordIdentifier>,
}

#[derive(Clone)]
struct SessionSegmentWrite {
    archive_file_name: String,
    identifier: SegmentIdentifier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FinalizedSessionFileFingerprint {
    identity: RegularFileIdentity,
    length: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    change_time_seconds: i64,
    #[cfg(unix)]
    change_time_nanoseconds: i64,
}

impl FinalizedSessionFileFingerprint {
    fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self> {
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

struct FinalizedSessionArchiveCertificate {
    path: PathBuf,
    held: File,
    fingerprint: FinalizedSessionFileFingerprint,
}

impl FinalizedSessionArchiveCertificate {
    fn capture(path: PathBuf) -> Result<Self> {
        let held = open_regular_file_no_follow(&path, false)?;
        let fingerprint = FinalizedSessionFileFingerprint::from_metadata(&held.metadata()?)?;
        let certificate = Self {
            path,
            held,
            fingerprint,
        };
        certificate.recertify()?;
        Ok(certificate)
    }

    fn recertify(&self) -> Result<()> {
        let held = FinalizedSessionFileFingerprint::from_metadata(&self.held.metadata()?)?;
        let named = FinalizedSessionFileFingerprint::from_metadata(&std::fs::symlink_metadata(
            &self.path,
        )?)?;
        if held != self.fingerprint || named != self.fingerprint {
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

struct FinalizedSessionCertificate {
    archives: Vec<FinalizedSessionArchiveCertificate>,
}

impl FinalizedSessionCertificate {
    fn capture(directory: &Path, writes: &[SessionSegmentWrite]) -> Result<Self> {
        let names: std::collections::BTreeSet<_> = writes
            .iter()
            .map(|write| write.archive_file_name.as_str())
            .collect();
        let mut archives = Vec::with_capacity(names.len());
        for name in names {
            archives.push(FinalizedSessionArchiveCertificate::capture(
                directory.join(name),
            )?);
        }
        Ok(Self { archives })
    }

    fn recertify(&self) -> Result<()> {
        for archive in &self.archives {
            archive.recertify()?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn substitute_first_path_if_armed(&self, cutpoint: &str) -> Result<()> {
        if let Some(archive) = self.archives.first() {
            crate::writer::cleanup_fault_injection::substitute_path_if_armed(
                cutpoint,
                &archive.path,
            )?;
        }
        Ok(())
    }
}

/// A read-write segment store session holding the repository lock.
pub struct WritableRepository {
    directory: PathBuf,
    _repository_lock: Arc<RepositoryLock>,
    maximum_archive_size: u64,
    /// Archives that existed before this session, newest first.
    base_archives: Vec<TarArchiveReader>,
    /// Segments written in this session, servable without a mapping.
    session_segments: RwLock<HashMap<SegmentIdentifier, SharedSegment>>,
    /// Exact physical write order, including the archive rotation boundary
    /// for every session segment. Cleanup certification must preserve this
    /// order because later reverse-order marking is semantically significant.
    session_segment_writes: RwLock<Vec<SessionSegmentWrite>>,
    parsed_segment_cache: RwLock<HashMap<SegmentIdentifier, Arc<ParsedSegment>>>,
    write_state: Mutex<WriteState>,
    /// Cleanup checkpoint commits seal and validate their archive on disk
    /// before making the new head durable in the journal.
    seal_archive_before_head: bool,
    /// Exact metadata inherited by every archive created by a prepared
    /// cleanup session. Normal write sessions retain their existing
    /// create-time behavior.
    cleanup_archive_metadata: Option<std::fs::Metadata>,
    #[cfg(test)]
    finalized_session_semantic_validations: std::sync::atomic::AtomicUsize,
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
            session_segment_writes: RwLock::new(Vec::new()),
            parsed_segment_cache: RwLock::new(HashMap::new()),
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
        crate::store::check_manifest(directory, !archive_file_names.is_empty())?;
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
            session_segment_writes: RwLock::new(Vec::new()),
            parsed_segment_cache: RwLock::new(HashMap::new()),
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

    fn lock_write_state(&self) -> std::sync::MutexGuard<'_, WriteState> {
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
    pub fn set_head(&self, expected: RecordIdentifier, new_head: RecordIdentifier) -> bool {
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

    /// Proves that every archive which a subsequent compaction cleanup may
    /// mutate has complete, self-consistent payloads and trailers.
    ///
    /// Compaction calls this before writing its deep copy. The reclaim pass
    /// repeats the same fresh-reader certification immediately before
    /// mutation, because an out-of-process pathname or byte change must still
    /// fail closed even while froe holds its advisory repository lock.
    /// Certifies every reclamation source before the first mutation,
    /// reporting it. The pass parses every data segment of every base
    /// archive, so compaction would otherwise begin with a long silence
    /// before its first reported step.
    pub(crate) fn preflight_reclaim_sources_with_progress(
        &self,
        observer: &mut dyn crate::progress::ProgressObserver,
    ) -> Result<()> {
        drop(self.open_certified_base_repository_with_progress(observer)?);
        Ok(())
    }

    fn open_certified_base_repository(&self) -> Result<crate::store::Repository> {
        self.open_certified_base_repository_with_progress(&mut crate::progress::DiscardedProgress)
    }

    fn open_certified_base_repository_with_progress(
        &self,
        observer: &mut dyn crate::progress::ProgressObserver,
    ) -> Result<crate::store::Repository> {
        let base_names: std::collections::HashSet<String> = self
            .base_archives
            .iter()
            .map(|archive| archive.file_name().to_owned())
            .collect();
        let repository = crate::store::Repository::open_with_progress(&self.directory, observer)?;
        reject_duplicate_active_segments(repository.archives())?;
        let mut certified_base_names = std::collections::HashSet::new();
        observer.step_began(
            &crate::progress::Step::new(
                "certifying source archives",
                crate::progress::WorkUnit::Archives,
            )
            .with_total(crate::progress::count(base_names.len())),
        );
        let mut certified = 0usize;
        for archive in repository.archives() {
            if base_names.contains(archive.file_name()) {
                observer.step_advanced(crate::progress::count(certified));
                if let Err(error) = certify_active_archive(&repository, archive) {
                    observer.step_ended();
                    return Err(error);
                }
                certified += 1;
                certified_base_names.insert(archive.file_name().to_owned());
            }
        }
        observer.step_advanced(crate::progress::count(certified));
        observer.step_ended();
        if certified_base_names != base_names {
            let mut missing: Vec<_> = base_names
                .difference(&certified_base_names)
                .cloned()
                .collect();
            missing.sort();
            return Err(Error::InvalidFormat {
                details: format!(
                    "fresh reclamation source provider omitted active base archive(s) {missing:?}"
                ),
            });
        }
        Ok(repository)
    }

    /// Reclaims segments older than `reference_generation` after a
    /// compaction: Oak's mark phase decides what goes, then each base
    /// archive is swept. Data segments are retained purely by the
    /// generation predicate with a single retained generation (`full`
    /// selects the full-compaction predicate); bulk segments are
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
    #[allow(
        clippy::too_many_lines,
        reason = "session validation, source certification, global marking, and ordered sweeping form one safety sequence"
    )]
    pub fn reclaim_old_generations(
        &mut self,
        reference_generation: GarbageCollectionGeneration,
        full: bool,
    ) -> Result<()> {
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
        let base_locations = unique_active_segment_locations(&self.base_archives)?;
        {
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
        }

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

        // Use one fresh read-only repository for every base-source
        // certificate in this reclaim pass. Its parsed-segment cache is
        // bounded, unlike the writable store's session cache: certifying all
        // base archives through `self` would otherwise pin the parsed record
        // table of every live and garbage segment until sweeping completed.
        // Keeping this provider alive also gives each immediate reopened-source
        // certificate a complete, stable cross-archive fallback without
        // repopulating `self.parsed_segment_cache`.
        let base_names: std::collections::HashSet<String> = self
            .base_archives
            .iter()
            .map(|archive| archive.file_name().to_owned())
            .collect();
        let certification_repository = self.open_certified_base_repository()?;

        // Archives this session wrote (now closed and complete on disk):
        // newer than every base archive. They are never swept, so every
        // data segment they hold stays on disk regardless of generation —
        // and each one therefore seeds the references set with the bulk
        // segments it points at, including pre-existing bulk segments in
        // base archives, which the empty seed alone would miss. Only
        // names matching the Oak archive pattern participate; unrelated
        // `*.tar` files in the directory are ignored, exactly as the
        // write open ignores them.
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
        for archive in &session_archives {
            seed_references_from_archive(archive, &mut references)?;
        }
        let protected_data_segments = std::collections::HashSet::new();
        let mut reclaimable = std::collections::HashSet::new();
        // Post-compaction cleanup has no dangling-future root: the caller
        // just committed the newly compacted head, so every compacted
        // segment written by that run belongs at or before that head.
        let mut ahead_of_root = None;
        for archive in &self.base_archives {
            mark_one_archive(
                archive,
                ReclaimPolicy {
                    reference: reference_generation,
                    full,
                    retained_generations: 1,
                    protected_data_segments: &protected_data_segments,
                },
                &mut references,
                &mut reclaimable,
                &mut ahead_of_root,
            )?;
        }

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
        let mut planned_base_sweeps = HashMap::new();
        for archive in &self.base_archives {
            if let Some(planned) = plan_archive_sweep(&self.directory, archive, &reclaimable)? {
                planned_base_sweeps.insert(archive.file_name().to_owned(), planned);
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
                )?;
                finalized_session_certificate.recertify()?;
                actually_unavailable.extend(outcome.newly_unavailable);
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
        // Retire stale mappings only after every immediate source certificate
        // and sweep has completed. Keeping `self` intact until here lets the
        // mark and sweep phases retain their original immutable source views.
        #[cfg(test)]
        if self
            .parsed_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
            != parsed_cache_entries_before_reclaim
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
        finalized_session_certificate.recertify()?;
        // Make the archive deletions and any swept replacements durable
        // before the caller proceeds to the journal rewrite.
        sync_directory_strict(&self.directory)?;
        Ok(())
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
        if let Some((structure, _)) = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&segment_identifier)
        {
            return Some(GarbageCollectionGeneration {
                generation: structure.generation,
                full_generation: structure.full_generation,
                is_compacted: structure.is_compacted,
            });
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
            .insert(segment.identifier, (structure, Arc::new(segment.bytes)));
        self.session_segment_writes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(SessionSegmentWrite {
                archive_file_name,
                identifier: segment.identifier,
            });
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
            crate::writer::cleanup_fault_injection::crash_if_armed(
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
    fn validate_finalized_session(
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

    #[allow(
        clippy::too_many_lines,
        reason = "session membership, payload, generation, trailer, and optional head checks form one fail-closed pre-mutation certificate"
    )]
    fn validate_finalized_session_semantics(&self, head: Option<RecordIdentifier>) -> Result<()> {
        #[cfg(test)]
        self.finalized_session_semantic_validations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let expected_segments: HashMap<SegmentIdentifier, SharedSegment> = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(identifier, (structure, bytes))| {
                (*identifier, (Arc::clone(structure), Arc::clone(bytes)))
            })
            .collect();
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
                .entry(write.archive_file_name.clone())
                .or_default()
                .push(write.identifier);
        }
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
            let expected_order =
                expected_archive_order
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
            let mut expected_graph = ExpectedGraph::new();
            let mut expected_binary_references = ExpectedBinaryReferences::new();
            for identifier in archive.segment_identifiers() {
                let Some((_, expected_bytes)) = expected_segments.get(&identifier) else {
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
                archive.validate_indexed_segment_entry(identifier)?;
                let actual_bytes =
                    archive
                        .segment_data(identifier)
                        .ok_or(Error::SegmentNotFound {
                            segment_identifier: identifier,
                        })?;
                if actual_bytes != expected_bytes.as_slice() {
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
                let expected_generation =
                    stored_segment_generation(identifier, &disk_segment.structure);
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
                    read_blob_identifiers(&provider, &disk_segment.structure).map_err(|error| {
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
            validate_exact_archive_trailers(
                archive,
                archive.file_name(),
                &expected_graph,
                &expected_binary_references,
            )?;
        }
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
    fn close_archive_writer(&self, tar_writer: TarArchiveWriter) -> Result<()> {
        let path = tar_writer.path().to_owned();
        if !tar_writer.close()? {
            return Ok(());
        }
        if let Some(source_metadata) = &self.cleanup_archive_metadata {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            preserve_file_metadata(&file, source_metadata)?;
        }
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
    fn write_initial_node(&self) -> Result<RecordIdentifier> {
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
fn journal_needs_separator(path: &Path) -> Result<bool> {
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
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&segment_identifier)
        {
            return Ok(SegmentView {
                structure: Arc::clone(structure),
                bytes: crate::segment::view::SegmentBytes::Shared(Arc::clone(bytes)),
            });
        }
        for archive in &self.base_archives {
            if let Some(bytes) = archive.segment_data(segment_identifier) {
                let cached = self
                    .parsed_segment_cache
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&segment_identifier)
                    .cloned();
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
    store: &'store WritableRepository,
}

impl SegmentSink for StoreSink<'_> {
    fn write_segment(&mut self, segment: BuiltSegment) -> Result<()> {
        self.store.persist_segment(segment)
    }
}

/// One archive's physical disposition in a standalone cleanup plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlannedArchiveSweep {
    /// Every entry is reclaimable; the archive can be unlinked whole.
    Remove {
        file_name: String,
        segment_count: usize,
        file_bytes: u64,
    },
    /// Enough entries are reclaimable to rewrite the survivors.
    Rewrite {
        file_name: String,
        replacement_name: String,
        segment_count: usize,
        eligible_entry_bytes: u64,
    },
    /// Reclaimable entries exist, but Oak's 25% savings gate keeps the
    /// archive byte-for-byte unchanged.
    DeferredBySavings {
        file_name: String,
        segment_count: usize,
        eligible_entry_bytes: u64,
    },
    /// Reclaimable entries exist, but the archive has exhausted the `a` to
    /// `z` rewrite namespace.
    DeferredAtLastGeneration {
        file_name: String,
        segment_count: usize,
        eligible_entry_bytes: u64,
    },
    /// Another generation pathname blocks a rewrite target or would be
    /// promoted by whole-file removal. Cleanup never truncates or promotes it
    /// implicitly; archive hygiene must classify it first.
    BlockedByOccupiedGeneration {
        file_name: String,
        occupied_name: String,
        segment_count: usize,
        eligible_entry_bytes: u64,
    },
}

impl PlannedArchiveSweep {
    pub(crate) fn file_name(&self) -> &str {
        match self {
            Self::Remove { file_name, .. }
            | Self::Rewrite { file_name, .. }
            | Self::DeferredBySavings { file_name, .. }
            | Self::DeferredAtLastGeneration { file_name, .. }
            | Self::BlockedByOccupiedGeneration { file_name, .. } => file_name,
        }
    }

    pub(crate) fn changes_disk(&self) -> bool {
        matches!(self, Self::Remove { .. } | Self::Rewrite { .. })
    }
}

/// Read-only result of the standalone FULL/retained-two mark phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StandaloneSegmentCleanupPlan {
    pub(crate) archives: Vec<PlannedArchiveSweep>,
    pub(crate) marked_segments: usize,
    reclaimable: std::collections::HashSet<SegmentIdentifier>,
}

impl StandaloneSegmentCleanupPlan {
    #[cfg(test)]
    pub(crate) fn reclaimable_segments(&self) -> &std::collections::HashSet<SegmentIdentifier> {
        &self.reclaimable
    }
}

/// Physical result of applying a standalone segment cleanup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StandaloneSegmentCleanupOutcome {
    pub(crate) rewritten_archives: usize,
    pub(crate) removed_archives: usize,
    pub(crate) removed_segments: usize,
    pub(crate) archive_bytes_before: u64,
    pub(crate) archive_bytes_after: u64,
    pub(crate) deletion_failures: Vec<DeferredFileDeletion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeferredFileDeletion {
    pub(crate) file_name: String,
    pub(crate) error: String,
    pub(crate) target_was_already_absent: bool,
}

/// The observed physical result of one archive sweep attempt.
///
/// `newly_unavailable` is populated only by the mutation branch that proved
/// its unlink or higher-generation publication completed. Callers must use
/// this set, rather than the earlier plan, when filtering graph edges in a
/// later rewrite.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ArchiveSweepDisposition {
    #[default]
    Unchanged,
    Removed,
    Rewritten,
}

#[derive(Debug, Default)]
struct ArchiveSweepOutcome {
    disposition: ArchiveSweepDisposition,
    deletion_failures: Vec<DeferredFileDeletion>,
    newly_unavailable: std::collections::HashSet<SegmentIdentifier>,
}

/// Plans Oak's standalone cleanup predicate: FULL GC, the current committed
/// head generation as reference, and two retained generations. `protected`
/// is a conservative keep-veto for journal history; it never makes a segment
/// reclaimable and therefore cannot weaken Oak's head/checkpoint safety.
pub(crate) fn plan_standalone_segment_cleanup(
    directory: &Path,
    repository: &crate::store::Repository,
    reference: GarbageCollectionGeneration,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<StandaloneSegmentCleanupPlan> {
    reject_duplicate_active_segments(repository.archives())?;
    certify_active_archives(repository, repository.archives())?;
    analyze_standalone_segment_cleanup(
        directory,
        repository.archives(),
        reference,
        current_head_segment,
        protected,
        observer,
    )
}

/// Segment identifiers that the actionable archive dispositions in `plan`
/// would make unavailable. Deferred and blocked archives contribute none.
/// Duplicate identifiers have already been rejected while constructing the
/// plan, so each identifier has exactly one physical active copy.
pub(crate) fn planned_unavailable_segments(
    directory: &Path,
    plan: &StandaloneSegmentCleanupPlan,
) -> Result<std::collections::HashSet<SegmentIdentifier>> {
    let actionable: std::collections::HashSet<&str> = plan
        .archives
        .iter()
        .filter(|archive| archive.changes_disk())
        .map(PlannedArchiveSweep::file_name)
        .collect();
    let archives = crate::store::open_all_archives(directory)?;
    let mut unavailable = std::collections::HashSet::new();
    for archive in archives {
        if !actionable.contains(archive.file_name()) {
            continue;
        }
        let Some(index) = archive.index() else {
            return Err(Error::InvalidFormat {
                details: format!(
                    "cleanup planned to mutate recovered archive {}, which has no valid index",
                    archive.file_name()
                ),
            });
        };
        unavailable.extend(
            index
                .entries()
                .iter()
                .map(|entry| entry.segment_identifier)
                .filter(|identifier| plan.reclaimable.contains(identifier)),
        );
    }
    Ok(unavailable)
}

/// Replans under the caller's held repository lock, optionally proves that
/// the authoritative plan is the one previously confirmed, and applies every
/// physically actionable archive sweep. No `gc.log` entry is written: this is
/// standalone cleanup, not a completed compaction cycle.
#[allow(
    clippy::too_many_lines,
    reason = "replanning, mutation, and exact partial-outcome accounting form one locked application sequence"
)]
pub(crate) fn apply_standalone_segment_cleanup(
    directory: &Path,
    reference: GarbageCollectionGeneration,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    expected: Option<&StandaloneSegmentCleanupPlan>,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<(
    StandaloneSegmentCleanupPlan,
    StandaloneSegmentCleanupOutcome,
)> {
    // A cleanup apply is allowed to destroy an entire source archive. Open a
    // fresh, lazy provider over the exact active set and certify every source
    // before the first mutation; recovered/indexless archives and incomplete
    // graph/BRF metadata are never eligible for standalone cleanup.
    let repository = crate::store::Repository::open_with_progress(directory, observer)?;
    reject_duplicate_active_segments(repository.archives())?;
    certify_active_archives_with_progress(&repository, repository.archives(), observer)?;
    apply_standalone_segment_cleanup_from_archives(
        directory,
        repository.archives(),
        Some(&repository),
        reference,
        current_head_segment,
        protected,
        expected,
        observer,
        #[cfg(test)]
        None,
    )
}

#[cfg(test)]
type StandaloneAfterPlanHook<'hook> = dyn Fn(&StandaloneSegmentCleanupPlan) -> Result<()> + 'hook;

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the test-only uncertified path and production certified path share one ordered mutation engine"
)]
fn apply_standalone_segment_cleanup_from_archives(
    directory: &Path,
    archives: &[TarArchiveReader],
    source_certificate_provider: Option<&dyn SegmentProvider>,
    reference: GarbageCollectionGeneration,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    expected: Option<&StandaloneSegmentCleanupPlan>,
    observer: &mut dyn crate::progress::ProgressObserver,
    #[cfg(test)] after_plan: Option<&StandaloneAfterPlanHook<'_>>,
) -> Result<(
    StandaloneSegmentCleanupPlan,
    StandaloneSegmentCleanupOutcome,
)> {
    let plan = crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "replanning segment reclamation",
            crate::progress::WorkUnit::Archives,
        )
        .with_total(crate::progress::count(archives.len())),
        |observer| {
            analyze_standalone_segment_cleanup(
                directory,
                archives,
                reference,
                current_head_segment,
                protected,
                observer,
            )
        },
    )?;
    if expected.is_some_and(|expected| expected != &plan) {
        return Err(Error::InvalidFormat {
            details: "the standalone segment-cleanup plan changed after confirmation; refusing \
                      to apply an unconfirmed archive mutation"
                .to_owned(),
        });
    }

    let archive_bytes_before = archive_file_bytes(directory)?;
    #[cfg(test)]
    if let Some(after_plan) = after_plan {
        after_plan(&plan)?;
    }
    let provider_order: Vec<&TarArchiveReader> = archives.iter().collect();
    let mut fallback_provider = None;
    let mut deletion_failures = Vec::new();
    let mut actually_unavailable = std::collections::HashSet::new();
    let mut observed_sweeps = HashMap::new();
    let planned_archives: HashMap<_, _> = plan
        .archives
        .iter()
        .map(|planned| (planned.file_name(), planned))
        .collect();
    // Apply whole removals first, then rewrites. A graph edge is filtered
    // only after its target has really become unavailable (or when the same
    // rewrite is about to make it unavailable). This retains conservative
    // extra edges to deferred, blocked, later, or failed sweep targets.
    crate::progress::observe(
        observer,
        &crate::progress::Step::new("sweeping archives", crate::progress::WorkUnit::Archives)
            .with_total(crate::progress::count(
                plan.archives
                    .iter()
                    .filter(|planned| planned.changes_disk())
                    .count(),
            )),
        |observer| {
            let mut swept = 0usize;
            for rewrite_phase in [false, true] {
                for archive in archives {
                    let Some(planned) = planned_archives.get(archive.file_name()) else {
                        continue;
                    };
                    let is_rewrite = matches!(planned, PlannedArchiveSweep::Rewrite { .. });
                    let is_remove = matches!(planned, PlannedArchiveSweep::Remove { .. });
                    if (!rewrite_phase && !is_remove) || (rewrite_phase && !is_rewrite) {
                        continue;
                    }
                    observer.step_advanced(crate::progress::count(swept));
                    let outcome = sweep_one_archive(
                        directory,
                        archive,
                        &plan.reclaimable,
                        &actually_unavailable,
                        &provider_order,
                        &mut fallback_provider,
                        source_certificate_provider,
                    )?;
                    if outcome.disposition != ArchiveSweepDisposition::Unchanged {
                        observed_sweeps.insert(
                            archive.file_name().to_owned(),
                            (outcome.disposition, outcome.newly_unavailable.len()),
                        );
                    }
                    deletion_failures.extend(outcome.deletion_failures);
                    actually_unavailable.extend(outcome.newly_unavailable);
                    swept += 1;
                    observer.step_advanced(crate::progress::count(swept));
                }
                #[cfg(test)]
                if !rewrite_phase {
                    probe_archive_sweep_phase_boundary("sweep.removals-complete-before-rewrites")?;
                }
            }
            Ok::<(), Error>(())
        },
    )?;
    drop(fallback_provider);
    drop(provider_order);
    sync_directory_strict(directory)?;

    let mut outcome = StandaloneSegmentCleanupOutcome {
        archive_bytes_before,
        archive_bytes_after: archive_file_bytes(directory)?,
        deletion_failures,
        ..StandaloneSegmentCleanupOutcome::default()
    };
    for archive in &plan.archives {
        match archive {
            PlannedArchiveSweep::Remove { file_name, .. }
                if observed_sweeps
                    .get(file_name)
                    .is_some_and(|(disposition, _)| {
                        *disposition == ArchiveSweepDisposition::Removed
                    }) =>
            {
                if directory.join(file_name).try_exists()? {
                    if !outcome
                        .deletion_failures
                        .iter()
                        .any(|failure| failure.file_name == *file_name)
                    {
                        outcome.deletion_failures.push(DeferredFileDeletion {
                            file_name: file_name.clone(),
                            error: "file reappeared after the archive unlink succeeded".to_owned(),
                            target_was_already_absent: false,
                        });
                    }
                } else {
                    outcome.removed_archives += 1;
                    outcome.removed_segments += observed_sweeps[file_name].1;
                }
            }
            PlannedArchiveSweep::Rewrite {
                file_name,
                replacement_name,
                ..
            } if observed_sweeps
                .get(file_name)
                .is_some_and(|(disposition, _)| {
                    *disposition == ArchiveSweepDisposition::Rewritten
                }) =>
            {
                if !directory.join(replacement_name).try_exists()? {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "cleanup published rewrite {file_name}, but replacement \
                             {replacement_name} is absent"
                        ),
                    });
                }
                outcome.rewritten_archives += 1;
                outcome.removed_segments += observed_sweeps[file_name].1;
                if directory.join(file_name).try_exists()?
                    && !outcome
                        .deletion_failures
                        .iter()
                        .any(|failure| failure.file_name == *file_name)
                {
                    outcome.deletion_failures.push(DeferredFileDeletion {
                        file_name: file_name.clone(),
                        error: "source archive remained after replacement publication".to_owned(),
                        target_was_already_absent: false,
                    });
                }
            }
            PlannedArchiveSweep::Remove { file_name, .. }
            | PlannedArchiveSweep::Rewrite { file_name, .. } => {
                if observed_sweeps.contains_key(file_name) {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "archive sweep for {file_name} returned a disposition inconsistent with the authoritative plan"
                        ),
                    });
                }
            }
            PlannedArchiveSweep::DeferredBySavings { .. }
            | PlannedArchiveSweep::DeferredAtLastGeneration { .. }
            | PlannedArchiveSweep::BlockedByOccupiedGeneration { .. } => {}
        }
    }
    outcome.deletion_failures.sort_by(|left, right| {
        left.file_name
            .cmp(&right.file_name)
            .then_with(|| left.error.cmp(&right.error))
    });
    outcome.deletion_failures.dedup();
    Ok((plan, outcome))
}

fn archive_file_bytes(directory: &Path) -> Result<u64> {
    let mut total = 0u64;
    for file_name in crate::store::list_archive_file_names(directory)? {
        if ArchiveFileName::parse(&file_name).is_some() {
            total = total
                .checked_add(std::fs::symlink_metadata(directory.join(file_name))?.len())
                .ok_or_else(|| Error::InvalidFormat {
                    details: "archive byte accounting overflow".to_owned(),
                })?;
        }
    }
    Ok(total)
}

pub(crate) fn sync_directory_strict(directory: &Path) -> Result<()> {
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

/// Stable identity of a regular file used in a destructive maintenance step.
/// The mutating writer is Unix-only, where `(device, inode)` binds a held file
/// descriptor to directory entries without trusting a replaceable pathname.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RegularFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RegularFileIdentity;

fn regular_file_identity(metadata: &std::fs::Metadata) -> Result<RegularFileIdentity> {
    if !metadata.file_type().is_file() {
        return Err(Error::InvalidFormat {
            details: "destructive maintenance target is not a regular file".to_owned(),
        });
    }
    filesystem_object_identity(metadata)
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "the non-Unix implementation is an intentional runtime refusal while Unix returns a verified identity"
)]
fn filesystem_object_identity(metadata: &std::fs::Metadata) -> Result<RegularFileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(RegularFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(Error::InvalidFormat {
            details: "destructive archive maintenance requires Unix file-identity checks"
                .to_owned(),
        })
    }
}

fn open_regular_file_no_follow(path: &Path, write: bool) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    regular_file_identity(&file.metadata()?)?;
    Ok(file)
}

fn held_file_identity(file: &File) -> Result<RegularFileIdentity> {
    regular_file_identity(&file.metadata()?)
}

fn path_file_identity(path: &Path) -> Result<RegularFileIdentity> {
    let metadata = std::fs::symlink_metadata(path)?;
    regular_file_identity(&metadata).map_err(|error| Error::InvalidFormat {
        details: format!(
            "{} is not the regular file required for destructive maintenance ({error})",
            path.display()
        ),
    })
}

fn path_object_identity(path: &Path) -> Result<RegularFileIdentity> {
    filesystem_object_identity(&std::fs::symlink_metadata(path)?)
}

fn require_path_file_identity(
    path: &Path,
    expected: RegularFileIdentity,
    description: &str,
) -> Result<()> {
    let actual = path_file_identity(path)?;
    if actual != expected {
        return Err(Error::InvalidFormat {
            details: format!(
                "{description} {} changed file identity before destructive maintenance",
                path.display()
            ),
        });
    }
    Ok(())
}

fn require_held_file_identity(
    file: &File,
    expected: RegularFileIdentity,
    description: &str,
) -> Result<()> {
    if held_file_identity(file)? != expected {
        return Err(Error::InvalidFormat {
            details: format!("{description} held file descriptor changed identity"),
        });
    }
    Ok(())
}

/// Removes a publication only when its pathname still names the inode that
/// this process proved it had just linked. A concurrent replacement is never
/// unlinked on the strength of an older observation.
fn remove_published_link_if_same(
    directory: &Path,
    path: &Path,
    published_identity: Option<RegularFileIdentity>,
) -> Result<()> {
    if let Some(published_identity) = published_identity
        && path_object_identity(path).ok() == Some(published_identity)
    {
        std::fs::remove_file(path)?;
    }
    sync_directory_strict(directory)
}

/// Owns cleanup of an archive staging inode until that inode has passed full
/// validation. The held descriptor is captured immediately after lazy
/// `create_new` succeeds, including when the first write itself returns an
/// error. Drop never trusts the pathname alone: a substituted object is left
/// untouched for diagnosis.
struct UncommittedArchiveStaging {
    directory: PathBuf,
    path: PathBuf,
    held: Option<(File, RegularFileIdentity)>,
    armed: bool,
}

impl UncommittedArchiveStaging {
    fn new(directory: &Path, path: PathBuf) -> Self {
        Self {
            directory: directory.to_owned(),
            path,
            held: None,
            armed: true,
        }
    }

    fn capture_created_file(&mut self, writer: &TarArchiveWriter) -> Result<()> {
        if self.held.is_some() {
            return Ok(());
        }
        let Some(file) = writer.created_file() else {
            return Ok(());
        };
        let held = file.try_clone()?;
        let identity = held_file_identity(&held)?;
        require_path_file_identity(&self.path, identity, "new archive staging file")?;
        self.held = Some((held, identity));
        Ok(())
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.held = None;
    }
}

impl Drop for UncommittedArchiveStaging {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some((held, identity)) = &self.held else {
            return;
        };
        if held_file_identity(held).ok() == Some(*identity)
            && path_object_identity(&self.path).ok() == Some(*identity)
            && std::fs::remove_file(&self.path).is_ok()
        {
            let _ = sync_directory_strict(&self.directory);
        }
    }
}

/// Copies durability-relevant filesystem metadata from `source` onto an open
/// replacement file and proves the result before publication.
///
/// Unix cleanup may run as an administrator, so relying on create-time owner
/// or umask can publish a root-owned or unreadable repository file. Ownership
/// and permission mismatches therefore fail closed rather than being warned.
pub(crate) fn preserve_file_metadata(target: &File, source: &std::fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let current = target.metadata()?;
        if current.uid() != source.uid() || current.gid() != source.gid() {
            // SAFETY: `target` owns a live file descriptor for the staged
            // regular file, and uid/gid values come directly from stat(2).
            if unsafe { libc::fchown(target.as_raw_fd(), source.uid(), source.gid()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        target.set_permissions(std::fs::Permissions::from_mode(source.mode()))?;
        target.sync_all()?;
        let installed = target.metadata()?;
        if installed.uid() != source.uid()
            || installed.gid() != source.gid()
            || installed.mode() & 0o7777 != source.mode() & 0o7777
        {
            return Err(Error::InvalidFormat {
                details: "replacement file ownership or permissions differ from the source after preservation"
                    .to_owned(),
            });
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        target.set_permissions(source.permissions())?;
        target.sync_all()?;
        Ok(())
    }
}

fn analyze_standalone_segment_cleanup(
    directory: &Path,
    archives: &[TarArchiveReader],
    reference: GarbageCollectionGeneration,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<StandaloneSegmentCleanupPlan> {
    reject_duplicate_active_segments(archives)?;

    let mut references = std::collections::HashSet::new();
    let mut reclaimable = std::collections::HashSet::new();
    let policy = ReclaimPolicy {
        reference,
        full: true,
        retained_generations: 2,
        protected_data_segments: protected,
    };
    // A skipped standalone compaction uses the exact durable head as Oak's
    // compacted-root boundary. In global reverse write order, compacted
    // entries newer than that root are incomplete/dangling compaction output.
    // One shared state is normative: resetting it per archive could delete
    // valid compacted segments in every older archive.
    let mut ahead_of_root = Some(current_head_segment);
    for (marked, archive) in archives.iter().enumerate() {
        observer.step_advanced(crate::progress::count(marked));
        mark_one_archive(
            archive,
            policy,
            &mut references,
            &mut reclaimable,
            &mut ahead_of_root,
        )?;
    }
    observer.step_advanced(crate::progress::count(archives.len()));
    if let Some(missing_root) = ahead_of_root {
        return Err(Error::InvalidFormat {
            details: format!(
                "current head segment {missing_root} was not encountered in global reverse archive order; refusing to apply the stateful dangling-future rule"
            ),
        });
    }

    let mut planned_archives = Vec::new();
    for archive in archives {
        if let Some(planned) = plan_archive_sweep(directory, archive, &reclaimable)? {
            planned_archives.push(planned);
        }
    }
    planned_archives.sort_by(|left, right| left.file_name().cmp(right.file_name()));

    Ok(StandaloneSegmentCleanupPlan {
        archives: planned_archives,
        marked_segments: reclaimable.len(),
        reclaimable,
    })
}

fn reject_duplicate_active_segments(archives: &[TarArchiveReader]) -> Result<()> {
    unique_active_segment_locations(archives).map(|_| ())
}

fn unique_active_segment_locations(
    archives: &[TarArchiveReader],
) -> Result<HashMap<SegmentIdentifier, &str>> {
    let mut locations: HashMap<SegmentIdentifier, &str> = HashMap::new();
    for archive in archives {
        for identifier in archive.segment_identifiers() {
            if let Some(previous) = locations.insert(identifier, archive.file_name()) {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "segment {identifier} occurs in both active archives {previous} and {}; \
                         refusing cleanup because a store-wide reclaim decision could remove the \
                         authoritative copy",
                        archive.file_name()
                    ),
                });
            }
        }
    }
    Ok(locations)
}

fn plan_archive_sweep(
    directory: &Path,
    archive: &TarArchiveReader,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
) -> Result<Option<PlannedArchiveSweep>> {
    let Some(name) = ArchiveFileName::parse(archive.file_name()) else {
        return Ok(None);
    };
    let Some(index) = archive.index() else {
        return Ok(None);
    };
    let mut before_entry_bytes = 0u64;
    let mut after_entry_bytes = 0u64;
    let mut eligible_entry_bytes = 0u64;
    let mut reclaimable_count = 0usize;
    for entry in index.entries() {
        let occupied = segment_entry_disk_bytes(archive.file_name(), entry.size)?;
        before_entry_bytes =
            before_entry_bytes
                .checked_add(occupied)
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!(
                        "archive size accounting overflow in {}",
                        archive.file_name()
                    ),
                })?;
        if reclaimable.contains(&entry.segment_identifier) {
            reclaimable_count += 1;
            eligible_entry_bytes =
                eligible_entry_bytes
                    .checked_add(occupied)
                    .ok_or_else(|| Error::InvalidFormat {
                        details: format!(
                            "cleanup size accounting overflow in {}",
                            archive.file_name()
                        ),
                    })?;
        } else {
            after_entry_bytes =
                after_entry_bytes
                    .checked_add(occupied)
                    .ok_or_else(|| Error::InvalidFormat {
                        details: format!(
                            "archive size accounting overflow in {}",
                            archive.file_name()
                        ),
                    })?;
        }
    }
    if reclaimable_count == 0 {
        return Ok(None);
    }
    if reclaimable_count == index.entries().len() {
        // Another generation normally cannot be active alongside this
        // reader: only one valid winner is selected. Removing that winner,
        // however, would promote any lower stale copy or higher recovered
        // residue on the next open, potentially shadowing healthy segments
        // with obsolete/damaged copies. Archive hygiene must classify every
        // alternate before whole-file deletion proceeds.
        if let Some(occupied_name) = alternate_generation_residue(directory, &name)? {
            return Ok(Some(PlannedArchiveSweep::BlockedByOccupiedGeneration {
                file_name: name.file_name,
                occupied_name,
                segment_count: reclaimable_count,
                eligible_entry_bytes,
            }));
        }
        return Ok(Some(PlannedArchiveSweep::Remove {
            file_name: name.file_name,
            segment_count: reclaimable_count,
            file_bytes: archive.file_size(),
        }));
    }
    // Exact Oak gate: both sizes are Java `int`s, multiplication by three
    // wraps in signed 32-bit arithmetic, division truncates toward zero, and
    // equality at 75% is deferred. Prove the accumulated entry sizes fit the
    // source domain before reproducing those arithmetic semantics.
    if oak_sweep_defers(before_entry_bytes, after_entry_bytes, archive.file_name())? {
        return Ok(Some(PlannedArchiveSweep::DeferredBySavings {
            file_name: name.file_name,
            segment_count: reclaimable_count,
            eligible_entry_bytes,
        }));
    }
    if name.file_generation >= 'z' {
        return Ok(Some(PlannedArchiveSweep::DeferredAtLastGeneration {
            file_name: name.file_name,
            segment_count: reclaimable_count,
            eligible_entry_bytes,
        }));
    }
    let next_letter = char::from(name.file_generation as u8 + 1);
    let replacement_name = format!("data{:05}{next_letter}.tar", name.archive_number);
    if directory.join(&replacement_name).try_exists()? {
        return Ok(Some(PlannedArchiveSweep::BlockedByOccupiedGeneration {
            file_name: name.file_name,
            occupied_name: replacement_name,
            segment_count: reclaimable_count,
            eligible_entry_bytes,
        }));
    }
    // Applying a multi-archive plan must not discover staging exhaustion only
    // after earlier archives were already swept. This read-only reservation
    // preflight is repeated by the exclusive writer at apply time, where a
    // race still fails safely without touching the source.
    next_archive_staging_name(directory, &replacement_name)?;
    Ok(Some(PlannedArchiveSweep::Rewrite {
        file_name: name.file_name,
        replacement_name,
        segment_count: reclaimable_count,
        eligible_entry_bytes,
    }))
}

/// Java's signed-`int` `beforeSize * 3 / 4` sweep threshold.
fn oak_sweep_threshold(before_size: i32) -> i32 {
    before_size.wrapping_mul(3) / 4
}

fn oak_sweep_defers(
    before_entry_bytes: u64,
    after_entry_bytes: u64,
    archive: &str,
) -> Result<bool> {
    let before_size = i32::try_from(before_entry_bytes).map_err(|_| Error::InvalidFormat {
        details: format!("archive entry bytes exceed Java's signed-i32 domain in {archive}"),
    })?;
    let after_size = i32::try_from(after_entry_bytes).map_err(|_| Error::InvalidFormat {
        details: format!("surviving entry bytes exceed Java's signed-i32 domain in {archive}"),
    })?;
    Ok(after_size >= oak_sweep_threshold(before_size))
}

fn segment_entry_disk_bytes(archive_name: &str, size: u32) -> Result<u64> {
    512u64
        .checked_add(u64::from(size))
        .and_then(|occupied| {
            occupied.checked_add(crate::writer::tar_writer::padding_size(size as usize) as u64)
        })
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("segment-entry size accounting overflow in {archive_name}"),
        })
}

fn alternate_generation_residue(
    directory: &Path,
    active: &ArchiveFileName,
) -> Result<Option<String>> {
    Ok(crate::store::list_archive_file_names(directory)?
        .into_iter()
        .filter_map(|file_name| ArchiveFileName::parse(&file_name))
        .filter(|candidate| {
            candidate.archive_number == active.archive_number
                && candidate.file_name != active.file_name
        })
        .max_by_key(|candidate| (candidate.file_generation, candidate.file_name.clone()))
        .map(|candidate| candidate.file_name))
}

/// Oak's `TarReader.mark` for one archive: entries are visited in
/// *reverse* file order, so a bulk segment — always written before the
/// data segments referencing it — is judged after all of them. Apart from
/// the stateful dangling-future rule, data segments use the generation
/// predicate and non-data segments use membership in the shared `references`
/// set (`remove` both queries and consumes, exactly like Java). Every *kept*
/// data segment protects the non-data segments it references — through the
/// graph trailer when present, else the segment header's reference list —
/// following every target for which Java's `isDataSegmentId` is false.
/// Reclaimable identifiers are
/// accumulated into one store-wide set shared by every archive.
#[derive(Clone, Copy)]
struct ReclaimPolicy<'protected> {
    reference: GarbageCollectionGeneration,
    full: bool,
    retained_generations: i32,
    protected_data_segments: &'protected std::collections::HashSet<SegmentIdentifier>,
}

fn mark_one_archive(
    reader: &TarArchiveReader,
    policy: ReclaimPolicy<'_>,
    references: &mut std::collections::HashSet<SegmentIdentifier>,
    reclaimable: &mut std::collections::HashSet<SegmentIdentifier>,
    ahead_of_root: &mut Option<SegmentIdentifier>,
) -> Result<()> {
    let mut entries: Vec<(SegmentIdentifier, Option<GarbageCollectionGeneration>, u32)> =
        match reader.index() {
            Some(index) => index
                .entries()
                .iter()
                .copied()
                .map(|entry| {
                    (
                        entry.segment_identifier,
                        Some(GarbageCollectionGeneration {
                            generation: entry.generation,
                            full_generation: entry.full_generation,
                            is_compacted: entry.is_compacted,
                        }),
                        entry.position,
                    )
                })
                .collect(),
            None => reader
                .segment_identifiers()
                .enumerate()
                .map(|(position, identifier)| (identifier, None, position as u32))
                .collect(),
        };
    entries.sort_by_key(|(_, _, position)| *position);

    let graph_adjacency: Option<HashMap<SegmentIdentifier, Vec<SegmentIdentifier>>> = reader
        .segment_graph()
        .map(|graph| graph.adjacency.into_iter().collect());

    for (identifier, generation, _) in entries.iter().rev() {
        let identifier = *identifier;
        let was_referenced = references.remove(&identifier);
        // Oak's `aheadOfRoot &= id != root` both excludes the root itself
        // and switches this rule off permanently for every older entry.
        let reached_root = ahead_of_root.is_some_and(|root| root == identifier);
        if reached_root {
            *ahead_of_root = None;
        }
        let dangling_future =
            ahead_of_root.is_some() && generation.is_some_and(|generation| generation.is_compacted);
        let protected_data =
            identifier.is_data_segment() && policy.protected_data_segments.contains(&identifier);
        let reclaim = if reached_root || protected_data {
            // Readable journal history is an additional conservative veto,
            // including for an otherwise dangling-future data segment. The
            // exact committed root is an unconditional veto too: cleanup's
            // outer generation-invariant check should make this redundant,
            // but a corrupt index must never make this primitive delete it.
            false
        } else if dangling_future {
            // This precedes kind/reachability checks exactly like Oak:
            // compacted bulk entries written after the root are dangling too.
            true
        } else if identifier.is_data_segment() {
            generation.is_some_and(|generation| {
                is_reclaimable(
                    policy.reference,
                    generation,
                    policy.full,
                    policy.retained_generations,
                )
            })
        } else {
            // Recovered archives cannot be swept, so none of their entries
            // may be marked. They must still participate in reverse-order
            // bulk-reference propagation or an older indexed archive could
            // lose a bulk segment referenced by recovered live data.
            generation.is_some() && !was_referenced
        };
        if reclaim {
            reclaimable.insert(identifier);
        } else if identifier.is_data_segment() {
            let targets = match &graph_adjacency {
                Some(adjacency) => adjacency.get(&identifier).cloned().unwrap_or_default(),
                None => {
                    ParsedSegment::parse(
                        identifier,
                        reader
                            .segment_data(identifier)
                            .ok_or(Error::SegmentNotFound {
                                segment_identifier: identifier,
                            })?,
                    )?
                    .referenced_segments
                }
            };
            for target in targets {
                if !target.is_data_segment() {
                    references.insert(target);
                }
            }
        }
    }
    Ok(())
}

/// Oak's `TarReader.sweep` for one base archive, with a precomputed
/// reclaim set from the mark phase: entries are judged and rewritten in
/// original file-position order, the generation triple comes from the
/// index entry, sub-25% savings keep the file untouched, and the graph
/// and binary-references trailers are *filtered* from the existing ones,
/// never recomputed — a raw segment scan cannot see every catalog entry,
/// and dropping one would let AEM's blob garbage collection delete a
/// still-referenced binary.
type ExpectedGraph = HashMap<SegmentIdentifier, std::collections::HashSet<SegmentIdentifier>>;
type ExpectedBinaryReferences =
    HashMap<(i32, i32, bool), HashMap<SegmentIdentifier, std::collections::HashSet<String>>>;

fn stored_segment_generation(
    identifier: SegmentIdentifier,
    structure: &ParsedSegment,
) -> GarbageCollectionGeneration {
    if identifier.is_data_segment() {
        GarbageCollectionGeneration {
            generation: structure.generation,
            full_generation: structure.full_generation,
            is_compacted: structure.is_compacted,
        }
    } else {
        GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        }
    }
}

fn normalized_archive_graph(
    archive: &TarArchiveReader,
    description: &str,
) -> Result<ExpectedGraph> {
    let graph = archive
        .segment_graph()
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("{description} has no valid segment graph"),
        })?;
    let mut normalized_graph = ExpectedGraph::new();
    for (source, targets) in graph.adjacency {
        let normalized: std::collections::HashSet<_> = targets.iter().copied().collect();
        if normalized.len() != targets.len()
            || normalized_graph.insert(source, normalized).is_some()
        {
            return Err(Error::InvalidFormat {
                details: format!("{description} contains duplicate graph rows or edges"),
            });
        }
    }
    Ok(normalized_graph)
}

fn normalized_archive_binary_references(
    archive: &TarArchiveReader,
    description: &str,
) -> Result<ExpectedBinaryReferences> {
    let catalog = archive
        .binary_references()
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("{description} has no valid binary-references catalog"),
        })?;
    let mut normalized_catalog = ExpectedBinaryReferences::new();
    for generation in catalog.generations {
        let key = (
            generation.generation,
            generation.full_generation,
            generation.is_compacted,
        );
        if normalized_catalog.contains_key(&key) {
            return Err(Error::InvalidFormat {
                details: format!("{description} repeats a binary-reference generation group"),
            });
        }
        let mut sources = HashMap::new();
        for (source, references) in generation.segments {
            let normalized: std::collections::HashSet<_> = references.iter().cloned().collect();
            if normalized.len() != references.len() || sources.insert(source, normalized).is_some()
            {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "{description} contains duplicate binary-reference rows or identifiers"
                    ),
                });
            }
        }
        normalized_catalog.insert(key, sources);
    }
    Ok(normalized_catalog)
}

fn validate_exact_archive_trailers(
    archive: &TarArchiveReader,
    description: &str,
    expected_graph: &ExpectedGraph,
    expected_binary_references: &ExpectedBinaryReferences,
) -> Result<()> {
    if normalized_archive_graph(archive, description)? != *expected_graph {
        return Err(Error::InvalidFormat {
            details: format!(
                "{description} segment graph differs from the exact reconstructed graph"
            ),
        });
    }
    if normalized_archive_binary_references(archive, description)? != *expected_binary_references {
        return Err(Error::InvalidFormat {
            details: format!(
                "{description} binary-reference catalog differs from the exact reconstructed catalog"
            ),
        });
    }
    Ok(())
}

/// Produces the fail-closed certificate required before cleanup may use an
/// active archive as a mark source or destroy it. A valid trailer index alone
/// is insufficient: every indexed TAR entry must still name and checksum its
/// payload, every data-segment header must agree with the index generation,
/// and the graph/BRF trailers must be the exact metadata reconstructed from
/// those payloads through the complete active repository provider.
pub(crate) fn certify_active_archive(
    provider: &dyn SegmentProvider,
    archive: &TarArchiveReader,
) -> Result<()> {
    let description = format!("active archive {}", archive.file_name());
    if archive.is_recovered() || archive.index().is_none() {
        return Err(Error::InvalidFormat {
            details: format!("{description} is recovered and has no valid index"),
        });
    }

    let mut expected_graph = ExpectedGraph::new();
    let mut expected_binary_references = ExpectedBinaryReferences::new();
    let index = archive
        .index()
        .expect("recovered/indexless archives were rejected above");
    for entry in index.entries() {
        let identifier = entry.segment_identifier;
        archive.validate_indexed_segment_entry(identifier)?;
        if !identifier.is_data_segment() {
            continue;
        }
        let bytes = archive
            .segment_data(identifier)
            .ok_or(Error::SegmentNotFound {
                segment_identifier: identifier,
            })?;
        let structure = ParsedSegment::parse(identifier, bytes)?;
        if entry.generation != structure.generation
            || entry.full_generation != structure.full_generation
            || entry.is_compacted != structure.is_compacted
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{description} has index/header generation disagreement for segment {identifier}"
                ),
            });
        }
        if !structure.referenced_segments.is_empty() {
            expected_graph.insert(
                identifier,
                structure.referenced_segments.iter().copied().collect(),
            );
        }
        let binary_references =
            read_blob_identifiers(provider, &structure).map_err(|error| Error::InvalidFormat {
                details: format!(
                    "{description} cannot reconstruct binary references for segment {identifier} through the complete active repository ({error})"
                ),
            })?;
        if !binary_references.is_empty() {
            expected_binary_references
                .entry((
                    structure.generation,
                    structure.full_generation,
                    structure.is_compacted,
                ))
                .or_default()
                .insert(identifier, binary_references.into_iter().collect());
        }
    }

    validate_exact_archive_trailers(
        archive,
        &description,
        &expected_graph,
        &expected_binary_references,
    )
}

pub(crate) fn certify_active_archives(
    provider: &dyn SegmentProvider,
    archives: &[TarArchiveReader],
) -> Result<()> {
    certify_active_archives_with_progress(
        provider,
        archives,
        &mut crate::progress::DiscardedProgress,
    )
}

/// Certifies exactly like [`certify_active_archives`], reporting its own
/// step. It parses every data segment of every archive, so on a large
/// store it is minutes of work — and it runs immediately after the
/// operator has confirmed a destructive cleanup, which is the worst
/// possible moment to say nothing.
pub(crate) fn certify_active_archives_with_progress(
    provider: &dyn SegmentProvider,
    archives: &[TarArchiveReader],
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<()> {
    crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "certifying source archives",
            crate::progress::WorkUnit::Archives,
        )
        .with_total(crate::progress::count(archives.len())),
        |observer| {
            for (certified, archive) in archives.iter().enumerate() {
                observer.step_advanced(crate::progress::count(certified));
                certify_active_archive(provider, archive)?;
            }
            observer.step_advanced(crate::progress::count(archives.len()));
            Ok(())
        },
    )
}

fn next_archive_staging_name(directory: &Path, replacement_name: &str) -> Result<String> {
    for counter in 0..=999u16 {
        let candidate = format!("{replacement_name}.cleaning.{counter:03}");
        match std::fs::symlink_metadata(directory.join(&candidate)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::InvalidFormat {
        details: format!(
            "all 1000 exclusive staging names for archive {replacement_name} are occupied"
        ),
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "staging, semantic validation, atomic publication, and source unlinking form one deliberately linear safety sequence"
)]
fn sweep_one_archive<'archives>(
    directory: &Path,
    reader: &'archives TarArchiveReader,
    reclaimable: &std::collections::HashSet<SegmentIdentifier>,
    previously_unavailable_graph_targets: &std::collections::HashSet<SegmentIdentifier>,
    all_archives: &[&'archives TarArchiveReader],
    fallback_provider: &mut Option<ArchiveSegmentsProvider<'archives>>,
    source_certificate_provider: Option<&dyn SegmentProvider>,
) -> Result<ArchiveSweepOutcome> {
    let path = directory.join(reader.file_name());
    let Some(planned) = plan_archive_sweep(directory, reader, reclaimable)? else {
        return Ok(ArchiveSweepOutcome::default());
    };

    // Bind every actionable source to a no-follow descriptor and retain its
    // inode identity through the destructive syscall. Standalone cleanup and
    // ordinary post-compaction cleanup additionally repeat the complete source
    // certificate through this exact descriptor-backed mapping. Replanning
    // prevents a semantically different but still well-formed source from
    // silently proceeding after the locked plan.
    let reopened_source = if planned.changes_disk() {
        let source_file = open_regular_file_no_follow(&path, false)?;
        let source_identity = held_file_identity(&source_file)?;
        let reopened = TarArchiveReader::open_file(&path, &source_file)?;
        if let Some(provider) = source_certificate_provider {
            certify_reopened_active_archive(provider, &reopened)?;
        }
        if plan_archive_sweep(directory, &reopened, reclaimable)?.as_ref() != Some(&planned) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "the actionable source archive {} changed before its immediate cleanup certificate",
                    reader.file_name()
                ),
            });
        }
        Some((reopened, source_file, source_identity))
    } else {
        None
    };
    let reader = reopened_source
        .as_ref()
        .map_or(reader, |(reopened, _, _)| reopened);
    let Some(index) = reader.index() else {
        // Post-compaction cleanup retains Oak's conservative treatment of
        // recovered base archives. Standalone cleanup cannot reach this branch
        // because its source certificate rejects an indexless archive.
        return Ok(ArchiveSweepOutcome::default());
    };
    let planned_unavailable: std::collections::HashSet<_> = index
        .entries()
        .iter()
        .map(|entry| entry.segment_identifier)
        .filter(|identifier| reclaimable.contains(identifier))
        .collect();

    let (replacement_name, planned_reclaimable_count) = match planned {
        PlannedArchiveSweep::Remove { .. } => {
            let (_, source_file, source_identity) = reopened_source
                .as_ref()
                .expect("an archive removal always has an actionable reopened source");
            #[cfg(test)]
            crate::writer::cleanup_fault_injection::substitute_path_if_armed(
                "sweep.remove-before-source-identity",
                &path,
            )?;
            require_held_file_identity(source_file, *source_identity, "certified removal source")?;
            require_path_file_identity(&path, *source_identity, "certified removal source")?;
            #[cfg(test)]
            crate::writer::cleanup_fault_injection::remove_path_if_armed(
                "sweep.remove-before-source-unlink-not-found",
                &path,
            )?;
            // Deletion failures are consistency-safe: ordinarily the old
            // archive remains authoritative for retry; `NotFound` records
            // that another actor already achieved this exact unlink.
            return Ok(match std::fs::remove_file(&path) {
                Ok(()) => ArchiveSweepOutcome {
                    disposition: ArchiveSweepDisposition::Removed,
                    deletion_failures: Vec::new(),
                    newly_unavailable: planned_unavailable,
                },
                Err(error) => ArchiveSweepOutcome {
                    disposition: ArchiveSweepDisposition::Unchanged,
                    deletion_failures: vec![DeferredFileDeletion {
                        file_name: reader.file_name().to_owned(),
                        error: error.to_string(),
                        target_was_already_absent: error.kind() == std::io::ErrorKind::NotFound,
                    }],
                    newly_unavailable: std::collections::HashSet::new(),
                },
            });
        }
        PlannedArchiveSweep::Rewrite {
            replacement_name,
            segment_count,
            ..
        } => (replacement_name, segment_count),
        PlannedArchiveSweep::DeferredBySavings { .. }
        | PlannedArchiveSweep::DeferredAtLastGeneration { .. }
        | PlannedArchiveSweep::BlockedByOccupiedGeneration { .. } => {
            return Ok(ArchiveSweepOutcome::default());
        }
    };

    // Partition the entries in file-position order, accumulating Oak's
    // sweep arithmetic (`i64` cannot wrap where Java's `int` could not
    // either: entries are position-bounded below 2 GiB).
    let mut entries: Vec<_> = index.entries().to_vec();
    entries.sort_by_key(|entry| entry.position);
    let mut survivors = Vec::new();
    for entry in entries {
        if !reclaimable.contains(&entry.segment_identifier) {
            survivors.push(entry);
        }
    }
    debug_assert_eq!(
        index.entries().len() - survivors.len(),
        planned_reclaimable_count
    );
    let current_rewrite_targets = planned_unavailable;

    // These two proofs deliberately have different graph scopes. Production
    // active-source certification above reconstructs the complete, unfiltered
    // graph from payloads before mutation. A replacement `.gph` is derived
    // subtractively: source entries not copied by this rewrite are left out,
    // while targets are filtered against both identifiers this run previously
    // made unavailable and identifiers belonging to those omitted entries.
    // Staged and published validation below compare it with that exact view.
    let trailers = FilteredTrailers::from_archive(
        reader,
        reclaimable,
        previously_unavailable_graph_targets,
        &current_rewrite_targets,
    );
    // When the original archive has no readable catalog, survivor
    // references are reconstructed by a strict scan resolving across
    // every segment of every base archive — and the sweep fails closed
    // on an unresolvable identifier rather than publish an incomplete
    // catalog that could let blob garbage collection delete referenced
    // binaries. (Java would publish an *empty* catalog here; the scan is
    // a strict superset of that.)
    let scan_provider = if trailers.catalog.is_none() {
        if fallback_provider.is_none() {
            *fallback_provider = Some(archive_segments_provider(all_archives)?);
        }
        fallback_provider.as_ref()
    } else {
        None
    };

    // Build under a name that cannot participate in archive selection. A
    // crash or validation failure can therefore leave only non-active
    // residue; the healthy source remains the selected generation. Trailer
    // entry names still use the final logical basename.
    let staging_name = next_archive_staging_name(directory, &replacement_name)?;
    let staging_path = directory.join(&staging_name);
    let replacement_path = directory.join(&replacement_name);
    let mut uncommitted_staging = UncommittedArchiveStaging::new(directory, staging_path.clone());
    let (_, source_file, source_identity) = reopened_source
        .as_ref()
        .expect("an archive rewrite always has an actionable reopened source");
    let source_metadata = source_file.metadata()?;
    let mut writer =
        TarArchiveWriter::new_exclusive_staged(directory, &staging_name, &replacement_name);
    let mut expected_graph = ExpectedGraph::new();
    let mut expected_binary_references = ExpectedBinaryReferences::new();
    if let Some(catalog_entries) = &trailers.catalog {
        for (generation, segment, references) in catalog_entries {
            writer.add_binary_references(*generation, *segment, references.iter().cloned());
            expected_binary_references
                .entry((
                    generation.generation,
                    generation.full_generation,
                    generation.is_compacted,
                ))
                .or_default()
                .entry(*segment)
                .or_default()
                .extend(references.iter().cloned());
        }
    }
    for entry in &survivors {
        let identifier = entry.segment_identifier;
        let Some(bytes) = reader.segment_data(identifier) else {
            return Err(Error::SegmentNotFound {
                segment_identifier: identifier,
            });
        };
        let generation = GarbageCollectionGeneration {
            generation: entry.generation,
            full_generation: entry.full_generation,
            is_compacted: entry.is_compacted,
        };
        let (references, binary_references) = if identifier.is_data_segment() {
            trailers.for_segment(
                identifier,
                bytes,
                previously_unavailable_graph_targets,
                &current_rewrite_targets,
                scan_provider,
            )?
        } else {
            (Vec::new(), Vec::new())
        };
        if !references.is_empty() {
            expected_graph
                .entry(identifier)
                .or_default()
                .extend(references.iter().copied());
        }
        if trailers.catalog.is_none() && !binary_references.is_empty() {
            expected_binary_references
                .entry((
                    generation.generation,
                    generation.full_generation,
                    generation.is_compacted,
                ))
                .or_default()
                .entry(identifier)
                .or_default()
                .extend(binary_references.iter().cloned());
        }
        let write_result = writer.write_segment(
            identifier,
            bytes,
            generation,
            &references,
            &binary_references,
        );
        // `TarArchiveWriter` creates lazily. Capture the descriptor before
        // propagating the write result so a first-write ENOSPC-style failure
        // cannot strand an unowned `.cleaning` pathname.
        uncommitted_staging.capture_created_file(&writer)?;
        write_result?;
    }
    writer.close()?;
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.staging-before-validation-open")?;
    let staging_file = open_regular_file_no_follow(&staging_path, true)?;
    preserve_file_metadata(&staging_file, &source_metadata)?;
    let staging_identity = held_file_identity(&staging_file)?;
    let staged_reader = TarArchiveReader::open_file(&staging_path, &staging_file)?;
    if let Err(error) = validate_open_swept_archive(
        reader,
        &staged_reader,
        &staging_path,
        &survivors,
        &expected_graph,
        &expected_binary_references,
    ) {
        return Err(Error::InvalidFormat {
            details: format!(
                "staged rewrite for {} failed complete survivor/trailer validation ({error}); the original {} was left untouched",
                replacement_name,
                reader.file_name()
            ),
        });
    }
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::substitute_path_if_armed(
        "sweep.staging-validated-before-publish",
        &staging_path,
    )?;
    require_held_file_identity(
        &staging_file,
        staging_identity,
        "validated archive staging file",
    )?;
    require_path_file_identity(
        &staging_path,
        staging_identity,
        "validated archive staging file",
    )?;
    // From this point onward the complete validated staging file is useful
    // crash evidence. Publication failures retain it intentionally; ordinary
    // success removes it explicitly below.
    uncommitted_staging.disarm();

    // `hard_link` is an atomic absent-only publication: unlike rename it
    // cannot overwrite a final path created after planning. Both names refer
    // to the already-synced, validated inode until staging cleanup.
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.before-publish-link")?;
    std::fs::hard_link(&staging_path, &replacement_path)?;

    // A pathname substitution in the narrow interval between the pre-link
    // identity check and `hard_link` must not turn an arbitrary inode into the
    // higher active archive generation. Capture the two names immediately; if
    // they agree with each other but not the validated descriptor, they still
    // identify the link this process just published and can be removed safely.
    let linked_stage_identity = path_object_identity(&staging_path).ok();
    let linked_replacement_identity = path_object_identity(&replacement_path).ok();
    let just_published_identity =
        linked_stage_identity.filter(|identity| Some(*identity) == linked_replacement_identity);
    if linked_stage_identity != Some(staging_identity)
        || linked_replacement_identity != Some(staging_identity)
    {
        remove_published_link_if_same(directory, &replacement_path, just_published_identity)?;
        return Err(Error::InvalidFormat {
            details: format!(
                "archive staging or published path changed identity while publishing {replacement_name}; the source was left untouched"
            ),
        });
    }

    let replacement_file = match open_regular_file_no_follow(&replacement_path, false) {
        Ok(file) => file,
        Err(error) => {
            remove_published_link_if_same(directory, &replacement_path, Some(staging_identity))?;
            return Err(error);
        }
    };
    let replacement_validation = (|| {
        require_held_file_identity(
            &replacement_file,
            staging_identity,
            "published archive replacement",
        )?;
        require_path_file_identity(
            &replacement_path,
            staging_identity,
            "published archive replacement",
        )?;
        let replacement_reader = TarArchiveReader::open_file(&replacement_path, &replacement_file)?;
        validate_open_swept_archive(
            reader,
            &replacement_reader,
            &replacement_path,
            &survivors,
            &expected_graph,
            &expected_binary_references,
        )
    })();
    if let Err(error) = replacement_validation {
        remove_published_link_if_same(directory, &replacement_path, Some(staging_identity))?;
        return Err(Error::InvalidFormat {
            details: format!(
                "published rewrite {replacement_name} failed descriptor-bound validation ({error}); the source was left untouched"
            ),
        });
    }
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.after-publish-link")?;
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.before-publish-directory-sync")?;
    sync_directory_strict(directory)?;
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.after-publish-directory-sync")?;
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::crash_if_armed("sweep.published-before-source-unlink");
    let mut deletion_failures = Vec::new();
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.before-staging-unlink")?;
    if let Err(error) = std::fs::remove_file(&staging_path) {
        deletion_failures.push(DeferredFileDeletion {
            file_name: staging_name,
            error: error.to_string(),
            target_was_already_absent: error.kind() == std::io::ErrorKind::NotFound,
        });
    }
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.after-staging-unlink")?;
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::crash_if_armed(
        "sweep.staging-unlinked-before-source-unlink",
    );
    // Deletion failures are consistency-safe: the published higher letter
    // wins and preserves every survivor; the old source is reported later.
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.before-source-unlink")?;
    let pre_source_unlink_identity = (|| {
        require_held_file_identity(
            &replacement_file,
            staging_identity,
            "published archive replacement",
        )?;
        require_path_file_identity(
            &replacement_path,
            staging_identity,
            "published archive replacement",
        )?;
        require_held_file_identity(source_file, *source_identity, "certified archive source")?;
        require_path_file_identity(&path, *source_identity, "certified archive source")
    })();
    if let Err(error) = pre_source_unlink_identity {
        remove_published_link_if_same(directory, &replacement_path, Some(staging_identity))?;
        return Err(Error::InvalidFormat {
            details: format!(
                "archive identity changed immediately before removing {} ({error}); the source pathname was left untouched",
                reader.file_name()
            ),
        });
    }
    if let Err(error) = std::fs::remove_file(&path) {
        deletion_failures.push(DeferredFileDeletion {
            file_name: reader.file_name().to_owned(),
            error: error.to_string(),
            target_was_already_absent: error.kind() == std::io::ErrorKind::NotFound,
        });
    }
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::fail_if_armed("sweep.after-source-unlink")?;
    #[cfg(test)]
    crate::writer::cleanup_fault_injection::crash_if_armed("sweep.source-unlinked");
    Ok(ArchiveSweepOutcome {
        disposition: ArchiveSweepDisposition::Rewritten,
        deletion_failures,
        newly_unavailable: current_rewrite_targets,
    })
}

#[cfg(test)]
fn probe_archive_sweep_phase_boundary(cutpoint: &str) -> Result<()> {
    crate::writer::cleanup_fault_injection::fail_if_armed(cutpoint)?;
    crate::writer::cleanup_fault_injection::crash_if_armed(cutpoint);
    Ok(())
}

/// Reopens a swept archive and proves that every survivor's payload and
/// generation metadata exactly match the immutable source before the source
/// may be removed.
#[allow(
    clippy::too_many_lines,
    reason = "payload, generation, order, graph, and BRF checks are one fail-closed archive validation certificate"
)]
#[cfg(test)]
fn validate_swept_archive(
    source: &TarArchiveReader,
    swept_path: &Path,
    survivors: &[crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &ExpectedGraph,
    expected_binary_references: &ExpectedBinaryReferences,
) -> Result<()> {
    let swept = TarArchiveReader::open(swept_path)?;
    validate_open_swept_archive(
        source,
        &swept,
        swept_path,
        survivors,
        expected_graph,
        expected_binary_references,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "payload, generation, order, graph, and BRF checks are one fail-closed archive validation certificate"
)]
fn validate_open_swept_archive(
    source: &TarArchiveReader,
    swept: &TarArchiveReader,
    swept_path: &Path,
    survivors: &[crate::tar_archive::index::SegmentIndexEntry],
    expected_graph: &ExpectedGraph,
    expected_binary_references: &ExpectedBinaryReferences,
) -> Result<()> {
    if swept.is_recovered() {
        return Err(Error::InvalidFormat {
            details: format!("{} has no valid index", swept_path.display()),
        });
    }
    if swept.segment_count() != survivors.len() {
        return Err(Error::InvalidFormat {
            details: format!(
                "{} contains {} segments, expected {}",
                swept_path.display(),
                swept.segment_count(),
                survivors.len()
            ),
        });
    }
    let mut actual_in_file_order = swept
        .index()
        .expect("a non-recovered archive has an index")
        .entries()
        .to_vec();
    actual_in_file_order.sort_by_key(|entry| entry.position);
    let actual_identifier_order: Vec<_> = actual_in_file_order
        .iter()
        .map(|entry| entry.segment_identifier)
        .collect();
    let expected_identifier_order: Vec<_> = survivors
        .iter()
        .map(|entry| entry.segment_identifier)
        .collect();
    if actual_identifier_order != expected_identifier_order {
        return Err(Error::InvalidFormat {
            details: format!(
                "{} changed the physical order of surviving segments",
                swept_path.display()
            ),
        });
    }
    for expected in survivors {
        swept.validate_indexed_segment_entry(expected.segment_identifier)?;
        let actual = swept
            .index_entry(expected.segment_identifier)
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "{} omits surviving segment {}",
                    swept_path.display(),
                    expected.segment_identifier
                ),
            })?;
        if actual.size != expected.size
            || actual.generation != expected.generation
            || actual.full_generation != expected.full_generation
            || actual.is_compacted != expected.is_compacted
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} changed generation metadata for surviving segment {}",
                    swept_path.display(),
                    expected.segment_identifier
                ),
            });
        }
        let source_bytes =
            source
                .segment_data(expected.segment_identifier)
                .ok_or(Error::SegmentNotFound {
                    segment_identifier: expected.segment_identifier,
                })?;
        let swept_bytes =
            swept
                .segment_data(expected.segment_identifier)
                .ok_or(Error::SegmentNotFound {
                    segment_identifier: expected.segment_identifier,
                })?;
        if source_bytes != swept_bytes {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} changed the payload of surviving segment {}",
                    swept_path.display(),
                    expected.segment_identifier
                ),
            });
        }
    }

    validate_exact_archive_trailers(
        swept,
        &swept_path.display().to_string(),
        expected_graph,
        expected_binary_references,
    )
}

pub(crate) fn is_reclaimable(
    reference: GarbageCollectionGeneration,
    segment: GarbageCollectionGeneration,
    full: bool,
    retained_generations: i32,
) -> bool {
    // Wrapping subtraction matches Java's `GCGeneration.compareWith`, which
    // uses plain int subtraction; it also cannot panic on the pathological
    // generation values a corrupt archive index might carry.
    if full {
        reference
            .full_generation
            .wrapping_sub(segment.full_generation)
            >= retained_generations
            || (reference.generation.wrapping_sub(segment.generation) >= retained_generations
                && !segment.is_compacted)
    } else {
        reference.generation.wrapping_sub(segment.generation) >= retained_generations
            && !(segment.is_compacted && segment.full_generation == reference.full_generation)
    }
}

/// Rewrites the manifest with `store.version=2` after validating it with
/// the same rules as the read path (archives without a manifest are the
/// legacy format; versions above 2 are from a newer Oak).
fn check_and_update_manifest(directory: &Path) -> Result<()> {
    #[allow(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "the Java store matches \".tar\" case-sensitively"
    )]
    let archives_exist = std::fs::read_dir(directory)?.any(|entry| {
        entry
            .ok()
            .and_then(|entry| entry.file_name().into_string().ok())
            .is_some_and(|name| name.ends_with(".tar"))
    });
    crate::store::check_manifest(directory, archives_exist)?;
    std::fs::write(
        directory.join("manifest"),
        "#written by froe\nstore.version=2\n",
    )?;
    Ok(())
}

/// Write-mode archive initialization: per archive number, the newest
/// generation letter with a valid index wins and stale letters are
/// deleted; numbers without any valid index are recovered — every letter
/// is scanned, backed up to a `.bak` name, and the recovered segments are
/// rewritten as a fresh archive under the lowest letter's name.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the Java store matches \".tar\" case-sensitively"
)]
fn initialize_archives_for_writing(
    directory: &Path,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<Vec<TarArchiveReader>> {
    let mut file_names = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let name = entry?.file_name();
        if let Ok(name) = name.into_string()
            && name.ends_with(".tar")
        {
            file_names.push(name);
        }
    }
    // Validate against duplicate (number, letter) pairs.
    select_newest_file_generations(&file_names)?;

    let mut by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>> =
        std::collections::BTreeMap::new();
    for file_name in &file_names {
        if let Some(parsed) = ArchiveFileName::parse(file_name) {
            by_number
                .entry(parsed.archive_number)
                .or_default()
                .push(parsed);
        }
    }

    let archive_numbers = by_number.len();
    let mut archives = crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "opening archives for writing",
            crate::progress::WorkUnit::Archives,
        )
        .with_total(crate::progress::count(archive_numbers)),
        |observer| open_archive_numbers_for_writing(directory, by_number, observer),
    )?;
    // Newest number first: the probe order for reads.
    archives.reverse();
    Ok(archives)
}

/// One archive number rebuilt by [`repair_indexless_archive_numbers`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairedArchive {
    /// The file name the rebuilt archive was installed under.
    pub(crate) file_name: String,
    /// Why the original index was rejected, for the plan and the record.
    pub(crate) reason: String,
    /// Size of the rebuilt archive on disk.
    pub(crate) bytes: u64,
}

/// The generation letter a rebuild is installed under: the lowest *non-empty*
/// one, not simply the lowest.
///
/// A zero-length letter is a writer's lazy next-archive creation, not an
/// archive. Taking it as the target would regress the active generation
/// letter, hard-link a bogus zero-byte `.bak`, and — because the target is
/// also the metadata source — give the rebuilt archive the residue's
/// ownership and mode instead of the archive's. The residue is left where it
/// is for cleanup's stale-archive task, which already owns it.
fn install_target_generation<'a>(
    directory: &Path,
    generations: &'a [ArchiveFileName],
) -> &'a ArchiveFileName {
    generations
        .iter()
        .find(|generation| {
            std::fs::metadata(directory.join(&generation.file_name))
                .is_ok_and(|metadata| metadata.len() != 0)
        })
        .unwrap_or(&generations[0])
}

/// The name of a non-empty `<archive>.recovering` file already beside one of
/// `generations`, if any.
///
/// A rebuild of this number would unlink it, and it is not froe's to unlink:
/// `recover_archive_number` removes its own staging file on every error path,
/// so the only way one survives is a crash mid-write — which is the state
/// cleanup's stale-temporaries task exists to adjudicate, and which it
/// retains unless it proves the bytes redundant.
fn existing_staging_residue(directory: &Path, generations: &[ArchiveFileName]) -> Option<String> {
    generations.iter().find_map(|generation| {
        let name = format!("{}.recovering", generation.file_name);
        std::fs::symlink_metadata(directory.join(&name))
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.len() != 0)
            .map(|_| name)
    })
}

/// Refuses a directory holding two spellings of one `(number, letter)` pair.
///
/// `data00007.tar` and `data00007a.tar` both parse as number 7 generation
/// `'a'`. Grouping them would make the install target whichever the listing
/// yielded first, so the repair refuses — and it must refuse before anything
/// irreversible, not after.
pub(crate) fn reject_duplicate_archive_generations(directory: &Path) -> Result<()> {
    let names: Vec<String> = physical_archive_names(directory)?
        .into_iter()
        .map(|parsed| parsed.file_name)
        .collect();
    select_newest_file_generations(&names)?;
    Ok(())
}

/// Authorizes writing version-2 data, called immediately before the first
/// rebuilt archive is installed.
///
/// The repair produces a version-2 binary-references trailer, so a
/// version-1 store has to be raised first — but only when a rebuild is
/// actually about to land, not when one is merely predicted. Expressing that
/// as a callback keeps the manifest policy in cleanup, where the plan that
/// announced it lives, while the timing stays here, where the install is.
pub(crate) trait AuthorizeVersionTwoWrite {
    /// Called once before the first install; later calls must be no-ops.
    fn authorize(&mut self) -> Result<()>;
}

/// The authorization a write session needs: none.
///
/// `WritableRepository::open` runs `check_and_update_manifest` before it
/// touches an archive, so the store is already version 2 by the time any
/// rebuild installs. Cleanup cannot do that — it may not upgrade a manifest
/// it did not plan to — which is the whole reason this is a callback.
pub(crate) struct VersionTwoAlreadyEstablished;

impl AuthorizeVersionTwoWrite for VersionTwoAlreadyEstablished {
    fn authorize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The archive file names a repair would replace, lowest number first.
///
/// The install targets specifically, which is what an ownership preflight
/// needs: those are the files `preserve_file_metadata` will try to match.
pub(crate) fn repair_target_names(directory: &Path) -> Result<Vec<String>> {
    let mut by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>> =
        std::collections::BTreeMap::new();
    for parsed in physical_archive_names(directory)? {
        by_number
            .entry(parsed.archive_number)
            .or_default()
            .push(parsed);
    }
    let mut targets = Vec::new();
    for mut generations in by_number.into_values() {
        generations.sort_by_key(|name| name.file_generation);
        let (winner, any_nonempty) = select_writable_generation(directory, &generations);
        if winner.is_some() || !any_nonempty {
            continue;
        }
        if any_recoverable_segment(directory, &generations) {
            targets.push(
                install_target_generation(directory, &generations)
                    .file_name
                    .clone(),
            );
        }
    }
    Ok(targets)
}

/// What a repair run would find: which archive numbers it can rebuild, and
/// which hold bytes no scan can read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct IndexlessSurvey {
    /// Numbers a rebuild would succeed on.
    pub(crate) repairable: usize,
    /// File names of numbers a rebuild would refuse, lowest number first.
    pub(crate) unrepairable: Vec<String>,
}

/// Surveys the archive numbers that have no valid index.
///
/// One predicate, asked once, so nothing downstream can drift from it. The
/// distinction it draws is the whole safety question of the repair task: a
/// number whose letters scan to at least one segment can be rebuilt, and one
/// whose letters scan to nothing cannot — `recover_archive_number` refuses
/// it rather than install an empty archive. Gating an irreversible step on
/// "index-less" instead of "repairable" is what let a run that repairs
/// nothing still upgrade a manifest; planning on one and gating on the other
/// is what let a doomed run pay for durable rewrites first.
pub(crate) fn survey_indexless_archive_numbers(directory: &Path) -> Result<IndexlessSurvey> {
    let mut by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>> =
        std::collections::BTreeMap::new();
    for parsed in physical_archive_names(directory)? {
        by_number
            .entry(parsed.archive_number)
            .or_default()
            .push(parsed);
    }
    let mut survey = IndexlessSurvey::default();
    for mut generations in by_number.into_values() {
        generations.sort_by_key(|name| name.file_generation);
        let (winner, any_nonempty) = select_writable_generation(directory, &generations);
        if winner.is_some() || !any_nonempty {
            continue;
        }
        if any_recoverable_segment(directory, &generations) {
            survey.repairable += 1;
        } else {
            survey.unrepairable.push(
                install_target_generation(directory, &generations)
                    .file_name
                    .clone(),
            );
        }
    }
    Ok(survey)
}

/// The refusal an unrepairable archive number earns, raised before anything
/// is authorized rather than after a rewrite has been paid for.
pub(crate) fn unrepairable_archives_refusal(unrepairable: &[String]) -> Error {
    Error::InvalidFormat {
        details: format!(
            "{} active archive(s) have no valid index and no segment any recovery scan can read: \
             {}. Repair would refuse them, so this run cannot complete however it is retried; \
             move those files aside to proceed, and keep them — they are the only copy of \
             whatever they hold",
            unrepairable.len(),
            unrepairable.join(", ")
        ),
    }
}

/// Rebuilds the index of every archive number that has none, and does
/// nothing else.
///
/// [`initialize_archives_for_writing`] also *deletes* non-winning generation
/// letters. That deletion is cleanup's `stale-archives` task, which plans it,
/// shows it, and asks — so repair must not perform it as a side effect or it
/// would delete archives the operator never authorised, under a task that
/// only promised to repair. This is the same normalization/authorization
/// split `open_prepared` states: cleanup may only take a side effect it has
/// independently planned.
///
/// All-empty numbers are skipped rather than repaired: there is nothing to
/// rebuild from, and the zero-byte files belong to `stale-archives` too.
/// Requires only the repository lock — `recover_archive_number` reads the
/// directory and writes beside it, holding no writer state.
pub(crate) fn repair_indexless_archive_numbers(
    directory: &Path,
    observer: &mut dyn crate::progress::ProgressObserver,
    authorize: &mut dyn AuthorizeVersionTwoWrite,
) -> Result<Vec<RepairedArchive>> {
    let names = physical_archive_names(directory)?;
    // The same validation `initialize_archives_for_writing` performs before
    // it groups, and for the same reason: `data00007.tar` and
    // `data00007a.tar` both parse as number 7 generation 'a', so without this
    // they land in one group and the install target becomes whichever the
    // directory listing happened to yield first. Repairing into that is a
    // nondeterministic, irreversible rewrite of a store that the very next
    // `Repository::open` refuses outright.
    select_newest_file_generations(
        &names
            .iter()
            .map(|name| name.file_name.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>> =
        std::collections::BTreeMap::new();
    for parsed in names {
        by_number
            .entry(parsed.archive_number)
            .or_default()
            .push(parsed);
    }
    let total = by_number.len();
    crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "repairing archive indexes",
            crate::progress::WorkUnit::Archives,
        )
        .with_total(crate::progress::count(total)),
        |observer| {
            let mut repaired = Vec::new();
            let mut failures: Vec<String> = Vec::new();
            for (examined, (number, mut generations)) in by_number.into_iter().enumerate() {
                observer.step_advanced(crate::progress::count(examined));
                generations.sort_by_key(|name| name.file_generation);
                let (winner, any_nonempty) = select_writable_generation(directory, &generations);
                if winner.is_some() || !any_nonempty {
                    continue;
                }
                // Captured before the rebuild: afterwards the archive is
                // indexed and the reason no longer exists to be read.
                let reason = generations
                    .iter()
                    .rev()
                    .find_map(|candidate| {
                        TarArchiveReader::open(&directory.join(&candidate.file_name))
                            .ok()
                            .and_then(|reader| reader.recovery_reason().map(str::to_owned))
                    })
                    .unwrap_or_else(|| "the index could not be read".to_owned());
                // `recover_archive_number` unlinks its own staging file before
                // writing. A staging file that is already there is not ours:
                // it is the residue of a rebuild interrupted mid-write, which
                // cleanup's stale-temporaries task recognises, plans, and
                // deliberately *retains* unless it is provably redundant —
                // because its merged content can be the only assembled copy
                // when an install was interrupted after a letter had already
                // been retired. Repair must not delete it as a side effect of
                // a task that only promised to rebuild an index.
                if let Some(residue) = existing_staging_residue(directory, &generations) {
                    failures.push(format!(
                        "archive number {number}: {residue} is the residue of an interrupted \
                         rebuild and may hold the only assembled copy of this archive; \
                         cleanup's stale-temporaries task decides its fate, so repair will not \
                         overwrite it — move it aside to retry"
                    ));
                    continue;
                }
                // Archive numbers are independent: one that cannot be rebuilt
                // says nothing about the next, and stopping at the first would
                // hide every later problem behind one repair-and-rerun cycle
                // apiece — on a store damaged throughout, that is one full
                // planning pass per archive. Collect and continue, so the
                // operator learns the whole picture from one run.
                match recover_archive_number(directory, &generations, authorize) {
                    Ok(rebuilt) => repaired.push(RepairedArchive {
                        file_name: rebuilt.file_name().to_owned(),
                        reason,
                        bytes: rebuilt.file_size(),
                    }),
                    Err(error) => failures.push(format!("archive number {number}: {error}")),
                }
            }
            observer.step_advanced(crate::progress::count(total));
            if failures.is_empty() {
                return Ok(repaired);
            }
            Err(unfinished_repair_refusal(&repaired, &failures))
        },
    )
}

/// The refusal a partially completed repair earns.
///
/// It carries what *succeeded*, because those archives were rewritten and
/// now have `.bak` files: reporting only the failure would leave the
/// operator believing the store is as they left it. This is the same
/// obligation `attach_planning_warnings` meets for planning, applied to the
/// one mutation that happens before there is a plan to record it in.
fn unfinished_repair_refusal(repaired: &[RepairedArchive], failures: &[String]) -> Error {
    let mut details = format!(
        "{} of {} archive index rebuild(s) failed: {}",
        failures.len(),
        failures.len() + repaired.len(),
        failures.join("; ")
    );
    if !repaired.is_empty() {
        let names: Vec<&str> = repaired
            .iter()
            .map(|archive| archive.file_name.as_str())
            .collect();
        let _ = write!(
            details,
            ". Already rebuilt and durable, with the originals retained under `.bak` names: {}. \
             Those need no second attempt; rerunning repairs only what is left",
            names.join(", ")
        );
    }
    Error::InvalidFormat { details }
}

/// Picks the generation letter of one archive number to write against:
/// newest letter first, the first valid index wins. Also reports whether
/// any letter held bytes at all.
///
/// Zero-length letters are skipped exactly as the read path skips them
/// (`crate::store::open_archives_newest_valid_first`): a writer creates its
/// next archive lazily, and an empty file is that creation's race window —
/// or what it leaves behind when it is killed inside it. Opening one yields
/// no segments, so recovering the number would rebuild it as an archive
/// with no entries, which is not a file `TarArchiveWriter` ever creates.
fn select_writable_generation(
    directory: &Path,
    generations: &[ArchiveFileName],
) -> (Option<TarArchiveReader>, bool) {
    let mut any_nonempty = false;
    for candidate in generations.iter().rev() {
        let path = directory.join(&candidate.file_name);
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == 0) {
            continue;
        }
        any_nonempty = true;
        if let Ok(reader) = TarArchiveReader::open(&path)
            && !reader.is_recovered()
        {
            return (Some(reader), any_nonempty);
        }
    }
    (None, any_nonempty)
}

/// Opens the winning generation letter of each archive number, deleting
/// the losers, and reports one completed archive number at a time.
fn open_archive_numbers_for_writing(
    directory: &Path,
    by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>>,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<Vec<TarArchiveReader>> {
    let archive_numbers = by_number.len();
    let mut archives = Vec::new();
    for (opened, (_, mut generations)) in by_number.into_iter().enumerate() {
        observer.step_advanced(crate::progress::count(opened));
        generations.sort_by_key(|name| name.file_generation);
        let (winner, any_nonempty) = select_writable_generation(directory, &generations);
        match winner {
            Some(reader) => {
                // Delete every other generation letter of this number.
                for stale in &generations {
                    if stale.file_name != reader.file_name() {
                        std::fs::remove_file(directory.join(&stale.file_name))?;
                    }
                }
                archives.push(reader);
            }
            // Every letter of this number is empty, so there is nothing to
            // recover and nothing to serve. Nothing is deleted here: the
            // number simply contributes no archive, which either frees it
            // for the next write to fill — the same thing the writer that
            // created it was about to do — or leaves the files for
            // cleanup's stale-archive task to remove under its own
            // plan-and-confirm contract. Reuse can only ever land on a
            // zero-byte file, because a single non-empty letter sends the
            // whole number down the recovery path instead.
            None if !any_nonempty => {}
            None => {
                archives.push(recover_archive_number(
                    directory,
                    &generations,
                    &mut VersionTwoAlreadyEstablished,
                )?);
            }
        }
    }
    observer.step_advanced(crate::progress::count(archive_numbers));
    Ok(archives)
}

/// The refusal an archive number earns when it holds bytes but no segment
/// the recovery scan can read. Naming the files matters more than usual
/// here: the operator has to decide whether to move them aside or keep them
/// as evidence, and neither the number nor an errno tells them which files
/// are involved.
///
/// The remedy deliberately does not name cleanup. `plan_stale_archives`
/// marks only *zero-byte* letters of an unindexed number stale; a non-empty
/// letter is preserved with a warning, precisely because it may still hold
/// unrecovered bytes. Telling the operator to run cleanup here would send
/// them to a command that will decline to act.
fn unrecoverable_archive_number_refusal(generations: &[ArchiveFileName]) -> Error {
    let names: Vec<&str> = generations
        .iter()
        .map(|generation| generation.file_name.as_str())
        .collect();
    Error::InvalidFormat {
        details: format!(
            "archive number {} has no valid index and no recoverable segment in {}; \
             refusing to replace it with an empty archive. Cleanup preserves this file \
             rather than removing it, so opening the store for writing needs it moved \
             aside — keep it, it is the only copy of whatever it holds",
            generations.first().map_or(0, |first| first.archive_number),
            names.join(", ")
        ),
    }
}

/// Recovers one archive number with no valid index: scans every letter in
/// ascending order (later letters overwrite duplicates), rebuilds the
/// recovered segments as a fresh archive, and only after that archive is
/// written, fsynced, and re-validated are the originals retired to
/// `.bak` names and the replacement installed under the lowest letter's
/// file name. A failure before installation leaves every original in
/// place; a failure during installation rolls back best-effort (see
/// [`install_recovered_archive`]).
/// Gives a rebuilt archive the ownership and mode of the archive it replaces,
/// rather than the process umask.
///
/// Every other replacement path in maintenance does this; without it a store
/// whose archives are group-owned and setgid silently loses both on the one
/// file that was rewritten, and a later cleanup's apply-identity preflight
/// reads the wrong metadata. A target that does not exist yet has nothing to
/// inherit, which is not an error.
fn inherit_replaced_archive_metadata(
    directory: &Path,
    target_name: &str,
    temporary_path: &Path,
) -> Result<()> {
    let Ok(source_metadata) = std::fs::metadata(directory.join(target_name)) else {
        return Ok(());
    };
    let staged = std::fs::OpenOptions::new()
        .write(true)
        .open(temporary_path)?;
    preserve_file_metadata(&staged, &source_metadata)
}

/// Charges the caller's version-2 price at the last instant before a rebuilt
/// archive becomes visible.
///
/// The staged rebuild already exists, is durable, and has re-opened with a
/// valid index; nothing version-2 is visible yet. If authorization fails the
/// staging file is removed like every other pre-install failure, so the
/// number is left exactly as it was found.
fn authorize_before_install(
    authorize: &mut dyn AuthorizeVersionTwoWrite,
    temporary_path: &Path,
) -> Result<()> {
    if let Err(error) = authorize.authorize() {
        let _ = std::fs::remove_file(temporary_path);
        return Err(error);
    }
    Ok(())
}

fn recover_archive_number(
    directory: &Path,
    generations: &[ArchiveFileName],
    authorize: &mut dyn AuthorizeVersionTwoWrite,
) -> Result<TarArchiveReader> {
    let recovered = scan_recoverable_segments(directory, generations);
    // A non-empty file that yields no segment is residue this function
    // cannot act on: writing the replacement would produce an archive with
    // no entries, which `TarArchiveWriter` never creates at all, and the
    // re-open below would then fail on a missing path with a bare errno.
    // Refuse with the file names instead, and say what clears them.
    if recovered.is_empty() {
        return Err(unrecoverable_archive_number_refusal(generations));
    }

    // Parse every segment once — data *and* bulk, so blob identifier
    // strings whose block lists spill into bulk segments resolve too.
    // The parsed structures also back the provider that resolves blob
    // identifier strings across the recovered segments of this archive
    // number.
    let mut parsed_segments: HashMap<SegmentIdentifier, Arc<ParsedSegment>> = HashMap::new();
    for (identifier, bytes) in &recovered {
        parsed_segments.insert(
            *identifier,
            Arc::new(ParsedSegment::parse(*identifier, bytes)?),
        );
    }
    let provider = ArchiveSegmentsProvider {
        segments: recovered
            .iter()
            .filter_map(|(identifier, bytes)| {
                parsed_segments
                    .get(identifier)
                    .map(|parsed| (*identifier, (Arc::clone(parsed), bytes.as_slice())))
            })
            .collect(),
    };

    // Build the replacement beside the originals; nothing is renamed or
    // deleted until it exists, is durable, and re-opens with a valid
    // index.
    let target_name = &install_target_generation(directory, generations).file_name;
    let temporary_name = format!("{target_name}.recovering");
    let temporary_path = directory.join(&temporary_name);
    let _ = std::fs::remove_file(&temporary_path);
    let write_replacement = || -> Result<()> {
        let mut writer = TarArchiveWriter::new(directory, &temporary_name);
        for (identifier, bytes) in &recovered {
            let (generation, references, binary_references) =
                if let Some(parsed) = parsed_segments.get(identifier) {
                    // Fail closed when a blob identifier cannot be
                    // resolved: publishing an incomplete catalog would let
                    // AEM's blob garbage collection delete a
                    // still-referenced binary.
                    let binary_references =
                        read_blob_identifiers(&provider, parsed).map_err(|error| {
                            Error::InvalidFormat {
                                details: format!(
                                    "cannot rebuild the binary references catalog while \
                                     recovering {target_name}: an external blob identifier in \
                                     segment {identifier} does not resolve within the recovered \
                                     segments ({error}); refusing to publish an incomplete \
                                     catalog, which could let blob garbage collection delete \
                                     referenced binaries"
                                ),
                            }
                        })?;
                    (
                        GarbageCollectionGeneration {
                            generation: parsed.generation,
                            full_generation: parsed.full_generation,
                            is_compacted: parsed.is_compacted,
                        },
                        parsed.referenced_segments.clone(),
                        binary_references,
                    )
                } else {
                    (
                        GarbageCollectionGeneration {
                            generation: 0,
                            full_generation: 0,
                            is_compacted: false,
                        },
                        Vec::new(),
                        Vec::new(),
                    )
                };
            writer.write_segment(
                *identifier,
                bytes,
                generation,
                &references,
                &binary_references,
            )?;
        }
        writer.close()?;
        Ok(())
    };
    if let Err(error) = write_replacement() {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = inherit_replaced_archive_metadata(directory, target_name, &temporary_path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    crate::writer::compaction::fsync_directory(directory);
    match TarArchiveReader::open(&temporary_path) {
        Ok(validated) if !validated.is_recovered() => drop(validated),
        Ok(_) => {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(Error::InvalidFormat {
                details: format!("the rebuilt archive {temporary_name} failed index validation"),
            });
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error);
        }
    }
    authorize_before_install(authorize, &temporary_path)?;
    install_recovered_archive(directory, generations, target_name, &temporary_path)
}

/// Scans every generation letter of one archive number in ascending
/// order — later letters overwrite duplicate segments — returning the
/// recovered segments in scan order.
/// Whether a rebuild of this archive number would find anything to rebuild
/// from — the same question [`scan_recoverable_segments`] answers, without
/// materializing an answer nobody wants.
///
/// The scan copies every segment's bytes into owned buffers, so asking it
/// this question allocates the whole archive to discard it, and does so in
/// the survey that now runs before every repair. `segment_count()` on a
/// recovery-scanned reader is the length of that same scan's entry list,
/// read straight off the memory map. It is also exactly what the cleanup
/// side reads off its already-open readers, so both callers now derive the
/// predicate the same way and cannot drift apart.
fn any_recoverable_segment(directory: &Path, generations: &[ArchiveFileName]) -> bool {
    generations.iter().any(|generation| {
        TarArchiveReader::open(&directory.join(&generation.file_name))
            .is_ok_and(|reader| reader.segment_count() > 0)
    })
}

fn scan_recoverable_segments(
    directory: &Path,
    generations: &[ArchiveFileName],
) -> Vec<(SegmentIdentifier, Vec<u8>)> {
    let mut recovered: Vec<(SegmentIdentifier, Vec<u8>)> = Vec::new();
    let mut positions: HashMap<SegmentIdentifier, usize> = HashMap::new();
    for generation in generations {
        let path = directory.join(&generation.file_name);
        if let Ok(reader) = TarArchiveReader::open(&path) {
            for identifier in reader.segment_identifiers() {
                if let Some(bytes) = reader.segment_data(identifier) {
                    if let Some(&position) = positions.get(&identifier) {
                        recovered[position].1 = bytes.to_vec();
                    } else {
                        positions.insert(identifier, recovered.len());
                        recovered.push((identifier, bytes.to_vec()));
                    }
                }
            }
        }
    }
    recovered
}

/// Retires the original generation letters to `.bak` names and installs
/// the validated replacement under the target name. The target's own
/// original is preserved through a hard link (or, on filesystems without
/// hard links, a full copy), so a `.tar` under the target name exists at
/// every instant; the other letters are plain renames. An *error* at any
/// step — including the final re-open — rolls every completed step back,
/// normally leaving the originals under their own names. The rollback is
/// best effort: a rollback rename that itself fails cannot be recovered
/// further, is dropped in favor of reporting the primary error, and can
/// leave a mix of `.bak` and installed states — as can a *crash*
/// mid-installation, the inherent limit of multi-file replacement. The
/// `.bak` copies always preserve the original bytes for manual repair.
fn install_recovered_archive(
    directory: &Path,
    generations: &[ArchiveFileName],
    target_name: &str,
    temporary_path: &Path,
) -> Result<TarArchiveReader> {
    let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut target_backup: Option<PathBuf> = None;
    let roll_back = |renamed: &[(PathBuf, PathBuf)]| {
        for (original, backup) in renamed.iter().rev() {
            let _ = std::fs::rename(backup, original);
        }
    };
    for generation in generations {
        let path = directory.join(&generation.file_name);
        // Zero-length letters hold nothing to preserve and are not archives;
        // retiring one would only manufacture an empty `.bak`. They stay for
        // the stale-archive task, which plans and confirms their removal.
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == 0) {
            continue;
        }
        let backup = backup_path(directory, &generation.file_name);
        if generation.file_name == *target_name {
            // The target keeps its directory entry: the backup is a
            // second link (or a copy) of the same content, never a
            // rename away.
            if std::fs::hard_link(&path, &backup).is_err()
                && let Err(error) = std::fs::copy(&path, &backup)
            {
                roll_back(&renamed);
                return Err(error.into());
            }
            target_backup = Some(backup);
        } else if let Err(error) = std::fs::rename(&path, &backup) {
            roll_back(&renamed);
            return Err(error.into());
        } else {
            renamed.push((path, backup));
        }
    }
    let target_path = directory.join(target_name);
    if let Err(error) = std::fs::rename(temporary_path, &target_path) {
        if let Some(backup) = &target_backup {
            let _ = std::fs::remove_file(backup);
        }
        roll_back(&renamed);
        return Err(error.into());
    }
    crate::writer::compaction::fsync_directory(directory);
    match TarArchiveReader::open(&target_path) {
        Ok(reader) => Ok(reader),
        Err(error) => {
            // The replacement was validated before installation, so a
            // failing re-open is environmental (for example an I/O
            // error). Restore the original atomically from its backup
            // link and undo the other renames before reporting.
            if let Some(backup) = &target_backup {
                let _ = std::fs::rename(backup, &target_path);
            }
            roll_back(&renamed);
            crate::writer::compaction::fsync_directory(directory);
            Err(error)
        }
    }
}

/// The first free `.bak` name for a damaged archive: `name.bak`, then
/// `name.2.bak`, `name.3.bak`, …
fn backup_path(directory: &Path, file_name: &str) -> PathBuf {
    let first = directory.join(format!("{file_name}.bak"));
    if !first.exists() {
        return first;
    }
    let mut counter = 2u32;
    loop {
        let candidate = directory.join(format!("{file_name}.{counter}.bak"));
        if !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}

/// The graph and binary-references trailers of an archive being swept,
/// filtered to the surviving segments — Oak filters the existing
/// trailers, never recomputes them; only a missing trailer falls back to
/// a per-segment header scan.
struct FilteredTrailers {
    graph_present: bool,
    /// The surviving catalog entries with their original generation
    /// triples, which the swept archive preserves verbatim; `None` when
    /// the original archive had no readable catalog.
    catalog: Option<Vec<(GarbageCollectionGeneration, SegmentIdentifier, Vec<String>)>>,
    graph_by_source: HashMap<SegmentIdentifier, Vec<SegmentIdentifier>>,
}

impl FilteredTrailers {
    fn from_archive(
        reader: &TarArchiveReader,
        reclaimable_sources: &std::collections::HashSet<SegmentIdentifier>,
        previously_unavailable_graph_targets: &std::collections::HashSet<SegmentIdentifier>,
        current_rewrite_targets: &std::collections::HashSet<SegmentIdentifier>,
    ) -> Self {
        let graph = reader.segment_graph();
        let mut graph_by_source: HashMap<SegmentIdentifier, Vec<SegmentIdentifier>> =
            HashMap::new();
        if let Some(graph) = &graph {
            for (source, targets) in &graph.adjacency {
                graph_by_source.insert(
                    *source,
                    targets
                        .iter()
                        .filter(|target| {
                            !previously_unavailable_graph_targets.contains(target)
                                && !current_rewrite_targets.contains(target)
                        })
                        .copied()
                        .collect(),
                );
            }
        }
        let catalog = reader.binary_references().map(|catalog| {
            let mut entries = Vec::new();
            for generation_references in catalog.generations {
                let generation = GarbageCollectionGeneration {
                    generation: generation_references.generation,
                    full_generation: generation_references.full_generation,
                    is_compacted: generation_references.is_compacted,
                };
                for (segment, references) in generation_references.segments {
                    if !reclaimable_sources.contains(&segment) {
                        entries.push((generation, segment, references));
                    }
                }
            }
            entries
        });
        Self {
            graph_present: graph.is_some(),
            catalog,
            graph_by_source,
        }
    }

    /// The filtered graph edges and — only when the original archive had
    /// no catalog to carry over — strictly scan-derived binary references
    /// of one surviving data segment, resolved through `scan_provider`
    /// (every segment of the archive). An unresolvable identifier fails
    /// the sweep rather than publish an incomplete catalog.
    fn for_segment(
        &self,
        identifier: SegmentIdentifier,
        bytes: &[u8],
        previously_unavailable_graph_targets: &std::collections::HashSet<SegmentIdentifier>,
        current_rewrite_targets: &std::collections::HashSet<SegmentIdentifier>,
        scan_provider: Option<&ArchiveSegmentsProvider<'_>>,
    ) -> Result<(Vec<SegmentIdentifier>, Vec<String>)> {
        let references = match self.graph_by_source.get(&identifier) {
            Some(filtered) => filtered.clone(),
            None if !self.graph_present => ParsedSegment::parse(identifier, bytes)?
                .referenced_segments
                .iter()
                .filter(|target| {
                    !previously_unavailable_graph_targets.contains(target)
                        && !current_rewrite_targets.contains(target)
                })
                .copied()
                .collect(),
            None => Vec::new(),
        };
        let binary_references = match scan_provider {
            // Carried over with original triples via
            // `TarArchiveWriter::add_binary_references` instead.
            None => Vec::new(),
            Some(provider) => {
                let parsed = provider
                    .segments
                    .get(&identifier)
                    .map(|(parsed, _)| Arc::clone(parsed))
                    .ok_or(Error::SegmentNotFound {
                        segment_identifier: identifier,
                    })?;
                read_blob_identifiers(provider, &parsed).map_err(|error| Error::InvalidFormat {
                    details: format!(
                        "cannot rebuild the binary references catalog while sweeping: an \
                             external blob identifier in segment {identifier} does not resolve \
                             within the archive ({error}); refusing to publish an incomplete \
                             catalog, which could let blob garbage collection delete referenced \
                             binaries"
                    ),
                })?
            }
        };
        Ok((references, binary_references))
    }
}

/// Parses every segment of the given archives — data and bulk — into a
/// provider, so blob identifier strings (including block lists spilling
/// into bulk segments, or strings stored in another archive) resolve
/// during catalog reconstruction. `readers` must be ordered newest
/// archive first — session archives before base archives; a segment
/// duplicated across archives resolves to the newest copy, the
/// repository's lookup contract.
fn archive_segments_provider<'archives>(
    readers: &[&'archives TarArchiveReader],
) -> Result<ArchiveSegmentsProvider<'archives>> {
    let mut segments = HashMap::new();
    for reader in readers {
        for identifier in reader.segment_identifiers() {
            if let Some(bytes) = reader.segment_data(identifier) {
                // First insertion wins: with newest-first iteration this
                // keeps the newest copy of a duplicated segment.
                if let std::collections::hash_map::Entry::Vacant(vacant) =
                    segments.entry(identifier)
                {
                    vacant.insert((Arc::new(ParsedSegment::parse(identifier, bytes)?), bytes));
                }
            }
        }
    }
    Ok(ArchiveSegmentsProvider { segments })
}

/// Seeds the shared references set from one session archive: session
/// archives are never swept, so *every* data segment they hold stays on
/// disk regardless of its generation, and each contributes the non-data
/// segments it references — through the graph trailer when present, else the
/// segment header's reference list.
fn seed_references_from_archive(
    reader: &TarArchiveReader,
    references: &mut std::collections::HashSet<SegmentIdentifier>,
) -> Result<()> {
    let graph_adjacency: Option<HashMap<SegmentIdentifier, Vec<SegmentIdentifier>>> = reader
        .segment_graph()
        .map(|graph| graph.adjacency.into_iter().collect());
    for identifier in reader.segment_identifiers() {
        if !identifier.is_data_segment() {
            continue;
        }
        let targets = match &graph_adjacency {
            Some(adjacency) => adjacency.get(&identifier).cloned().unwrap_or_default(),
            None => match reader.segment_data(identifier) {
                Some(bytes) => ParsedSegment::parse(identifier, bytes)?.referenced_segments,
                None => Vec::new(),
            },
        };
        for target in targets {
            if !target.is_data_segment() {
                references.insert(target);
            }
        }
    }
    Ok(())
}

/// Extracts every external blob identifier recorded in one segment,
/// resolving large (`0xF0`-class) identifiers through `provider`. Fails
/// when any identifier cannot be resolved: a rebuilt catalog missing an
/// entry would let AEM's blob garbage collection delete a binary that is
/// still referenced, so callers that *publish* the catalog must fail
/// closed instead.
pub(crate) fn read_blob_identifiers(
    provider: &dyn SegmentProvider,
    structure: &ParsedSegment,
) -> Result<Vec<String>> {
    let mut identifiers = Vec::new();
    let view = provider.segment(structure.identifier)?;
    for entry in structure.record_table() {
        if entry.record_type() != Some(RecordType::ExternalBlobIdentifier) {
            continue;
        }
        let head = view.read_u8(entry.record_number, 0)?;
        if head & 0xF0 == 0xE0 {
            let stored = view.read_u16(entry.record_number, 0)?;
            let length = usize::from(stored & 0x0FFF);
            let reference_bytes = view.read_bytes(entry.record_number, 2, length)?;
            identifiers.push(String::from_utf8_lossy(reference_bytes).into_owned());
        } else if head & 0xF8 == 0xF0 {
            let string_identifier = view.read_record_identifier(entry.record_number, 1, 0)?;
            identifiers.push(read_string(provider, string_identifier)?);
        }
    }
    Ok(identifiers)
}

/// Certifies one freshly reopened source without resolving any UUID that it
/// contains through an older repository mapping. References to segments in
/// other archives still delegate to the complete provider captured by the
/// caller.
fn certify_reopened_active_archive(
    fallback: &dyn SegmentProvider,
    archive: &TarArchiveReader,
) -> Result<()> {
    let provider = ReopenedSourceProvider {
        source: archive_segments_provider(&[archive])?,
        fallback,
    };
    certify_active_archive(&provider, archive)
}

/// A complete provider whose freshly reopened source archive shadows the
/// caller's earlier repository mapping. This is required for semantic source
/// certification: `read_blob_identifiers` starts by resolving the segment
/// being inspected, and delegating that UUID would compare a fresh record
/// table against stale payload bytes.
struct ReopenedSourceProvider<'source, 'fallback> {
    source: ArchiveSegmentsProvider<'source>,
    fallback: &'fallback dyn SegmentProvider,
}

impl SegmentProvider for ReopenedSourceProvider<'_, '_> {
    fn segment(&self, identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        if let Some((structure, bytes)) = self.source.segments.get(&identifier) {
            return Ok(SegmentView {
                structure: Arc::clone(structure),
                bytes: (*bytes).into(),
            });
        }
        self.fallback.segment(identifier)
    }

    fn string(&self, identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, identifier).map(Arc::from)
    }

    fn template(&self, identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, identifier).map(Arc::new)
    }
}

/// A provider over the segments of one archive (recovered or read from
/// an open reader), so blob identifier strings referenced across
/// segments of the same archive resolve during catalog reconstruction.
struct ArchiveSegmentsProvider<'bytes> {
    segments: HashMap<SegmentIdentifier, (Arc<ParsedSegment>, &'bytes [u8])>,
}

impl SegmentProvider for ArchiveSegmentsProvider<'_> {
    fn segment(&self, segment_identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        let (structure, bytes) = self
            .segments
            .get(&segment_identifier)
            .ok_or(Error::SegmentNotFound { segment_identifier })?;
        Ok(SegmentView {
            structure: Arc::clone(structure),
            bytes: (*bytes).into(),
        })
    }

    fn string(&self, record_identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, record_identifier).map(Arc::from)
    }

    fn template(&self, record_identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, record_identifier).map(Arc::new)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::io::Write as _;
    use std::sync::Arc;

    #[cfg(unix)]
    use super::certify_active_archive;
    use super::{
        PlannedArchiveSweep, ReclaimPolicy, WritableRepository, analyze_standalone_segment_cleanup,
        archive_segments_provider, is_reclaimable, mark_one_archive, next_cleanup_archive_number,
        oak_sweep_defers, oak_sweep_threshold, plan_archive_sweep, read_blob_identifiers,
        seed_references_from_archive, stored_segment_generation, sweep_one_archive,
        validate_swept_archive,
    };
    use crate::content::provider::SegmentProvider;
    use crate::segment::identifier::SegmentIdentifier;
    #[cfg(unix)]
    use crate::segment::parsed_segment::ParsedSegment;
    use crate::segment::record::{RecordIdentifier, RecordType};
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    #[cfg(unix)]
    use crate::tar_archive::file_name::ArchiveFileName;
    use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use crate::writer::repository_lock::RepositoryLock;
    use crate::writer::segment_builder::{GarbageCollectionGeneration, SegmentBufferBuilder};
    use crate::writer::tar_writer::TarArchiveWriter;

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("froe-store-writer-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn open_prepared_store(
        directory: &std::path::Path,
        repository_lock: Arc<RepositoryLock>,
    ) -> WritableRepository {
        let certified =
            next_cleanup_archive_number(directory).expect("certify next physical archive number");
        WritableRepository::open_prepared(directory, repository_lock, certified)
            .expect("prepared writer")
    }

    #[derive(Clone)]
    struct TestArchiveEntry {
        identifier: SegmentIdentifier,
        content: Vec<u8>,
        generation: GarbageCollectionGeneration,
        references: Vec<SegmentIdentifier>,
        binary_references: Vec<String>,
    }

    impl TestArchiveEntry {
        fn new(
            identifier: SegmentIdentifier,
            size: usize,
            generation: GarbageCollectionGeneration,
        ) -> Self {
            Self {
                identifier,
                content: vec![identifier.most_significant_bits as u8; size],
                generation,
                references: Vec::new(),
                binary_references: Vec::new(),
            }
        }

        fn referencing(mut self, references: &[SegmentIdentifier]) -> Self {
            self.references.extend_from_slice(references);
            self
        }
    }

    fn data_identifier(seed: u64) -> SegmentIdentifier {
        SegmentIdentifier::new(seed, 0xA000_0000_0000_0000 | seed)
    }

    fn bulk_identifier(seed: u64) -> SegmentIdentifier {
        SegmentIdentifier::new(seed, 0xB000_0000_0000_0000 | seed)
    }

    fn non_data_identifier(seed: u64) -> SegmentIdentifier {
        SegmentIdentifier::new(seed, 0xC000_0000_0000_0000 | seed)
    }

    const fn generation(
        generation: i32,
        full_generation: i32,
        is_compacted: bool,
    ) -> GarbageCollectionGeneration {
        GarbageCollectionGeneration {
            generation,
            full_generation,
            is_compacted,
        }
    }

    fn write_test_archive(directory: &TestDirectory, name: &str, entries: &[TestArchiveEntry]) {
        let mut writer = TarArchiveWriter::new(&directory.path, name);
        for entry in entries {
            writer
                .write_segment(
                    entry.identifier,
                    &entry.content,
                    entry.generation,
                    &entry.references,
                    &entry.binary_references,
                )
                .expect("write test segment");
        }
        assert!(writer.close().expect("close test archive"));
    }

    fn write_manifest(directory: &TestDirectory) {
        std::fs::write(directory.path.join("manifest"), b"store.version=2\n")
            .expect("write manifest");
    }

    // Low-level mark/sweep fixtures intentionally use tiny synthetic segment
    // payloads and no journal. Production standalone cleanup enters through
    // the repository-backed certificate wrappers; these helpers keep the
    // arithmetic/ordering unit tests scoped to their primitive.
    fn plan_standalone_segment_cleanup(
        directory: &std::path::Path,
        reference: GarbageCollectionGeneration,
        current_head_segment: SegmentIdentifier,
        protected: &HashSet<SegmentIdentifier>,
    ) -> crate::error::Result<super::StandaloneSegmentCleanupPlan> {
        let archives = crate::store::open_all_archives(directory)?;
        analyze_standalone_segment_cleanup(
            directory,
            &archives,
            reference,
            current_head_segment,
            protected,
            &mut crate::progress::DiscardedProgress,
        )
    }

    fn apply_standalone_segment_cleanup(
        directory: &std::path::Path,
        reference: GarbageCollectionGeneration,
        current_head_segment: SegmentIdentifier,
        protected: &HashSet<SegmentIdentifier>,
        expected: Option<&super::StandaloneSegmentCleanupPlan>,
    ) -> crate::error::Result<(
        super::StandaloneSegmentCleanupPlan,
        super::StandaloneSegmentCleanupOutcome,
    )> {
        let archives = crate::store::open_all_archives(directory)?;
        super::apply_standalone_segment_cleanup_from_archives(
            directory,
            &archives,
            None,
            reference,
            current_head_segment,
            protected,
            expected,
            &mut crate::progress::DiscardedProgress,
            None,
        )
    }

    #[derive(Clone, Copy)]
    enum OmittedSessionTrailer {
        Graph,
        BinaryReferences,
    }

    fn write_session_semantic_fixture(
        store: &WritableRepository,
        generation: GarbageCollectionGeneration,
    ) -> (RecordIdentifier, RecordIdentifier) {
        let mut child_writer = store.record_writer(generation);
        let external = child_writer
            .write_external_binary_identifier("live-external-blob")
            .expect("external blob identifier");
        let child = child_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "binary".to_owned(),
                    property_type: crate::content::property::PropertyType::Binary,
                    values: PropertyValuesToWrite::Single(external),
                }],
            )
            .expect("binary-bearing child");
        child_writer.finish().expect("finish child archive");

        let mut head_writer = store.record_writer(generation);
        let head = head_writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: child,
                },
                &[],
            )
            .expect("cross-segment super root");
        head_writer.finish().expect("finish head archive");
        (head, child)
    }

    fn rewrite_session_archive_omitting_trailer(
        store: &WritableRepository,
        target: SegmentIdentifier,
        omitted: OmittedSessionTrailer,
    ) {
        let file_name = crate::store::list_archive_file_names(&store.directory)
            .expect("list archives")
            .into_iter()
            .find(|file_name| {
                TarArchiveReader::open(&store.directory.join(file_name))
                    .is_ok_and(|archive| archive.contains_segment(target))
            })
            .expect("archive containing target session segment");
        let (structure, bytes) = store
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&target)
            .map(|(structure, bytes)| (Arc::clone(structure), Arc::clone(bytes)))
            .expect("target belongs to session");
        let generation = stored_segment_generation(target, &structure);
        let mut references = structure.referenced_segments.clone();
        let mut binary_references =
            read_blob_identifiers(store, &structure).expect("reconstruct fixture BRF");
        match omitted {
            OmittedSessionTrailer::Graph => references.clear(),
            OmittedSessionTrailer::BinaryReferences => binary_references.clear(),
        }

        std::fs::remove_file(store.directory.join(&file_name))
            .expect("remove complete session TAR");
        let mut writer = TarArchiveWriter::new(&store.directory, &file_name);
        writer
            .write_segment(target, &bytes, generation, &references, &binary_references)
            .expect("write valid-checksum semantic corruption");
        writer.close().expect("finalize semantic corruption");
    }

    fn rewrite_session_archive_in_order(
        store: &WritableRepository,
        file_name: &str,
        order: &[SegmentIdentifier],
    ) {
        std::fs::remove_file(store.directory.join(file_name)).expect("remove complete session TAR");
        let mut writer = TarArchiveWriter::new(&store.directory, file_name);
        for identifier in order {
            let (structure, bytes) = store
                .session_segments
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(identifier)
                .map(|(structure, bytes)| (Arc::clone(structure), Arc::clone(bytes)))
                .expect("ordered segment belongs to session");
            let binary_references =
                read_blob_identifiers(store, &structure).expect("reconstruct fixture BRF");
            writer
                .write_segment(
                    *identifier,
                    &bytes,
                    stored_segment_generation(*identifier, &structure),
                    &structure.referenced_segments,
                    &binary_references,
                )
                .expect("write reordered session segment");
        }
        writer.close().expect("finalize reordered session archive");
    }

    fn truncate_archive_before_trailers(directory: &TestDirectory, name: &str) {
        let path = directory.path.join(name);
        let full = std::fs::read(&path).expect("read complete archive");
        let trailer_start = full
            .windows(4)
            .position(|window| window == b".brf")
            .map(|position| (position / 512) * 512)
            .expect("binary-reference trailer header exists");
        let mut truncated = full[..trailer_start].to_vec();
        truncated.extend_from_slice(&[0u8; 1024]);
        std::fs::write(path, truncated).expect("remove archive trailers");
    }

    #[test]
    fn full_reclaimer_retained_two_honors_exact_and_wrapping_boundaries() {
        let reference = generation(10, 8, true);

        // Full-generation age is decisive for compacted and non-compacted
        // segments, with equality at the retained count included.
        assert!(is_reclaimable(reference, generation(10, 6, true), true, 2));
        assert!(!is_reclaimable(reference, generation(10, 7, true), true, 2));
        // Generation age is an alternate path only for non-compacted data.
        assert!(is_reclaimable(reference, generation(8, 7, false), true, 2));
        assert!(!is_reclaimable(reference, generation(8, 7, true), true, 2));
        assert!(!is_reclaimable(reference, generation(9, 7, false), true, 2));

        // Java subtraction wraps in signed i32 arithmetic. These pairs
        // straddle the boundary and distinguish a wrapped delta of 1 from 2.
        let wrapping_reference = generation(i32::MIN, i32::MIN, false);
        assert!(!is_reclaimable(
            wrapping_reference,
            generation(i32::MAX, i32::MAX, false),
            true,
            2
        ));
        assert!(is_reclaimable(
            wrapping_reference,
            generation(i32::MAX - 1, i32::MAX - 1, false),
            true,
            2
        ));
        assert!(!is_reclaimable(
            generation(i32::MAX, i32::MAX, false),
            generation(i32::MIN, i32::MIN, false),
            true,
            2
        ));
    }

    #[test]
    fn post_compaction_reclaimer_still_retains_exactly_one_generation() {
        let reference = generation(5, 5, true);
        assert!(is_reclaimable(reference, generation(4, 4, true), true, 1));
        assert!(!is_reclaimable(reference, generation(5, 5, true), true, 1));
        assert!(is_reclaimable(reference, generation(4, 5, false), false, 1));
        assert!(!is_reclaimable(reference, generation(4, 5, true), false, 1));
    }

    #[test]
    fn exact_twenty_five_percent_savings_is_deferred_but_more_is_rewritten() {
        let directory = TestDirectory::new("savings-gate");
        let entries: Vec<_> = (1..=4)
            .map(|seed| TestArchiveEntry::new(data_identifier(seed), 1, generation(0, 0, false)))
            .collect();
        write_test_archive(&directory, "data00000a.tar", &entries);
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open archive");

        let exactly_one_quarter = HashSet::from([entries[0].identifier]);
        let exact = plan_archive_sweep(&directory.path, &reader, &exactly_one_quarter)
            .expect("plan exact threshold")
            .expect("one segment is eligible");
        assert!(matches!(
            exact,
            PlannedArchiveSweep::DeferredBySavings {
                segment_count: 1,
                eligible_entry_bytes: 1024,
                ..
            }
        ));

        let more_than_one_quarter = HashSet::from([entries[0].identifier, entries[1].identifier]);
        let rewrite = plan_archive_sweep(&directory.path, &reader, &more_than_one_quarter)
            .expect("plan above threshold")
            .expect("two segments are eligible");
        assert!(matches!(
            rewrite,
            PlannedArchiveSweep::Rewrite {
                segment_count: 2,
                eligible_entry_bytes: 2048,
                ref replacement_name,
                ..
            } if replacement_name == "data00000b.tar"
        ));
    }

    #[test]
    fn sweep_gate_reproduces_java_signed_i32_wrap_and_rejects_larger_domains() {
        assert_eq!(oak_sweep_threshold(4), 3);
        assert!(oak_sweep_defers(4, 3, "boundary").expect("equality defers"));
        assert!(!oak_sweep_defers(4, 2, "boundary").expect("more savings rewrites"));

        let largest_unwrapped = i32::MAX / 3;
        assert_eq!(
            oak_sweep_threshold(largest_unwrapped),
            largest_unwrapped * 3 / 4
        );
        assert_eq!(
            oak_sweep_threshold(largest_unwrapped + 1),
            i32::MIN.saturating_add(1) / 4,
            "the multiplication wraps before Java's truncating division"
        );
        assert!(
            oak_sweep_defers((largest_unwrapped + 1) as u64, 0, "wrapped")
                .expect("wrapped Java arithmetic"),
            "a negative wrapped threshold makes every nonnegative survivor size defer"
        );

        assert!(oak_sweep_defers(i32::MAX as u64 + 1, 0, "oversize").is_err());
        assert!(oak_sweep_defers(1, i32::MAX as u64 + 1, "oversize").is_err());
    }

    #[test]
    fn post_compaction_mark_does_not_arm_dangling_future_cleanup() {
        let directory = TestDirectory::new("post-compaction-no-dangling-root");
        let compacted = data_identifier(5);
        let reference = generation(5, 5, true);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(compacted, 1, reference)],
        );
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open archive");
        let protected = HashSet::new();
        let policy = ReclaimPolicy {
            reference,
            full: true,
            retained_generations: 1,
            protected_data_segments: &protected,
        };
        let mut references = HashSet::new();
        let mut reclaimable = HashSet::new();
        let mut disabled = None;
        mark_one_archive(
            &reader,
            policy,
            &mut references,
            &mut reclaimable,
            &mut disabled,
        )
        .expect("mark");
        assert!(reclaimable.is_empty());
        assert_eq!(disabled, None);
    }

    #[test]
    fn post_compaction_reclaim_refuses_duplicate_base_uuids_before_mutation() {
        let directory = TestDirectory::new("post-compaction-duplicate-base");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let original_path = directory.path.join("data00000a.tar");
        let duplicate_path = directory.path.join("data00001a.tar");
        std::fs::copy(&original_path, &duplicate_path).expect("copy duplicate archive");

        let original_before = std::fs::read(&original_path).expect("read original");
        let duplicate_before = std::fs::read(&duplicate_path).expect("read duplicate");
        let mut store = WritableRepository::open(&directory.path).expect("open duplicate store");
        assert_eq!(store.base_archives.len(), 2);
        let reference = store.writing_generation().expect("head generation");
        let error = store
            .reclaim_old_generations(reference, true)
            .expect_err("ambiguous global UUID marking must fail closed");
        assert!(error.to_string().contains("both active archives"));
        assert_eq!(
            store.base_archives.len(),
            2,
            "preflight must run before taking the active reader set"
        );
        assert_eq!(
            std::fs::read(&original_path).expect("original remains"),
            original_before
        );
        assert_eq!(
            std::fs::read(&duplicate_path).expect("duplicate remains"),
            duplicate_before
        );
        store.close().expect("close after refusal");

        let repository = Repository::open(&directory.path).expect("repository remains readable");
        repository.content_root().expect("content remains healthy");
    }

    #[test]
    fn post_compaction_certification_does_not_fill_the_writable_base_cache() {
        const ORPHAN_SEGMENTS: usize = 128;

        let directory = TestDirectory::new("post-compaction-bounded-certificate-cache");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap writer");
            let write_generation = store.writing_generation().expect("write generation");
            for _ in 0..ORPHAN_SEGMENTS {
                let mut writer = store.record_writer(write_generation);
                writer
                    .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                    .expect("write orphan node");
                writer.finish().expect("persist orphan segment");
            }
            store.close().expect("close many-segment base archive");
        }

        let mut store = WritableRepository::open(&directory.path).expect("open base store");
        let base_segment_count: usize = store
            .base_archives
            .iter()
            .map(TarArchiveReader::segment_count)
            .sum();
        assert!(
            base_segment_count >= ORPHAN_SEGMENTS,
            "fixture must exercise certification over many base segments"
        );
        assert!(
            store
                .parsed_segment_cache
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "write-open must begin with an empty parsed base cache"
        );

        store
            .reclaim_old_generations(generation(0, 0, false), false)
            .expect("certify and retain the generation-zero base");

        assert!(
            store
                .parsed_segment_cache
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "post-compaction certification must use the bounded fresh provider"
        );
        store.close().expect("close after cache regression");
        Repository::open(&directory.path)
            .expect("reopen after cache regression")
            .content_root()
            .expect("content remains healthy");
    }

    #[test]
    fn occupied_next_generation_is_never_truncated_or_rewritten() {
        let directory = TestDirectory::new("occupied-next-generation");
        let root = data_identifier(10);
        let old_one = data_identifier(11);
        let old_two = data_identifier(12);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(root, 1, generation(4, 4, false)),
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
            ],
        );
        let occupied = b"interrupted-cleanup-evidence-must-survive";
        std::fs::write(directory.path.join("data00000b.tar"), occupied)
            .expect("write occupied target");
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open source");
        let cleaned = HashSet::from([old_one, old_two]);

        let planned = plan_archive_sweep(&directory.path, &reader, &cleaned)
            .expect("plan")
            .expect("archive has reclaimable entries");
        assert!(matches!(
            planned,
            PlannedArchiveSweep::BlockedByOccupiedGeneration {
                ref occupied_name,
                ..
            } if occupied_name == "data00000b.tar"
        ));
        let mut fallback = None;
        sweep_one_archive(
            &directory.path,
            &reader,
            &cleaned,
            &cleaned,
            &[&reader],
            &mut fallback,
            None,
        )
        .expect("blocked sweep is a safe no-op");
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar")).expect("read occupied target"),
            occupied
        );
        assert!(directory.path.join("data00000a.tar").exists());
    }

    #[test]
    fn staging_namespace_exhaustion_fails_during_plan_without_mutation() {
        let directory = TestDirectory::new("staging-namespace-exhausted");
        let root = data_identifier(1010);
        let old_one = data_identifier(1011);
        let old_two = data_identifier(1012);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(root, 1, generation(4, 4, false)),
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
            ],
        );
        let source_path = directory.path.join("data00000a.tar");
        let source_before = std::fs::read(&source_path).expect("source before");
        for counter in 0..=999u16 {
            std::fs::write(
                directory
                    .path
                    .join(format!("data00000b.tar.cleaning.{counter:03}")),
                counter.to_be_bytes(),
            )
            .expect("occupy staging name");
        }
        let reader = TarArchiveReader::open(&source_path).expect("open source");
        let error =
            plan_archive_sweep(&directory.path, &reader, &HashSet::from([old_one, old_two]))
                .expect_err("planning must detect that no exclusive staging name exists");
        assert!(
            error
                .to_string()
                .contains("all 1000 exclusive staging names")
        );
        assert_eq!(
            std::fs::read(&source_path).expect("source after refusal"),
            source_before
        );
        assert!(!directory.path.join("data00000b.tar").exists());
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar.cleaning.000"))
                .expect("first residue"),
            0u16.to_be_bytes()
        );
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar.cleaning.999"))
                .expect("last residue"),
            999u16.to_be_bytes()
        );
    }

    #[test]
    fn occupied_higher_generation_blocks_whole_archive_removal() {
        let directory = TestDirectory::new("occupied-blocks-whole-removal");
        let obsolete = data_identifier(13);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(obsolete, 1, generation(0, 0, false))],
        );
        let occupied = b"damaged-higher-generation-must-not-become-active";
        std::fs::write(directory.path.join("data00000c.tar"), occupied)
            .expect("write recovered residue");
        let source_path = directory.path.join("data00000a.tar");
        let source_before = std::fs::read(&source_path).expect("read source");
        let reader = TarArchiveReader::open(&source_path).expect("open source");
        let cleaned = HashSet::from([obsolete]);

        assert!(matches!(
            plan_archive_sweep(&directory.path, &reader, &cleaned)
                .expect("plan")
                .expect("eligible archive"),
            PlannedArchiveSweep::BlockedByOccupiedGeneration {
                occupied_name,
                segment_count: 1,
                ..
            } if occupied_name == "data00000c.tar"
        ));
        let mut fallback = None;
        sweep_one_archive(
            &directory.path,
            &reader,
            &cleaned,
            &cleaned,
            &[&reader],
            &mut fallback,
            None,
        )
        .expect("blocked removal is a no-op");
        assert_eq!(
            std::fs::read(source_path).expect("source remains"),
            source_before
        );
        assert_eq!(
            std::fs::read(directory.path.join("data00000c.tar")).expect("residue remains"),
            occupied
        );
    }

    #[test]
    fn lower_stale_generation_blocks_whole_active_archive_removal() {
        let directory = TestDirectory::new("lower-letter-blocks-whole-removal");
        let stale = data_identifier(14);
        let obsolete = data_identifier(15);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(stale, 1, generation(0, 0, false))],
        );
        write_test_archive(
            &directory,
            "data00000b.tar",
            &[TestArchiveEntry::new(obsolete, 1, generation(0, 0, false))],
        );
        let stale_path = directory.path.join("data00000a.tar");
        let active_path = directory.path.join("data00000b.tar");
        let stale_before = std::fs::read(&stale_path).expect("read stale generation");
        let active_before = std::fs::read(&active_path).expect("read active generation");
        let active = TarArchiveReader::open(&active_path).expect("open active generation");
        let cleaned = HashSet::from([obsolete]);

        assert!(matches!(
            plan_archive_sweep(&directory.path, &active, &cleaned)
                .expect("plan")
                .expect("eligible archive"),
            PlannedArchiveSweep::BlockedByOccupiedGeneration {
                occupied_name,
                segment_count: 1,
                ..
            } if occupied_name == "data00000a.tar"
        ));
        let mut fallback = None;
        sweep_one_archive(
            &directory.path,
            &active,
            &cleaned,
            &cleaned,
            &[&active],
            &mut fallback,
            None,
        )
        .expect("blocked removal is a no-op");
        assert_eq!(
            std::fs::read(active_path).expect("active remains"),
            active_before
        );
        assert_eq!(
            std::fs::read(stale_path).expect("stale remains"),
            stale_before
        );
    }

    #[test]
    fn last_generation_z_is_deferred_without_creating_an_invalid_successor() {
        let directory = TestDirectory::new("generation-z");
        let root = data_identifier(20);
        let old_one = data_identifier(21);
        let old_two = data_identifier(22);
        write_test_archive(
            &directory,
            "data00000z.tar",
            &[
                TestArchiveEntry::new(root, 1, generation(4, 4, false)),
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
            ],
        );
        let path = directory.path.join("data00000z.tar");
        let before = std::fs::read(&path).expect("read source");
        let reader = TarArchiveReader::open(&path).expect("open source");
        let cleaned = HashSet::from([old_one, old_two]);
        assert!(matches!(
            plan_archive_sweep(&directory.path, &reader, &cleaned)
                .expect("plan")
                .expect("has eligible entries"),
            PlannedArchiveSweep::DeferredAtLastGeneration { .. }
        ));
        let mut fallback = None;
        sweep_one_archive(
            &directory.path,
            &reader,
            &cleaned,
            &cleaned,
            &[&reader],
            &mut fallback,
            None,
        )
        .expect("z sweep is a no-op");
        assert_eq!(std::fs::read(path).expect("read after"), before);
        assert_eq!(
            std::fs::read_dir(&directory.path)
                .expect("list")
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tar"))
                .count(),
            1
        );
    }

    #[test]
    fn dangling_future_state_is_global_ordered_and_history_vetoed() {
        let directory = TestDirectory::new("dangling-future-order");
        let older_compacted = data_identifier(30);
        let root = data_identifier(31);
        let protected_future = data_identifier(32);
        let future = data_identifier(33);
        let referenced_future_bulk = bulk_identifier(34);
        let protected_bulk = bulk_identifier(35);
        let current = generation(7, 7, true);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(older_compacted, 1, current),
                TestArchiveEntry::new(root, 1, current),
                TestArchiveEntry::new(protected_bulk, 1, generation(0, 0, false)),
                TestArchiveEntry::new(protected_future, 1, current).referencing(&[protected_bulk]),
                TestArchiveEntry::new(future, 1, current),
                TestArchiveEntry::new(referenced_future_bulk, 1, current),
            ],
        );
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open archive");
        let protected = HashSet::from([protected_future]);
        let policy = ReclaimPolicy {
            reference: current,
            full: true,
            retained_generations: 2,
            protected_data_segments: &protected,
        };
        let mut references = HashSet::from([referenced_future_bulk]);
        let mut reclaimable = HashSet::new();
        let mut ahead_of_root = Some(root);
        mark_one_archive(
            &reader,
            policy,
            &mut references,
            &mut reclaimable,
            &mut ahead_of_root,
        )
        .expect("mark");

        assert!(reclaimable.contains(&future));
        assert!(
            reclaimable.contains(&referenced_future_bulk),
            "dangling-future precedes bulk reachability"
        );
        assert!(!reclaimable.contains(&protected_future));
        assert!(
            !reclaimable.contains(&protected_bulk),
            "a history-vetoed data segment must still protect its bulk closure"
        );
        assert!(!reclaimable.contains(&root));
        assert!(!reclaimable.contains(&older_compacted));
        assert_eq!(ahead_of_root, None, "the root disarms the rule forever");
    }

    #[test]
    fn standalone_mark_fails_closed_when_the_exact_head_segment_is_absent() {
        let directory = TestDirectory::new("dangling-root-absent");
        let present = data_identifier(40);
        let missing_root = data_identifier(41);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(present, 1, generation(4, 4, true))],
        );
        let reader =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open archive");
        let error = analyze_standalone_segment_cleanup(
            &directory.path,
            &[reader],
            generation(4, 4, true),
            missing_root,
            &HashSet::new(),
            &mut crate::progress::DiscardedProgress,
        )
        .expect_err("missing root must refuse cleanup");
        assert!(error.to_string().contains("was not encountered"));
        assert!(directory.path.join("data00000a.tar").exists());
    }

    #[test]
    fn dangling_future_boundary_is_shared_across_archive_files() {
        let directory = TestDirectory::new("dangling-future-cross-archive");
        let older = data_identifier(50);
        let root = data_identifier(51);
        let future = data_identifier(52);
        let current = generation(9, 9, true);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(older, 1, current),
                TestArchiveEntry::new(root, 1, current),
            ],
        );
        write_test_archive(
            &directory,
            "data00001a.tar",
            &[TestArchiveEntry::new(future, 1, current)],
        );
        write_manifest(&directory);

        let plan = plan_standalone_segment_cleanup(&directory.path, current, root, &HashSet::new())
            .expect("plan across archives");
        assert_eq!(plan.marked_segments, 1);
        assert_eq!(plan.reclaimable_segments(), &HashSet::from([future]));
        assert!(matches!(
            plan.archives.as_slice(),
            [PlannedArchiveSweep::Remove {
                file_name,
                segment_count: 1,
                ..
            }] if file_name == "data00001a.tar"
        ));
        assert!(!plan.reclaimable_segments().contains(&root));
        assert!(!plan.reclaimable_segments().contains(&older));
    }

    #[test]
    fn kept_data_in_a_newer_tar_protects_bulk_in_an_older_tar() {
        let directory = TestDirectory::new("cross-tar-bulk-reference");
        let bulk = bulk_identifier(60);
        let root = data_identifier(61);
        let current = generation(6, 6, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(bulk, 128, generation(0, 0, false))],
        );
        write_test_archive(
            &directory,
            "data00001a.tar",
            &[TestArchiveEntry::new(root, 128, current).referencing(&[bulk])],
        );
        write_manifest(&directory);

        let plan = plan_standalone_segment_cleanup(&directory.path, current, root, &HashSet::new())
            .expect("plan");
        assert!(!plan.reclaimable_segments().contains(&bulk));
        assert_eq!(plan.marked_segments, 0);
        assert!(plan.archives.is_empty());
    }

    #[test]
    fn mark_and_session_seed_follow_every_non_data_identifier() {
        let directory = TestDirectory::new("cross-tar-non-data-reference");
        let non_data = non_data_identifier(65);
        let root = data_identifier(66);
        let current = generation(6, 6, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(
                non_data,
                128,
                generation(0, 0, false),
            )],
        );
        write_test_archive(
            &directory,
            "data00001a.tar",
            &[TestArchiveEntry::new(root, 128, current).referencing(&[non_data])],
        );
        write_manifest(&directory);

        let plan = plan_standalone_segment_cleanup(&directory.path, current, root, &HashSet::new())
            .expect("plan");
        assert!(
            !plan.reclaimable_segments().contains(&non_data),
            "Oak follows every non-data identifier, not only the canonical 0xB kind"
        );

        let session = TarArchiveReader::open(&directory.path.join("data00001a.tar"))
            .expect("open session-style archive");
        let mut references = HashSet::new();
        seed_references_from_archive(&session, &mut references).expect("seed session references");
        assert_eq!(references, HashSet::from([non_data]));
    }

    #[test]
    fn store_wide_reclaim_set_filters_cross_archive_graph_targets() {
        let directory = TestDirectory::new("global-graph-filter");
        let target = data_identifier(70);
        let old_one = data_identifier(71);
        let old_two = data_identifier(72);
        let root = data_identifier(73);
        let reference = generation(5, 5, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(target, 1, generation(0, 0, false))],
        );
        write_test_archive(
            &directory,
            "data00001a.tar",
            &[
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
                TestArchiveEntry::new(root, 1, reference).referencing(&[target]),
            ],
        );
        write_manifest(&directory);

        let expected =
            plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
                .expect("plan");
        assert_eq!(
            expected.reclaimable_segments(),
            &HashSet::from([target, old_one, old_two])
        );
        assert!(expected.archives.iter().any(|archive| matches!(
            archive,
            PlannedArchiveSweep::Remove { file_name, .. }
                if file_name == "data00000a.tar"
        )));
        assert!(expected.archives.iter().any(|archive| matches!(
            archive,
            PlannedArchiveSweep::Rewrite {
                file_name,
                replacement_name,
                ..
            } if file_name == "data00001a.tar" && replacement_name == "data00001b.tar"
        )));

        let (_, outcome) = apply_standalone_segment_cleanup(
            &directory.path,
            reference,
            root,
            &HashSet::new(),
            Some(&expected),
        )
        .expect("apply");
        assert_eq!(outcome.removed_archives, 1);
        assert_eq!(outcome.rewritten_archives, 1);
        assert_eq!(outcome.removed_segments, 3);
        assert!(!directory.path.join("data00000a.tar").exists());
        assert!(!directory.path.join("data00001a.tar").exists());

        let swept = TarArchiveReader::open(&directory.path.join("data00001b.tar"))
            .expect("open swept archive");
        assert_eq!(swept.segment_count(), 1);
        assert!(swept.contains_segment(root));
        let graph = swept.segment_graph().expect("graph remains valid");
        assert!(
            graph
                .adjacency
                .iter()
                .flat_map(|(_, targets)| targets)
                .all(|identifier| *identifier != target),
            "the target reclaimed from another tar must be filtered globally"
        );
    }

    #[test]
    fn deferred_cross_archive_target_remains_in_rewritten_graph() {
        let directory = TestDirectory::new("deferred-global-graph-target");
        let target = data_identifier(74);
        let retained_one = data_identifier(75);
        let retained_two = data_identifier(76);
        let retained_three = data_identifier(77);
        let old_one = data_identifier(78);
        let old_two = data_identifier(79);
        let root = data_identifier(80);
        let reference = generation(5, 5, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(target, 1, generation(0, 0, false)),
                TestArchiveEntry::new(retained_one, 1, reference),
                TestArchiveEntry::new(retained_two, 1, reference),
                TestArchiveEntry::new(retained_three, 1, reference),
            ],
        );
        write_test_archive(
            &directory,
            "data00001a.tar",
            &[
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
                TestArchiveEntry::new(root, 1, reference).referencing(&[target]),
            ],
        );
        write_manifest(&directory);

        let expected =
            plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
                .expect("plan");
        assert!(expected.archives.iter().any(|archive| matches!(
            archive,
            PlannedArchiveSweep::DeferredBySavings { file_name, .. }
                if file_name == "data00000a.tar"
        )));
        assert!(expected.archives.iter().any(|archive| matches!(
            archive,
            PlannedArchiveSweep::Rewrite { file_name, .. }
                if file_name == "data00001a.tar"
        )));

        apply_standalone_segment_cleanup(
            &directory.path,
            reference,
            root,
            &HashSet::new(),
            Some(&expected),
        )
        .expect("apply");

        assert!(
            directory.path.join("data00000a.tar").exists(),
            "the savings-deferred target remains physically available"
        );
        let swept = TarArchiveReader::open(&directory.path.join("data00001b.tar"))
            .expect("open rewritten source");
        let graph = swept.segment_graph().expect("graph remains valid");
        assert_eq!(
            graph.as_map()[&root],
            [target],
            "a deferred target must not be filtered by a wider global reclaim set"
        );
    }

    #[test]
    fn immediate_replan_noop_is_not_reported_as_a_completed_rewrite() {
        let directory = TestDirectory::new("rewrite-replan-noop-outcome");
        let old_one = data_identifier(81);
        let old_two = data_identifier(82);
        let root = data_identifier(83);
        let reference = generation(5, 5, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
                TestArchiveEntry::new(root, 1, reference),
            ],
        );
        write_manifest(&directory);

        let archives = crate::store::open_all_archives(&directory.path).expect("open archives");
        let occupied = b"occupied after authoritative planning";
        let after_plan = |plan: &super::StandaloneSegmentCleanupPlan| {
            let replacement = plan
                .archives
                .iter()
                .find_map(|archive| match archive {
                    PlannedArchiveSweep::Rewrite {
                        file_name,
                        replacement_name,
                        ..
                    } if file_name == "data00000a.tar" => Some(replacement_name),
                    _ => None,
                })
                .expect("the authoritative outer plan must request a rewrite");
            std::fs::write(directory.path.join(replacement), occupied)?;
            Ok(())
        };
        let (plan, outcome) = super::apply_standalone_segment_cleanup_from_archives(
            &directory.path,
            &archives,
            None,
            reference,
            root,
            &HashSet::new(),
            None,
            &mut crate::progress::DiscardedProgress,
            Some(&after_plan),
        )
        .expect("an occupied immediate replan is a safe no-op");

        assert!(matches!(
            plan.archives.as_slice(),
            [PlannedArchiveSweep::Rewrite { .. }]
        ));
        assert_eq!(outcome.rewritten_archives, 0);
        assert_eq!(outcome.removed_archives, 0);
        assert_eq!(outcome.removed_segments, 0);
        assert!(outcome.deletion_failures.is_empty());
        assert!(directory.path.join("data00000a.tar").exists());
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar"))
                .expect("read occupied replacement"),
            occupied,
            "an unrelated occupied generation must not be credited as cleanup output"
        );
    }

    #[test]
    fn rewrite_replan_noop_reports_no_unavailable_graph_targets() {
        let directory = TestDirectory::new("rewrite-replan-noop-graph-target");
        let target = data_identifier(83);
        let retained = data_identifier(84);
        let old_one = data_identifier(85);
        let old_two = data_identifier(86);
        let root = data_identifier(87);
        let reference = generation(5, 5, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(target, 1, generation(0, 0, false)),
                TestArchiveEntry::new(retained, 1, reference),
            ],
        );
        write_test_archive(
            &directory,
            "data00001a.tar",
            &[
                TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
                TestArchiveEntry::new(root, 1, reference).referencing(&[target]),
            ],
        );

        let first = TarArchiveReader::open(&directory.path.join("data00000a.tar"))
            .expect("open first rewrite source");
        let second = TarArchiveReader::open(&directory.path.join("data00001a.tar"))
            .expect("open second rewrite source");
        let reclaimable = HashSet::from([target, old_one, old_two]);
        assert!(matches!(
            plan_archive_sweep(&directory.path, &first, &reclaimable)
                .expect("initial first plan")
                .expect("first archive is initially actionable"),
            PlannedArchiveSweep::Rewrite { .. }
        ));
        assert!(matches!(
            plan_archive_sweep(&directory.path, &second, &reclaimable)
                .expect("initial second plan")
                .expect("second archive is actionable"),
            PlannedArchiveSweep::Rewrite { .. }
        ));

        // Model a pathname appearing after the outer plan but before the
        // immediate per-archive replan. The first sweep must return a proven
        // no-publication outcome, not inherit the stale Rewrite disposition.
        let occupied = b"occupied after outer planning";
        std::fs::write(directory.path.join("data00000b.tar"), occupied)
            .expect("occupy first replacement");
        let provider_order = [&first, &second];
        let mut fallback = None;
        let mut actually_unavailable = HashSet::new();
        let first_outcome = sweep_one_archive(
            &directory.path,
            &first,
            &reclaimable,
            &actually_unavailable,
            &provider_order,
            &mut fallback,
            None,
        )
        .expect("blocked immediate replan is a no-op");
        assert!(first_outcome.deletion_failures.is_empty());
        assert!(
            first_outcome.newly_unavailable.is_empty(),
            "a planned rewrite that never published cannot justify graph filtering"
        );
        assert!(
            directory.path.join("data00000a.tar").exists(),
            "the blocked immediate replan must leave its source available"
        );
        assert_eq!(
            std::fs::read(directory.path.join("data00000b.tar")).expect("read occupied target"),
            occupied,
            "the blocked immediate replan must not replace the new pathname"
        );
        actually_unavailable.extend(first_outcome.newly_unavailable);

        let second_outcome = sweep_one_archive(
            &directory.path,
            &second,
            &reclaimable,
            &actually_unavailable,
            &provider_order,
            &mut fallback,
            None,
        )
        .expect("second rewrite publishes");
        assert_eq!(
            second_outcome.newly_unavailable,
            HashSet::from([old_one, old_two])
        );

        let rewritten = TarArchiveReader::open(&directory.path.join("data00001b.tar"))
            .expect("open second replacement");
        assert_eq!(
            rewritten.segment_graph().expect("valid graph").as_map()[&root],
            [target],
            "the later rewrite must retain an edge to the still-available first target"
        );
    }

    #[test]
    fn sweep_preserves_survivor_brf_generation_triples_and_omits_removed_sources() {
        let directory = TestDirectory::new("brf-filter-and-triples");
        let root = data_identifier(80);
        let removed_one = data_identifier(81);
        let removed_two = data_identifier(82);
        let reference = generation(6, 6, false);
        let survivor_catalog_generation = generation(17, 11, true);
        let removed_catalog_generation = generation(18, 12, false);

        let mut writer = TarArchiveWriter::new(&directory.path, "data00000a.tar");
        writer.add_binary_references(survivor_catalog_generation, root, ["live-blob".to_owned()]);
        writer.add_binary_references(
            removed_catalog_generation,
            removed_one,
            ["dead-blob-one".to_owned()],
        );
        writer.add_binary_references(
            removed_catalog_generation,
            removed_two,
            ["dead-blob-two".to_owned()],
        );
        for entry in [
            TestArchiveEntry::new(root, 1, reference),
            TestArchiveEntry::new(removed_one, 1, generation(0, 0, false)),
            TestArchiveEntry::new(removed_two, 1, generation(0, 0, false)),
        ] {
            writer
                .write_segment(entry.identifier, &entry.content, entry.generation, &[], &[])
                .expect("write segment");
        }
        writer.close().expect("close archive");
        write_manifest(&directory);

        let plan =
            plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
                .expect("plan");
        apply_standalone_segment_cleanup(
            &directory.path,
            reference,
            root,
            &HashSet::new(),
            Some(&plan),
        )
        .expect("apply");

        let swept = TarArchiveReader::open(&directory.path.join("data00000b.tar"))
            .expect("open swept archive");
        let catalog = swept.binary_references().expect("catalog survives");
        assert_eq!(catalog.generations.len(), 1);
        let generation = &catalog.generations[0];
        assert_eq!(
            generation.generation,
            survivor_catalog_generation.generation
        );
        assert_eq!(
            generation.full_generation,
            survivor_catalog_generation.full_generation
        );
        assert_eq!(
            generation.is_compacted,
            survivor_catalog_generation.is_compacted
        );
        assert_eq!(
            generation.segments,
            vec![(root, vec!["live-blob".to_owned()])]
        );
        assert!(catalog.generations.iter().all(|generation| {
            generation
                .segments
                .iter()
                .all(|(source, _)| *source != removed_one && *source != removed_two)
        }));
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one fixture exercises graph, BRF, ordering, logical-name, and non-active-residue validation together"
    )]
    #[test]
    fn staged_rewrite_validation_requires_exact_trailers_and_physical_order() {
        let directory = TestDirectory::new("staged-semantic-validation");
        let first = data_identifier(83);
        let second = data_identifier(84);
        let generation = generation(7, 6, true);
        let first_bytes = vec![0x83; 64];
        let second_bytes = vec![0x84; 64];
        let live_blob = "live-blob".to_owned();

        let mut source_writer = TarArchiveWriter::new(&directory.path, "data00000a.tar");
        source_writer
            .write_segment(
                first,
                &first_bytes,
                generation,
                &[second],
                std::slice::from_ref(&live_blob),
            )
            .expect("write source first");
        source_writer
            .write_segment(second, &second_bytes, generation, &[], &[])
            .expect("write source second");
        source_writer.close().expect("close source");
        let source_path = directory.path.join("data00000a.tar");
        let source_before = std::fs::read(&source_path).expect("read source");
        let source = TarArchiveReader::open(&source_path).expect("open source");
        let mut survivors = source.index().expect("source index").entries().to_vec();
        survivors.sort_by_key(|entry| entry.position);

        let expected_graph = HashMap::from([(first, HashSet::from([second]))]);
        let expected_binary_references = HashMap::from([(
            (
                generation.generation,
                generation.full_generation,
                generation.is_compacted,
            ),
            HashMap::from([(first, HashSet::from([live_blob.clone()]))]),
        )]);

        let write_staged =
            |physical_name: &str, order: &[SegmentIdentifier], graph: bool, catalog: bool| {
                let mut writer = TarArchiveWriter::new_exclusive_staged(
                    &directory.path,
                    physical_name,
                    "data00000b.tar",
                );
                for identifier in order {
                    let bytes = if *identifier == first {
                        &first_bytes
                    } else {
                        &second_bytes
                    };
                    let references = if *identifier == first && graph {
                        vec![second]
                    } else {
                        Vec::new()
                    };
                    let binary_references = if *identifier == first && catalog {
                        vec![live_blob.clone()]
                    } else {
                        Vec::new()
                    };
                    writer
                        .write_segment(
                            *identifier,
                            bytes,
                            generation,
                            &references,
                            &binary_references,
                        )
                        .expect("write staged survivor");
                }
                writer.close().expect("close staged archive");
            };

        let missing_graph = "data00000b.tar.cleaning.000";
        write_staged(missing_graph, &[first, second], false, true);
        let graph_error = validate_swept_archive(
            &source,
            &directory.path.join(missing_graph),
            &survivors,
            &expected_graph,
            &expected_binary_references,
        )
        .expect_err("a valid-checksum rewrite may not omit a live graph edge");
        assert!(graph_error.to_string().contains("graph differs"));

        let missing_catalog = "data00000b.tar.cleaning.001";
        write_staged(missing_catalog, &[first, second], true, false);
        let catalog_error = validate_swept_archive(
            &source,
            &directory.path.join(missing_catalog),
            &survivors,
            &expected_graph,
            &expected_binary_references,
        )
        .expect_err("a valid-checksum rewrite may not omit a live BRF entry");
        assert!(catalog_error.to_string().contains("catalog differs"));

        let reordered = "data00000b.tar.cleaning.002";
        write_staged(reordered, &[second, first], true, true);
        let order_error = validate_swept_archive(
            &source,
            &directory.path.join(reordered),
            &survivors,
            &expected_graph,
            &expected_binary_references,
        )
        .expect_err("a semantically equal rewrite may not reorder physical segments");
        assert!(order_error.to_string().contains("physical order"));

        let staged_bytes = std::fs::read(directory.path.join(missing_graph)).expect("read stage");
        for suffix in ["brf", "gph", "idx"] {
            let logical = format!("data00000b.tar.{suffix}");
            assert!(
                staged_bytes
                    .windows(logical.len())
                    .any(|window| window == logical.as_bytes()),
                "staged trailers must carry the final logical basename"
            );
        }

        write_manifest(&directory);
        let selected = crate::store::open_all_archives(&directory.path)
            .expect("non-active staging residue is ignored");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].file_name(), "data00000a.tar");
        assert_eq!(
            std::fs::read(source_path).expect("healthy source remains"),
            source_before
        );
        assert!(!directory.path.join("data00000b.tar").exists());
    }

    #[cfg(unix)]
    #[test]
    fn swept_archive_preserves_source_owner_group_and_mode_before_publication() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = TestDirectory::new("sweep-file-metadata");
        let root = data_identifier(85);
        let old_one = data_identifier(86);
        let old_two = data_identifier(87);
        let reference = generation(4, 4, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(root, 64, reference),
                TestArchiveEntry::new(old_one, 64, generation(0, 0, false)),
                TestArchiveEntry::new(old_two, 64, generation(0, 0, false)),
            ],
        );
        let source_path = directory.path.join("data00000a.tar");
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o640))
            .expect("set distinctive source mode");
        let source_metadata = std::fs::metadata(&source_path).expect("source metadata");
        write_manifest(&directory);

        let plan =
            plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
                .expect("plan rewrite");
        assert!(matches!(
            plan.archives.as_slice(),
            [PlannedArchiveSweep::Rewrite { .. }]
        ));
        apply_standalone_segment_cleanup(
            &directory.path,
            reference,
            root,
            &HashSet::new(),
            Some(&plan),
        )
        .expect("publish metadata-preserving rewrite");

        let replacement_path = directory.path.join("data00000b.tar");
        let replacement_metadata =
            std::fs::metadata(&replacement_path).expect("replacement metadata");
        assert_eq!(replacement_metadata.uid(), source_metadata.uid());
        assert_eq!(replacement_metadata.gid(), source_metadata.gid());
        assert_eq!(
            replacement_metadata.mode() & 0o7777,
            source_metadata.mode() & 0o7777
        );
        assert_eq!(replacement_metadata.mode() & 0o7777, 0o640);
        assert!(
            std::fs::read_dir(&directory.path)
                .expect("list repository")
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().contains(".cleaning.")),
            "successful publication removes its non-active staging link"
        );
        let replacement = TarArchiveReader::open(&replacement_path).expect("open replacement");
        assert!(!replacement.is_recovered());
        assert!(replacement.contains_segment(root));
    }

    #[test]
    fn duplicate_segment_identifiers_across_active_archives_refuse_cleanup() {
        let directory = TestDirectory::new("duplicate-active-segments");
        let duplicate = data_identifier(90);
        let reference = generation(3, 3, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(duplicate, 1, reference)],
        );
        write_test_archive(
            &directory,
            "data00001a.tar",
            &[TestArchiveEntry::new(duplicate, 1, reference)],
        );
        write_manifest(&directory);

        let error =
            plan_standalone_segment_cleanup(&directory.path, reference, duplicate, &HashSet::new())
                .expect_err("duplicates make a global decision ambiguous");
        let message = error.to_string();
        assert!(message.contains("both active archives"));
        assert!(message.contains("data00000a.tar"));
        assert!(message.contains("data00001a.tar"));
    }

    #[test]
    fn recovered_newer_archive_is_not_swept_and_still_protects_older_bulk() {
        let directory = TestDirectory::new("recovered-protects-bulk");
        let bulk = bulk_identifier(100);
        let root = data_identifier(101);
        let reference = generation(4, 4, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(bulk, 64, generation(0, 0, false))],
        );

        let mut builder = SegmentBufferBuilder::new(root, reference);
        let record = builder
            .allocate(RecordType::Value, 6, &[bulk])
            .expect("allocate referencing record");
        let reference_number = builder.reference_for(bulk);
        let mut record_bytes = [0u8; 6];
        SegmentBufferBuilder::write_record_identifier_bytes(reference_number, 0, &mut record_bytes);
        builder
            .record_bytes_mut(record)
            .copy_from_slice(&record_bytes);
        let built = builder.finish();
        let mut writer = TarArchiveWriter::new(&directory.path, "data00001a.tar");
        writer
            .write_segment(root, &built.bytes, reference, &[bulk], &[])
            .expect("write root");
        writer.close().expect("close root archive");
        truncate_archive_before_trailers(&directory, "data00001a.tar");
        write_manifest(&directory);
        assert!(
            TarArchiveReader::open(&directory.path.join("data00001a.tar"))
                .expect("open recovered archive")
                .is_recovered()
        );

        let plan =
            plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
                .expect("recovered archive participates conservatively");
        assert!(!plan.reclaimable_segments().contains(&root));
        assert!(!plan.reclaimable_segments().contains(&bulk));
        assert!(plan.archives.is_empty());
    }

    #[test]
    fn malformed_recovered_root_fails_closed_without_mutating_the_archive() {
        let directory = TestDirectory::new("malformed-recovered-root");
        let root = data_identifier(110);
        let reference = generation(4, 4, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[TestArchiveEntry::new(root, 64, reference)],
        );
        truncate_archive_before_trailers(&directory, "data00000a.tar");
        write_manifest(&directory);
        let path = directory.path.join("data00000a.tar");
        let before = std::fs::read(&path).expect("read recovered archive");

        let error =
            plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
                .expect_err("malformed kept data cannot safely propagate references");
        assert!(error.to_string().contains("magic bytes"));
        assert_eq!(std::fs::read(path).expect("read after refusal"), before);
    }

    #[test]
    fn missing_brf_reconstruction_failure_leaves_original_and_no_replacement() {
        let directory = TestDirectory::new("missing-brf-fail-closed");
        let root = data_identifier(120);
        let removed_one = data_identifier(121);
        let removed_two = data_identifier(122);
        let reference = generation(5, 5, false);
        write_test_archive(
            &directory,
            "data00000a.tar",
            &[
                TestArchiveEntry::new(root, 64, reference),
                TestArchiveEntry::new(removed_one, 64, generation(0, 0, false)),
                TestArchiveEntry::new(removed_two, 64, generation(0, 0, false)),
            ],
        );
        let source_path = directory.path.join("data00000a.tar");
        let mut bytes = std::fs::read(&source_path).expect("read archive");
        let brf_magic = bytes
            .windows(4)
            .position(|window| window == [0x0A, 0x31, 0x42, 0x0A])
            .expect("brf magic");
        bytes[brf_magic] ^= 0x01;
        std::fs::write(&source_path, &bytes).expect("corrupt only brf footer");
        write_manifest(&directory);
        let reader = TarArchiveReader::open(&source_path).expect("index remains valid");
        assert!(reader.index().is_some());
        assert!(reader.segment_graph().is_some());
        assert!(reader.binary_references().is_none());
        drop(reader);

        let plan =
            plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
                .expect("mark does not need brf");
        let error = apply_standalone_segment_cleanup(
            &directory.path,
            reference,
            root,
            &HashSet::new(),
            Some(&plan),
        )
        .expect_err("catalog reconstruction must fail closed on malformed data");
        assert!(error.to_string().contains("magic bytes"));
        assert_eq!(std::fs::read(&source_path).expect("source remains"), bytes);
        assert!(!directory.path.join("data00000b.tar").exists());
    }

    #[test]
    fn bootstraps_a_fresh_store_that_the_reader_opens() {
        let directory = TestDirectory::new("bootstrap");
        let store = WritableRepository::open(&directory.path).expect("open fresh store");
        store.close().expect("close");

        let manifest =
            std::fs::read_to_string(directory.path.join("manifest")).expect("manifest exists");
        assert!(manifest.contains("store.version=2"));
        assert!(directory.path.join("repo.lock").exists());

        let journal = std::fs::read_to_string(directory.path.join("journal.log")).expect("journal");
        assert_eq!(journal.lines().count(), 1, "exactly one bootstrap revision");
        assert!(journal.contains(" root "));

        let repository = Repository::open(&directory.path).expect("reader opens");
        assert!(
            !repository.archives()[0].is_recovered(),
            "the archive has a valid index"
        );
        let content_root = repository.content_root().expect("content root exists");
        assert_eq!(content_root.child_node_count().expect("count"), 0);
        assert!(content_root.properties().expect("properties").is_empty());
    }

    #[test]
    fn catalog_provider_resolves_duplicate_segments_to_the_newest_archive() {
        let directory = TestDirectory::new("provider-newest-wins");
        std::fs::create_dir_all(&directory.path).expect("create directory");
        let bulk = crate::writer::identifier_generator::new_bulk_segment_identifier();
        let generation = GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        };
        let write_archive = |name: &str, content: &[u8]| {
            let mut writer = TarArchiveWriter::new(&directory.path, name);
            writer
                .write_segment(bulk, content, generation, &[], &[])
                .expect("write segment");
            writer.close().expect("close archive");
        };
        write_archive("data00000a.tar", b"old-archive-copy");
        write_archive("data00001a.tar", b"new-archive-copy");

        // Newest archive first, the order `base_archives` maintains.
        let newest =
            TarArchiveReader::open(&directory.path.join("data00001a.tar")).expect("open newest");
        let oldest =
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open oldest");
        let provider = archive_segments_provider(&[&newest, &oldest]).expect("provider");
        let view = provider.segment(bulk).expect("duplicate resolves");
        assert_eq!(
            &view.bytes[..],
            b"new-archive-copy",
            "a duplicated segment resolves to the newest archive's copy"
        );
    }

    #[cfg(unix)]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression must build a same-layout valid-CRC source swap and exercise the complete pre-publication sweep path"
    )]
    #[test]
    fn immediate_source_certificate_uses_the_reopened_blob_payload() {
        const ORIGINAL_BLOB: &[u8] = b"live-external-blob";
        const SWAPPED_BLOB: &[u8] = b"evil-external-blob";
        assert_eq!(ORIGINAL_BLOB.len(), SWAPPED_BLOB.len());

        let directory = TestDirectory::new("reopened-source-provider");
        let blob_segment = {
            let store = WritableRepository::open(&directory.path).expect("bootstrap writer");
            let previous = store.head();
            let write_generation = store.writing_generation().expect("write generation");
            let (head, child) = write_session_semantic_fixture(&store, write_generation);
            assert!(store.set_head(previous, head));
            store.close().expect("close blob-bearing source");
            child.segment
        };

        // Keep this repository open across the path replacement. It models the
        // complete provider captured before an actionable source is reopened.
        let stale_repository = Repository::open(&directory.path).expect("open original mapping");
        let source = stale_repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(blob_segment))
            .expect("archive containing blob segment");
        certify_active_archive(&stale_repository, source).expect("original source is certified");
        let source_name = source.file_name().to_owned();
        let source_path = directory.path.join(&source_name);
        let swap_name = format!("{source_name}.swapped");
        let swap_path = directory.path.join(&swap_name);

        // Rebuild a byte-valid archive with the same UUIDs, index layout,
        // generations, graph, and stale BRF. Only the inline blob identifier
        // changes, at equal length, so the sweep plan remains unchanged while
        // the segment-entry CRC is recomputed by the writer.
        let mut swapped_writer =
            TarArchiveWriter::new_exclusive_staged(&directory.path, &swap_name, &source_name);
        for catalog_generation in source
            .binary_references()
            .expect("source binary-reference catalog")
            .generations
        {
            let catalog_gc_generation = GarbageCollectionGeneration {
                generation: catalog_generation.generation,
                full_generation: catalog_generation.full_generation,
                is_compacted: catalog_generation.is_compacted,
            };
            for (identifier, references) in catalog_generation.segments {
                swapped_writer.add_binary_references(catalog_gc_generation, identifier, references);
            }
        }
        let mut entries = source.index().expect("source index").entries().to_vec();
        entries.sort_by_key(|entry| entry.position);
        let mut changed_blob = false;
        for entry in &entries {
            let identifier = entry.segment_identifier;
            let mut bytes = source
                .segment_data(identifier)
                .expect("indexed source payload")
                .to_vec();
            let structure = ParsedSegment::parse(identifier, &bytes).expect("source segment");
            if identifier == blob_segment {
                let external = structure
                    .record_table()
                    .iter()
                    .find(|record| record.record_type() == Some(RecordType::ExternalBlobIdentifier))
                    .expect("inline external-blob record");
                let position = structure
                    .buffer_position(external.offset)
                    .expect("external-blob record position");
                let encoded_length = u16::from_be_bytes([bytes[position], bytes[position + 1]]);
                assert_eq!(encoded_length & 0xF000, 0xE000);
                assert_eq!(usize::from(encoded_length & 0x0FFF), ORIGINAL_BLOB.len());
                assert_eq!(
                    &bytes[position + 2..position + 2 + ORIGINAL_BLOB.len()],
                    ORIGINAL_BLOB
                );
                bytes[position + 2..position + 2 + SWAPPED_BLOB.len()]
                    .copy_from_slice(SWAPPED_BLOB);
                changed_blob = true;
            }
            let changed_structure =
                ParsedSegment::parse(identifier, &bytes).expect("same-layout changed segment");
            swapped_writer
                .write_segment(
                    identifier,
                    &bytes,
                    GarbageCollectionGeneration {
                        generation: entry.generation,
                        full_generation: entry.full_generation,
                        is_compacted: entry.is_compacted,
                    },
                    &changed_structure.referenced_segments,
                    &[],
                )
                .expect("write changed source entry");
        }
        assert!(changed_blob, "the fixture must change one blob identifier");
        swapped_writer.close().expect("close changed source");
        let swapped_bytes = std::fs::read(&swap_path).expect("read changed source");
        std::fs::rename(&swap_path, &source_path).expect("replace source pathname");

        let reopened = TarArchiveReader::open(&source_path).expect("reopen changed source");
        certify_active_archive(&stale_repository, &reopened)
            .expect("the fixture demonstrates that the stale provider alone misses the change");

        let cleaned: HashSet<_> = source
            .segment_identifiers()
            .filter(|identifier| *identifier != blob_segment)
            .collect();
        assert!(
            !cleaned.is_empty(),
            "the fixture must request a partial rewrite"
        );
        assert!(matches!(
            plan_archive_sweep(&directory.path, source, &cleaned).expect("source sweep plan"),
            Some(PlannedArchiveSweep::Rewrite { .. })
        ));
        let mut fallback_provider = None;
        let error = sweep_one_archive(
            &directory.path,
            source,
            &cleaned,
            &cleaned,
            &[source],
            &mut fallback_provider,
            Some(&stale_repository),
        )
        .expect_err("fresh source payload must invalidate its stale BRF before publication");

        assert!(error.to_string().contains("catalog differs"), "{error}");
        assert_eq!(
            std::fs::read(&source_path).expect("changed source remains"),
            swapped_bytes
        );
        let parsed_name = ArchiveFileName::parse(&source_name).expect("source archive name");
        let next_generation = char::from(parsed_name.file_generation as u8 + 1);
        assert!(
            !directory
                .path
                .join(format!(
                    "data{:05}{next_generation}.tar",
                    parsed_name.archive_number
                ))
                .exists(),
            "no replacement may be published after the fresh certificate fails"
        );
    }

    #[test]
    fn reclaim_marks_session_archives_so_referenced_base_bulk_survives() {
        assert_session_reference_keeps_base_bulk_alive("session-mark", 2);
    }

    #[test]
    fn old_generation_session_segments_also_seed_bulk_reachability() {
        // Session archives are never swept, so even a session data
        // segment *below* the reference generation stays on disk — its
        // bulk references must be seeded too, or the retained segment
        // would dangle.
        assert_session_reference_keeps_base_bulk_alive("session-mark-old-gen", 0);
    }

    /// Builds a store whose base archives hold a bulk segment, persists
    /// one session data segment at `session_generation` referencing that
    /// bulk segment, reclaims at generation 2, and asserts the bulk
    /// segment survives.
    fn assert_session_reference_keeps_base_bulk_alive(name: &str, session_generation: i32) {
        let directory = TestDirectory::new(name);
        // Session A: a bulk-backed value, so the next session's base
        // archives hold a format-mandated (0, 0, false) bulk segment.
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            writer
                .write_string(&"bulk-payload ".repeat(25_000))
                .expect("large value");
            writer.finish().expect("finish");
            store.close().expect("close");
        }
        let bulk_identifier = {
            let repository = Repository::open(&directory.path).expect("reader");
            repository
                .segment_identifiers()
                .find(|identifier| identifier.is_bulk_segment())
                .expect("the large value produced a bulk segment")
        };

        // Session B: persist one data segment at `session_generation`
        // whose reference table names the pre-existing bulk segment,
        // then reclaim at generation 2. The session archive is outside
        // the base snapshot, so only the session-archive seeding can
        // protect the bulk segment.
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            let generation = GarbageCollectionGeneration {
                generation: session_generation,
                full_generation: session_generation,
                is_compacted: false,
            };
            let mut builder = SegmentBufferBuilder::new(
                crate::writer::identifier_generator::new_data_segment_identifier(),
                generation,
            );
            let record = builder
                .allocate(RecordType::Value, 6, &[bulk_identifier])
                .expect("fits");
            let reference = builder.reference_for(bulk_identifier);
            let mut identifier_bytes = [0u8; 6];
            SegmentBufferBuilder::write_record_identifier_bytes(
                reference,
                0,
                &mut identifier_bytes,
            );
            builder
                .record_bytes_mut(record)
                .copy_from_slice(&identifier_bytes);
            store.persist_segment(builder.finish()).expect("persist");
            let reference_generation = GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            };
            store
                .reclaim_old_generations(reference_generation, false)
                .expect("reclaim");
        }

        // The bulk segment must survive in some archive on disk: the
        // session data segment stays on disk and references it.
        let mut bulk_survives = false;
        for file_name in crate::store::list_archive_file_names(&directory.path).expect("list") {
            if let Ok(reader) = TarArchiveReader::open(&directory.path.join(&file_name))
                && reader.contains_segment(bulk_identifier)
            {
                bulk_survives = true;
            }
        }
        assert!(
            bulk_survives,
            "the session archive's reference must keep the base bulk segment alive"
        );
    }

    #[test]
    fn reclaim_ignores_unrelated_tar_files() {
        let directory = TestDirectory::new("unrelated-tar");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // A zero-byte file that matches the `.tar` suffix but not the Oak
        // archive name pattern must not break reclamation.
        std::fs::write(directory.path.join("notes.tar"), b"").expect("write unrelated file");
        let mut store = WritableRepository::open(&directory.path).expect("open");
        let generation = store.writing_generation().expect("generation");
        store
            .reclaim_old_generations(generation, false)
            .expect("reclaim ignores the unrelated file");
        store.close().expect("close");
        assert!(
            directory.path.join("notes.tar").exists(),
            "the unrelated file is left untouched"
        );
    }

    #[test]
    fn refuses_to_bootstrap_over_a_populated_store_with_no_resolvable_journal() {
        let directory = TestDirectory::new("refuse-bootstrap");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            crate::writer::commit::create_checkpoint(&store, 10_000_000, &[]).expect("checkpoint");
            store.close().expect("close");
        }
        std::fs::write(directory.path.join("journal.log"), b"").expect("truncate journal");

        assert!(
            WritableRepository::open(&directory.path).is_err(),
            "a populated store with no resolvable journal must not bootstrap an empty head"
        );

        // The refusal leaves the store intact; journal recovery restores
        // it and the write open then succeeds.
        crate::writer::backup::recover_journal(&directory.path).expect("recover");
        let store = WritableRepository::open(&directory.path).expect("open after recovery");
        store.close().expect("close");
    }

    #[test]
    fn flush_without_head_movement_syncs_segments_but_appends_no_journal_line() {
        let directory = TestDirectory::new("flush-pending");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        // Write a segment without moving the head, then flush: the
        // archive fsync must run (flush succeeds with a pending writer)
        // while the journal stays untouched.
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("node");
        writer.finish().expect("finish");
        store.flush().expect("flush with pending segments");
        let journal = std::fs::read_to_string(directory.path.join("journal.log")).expect("journal");
        assert_eq!(
            journal.lines().count(),
            1,
            "only the bootstrap line: an unchanged head appends nothing"
        );
        store.close().expect("close");
    }

    #[test]
    fn head_moving_flush_separates_an_unterminated_malformed_journal_tail() {
        let directory = TestDirectory::new("torn-journal-tail");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let journal_path = directory.path.join("journal.log");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open journal for simulated torn append")
            .write_all(b"malformed-unterminated-tail")
            .expect("append torn tail");

        let committed_head = {
            let store = WritableRepository::open(&directory.path).expect("bind before torn tail");
            crate::writer::commit::create_checkpoint(&store, 10_000_000, &[])
                .expect("head-moving checkpoint");
            let head = store.head();
            store.close().expect("close after checkpoint");
            head
        };

        let journal = std::fs::read(&journal_path).expect("read journal");
        assert!(
            journal
                .windows(b"malformed-unterminated-tail\n".len())
                .any(|window| window == b"malformed-unterminated-tail\n"),
            "the new durable revision must not be concatenated to a malformed tail"
        );
        let committed_prefix = format!(
            "{}:{} root ",
            committed_head.segment, committed_head.record_number
        );
        assert!(
            journal
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(committed_prefix.as_bytes())),
            "the exact committed head must occupy its own journal line"
        );

        let repository = Repository::open(&directory.path).expect("reopen healthy repository");
        assert_eq!(repository.head_record_identifier(), committed_head);
        repository
            .content_root()
            .expect("content root remains readable");
        assert_eq!(repository.checkpoints().expect("checkpoints").len(), 1);
    }

    #[test]
    fn prepared_flush_leaves_journal_unchanged_when_finalized_head_validation_fails() {
        let directory = TestDirectory::new("prepared-flush-validation-failure");
        let durable_head = {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            let head = store.head();
            store.close().expect("close bootstrap");
            head
        };
        let journal_path = directory.path.join("journal.log");
        let journal_before = std::fs::read(&journal_path).expect("journal before");

        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let valid_node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("node");
        writer.finish().expect("persist node");
        let invalid_head = RecordIdentifier::new(valid_node.segment, u32::MAX);
        assert!(store.set_head(durable_head, invalid_head));

        let error = store
            .flush()
            .expect_err("on-disk head validation must precede journal append");
        assert!(error.to_string().contains("not a finalized node record"));
        assert_eq!(
            std::fs::read(&journal_path).expect("journal after refusal"),
            journal_before,
            "archive finalization/validation failure may not expose a new journal revision"
        );
        let finalized = TarArchiveReader::open(&directory.path.join("data00001a.tar"))
            .expect("session archive was finalized before validation");
        assert!(!finalized.is_recovered());
        drop(store);
        drop(repository_lock);

        let repository = Repository::open(&directory.path).expect("old revision remains healthy");
        assert_eq!(repository.head_record_identifier(), durable_head);
        repository
            .content_root()
            .expect("durable root remains readable");
    }

    fn assert_prepared_session_trailer_omission_fails_closed(
        name: &str,
        omitted: OmittedSessionTrailer,
        expected_error: &str,
    ) {
        let directory = TestDirectory::new(name);
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let journal_path = directory.path.join("journal.log");
        let base_path = directory.path.join("data00000a.tar");
        let journal_before = std::fs::read(&journal_path).expect("journal before");
        let base_before = std::fs::read(&base_path).expect("base before");

        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let mut store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        store.maximum_archive_size = 1;
        let previous = store.head();
        let generation = store.writing_generation().expect("generation");
        let (head, child) = write_session_semantic_fixture(&store, generation);
        let target = match omitted {
            OmittedSessionTrailer::Graph => head.segment,
            OmittedSessionTrailer::BinaryReferences => child.segment,
        };
        rewrite_session_archive_omitting_trailer(&store, target, omitted);
        assert!(store.set_head(previous, head));

        let error = store
            .flush()
            .expect_err("semantic session certificate must precede journal append");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected validation error: {error}"
        );
        assert_eq!(
            std::fs::read(&journal_path).expect("journal after refusal"),
            journal_before,
            "a semantically incomplete session TAR cannot change the journal"
        );
        assert_eq!(
            std::fs::read(&base_path).expect("base after refusal"),
            base_before,
            "prepared validation failure cannot mutate base archives"
        );
        drop(store);
        drop(repository_lock);
    }

    #[test]
    fn prepared_flush_rejects_valid_checksum_session_tar_with_omitted_graph() {
        assert_prepared_session_trailer_omission_fails_closed(
            "prepared-session-missing-graph",
            OmittedSessionTrailer::Graph,
            "segment graph differs",
        );
    }

    #[test]
    fn prepared_flush_rejects_valid_checksum_session_tar_with_omitted_brf() {
        assert_prepared_session_trailer_omission_fails_closed(
            "prepared-session-missing-brf",
            OmittedSessionTrailer::BinaryReferences,
            "binary-reference catalog differs",
        );
    }

    #[test]
    fn prepared_flush_rejects_reordered_session_segments() {
        let directory = TestDirectory::new("prepared-session-reordered");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let journal_path = directory.path.join("journal.log");
        let base_path = directory.path.join("data00000a.tar");
        let journal_before = std::fs::read(&journal_path).expect("journal before");
        let base_before = std::fs::read(&base_path).expect("base before");

        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        let previous = store.head();
        let generation = store.writing_generation().expect("generation");
        let (head, child) = write_session_semantic_fixture(&store, generation);
        let (file_name, finished) = {
            let mut state = store.lock_write_state();
            let writer = state.tar_writer.take().expect("one open session archive");
            let file_name = writer
                .path()
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .expect("generated archive name")
                .to_owned();
            (file_name, writer)
        };
        store
            .close_archive_writer(finished)
            .expect("finalize original session archive");
        let recorded_order: Vec<_> = store
            .session_segment_writes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|write| (write.archive_file_name.clone(), write.identifier))
            .collect();
        assert_eq!(
            recorded_order,
            vec![
                (file_name.clone(), child.segment),
                (file_name.clone(), head.segment)
            ],
            "the fixture must put both segments in one archive in child-before-head order"
        );
        rewrite_session_archive_in_order(&store, &file_name, &[head.segment, child.segment]);
        assert!(store.set_head(previous, head));

        let error = store
            .flush()
            .expect_err("physical session order must be certified before journal append");
        assert!(
            error.to_string().contains("physical write order"),
            "unexpected validation error: {error}"
        );
        assert_eq!(
            std::fs::read(&journal_path).expect("journal after refusal"),
            journal_before
        );
        assert_eq!(
            std::fs::read(&base_path).expect("base after refusal"),
            base_before
        );
        drop(store);
        drop(repository_lock);
    }

    #[test]
    fn prepared_flush_rejects_changed_session_archive_boundaries() {
        let directory = TestDirectory::new("prepared-session-boundary-swap");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let journal_path = directory.path.join("journal.log");
        let base_path = directory.path.join("data00000a.tar");
        let journal_before = std::fs::read(&journal_path).expect("journal before");
        let base_before = std::fs::read(&base_path).expect("base before");

        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let mut store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        store.maximum_archive_size = 1;
        let previous = store.head();
        let generation = store.writing_generation().expect("generation");
        let (head, child) = write_session_semantic_fixture(&store, generation);
        let recorded_writes = store
            .session_segment_writes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(recorded_writes.len(), 2);
        assert_eq!(recorded_writes[0].identifier, child.segment);
        assert_eq!(recorded_writes[1].identifier, head.segment);
        assert_ne!(
            recorded_writes[0].archive_file_name, recorded_writes[1].archive_file_name,
            "the fixture must rotate between the two session segments"
        );

        let first = directory.path.join(&recorded_writes[0].archive_file_name);
        let second = directory.path.join(&recorded_writes[1].archive_file_name);
        let temporary = directory.path.join("session-boundary-swap.tmp");
        std::fs::rename(&first, &temporary).expect("move first archive aside");
        std::fs::rename(&second, &first).expect("move second into first boundary");
        std::fs::rename(&temporary, &second).expect("move first into second boundary");
        assert!(store.set_head(previous, head));

        let error = store
            .flush()
            .expect_err("session archive boundaries must be certified before journal append");
        assert!(
            error.to_string().contains("archive boundary"),
            "unexpected validation error: {error}"
        );
        assert_eq!(
            std::fs::read(&journal_path).expect("journal after refusal"),
            journal_before
        );
        assert_eq!(
            std::fs::read(&base_path).expect("base after refusal"),
            base_before
        );
        drop(store);
        drop(repository_lock);
    }

    #[test]
    fn prepared_session_validation_is_lazy_over_a_large_base() {
        const UNREFERENCED_BASE_SEGMENTS: u64 = 2_048;

        let directory = TestDirectory::new("prepared-session-lazy-provider");
        let durable_head = {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            let head = store.head();
            store.close().expect("close bootstrap");
            head
        };

        // These entries have valid TAR headers, payload checksums, and an
        // index, but are deliberately not parseable segment payloads. An
        // eager whole-repository provider fails on the first one; a lazy
        // provider must never inspect any because the new head cannot reach
        // them.
        let malformed = [0xFF];
        let malformed_generation = generation(0, 0, false);
        let mut large_base = TarArchiveWriter::new(&directory.path, "data00001a.tar");
        for seed in 10_000..10_000 + UNREFERENCED_BASE_SEGMENTS {
            large_base
                .write_segment(
                    data_identifier(seed),
                    &malformed,
                    malformed_generation,
                    &[],
                    &[],
                )
                .expect("write indexed malformed base segment");
        }
        large_base.close().expect("close large base archive");

        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let mut store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        store.maximum_archive_size = 1;
        let generation = store.writing_generation().expect("generation");
        let (head, child) = write_session_semantic_fixture(&store, generation);

        // Both rotated session TARs are finalized but the journal still
        // names the old head. Repository's location map nevertheless exposes
        // every active segment lazily, including these unjournaled writes.
        let fresh = Repository::open(&directory.path).expect("fresh lazy repository");
        assert_eq!(fresh.head_record_identifier(), durable_head);
        fresh
            .segment(head.segment)
            .expect("unjournaled finalized head segment is addressable");
        fresh
            .segment(child.segment)
            .expect("unjournaled finalized child segment is addressable");
        assert!(
            fresh.segment(data_identifier(10_000)).is_err(),
            "the base fixture must prove eager parsing would fail"
        );
        drop(fresh);

        assert!(store.set_head(durable_head, head));
        store
            .flush()
            .expect("lazy session certification ignores unreachable malformed base segments");
        drop(store);
        drop(repository_lock);

        let reopened = Repository::open(&directory.path).expect("reopen committed repository");
        assert_eq!(reopened.head_record_identifier(), head);
        reopened
            .content_root()
            .expect("new session head remains healthy");
    }

    fn assert_postcomp_session_trailer_omission_fails_closed(
        name: &str,
        omitted: OmittedSessionTrailer,
        expected_error: &str,
    ) {
        let directory = TestDirectory::new(name);
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let journal_path = directory.path.join("journal.log");
        let base_path = directory.path.join("data00000a.tar");
        let base_before = std::fs::read(&base_path).expect("base before");

        let mut store = WritableRepository::open(&directory.path).expect("compaction writer");
        store.maximum_archive_size = 1;
        let previous = store.head();
        let reference = generation(2, 2, true);
        let (head, child) = write_session_semantic_fixture(&store, reference);
        assert!(store.set_head(previous, head));
        store.flush().expect("commit compacted fixture head");
        let journal_before = std::fs::read(&journal_path).expect("committed journal");
        let target = match omitted {
            OmittedSessionTrailer::Graph => head.segment,
            OmittedSessionTrailer::BinaryReferences => child.segment,
        };
        rewrite_session_archive_omitting_trailer(&store, target, omitted);

        let error = store
            .reclaim_old_generations(reference, true)
            .expect_err("semantic session certificate must precede base mutation");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected validation error: {error}"
        );
        assert_eq!(
            std::fs::read(&journal_path).expect("journal after refusal"),
            journal_before,
            "post-compaction validation failure cannot rewrite the journal"
        );
        assert_eq!(
            std::fs::read(&base_path).expect("base after refusal"),
            base_before,
            "post-compaction validation failure must precede every base mutation"
        );
        assert!(!directory.path.join("data00000b.tar").exists());
    }

    #[test]
    fn postcomp_reclaim_rejects_valid_checksum_session_tar_with_omitted_graph() {
        assert_postcomp_session_trailer_omission_fails_closed(
            "postcomp-session-missing-graph",
            OmittedSessionTrailer::Graph,
            "segment graph differs",
        );
    }

    #[test]
    fn postcomp_reclaim_rejects_valid_checksum_session_tar_with_omitted_brf() {
        assert_postcomp_session_trailer_omission_fails_closed(
            "postcomp-session-missing-brf",
            OmittedSessionTrailer::BinaryReferences,
            "binary-reference catalog differs",
        );
    }

    #[test]
    fn postcomp_reclaim_runs_one_finalized_session_semantic_traversal() {
        use std::sync::atomic::Ordering;

        let directory = TestDirectory::new("postcomp-single-session-traversal");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let mut store = WritableRepository::open(&directory.path).expect("compaction writer");
        let reference = generation(2, 2, true);
        let previous = store.head();
        let (head, _) = write_session_semantic_fixture(&store, reference);
        assert!(store.set_head(previous, head));
        store.flush().expect("commit compacted fixture head");
        store
            .finalized_session_semantic_validations
            .store(0, Ordering::Relaxed);

        store
            .reclaim_old_generations(reference, true)
            .expect("reclaim succeeds");
        assert_eq!(
            store
                .finalized_session_semantic_validations
                .load(Ordering::Relaxed),
            1,
            "one descriptor-bound semantic certificate is sufficient under the held lock"
        );
    }

    #[test]
    fn prepared_head_moving_flush_runs_one_finalized_session_semantic_traversal() {
        use std::sync::atomic::Ordering;

        let directory = TestDirectory::new("prepared-flush-single-session-traversal");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        let previous = store.head();
        let generation = store.writing_generation().expect("write generation");
        let (head, _) = write_session_semantic_fixture(&store, generation);
        assert!(store.set_head(previous, head));
        store
            .finalized_session_semantic_validations
            .store(0, Ordering::Relaxed);

        store.flush().expect("commit prepared head");
        assert_eq!(
            store
                .finalized_session_semantic_validations
                .load(Ordering::Relaxed),
            1,
            "one full semantic traversal plus descriptor recertification is sufficient before journal visibility"
        );
        drop(store);
        drop(repository_lock);

        let repository = Repository::open(&directory.path).expect("reopen committed head");
        assert_eq!(repository.head_record_identifier(), head);
        repository
            .content_root()
            .expect("committed content remains readable");
    }

    #[cfg(unix)]
    #[test]
    fn prepared_rotated_archives_inherit_active_archive_metadata_before_commit() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = TestDirectory::new("prepared-archive-metadata");
        let previous_head = {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            let head = store.head();
            store.close().expect("close bootstrap");
            head
        };
        let source_path = directory.path.join("data00000a.tar");
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o640))
            .expect("set source mode");
        let source_metadata = std::fs::metadata(&source_path).expect("source metadata");

        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let mut store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        store.maximum_archive_size = 1;
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let content_root = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("content root");
        let new_head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("rotation finalizes archive");
        assert!(
            store.lock_write_state().tar_writer.is_none(),
            "the tiny threshold exercises the rotation close path"
        );
        assert!(store.set_head(previous_head, new_head));
        store.flush().expect("validate then commit prepared head");

        let created_metadata =
            std::fs::metadata(directory.path.join("data00001a.tar")).expect("created archive");
        assert_eq!(created_metadata.uid(), source_metadata.uid());
        assert_eq!(created_metadata.gid(), source_metadata.gid());
        assert_eq!(
            created_metadata.mode() & 0o7777,
            source_metadata.mode() & 0o7777
        );
        store.close().expect("close prepared writer");
        drop(repository_lock);

        let repository = Repository::open(&directory.path).expect("reopen committed store");
        assert_eq!(repository.head_record_identifier(), new_head);
        repository.content_root().expect("new root is traversable");
    }

    #[test]
    fn archive_number_exhaustion_never_wraps_or_truncates_archive_zero() {
        let directory = TestDirectory::new("archive-number-exhaustion");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let archive_zero = directory.path.join("data00000a.tar");
        let archive_max = directory.path.join("data4294967295a.tar");
        std::fs::copy(&archive_zero, &archive_max).expect("install maximum-number fixture");
        let zero_before = std::fs::read(&archive_zero).expect("archive zero before");
        let max_before = std::fs::read(&archive_max).expect("archive max before");
        let journal_before =
            std::fs::read(directory.path.join("journal.log")).expect("journal before");

        {
            let store = WritableRepository::open(&directory.path).expect("normal open at max");
            assert_eq!(store.lock_write_state().next_archive_number, None);
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("buffer node");
            let Err(error) = writer.finish() else {
                panic!("normal writer must refuse namespace wrap");
            };
            assert!(error.to_string().contains("namespace is exhausted"));
            store.close().expect("unchanged normal store closes");
        }

        let error = next_cleanup_archive_number(&directory.path)
            .expect_err("prepared cleanup planning must refuse namespace wrap");
        assert!(error.to_string().contains("namespace is exhausted"));

        assert_eq!(
            std::fs::read(&archive_zero).expect("archive zero after"),
            zero_before
        );
        assert_eq!(
            std::fs::read(&archive_max).expect("archive max after"),
            max_before
        );
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("journal after"),
            journal_before
        );
        assert!(!directory.path.join("data00001a.tar").exists());
    }

    #[test]
    fn prepared_writer_never_truncates_a_next_archive_occupied_after_open() {
        let directory = TestDirectory::new("prepared-next-archive-race");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let repository_lock =
            Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
        let store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
        assert_eq!(store.lock_write_state().next_archive_number, Some(1));

        let occupied_path = directory.path.join("data00001a.tar");
        let residue = b"interrupted writer recovery evidence";
        std::fs::write(&occupied_path, residue).expect("occupy planned next archive after open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("buffer node");
        let Err(error) = writer.finish() else {
            panic!("exclusive prepared writer must reject the occupied path");
        };
        assert!(error.to_string().contains("exists"));
        assert_eq!(
            std::fs::read(&occupied_path).expect("read occupied path"),
            residue,
            "neither the failed write nor later close may truncate or rewrite residue"
        );
        store.close().expect("unchanged prepared store closes");
        drop(repository_lock);
        assert_eq!(
            std::fs::read(occupied_path).expect("read residue after close"),
            residue
        );
    }

    #[test]
    fn prepared_open_rechecks_certified_archive_aliases_and_higher_numbers() {
        for (case, occupied_name, expected_error) in [
            (
                "prepared-certified-lettered-alias",
                "data00001a.tar",
                "output alias",
            ),
            (
                "prepared-certified-letterless-alias",
                "data00001.tar",
                "output alias",
            ),
            (
                "prepared-certified-higher-number",
                "data00002b.tar",
                "at or above",
            ),
        ] {
            let directory = TestDirectory::new(case);
            {
                let store = WritableRepository::open(&directory.path).expect("bootstrap");
                store.close().expect("close bootstrap");
            }
            let certified =
                next_cleanup_archive_number(&directory.path).expect("initial certificate");
            assert_eq!(certified, 1);
            let repository_lock =
                Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
            std::fs::write(directory.path.join(occupied_name), b"")
                .expect("occupy namespace after certification");

            let Err(error) = WritableRepository::open_prepared(
                &directory.path,
                Arc::clone(&repository_lock),
                certified,
            ) else {
                panic!("strict prepared open must reject {occupied_name}");
            };
            assert!(error.to_string().contains(expected_error), "{error}");
        }
    }

    #[test]
    fn post_compaction_reclaim_validates_finalized_session_head_before_base_mutation() {
        let directory = TestDirectory::new("postcomp-finalized-head-ordering");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let base_path = directory.path.join("data00000a.tar");
        let base_before = std::fs::read(&base_path).expect("base before");

        let mut store = WritableRepository::open(&directory.path).expect("open for compaction");
        let reference = generation(2, 2, true);
        let mut writer = store.record_writer(reference);
        let valid_node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("compacted node");
        writer.finish().expect("persist compacted node");
        let invalid_head = RecordIdentifier::new(valid_node.segment, u32::MAX);
        assert!(store.set_head(store.head(), invalid_head));
        store
            .flush()
            .expect("normal commit exposes the deliberately invalid test head");

        let error = store
            .reclaim_old_generations(reference, true)
            .expect_err("finalized head validation must precede base sweep");
        assert!(error.to_string().contains("not a finalized node record"));
        assert_eq!(
            std::fs::read(&base_path).expect("base after refusal"),
            base_before,
            "no base archive may be deleted or rewritten before exact-head validation"
        );
        assert!(!directory.path.join("data00000b.tar").exists());
    }

    #[test]
    fn post_compaction_reclaim_certifies_base_payload_before_mutation() {
        let directory = TestDirectory::new("postcomp-base-source-certificate");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let base_path = directory.path.join("data00000a.tar");
        let repository = Repository::open(&directory.path).expect("open healthy base");
        let head = repository.head_record_identifier();
        let entry = *repository
            .archives()
            .iter()
            .find_map(|archive| archive.index_entry(head.segment))
            .expect("head index entry");
        drop(repository);
        let mut corrupt_base = std::fs::read(&base_path).expect("read base");
        corrupt_base[entry.position as usize + entry.size as usize - 1] ^= 0x01;
        std::fs::write(&base_path, &corrupt_base).expect("corrupt base payload CRC");
        let journal_before =
            std::fs::read(directory.path.join("journal.log")).expect("journal before");

        let mut store =
            WritableRepository::open(&directory.path).expect("open corrupt-indexed base");
        let error = store
            .reclaim_old_generations(generation(2, 2, true), true)
            .expect_err("base source certificate must precede post-compaction sweeping");

        assert!(error.to_string().contains("payload CRC"), "{error}");
        assert_eq!(
            std::fs::read(&base_path).expect("base after refusal"),
            corrupt_base,
            "post-compaction certification must not rewrite its corrupt source"
        );
        assert_eq!(
            std::fs::read(directory.path.join("journal.log")).expect("journal after refusal"),
            journal_before,
            "post-compaction certification must not change the journal"
        );
        assert!(!directory.path.join("data00000b.tar").exists());
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
            assert!(store.set_head(previous, head), "compare and set succeeds");
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
    fn flushing_without_head_movement_writes_no_journal_line() {
        let directory = TestDirectory::new("no-movement");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.flush().expect("first flush");
        store.flush().expect("second flush");
        store.close().expect("close");
        let journal = std::fs::read_to_string(directory.path.join("journal.log")).expect("journal");
        assert_eq!(journal.lines().count(), 1);
    }

    #[test]
    fn stale_generation_letters_are_deleted_at_write_open() {
        let directory = TestDirectory::new("stale-letters");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // Fabricate a stale lower letter alongside the valid archive by
        // copying it: the write open must keep the higher letter and
        // delete the lower one.
        let valid = std::fs::read(directory.path.join("data00000a.tar")).expect("read");
        std::fs::write(directory.path.join("data00000b.tar"), &valid).expect("write copy");
        {
            let store = WritableRepository::open(&directory.path).expect("reopen");
            assert!(store.head().record_number > 0 || store.head().record_number == 0);
            store.close().expect("close");
        }
        assert!(
            !directory.path.join("data00000a.tar").exists(),
            "the lower letter is deleted"
        );
        assert!(directory.path.join("data00000b.tar").exists());
    }

    #[test]
    fn archives_without_an_index_are_recovered_with_backups() {
        let directory = TestDirectory::new("write-recovery");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // Truncate the archive's trailers, leaving only entry data.
        let path = directory.path.join("data00000a.tar");
        let full = std::fs::read(&path).expect("read");
        // Find the first trailer: the '.brf' entry header.
        let trailer_start = full
            .windows(4)
            .position(|window| window == b".brf")
            .map(|position| (position / 512) * 512)
            .expect("brf trailer present");
        let mut truncated = full[..trailer_start].to_vec();
        truncated.extend_from_slice(&[0u8; 1024]);
        std::fs::write(&path, &truncated).expect("truncate");

        {
            let store = WritableRepository::open(&directory.path).expect("recovering open");
            let head = store.head();
            assert!(
                store.segment(head.segment).is_ok(),
                "head segment recovered"
            );
            store.close().expect("close");
        }
        assert!(
            directory.path.join("data00000a.tar.bak").exists(),
            "the damaged archive is backed up"
        );
        let repository = Repository::open(&directory.path).expect("reader opens");
        assert!(
            !repository
                .archives()
                .iter()
                .any(crate::tar_archive::archive::TarArchiveReader::is_recovered),
            "the regenerated archive has a valid index"
        );
        repository.content_root().expect("content root resolves");
    }

    /// An empty archive file is what a writer killed inside its own lazy
    /// next-archive creation leaves behind — the read path has always
    /// skipped it. Before this was mirrored here, the number fell to
    /// `recover_archive_number`, which rebuilt it as an archive with no
    /// entries; `TarArchiveWriter` never creates a file for that, so the
    /// re-open failed on a missing path and every froe write command
    /// reported a bare `No such file or directory`.
    #[test]
    fn an_empty_archive_file_does_not_break_opening_for_writing() {
        let directory = TestDirectory::new("empty-archive-open");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        let empty = directory.path.join("data00009a.tar");
        std::fs::write(&empty, b"").expect("create the empty archive");

        let store = WritableRepository::open(&directory.path).expect("write open must succeed");
        let head = store.head();
        assert!(store.segment(head.segment).is_ok(), "head still resolves");
        store.close().expect("close");

        Repository::open(&directory.path).expect("the reader still opens");
    }

    /// The empty number contributes no archive, so nothing is deleted as a
    /// side effect of opening. Reuse is the only other outcome, and it can
    /// only ever overwrite zero bytes.
    #[test]
    fn an_empty_archive_file_is_never_deleted_by_opening_for_writing() {
        let directory = TestDirectory::new("empty-archive-retained");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // A number above the next one froe would allocate, so the open
        // cannot reach it by filling it.
        let empty = directory.path.join("data00500a.tar");
        std::fs::write(&empty, b"").expect("create the empty archive");

        let store = WritableRepository::open(&directory.path).expect("write open");
        store.close().expect("close");

        assert!(
            empty.exists(),
            "opening for writing must not delete the empty archive; cleanup removes it \
             under its own plan-and-confirm contract"
        );
    }

    /// Skipping an all-empty archive number must not free it for reuse: the
    /// letterless spelling of a number collides with the lettered one, and
    /// `group_file_generations_newest_first` refuses that pair outright, so
    /// a store that allocated into it could never be opened again by
    /// anything. Allocation therefore reads the physical namespace.
    #[test]
    fn an_empty_archive_number_is_never_reallocated_over_its_own_residue() {
        let directory = TestDirectory::new("empty-archive-namespace");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // Letterless: `ArchiveFileName::parse` reads this as number 1,
        // generation 'a' — the same pair a written `data00001a.tar` claims.
        std::fs::write(directory.path.join("data00001.tar"), b"").expect("empty residue");

        {
            let store = WritableRepository::open(&directory.path).expect("write open");
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            writer.write_string("forces a new archive").expect("string");
            writer.finish().expect("finish");
            store.close().expect("close");
        }

        assert!(
            !directory.path.join("data00001a.tar").exists(),
            "allocation must skip the number the letterless residue claims"
        );
        Repository::open(&directory.path).expect("the store is still openable");
        WritableRepository::open(&directory.path)
            .expect("and still writable")
            .close()
            .expect("close");
    }

    /// A non-empty letter that yields no recoverable segment is residue
    /// `recover_archive_number` cannot act on. It must say so, not surface
    /// the `ENOENT` of the replacement it declined to write.
    #[test]
    fn an_unrecoverable_archive_is_refused_with_its_file_name() {
        let directory = TestDirectory::new("unrecoverable-archive");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        // 512-byte blocks that are neither a valid index nor a parseable
        // tar entry: the scan recovers nothing from them.
        let junk = directory.path.join("data00009a.tar");
        std::fs::write(&junk, vec![0x5au8; 4096]).expect("write junk archive");

        let message = match WritableRepository::open(&directory.path) {
            Ok(_) => panic!("opening for writing must refuse an unrecoverable archive"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("data00009a.tar"),
            "the refusal names the unusable file: {message}"
        );
        assert!(
            message.contains("no recoverable segment"),
            "the refusal states why: {message}"
        );
        assert!(
            !message.contains("No such file or directory"),
            "the refusal must not surface a bare errno: {message}"
        );
        assert!(junk.exists(), "the refusal leaves the file in place");
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
