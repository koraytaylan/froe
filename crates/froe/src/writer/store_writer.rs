//! The writable repository: Oak's read-write file store lifecycle.
//!
//! Opening follows the normative order of the Java `FileStore`: the
//! journal handle first, then the exclusive repository lock, then the
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

/// The mutable write-side state, serialized behind one mutex.
struct WriteState {
    journal_file: File,
    tar_writer: Option<TarArchiveWriter>,
    next_archive_number: u32,
    head: RecordIdentifier,
    persisted_head: Option<RecordIdentifier>,
}

/// A read-write segment store session holding the repository lock.
pub struct WritableRepository {
    directory: PathBuf,
    _repository_lock: RepositoryLock,
    maximum_archive_size: u64,
    /// Archives that existed before this session, newest first.
    base_archives: Vec<TarArchiveReader>,
    /// Segments written in this session, servable without a mapping.
    session_segments: RwLock<HashMap<SegmentIdentifier, SharedSegment>>,
    parsed_segment_cache: RwLock<HashMap<SegmentIdentifier, Arc<ParsedSegment>>>,
    write_state: Mutex<WriteState>,
}

impl WritableRepository {
    /// Opens (or bootstraps) a segment store for writing.
    pub fn open(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;

        // Journal handle first, then the lock — the Java open order.
        let journal_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("journal.log"))?;
        let repository_lock = RepositoryLock::acquire(directory)?;

        // Writing needs collision-safe segment identifiers; without the
        // operating system entropy source there is no safe way to write.
        crate::writer::identifier_generator::verify_entropy_source()?;

        check_and_update_manifest(directory)?;

        let base_archives = initialize_archives_for_writing(directory)?;
        let next_archive_number = base_archives
            .iter()
            .filter_map(|archive| ArchiveFileName::parse(archive.file_name()))
            .map(|name| name.archive_number + 1)
            .max()
            .unwrap_or(0);

        let store = Self {
            directory: directory.to_owned(),
            _repository_lock: repository_lock,
            maximum_archive_size: DEFAULT_MAXIMUM_ARCHIVE_SIZE,
            base_archives,
            session_segments: RwLock::new(HashMap::new()),
            parsed_segment_cache: RwLock::new(HashMap::new()),
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
    pub fn reclaim_old_generations(
        &mut self,
        reference_generation: GarbageCollectionGeneration,
        full: bool,
    ) -> Result<()> {
        // Finalize the session archive so its new-generation segments are
        // complete on disk before old archives are removed.
        {
            let mut state = self.lock_write_state();
            if let Some(tar_writer) = state.tar_writer.take() {
                tar_writer.close()?;
            }
        }

        // The taken readers drive both the mark and the sweep. Files are
        // unlinked while other archives' maps are still open, which is
        // fine on Unix (the write path already requires a Unix entropy
        // source).
        let base_archives = std::mem::take(&mut self.base_archives);
        self.parsed_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        // Archives this session wrote (now closed and complete on disk):
        // newer than every base archive, so they open the mark. Their
        // data segments carry the reference generation and are retained,
        // seeding the references set with every bulk segment they point
        // at — including pre-existing bulk segments in base archives,
        // which the empty seed alone would miss.
        let base_names: std::collections::HashSet<&str> = base_archives
            .iter()
            .map(TarArchiveReader::file_name)
            .collect();
        let mut session_archives = Vec::new();
        for file_name in crate::store::list_archive_file_names(&self.directory)? {
            if !base_names.contains(file_name.as_str()) {
                session_archives.push(TarArchiveReader::open(&self.directory.join(&file_name))?);
            }
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
            // Mark only: session archives are never swept, so their
            // reclaim sets are discarded.
            let _ = mark_one_archive(archive, reference_generation, full, &mut references)?;
        }
        drop(session_archives);
        let mut reclaim_sets = Vec::with_capacity(base_archives.len());
        for archive in &base_archives {
            reclaim_sets.push(mark_one_archive(
                archive,
                reference_generation,
                full,
                &mut references,
            )?);
        }

        // Store-wide fallback provider for catalog reconstruction, built
        // only if some swept archive turns out to have no readable
        // catalog.
        let mut fallback_provider: Option<ArchiveSegmentsProvider<'_>> = None;
        for (archive, cleaned) in base_archives.iter().zip(&reclaim_sets) {
            sweep_one_archive(
                &self.directory,
                archive,
                cleaned,
                &base_archives,
                &mut fallback_provider,
            )?;
        }
        drop(fallback_provider);
        drop(base_archives);
        // Make the archive deletions and any swept replacements durable
        // before the caller proceeds to the journal rewrite.
        crate::writer::compaction::fsync_directory(&self.directory);
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
        if state.tar_writer.is_none() {
            let archive_number = state.next_archive_number;
            state.next_archive_number += 1;
            state.tar_writer = Some(TarArchiveWriter::new(
                &self.directory,
                &format!("data{archive_number:05}a.tar"),
            ));
        }
        let tar_writer = state
            .tar_writer
            .as_mut()
            .ok_or_else(|| Error::InvalidFormat {
                details: "the archive writer disappeared while locked".to_owned(),
            })?;
        let tar_generation = if segment.identifier.is_bulk_segment() {
            GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            }
        } else {
            segment.generation
        };
        let length = tar_writer.write_segment(
            segment.identifier,
            &segment.bytes,
            tar_generation,
            &segment.referenced_segments,
            &segment.binary_reference_identifiers,
        )?;
        if length >= self.maximum_archive_size
            && let Some(finished) = state.tar_writer.take()
        {
            finished.close()?;
        }
        drop(state);

        self.session_segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(segment.identifier, (structure, Arc::new(segment.bytes)));
        Ok(())
    }

