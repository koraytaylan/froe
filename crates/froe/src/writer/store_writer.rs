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
        // initial-node bootstrap for a fresh store.
        let journal_entries = read_journal(&directory.join("journal.log")).unwrap_or_default();
        let persisted = journal_entries
            .iter()
            .filter_map(JournalEntry::record_identifier)
            .find(|identifier| store.contains_segment(identifier.segment));
        if let Some(head) = persisted {
            let mut state = store.lock_write_state();
            state.head = head;
            state.persisted_head = Some(head);
        } else {
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
            if let Some(name) = entry.file_name().to_str() {
                if ArchiveFileName::parse(name).is_some() {
                    total += entry.metadata()?.len();
                }
            }
        }
        Ok(total)
    }

    /// Reclaims segments older than `reference_generation` after a
    /// compaction, using Oak's reclaim predicate with a single retained
    /// generation. `full` selects the full-compaction predicate; a base
    /// archive whose segments all reclaim is deleted, one with survivors
    /// is rewritten to the next generation letter with only the
    /// survivors.
    ///
    /// This is safe only when every record reachable from the current
    /// head lives in `reference_generation` — which compaction's deep
    /// copy guarantees.
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

        // Drop the memory maps before deleting or rewriting the files.
        let base_archives = std::mem::take(&mut self.base_archives);
        let archive_files: Vec<ArchiveFileName> = base_archives
            .iter()
            .filter_map(|archive| ArchiveFileName::parse(archive.file_name()))
            .collect();
        drop(base_archives);
        self.parsed_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        for archive_name in archive_files {
            self.reclaim_one_archive(&archive_name, reference_generation, full)?;
        }
        // Make the archive deletions and any swept replacements durable
        // before the caller proceeds to the journal rewrite.
        crate::writer::compaction::fsync_directory(&self.directory);
        Ok(())
    }

    /// Applies the reclaim predicate to one base archive.
    fn reclaim_one_archive(
        &self,
        archive_name: &ArchiveFileName,
        reference: GarbageCollectionGeneration,
        full: bool,
    ) -> Result<()> {
        let path = self.directory.join(&archive_name.file_name);
        let reader = TarArchiveReader::open(&path)?;
        let mut survivors: Vec<(SegmentIdentifier, Vec<u8>)> = Vec::new();
        for identifier in reader.segment_identifiers() {
            let generation = archive_segment_generation(&reader, identifier)?;
            if !is_reclaimable(reference, generation, full) {
                if let Some(bytes) = reader.segment_data(identifier) {
                    survivors.push((identifier, bytes.to_vec()));
                }
            }
        }
        let survivor_count = survivors.len();
        drop(reader);

        if survivor_count == 0 {
            std::fs::remove_file(&path)?;
            return Ok(());
        }
        // A file already at generation `z` is never rewritten (Oak stops
        // garbage collection at `z`); leave its survivors in place.
        if archive_name.file_generation >= 'z' {
            return Ok(());
        }
        // Rewrite the survivors to the next generation letter, then delete
        // the original — matching Oak's sweep.
        let next_letter = char::from(archive_name.file_generation as u8 + 1);
        let swept_name = format!("data{:05}{next_letter}.tar", archive_name.archive_number);
        let mut writer = TarArchiveWriter::new(&self.directory, &swept_name);
        for (identifier, bytes) in &survivors {
            let (generation, references, binary_references) = if identifier.is_data_segment() {
                let parsed = ParsedSegment::parse(*identifier, bytes)?;
                let binary_references = read_segment_blob_identifiers(&parsed, bytes)?;
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
        std::fs::remove_file(&path)?;
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
        if length >= self.maximum_archive_size {
            if let Some(finished) = state.tar_writer.take() {
                finished.close()?;
            }
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
    /// journal line, fdatasynced.
    pub fn flush(&self) -> Result<()> {
        let mut state = self.lock_write_state();
        if state.persisted_head == Some(state.head) {
            return Ok(());
        }
        if let Some(tar_writer) = &mut state.tar_writer {
            tar_writer.flush()?;
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

/// The generation of a segment in an archive: from its index entry when
/// present, otherwise parsed from the segment header.
fn archive_segment_generation(
    reader: &TarArchiveReader,
    identifier: SegmentIdentifier,
) -> Result<GarbageCollectionGeneration> {
    if let Some(entry) = reader.index_entry(identifier) {
        return Ok(GarbageCollectionGeneration {
            generation: entry.generation,
            full_generation: entry.full_generation,
            is_compacted: entry.is_compacted,
        });
    }
    if identifier.is_bulk_segment() {
        return Ok(GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        });
    }
    let bytes = reader
        .segment_data(identifier)
        .ok_or(Error::SegmentNotFound {
            segment_identifier: identifier,
        })?;
    let parsed = ParsedSegment::parse(identifier, bytes)?;
    Ok(GarbageCollectionGeneration {
        generation: parsed.generation,
        full_generation: parsed.full_generation,
        is_compacted: parsed.is_compacted,
    })
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
        if let Ok(name) = name.into_string() {
            if name.ends_with(".tar") {
                file_names.push(name);
            }
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
/// ascending order (later letters overwrite duplicates), renames the
/// originals to `.bak` names, and rewrites the recovered segments as a
/// fresh archive under the lowest letter's file name.
fn recover_archive_number(
    directory: &Path,
    generations: &[ArchiveFileName],
) -> Result<TarArchiveReader> {
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
        std::fs::rename(&path, backup_path(directory, &generation.file_name))?;
    }

    let target_name = &generations[0].file_name;
    let mut writer = TarArchiveWriter::new(directory, target_name);
    for (identifier, bytes) in &recovered {
        let (generation, references, binary_references) = if identifier.is_data_segment() {
            let parsed = ParsedSegment::parse(*identifier, bytes)?;
            let references = parsed.referenced_segments.clone();
            let binary_references = read_segment_blob_identifiers(&parsed, bytes)?;
            (
                GarbageCollectionGeneration {
                    generation: parsed.generation,
                    full_generation: parsed.full_generation,
                    is_compacted: parsed.is_compacted,
                },
                references,
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
    TarArchiveReader::open(&directory.join(target_name))
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

/// Extracts every external blob identifier recorded in a segment, for
/// the archive's binary references catalog. Large blob identifiers may
/// reference a string record in another segment; those cannot be read
/// here and recovery preserves only same-segment identifiers, which is
/// what a raw segment scan can see.
pub(crate) fn read_segment_blob_identifiers(
    parsed: &ParsedSegment,
    bytes: &[u8],
) -> Result<Vec<String>> {
    let mut identifiers = Vec::new();
    for entry in parsed.record_table() {
        if entry.record_type != RecordType::ExternalBlobIdentifier {
            continue;
        }
        let position = parsed.buffer_position(entry.offset)?;
        if position >= bytes.len() {
            continue;
        }
        let head = bytes[position];
        if head & 0xF0 == 0xE0 && position + 2 <= bytes.len() {
            let length = ((usize::from(head) & 0x0F) << 8) | usize::from(bytes[position + 1]);
            if position + 2 + length <= bytes.len() {
                identifiers.push(
                    String::from_utf8_lossy(&bytes[position + 2..position + 2 + length])
                        .into_owned(),
                );
            }
        }
    }
    Ok(identifiers)
}

#[cfg(test)]
mod tests {
    use super::WritableRepository;
    use crate::content::provider::SegmentProvider;
    use crate::store::Repository;
    use crate::writer::record_writer::ChildNodesToWrite;

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