    /// Flushes with Oak's durability ordering: archive fsync first, then
    /// — only when the head moved since the last flush — one appended
    /// journal line, fdatasynced. Pending segment bytes are forced to
    /// disk even when the head is unchanged, exactly like Java's
    /// `flush()`; only the journal line is conditional.
    pub fn flush(&self) -> Result<()> {
        let mut state = self.lock_write_state();
        if let Some(tar_writer) = &mut state.tar_writer {
            tar_writer.flush()?;
        }
        if state.persisted_head == Some(state.head) {
            return Ok(());
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let head = state.head;
        let line = format!(
            "{}:{} root {timestamp}\n",
            head.segment, head.record_number as i32
        );
        state.journal_file.write_all(line.as_bytes())?;
        state.journal_file.sync_data()?;
        state.persisted_head = Some(head);
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
            tar_writer.close()?;
        }
        drop(state);
        crate::writer::compaction::fsync_directory(&self.directory);
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

/// The reclaim predicate for one segment, Oak's `newOldReclaimer` with a
/// single retained generation. `reference` is the generation the
/// compaction produced; `segment` is a candidate segment's generation.
/// Oak's `TarReader.mark` for one archive: entries are visited in
/// *reverse* file order, so a bulk segment — always written before the
/// data segments referencing it — is judged after all of them. Data
/// segments are judged by the generation predicate alone; bulk segments
/// purely by membership in the shared `references` set (`remove` both
/// queries and consumes, exactly like Java). Every *kept* data segment
/// protects the bulk segments it references — through the graph trailer
/// when present, else the segment header's reference list — following
/// only bulk targets, Java's `shouldFollow`. Returns the archive's
/// reclaimable set.
fn mark_one_archive(
    reader: &TarArchiveReader,
    reference: GarbageCollectionGeneration,
    full: bool,
    references: &mut std::collections::HashSet<SegmentIdentifier>,
) -> Result<std::collections::HashSet<SegmentIdentifier>> {
    let mut reclaimable = std::collections::HashSet::new();
    let Some(index) = reader.index() else {
        // No valid index: the sweep leaves such an archive untouched, so
        // nothing of it is reclaimable.
        return Ok(reclaimable);
    };
    let mut entries: Vec<_> = index.entries().to_vec();
    entries.sort_by_key(|entry| entry.position);

    let graph_adjacency: Option<HashMap<SegmentIdentifier, Vec<SegmentIdentifier>>> = reader
        .segment_graph()
        .map(|graph| graph.adjacency.into_iter().collect());

    for entry in entries.iter().rev() {
        let identifier = entry.segment_identifier;
        let was_referenced = references.remove(&identifier);
        let reclaim = if identifier.is_data_segment() {
            let generation = GarbageCollectionGeneration {
                generation: entry.generation,
                full_generation: entry.full_generation,
                is_compacted: entry.is_compacted,
            };
            is_reclaimable(reference, generation, full)
        } else {
            !was_referenced
        };
        if reclaim {
            reclaimable.insert(identifier);
        } else if identifier.is_data_segment() {
            let targets = match &graph_adjacency {
                Some(adjacency) => adjacency.get(&identifier).cloned().unwrap_or_default(),
                None => match reader.segment_data(identifier) {
                    Some(bytes) => ParsedSegment::parse(identifier, bytes)?.referenced_segments,
                    None => Vec::new(),
                },
            };
            for target in targets {
                if target.is_bulk_segment() {
                    references.insert(target);
                }
            }
        }
    }
    Ok(reclaimable)
}

/// Oak's `TarReader.sweep` for one base archive, with a precomputed
/// reclaim set from the mark phase: entries are judged and rewritten in
/// original file-position order, the generation triple comes from the
/// index entry, sub-25% savings keep the file untouched, and the graph
/// and binary-references trailers are *filtered* from the existing ones,
/// never recomputed — a raw segment scan cannot see every catalog entry,
/// and dropping one would let AEM's blob garbage collection delete a
/// still-referenced binary.
fn sweep_one_archive<'archives>(
    directory: &Path,
    reader: &'archives TarArchiveReader,
    cleaned: &std::collections::HashSet<SegmentIdentifier>,
    all_archives: &'archives [TarArchiveReader],
    fallback_provider: &mut Option<ArchiveSegmentsProvider<'archives>>,
) -> Result<()> {
    let Some(archive_name) = ArchiveFileName::parse(reader.file_name()) else {
        return Ok(());
    };
    let path = directory.join(&archive_name.file_name);
    let Some(index) = reader.index() else {
        // Archives are recovered (rewritten with an index) at open, so a
        // base archive always has one; leave it untouched if a later
        // corruption made it unreadable anyway.
        return Ok(());
    };

    // Partition the entries in file-position order, accumulating Oak's
    // sweep arithmetic (`i64` cannot wrap where Java's `int` could not
    // either: entries are position-bounded below 2 GiB).
    let mut entries: Vec<_> = index.entries().to_vec();
    entries.sort_by_key(|entry| entry.position);
    let mut survivors = Vec::new();
    let mut size_before: i64 = 0;
    let mut size_after: i64 = 0;
    for entry in entries {
        let entry_size = 512
            + i64::from(entry.size)
            + crate::writer::tar_writer::padding_size(entry.size as usize) as i64;
        size_before += entry_size;
        if !cleaned.contains(&entry.segment_identifier) {
            size_after += entry_size;
            survivors.push(entry);
        }
    }

    if survivors.is_empty() {
        // Deletion failures are never fatal, matching Oak's retrying
        // FileReaper: both letters coexisting is a state the next open
        // resolves safely (newest valid index wins).
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    // Oak's savings gate: below 25% reclaimed, the file is kept as-is
    // rather than rewritten.
    if size_after >= size_before * 3 / 4 {
        return Ok(());
    }
    // A file already at generation `z` is never rewritten (Oak stops
    // garbage collection at `z`); leave its survivors in place.
    if archive_name.file_generation >= 'z' {
        return Ok(());
    }

    let trailers = FilteredTrailers::from_archive(reader, cleaned);
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

    // Rewrite the survivors to the next generation letter, then delete
    // the original — matching Oak's sweep.
    let next_letter = char::from(archive_name.file_generation as u8 + 1);
    let swept_name = format!("data{:05}{next_letter}.tar", archive_name.archive_number);
    let mut writer = TarArchiveWriter::new(directory, &swept_name);
    if let Some(catalog_entries) = &trailers.catalog {
        for (generation, segment, references) in catalog_entries {
            writer.add_binary_references(*generation, *segment, references.iter().cloned());
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
            trailers.for_segment(identifier, bytes, cleaned, scan_provider)?
        } else {
            (Vec::new(), Vec::new())
        };
        writer.write_segment(
            identifier,
            bytes,
            generation,
            &references,
            &binary_references,
        )?;
    }
    writer.close()?;
    // The swept file and its directory entry must be durable, and the
    // swept file must re-open with a valid index, *before* the original
    // is deleted — a crash in between must never leave both generations
    // unusable, and a bad rewrite must never destroy the only good copy.
    crate::writer::compaction::fsync_directory(directory);
    let swept_path = directory.join(&swept_name);
    let swept_is_valid =
        TarArchiveReader::open(&swept_path).is_ok_and(|swept| !swept.is_recovered());
    if swept_is_valid {
        // Deletion failures are never fatal, matching Oak's retrying
        // FileReaper: both letters coexisting is resolved safely at the
        // next open (newest valid index wins). The original stays mapped
        // by its reader until the reclaim finishes — unlinking a mapped
        // file is safe on Unix.
        let _ = std::fs::remove_file(&path);
    } else {
        // Keep the original untouched; discard the bad rewrite, as Java
        // falls back to the original reader on a failed re-open.
        let _ = std::fs::remove_file(&swept_path);
    }
    Ok(())
}

fn is_reclaimable(
    reference: GarbageCollectionGeneration,
    segment: GarbageCollectionGeneration,
    full: bool,
) -> bool {
    const RETAINED: i32 = 1;
    // Wrapping subtraction matches Java's `GCGeneration.compareWith`, which
    // uses plain int subtraction; it also cannot panic on the pathological
    // generation values a corrupt archive index might carry.
    if full {
        reference
            .full_generation
            .wrapping_sub(segment.full_generation)
            >= RETAINED
            || (reference.generation.wrapping_sub(segment.generation) >= RETAINED
                && !segment.is_compacted)
    } else {
        reference.generation.wrapping_sub(segment.generation) >= RETAINED
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
fn initialize_archives_for_writing(directory: &Path) -> Result<Vec<TarArchiveReader>> {
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

    let mut archives = Vec::new();
    for (_, mut generations) in by_number {
        generations.sort_by_key(|name| name.file_generation);
        let mut winner: Option<TarArchiveReader> = None;
        // Newest letter first: the first valid index wins.
        for candidate in generations.iter().rev() {
            let path = directory.join(&candidate.file_name);
            match TarArchiveReader::open(&path) {
                Ok(reader) if !reader.is_recovered() => {
                    winner = Some(reader);
                    break;
                }
                _ => {}
            }
        }
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
            None => {
                archives.push(recover_archive_number(directory, &generations)?);
            }
        }
    }
    // Newest number first: the probe order for reads.
    archives.reverse();
    Ok(archives)
}

/// Recovers one archive number with no valid index: scans every letter in
/// ascending order (later letters overwrite duplicates), rebuilds the
/// recovered segments as a fresh archive, and only after that archive is
/// written, fsynced, and re-validated are the originals retired to
/// `.bak` names and the replacement installed under the lowest letter's
/// file name. A failure before installation leaves every original in
/// place; a failure during installation rolls back best-effort (see
/// [`install_recovered_archive`]).
fn recover_archive_number(
    directory: &Path,
    generations: &[ArchiveFileName],
) -> Result<TarArchiveReader> {
    let recovered = scan_recoverable_segments(directory, generations);

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
    let target_name = &generations[0].file_name;
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
    install_recovered_archive(directory, generations, target_name, &temporary_path)
}

/// Scans every generation letter of one archive number in ascending
/// order — later letters overwrite duplicate segments — returning the
/// recovered segments in scan order.
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
        cleaned: &std::collections::HashSet<SegmentIdentifier>,
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
                        .filter(|target| !cleaned.contains(target))
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
                    if !cleaned.contains(&segment) {
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
        cleaned: &std::collections::HashSet<SegmentIdentifier>,
        scan_provider: Option<&ArchiveSegmentsProvider<'_>>,
    ) -> Result<(Vec<SegmentIdentifier>, Vec<String>)> {
        let references = match self.graph_by_source.get(&identifier) {
            Some(filtered) => filtered.clone(),
            None if !self.graph_present => ParsedSegment::parse(identifier, bytes)?
                .referenced_segments
                .iter()
                .filter(|target| !cleaned.contains(target))
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
/// archive first; a segment duplicated across archives resolves to the
/// newest copy, the repository's lookup contract.
fn archive_segments_provider(readers: &[TarArchiveReader]) -> Result<ArchiveSegmentsProvider<'_>> {
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

/// Extracts every external blob identifier recorded in one segment,
/// resolving large (`0xF0`-class) identifiers through `provider`. Fails
/// when any identifier cannot be resolved: a rebuilt catalog missing an
/// entry would let AEM's blob garbage collection delete a binary that is
/// still referenced, so callers that *publish* the catalog must fail
/// closed instead.
fn read_blob_identifiers(
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
    use super::{WritableRepository, archive_segments_provider};
    use crate::content::provider::SegmentProvider;
    use crate::segment::record::RecordType;
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::record_writer::ChildNodesToWrite;
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
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
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
        let readers = vec![
            TarArchiveReader::open(&directory.path.join("data00001a.tar")).expect("open newest"),
            TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open oldest"),
        ];
        let provider = archive_segments_provider(&readers).expect("provider");
        let view = provider.segment(bulk).expect("duplicate resolves");
        assert_eq!(
            &view.bytes[..],
            b"new-archive-copy",
            "a duplicated segment resolves to the newest archive's copy"
        );
    }

    #[test]
    fn reclaim_marks_session_archives_so_referenced_base_bulk_survives() {
        let directory = TestDirectory::new("session-mark");
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

        // Session B: persist one generation-2 data segment whose
        // reference table names the pre-existing bulk segment, then
        // reclaim at generation 2. The session archive is outside the
        // base snapshot, so only the session-archive mark can protect
        // the bulk segment.
        {
            let mut store = WritableRepository::open(&directory.path).expect("open");
            let generation_two = GarbageCollectionGeneration {
                generation: 2,
                full_generation: 2,
                is_compacted: false,
            };
            let mut builder = SegmentBufferBuilder::new(
                crate::writer::identifier_generator::new_data_segment_identifier(),
                generation_two,
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
            store
                .reclaim_old_generations(generation_two, false)
                .expect("reclaim");
        }

        // The bulk segment must survive in some archive on disk: the
        // retained session data segment references it.
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
