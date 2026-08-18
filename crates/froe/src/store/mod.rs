//! The repository: opening a segment store directory read-only.
//!
//! Opening follows the read-only path of Oak's file store:
//!
//! 1. validate the `manifest` (a store without one but with archives is
//!    the legacy pre-tar format and is rejected; versions above 2 are
//!    newer than this reader);
//! 2. discover the `data*.tar` archives, keeping only the highest file
//!    generation letter of each archive number, and open them all —
//!    archives without a valid index (such as the one a live repository
//!    is currently writing) are recovered by scanning in memory;
//! 3. scan `journal.log` backwards for the newest revision whose segment
//!    exists, which becomes the head.
//!
//! The repository never takes the repository lock and never writes, so it
//! can safely open a live repository or a backup (like Oak it memory-maps
//! archives, relying on the store's never-modify-in-place file protocol —
//! see [`crate::tar_archive::archive`]). It implements
//! [`SegmentProvider`] with bounded caches for parsed segments, strings,
//! and templates — the hot metadata of any traversal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::cache::{BoundedCache, CacheWeight};
use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::journal::{JournalEntry, read_journal};
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;
use crate::segment::view::SegmentView;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::group_file_generations_newest_first;

mod archives;
mod manifest;

pub use archives::*;
pub(crate) use manifest::*;

/// Cache budgets, in bytes.
///
/// Bytes rather than entries: a parsed segment's resident size follows the
/// number of records the segment happens to hold, which spans two orders of
/// magnitude across real stores. The previous entry caps held about 120 MB
/// of parsed segments on a typical AEM store and about 1.4 GB on a dense
/// one — the same configuration, an order of magnitude apart. These figures
/// are what the process actually holds, whatever the segments look like.
pub(crate) const SEGMENT_CACHE_BUDGET_BYTES: usize = 192 * 1024 * 1024;

pub(crate) const STRING_CACHE_BUDGET_BYTES: usize = 48 * 1024 * 1024;

pub(crate) const TEMPLATE_CACHE_BUDGET_BYTES: usize = 48 * 1024 * 1024;

/// A read-only segment store repository.
pub struct Repository {
    pub(crate) directory: PathBuf,
    /// Open archives, newest archive number first — the probe order for
    /// segments duplicated across archives.
    pub(crate) archives: Vec<TarArchiveReader>,
    /// Segment identifier to position in [`Self::archives`], resolved
    /// newest-archive-wins.
    pub(crate) segment_locations: HashMap<SegmentIdentifier, usize>,
    /// Journal entries, newest first.
    pub(crate) journal_entries: Vec<JournalEntry>,
    /// The resolved head: the record identifier of the super-root node.
    pub(crate) head_record_identifier: RecordIdentifier,
    pub(crate) parsed_segment_cache: RwLock<BoundedCache<SegmentIdentifier, Arc<ParsedSegment>>>,
    pub(crate) string_cache: RwLock<BoundedCache<RecordIdentifier, Arc<str>>>,
    pub(crate) template_cache: RwLock<BoundedCache<RecordIdentifier, Arc<Template>>>,
}

impl Repository {
    /// Opens the segment store in `directory` read-only.
    pub fn open(directory: &Path) -> Result<Self> {
        Self::open_with_progress(directory, &mut DiscardedProgress)
    }

    /// Opens the segment store in `directory` read-only, reporting the
    /// archive scan — the part that takes real time on a large store — to
    /// `observer`.
    pub fn open_with_progress(
        directory: &Path,
        observer: &mut dyn ProgressObserver,
    ) -> Result<Self> {
        if !directory.is_dir() {
            return Err(Error::InvalidFormat {
                details: format!("{} is not a directory", directory.display()),
            });
        }

        let archive_file_names = list_archive_file_names(directory)?;
        check_manifest(directory, ArchivePresence::of(&archive_file_names))?;

        let archives = open_archives_newest_valid_first(directory, &archive_file_names, observer)?;

        // Reserved up front from the archives' own entry counts. Growing a
        // map of this size by doubling holds the old and new tables at once
        // at every rehash, so the peak was half again the steady size for a
        // total that is knowable before the first insert.
        let expected_segments: usize = archives.iter().map(TarArchiveReader::segment_count).sum();
        let mut segment_locations = HashMap::with_capacity(expected_segments);
        for (archive_position, archive) in archives.iter().enumerate() {
            for segment_identifier in archive.segment_identifiers() {
                segment_locations
                    .entry(segment_identifier)
                    .or_insert(archive_position);
            }
        }

        let journal_entries = read_journal(&directory.join("journal.log"))?;
        let head_record_identifier = journal_entries
            .iter()
            .filter_map(JournalEntry::record_identifier)
            .find(|identifier| segment_locations.contains_key(&identifier.segment))
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "no journal revision in {} references an existing segment; \
                     cannot open a read-only store from an empty journal",
                    directory.display()
                ),
            })?;

        Ok(Self {
            directory: directory.to_owned(),
            archives,
            segment_locations,
            journal_entries,
            head_record_identifier,
            parsed_segment_cache: RwLock::new(BoundedCache::new(SEGMENT_CACHE_BUDGET_BYTES)),
            string_cache: RwLock::new(BoundedCache::new(STRING_CACHE_BUDGET_BYTES)),
            template_cache: RwLock::new(BoundedCache::new(TEMPLATE_CACHE_BUDGET_BYTES)),
        })
    }

    /// The directory this repository was opened from.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The open archives, newest archive number first.
    #[must_use]
    pub fn archives(&self) -> &[TarArchiveReader] {
        &self.archives
    }

    /// The journal entries, newest first.
    #[must_use]
    pub fn journal_entries(&self) -> &[JournalEntry] {
        &self.journal_entries
    }

    /// The record identifier of the current head (the super-root node).
    #[must_use]
    pub fn head_record_identifier(&self) -> RecordIdentifier {
        self.head_record_identifier
    }

    /// The number of segments across all archives (duplicates counted
    /// once).
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segment_locations.len()
    }

    /// Whether any archive contains the given segment.
    #[must_use]
    pub fn contains_segment(&self, segment_identifier: SegmentIdentifier) -> bool {
        self.segment_locations.contains_key(&segment_identifier)
    }

    /// All segment identifiers, in archive probe order. A segment served by
    /// two archives is yielded once per archive.
    pub fn segment_identifiers(&self) -> impl Iterator<Item = SegmentIdentifier> + '_ {
        self.archives
            .iter()
            .flat_map(TarArchiveReader::segment_identifiers)
    }

    /// Every segment identifier exactly once, in archive probe order.
    ///
    /// A store-wide scan that must not process a segment twice uses this
    /// rather than accumulating its own seen-set: the location map that
    /// settles duplicates is already built, so the deduplication is free
    /// where the caller's would have been one entry per segment.
    pub fn distinct_segment_identifiers(&self) -> impl Iterator<Item = SegmentIdentifier> + '_ {
        self.archives
            .iter()
            .enumerate()
            .flat_map(move |(position, archive)| {
                archive.segment_identifiers().filter(move |identifier| {
                    self.segment_locations.get(identifier) == Some(&position)
                })
            })
    }

    /// Returns the exact stored bytes of a segment from the active archive
    /// selection without parsing its header or record tables.
    ///
    /// This is the read-only escape hatch used by last-resort diagnostics:
    /// callers can still inspect a segment whose structure is corrupt enough
    /// that [`SegmentProvider::segment`] cannot construct a [`SegmentView`].
    pub fn segment_bytes(&self, segment_identifier: SegmentIdentifier) -> Result<&[u8]> {
        let archive_position = *self
            .segment_locations
            .get(&segment_identifier)
            .ok_or(Error::SegmentNotFound { segment_identifier })?;
        self.archives[archive_position]
            .segment_data(segment_identifier)
            .ok_or(Error::SegmentNotFound { segment_identifier })
    }

    /// The node state at an arbitrary node record.
    #[must_use]
    pub fn node(&self, record_identifier: RecordIdentifier) -> NodeState<'_> {
        NodeState::new(self, record_identifier)
    }

    /// The head node state: the *super-root*, whose children are `root`
    /// (the content tree) and, when checkpoints exist, `checkpoints`.
    #[must_use]
    pub fn head(&self) -> NodeState<'_> {
        self.node(self.head_record_identifier)
    }

    /// The content root — the node JCR paths are relative to.
    pub fn content_root(&self) -> Result<NodeState<'_>> {
        self.head()
            .child_node("root")?
            .ok_or_else(|| Error::InvalidFormat {
                details: "the super-root has no \"root\" child node".to_owned(),
            })
    }

    /// Resolves a content path such as `/content/dam` from the content
    /// root. `/` resolves to the content root itself.
    pub fn node_at_path(&self, path: &str) -> Result<Option<NodeState<'_>>> {
        let mut current = self.content_root()?;
        for name in path.split('/').filter(|name| !name.is_empty()) {
            match current.child_node(name)? {
                Some(child) => current = child,
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }

    /// The repository's checkpoints as `(name, checkpoint node)` pairs.
    /// Each checkpoint node has `created` and `timestamp` properties, a
    /// `properties` child with caller-supplied metadata, and a `root`
    /// child holding the content snapshot.
    pub fn checkpoints(&self) -> Result<Vec<(String, NodeState<'_>)>> {
        match self.head().child_node("checkpoints")? {
            None => Ok(Vec::new()),
            Some(checkpoints) => checkpoints.child_node_entries(),
        }
    }
}

impl SegmentProvider for Repository {
    fn segment(&self, segment_identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        let bytes = self.segment_bytes(segment_identifier)?;
        let structure =
            load_through_cache(&self.parsed_segment_cache, &segment_identifier, || {
                ParsedSegment::parse(segment_identifier, bytes).map(Arc::new)
            })?;
        Ok(SegmentView {
            structure,
            bytes: bytes.into(),
        })
    }

    fn string(&self, record_identifier: RecordIdentifier) -> Result<Arc<str>> {
        load_through_cache(&self.string_cache, &record_identifier, || {
            read_string(self, record_identifier).map(Arc::from)
        })
    }

    fn template(&self, record_identifier: RecordIdentifier) -> Result<Arc<Template>> {
        load_through_cache(&self.template_cache, &record_identifier, || {
            read_template(self, record_identifier).map(Arc::new)
        })
    }
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Repository({}, {} archives, {} segments)",
            self.directory.display(),
            self.archives.len(),
            self.segment_locations.len()
        )
    }
}

/// Loads `key` from `cache`, filling a miss from `load` without holding the
/// lock across the derivation — a miss re-reads mapped bytes, never waits
/// on another thread's decode of a different key.
pub(crate) fn load_through_cache<Key, Value>(
    cache: &RwLock<BoundedCache<Key, Value>>,
    key: &Key,
    load: impl FnOnce() -> Result<Value>,
) -> Result<Value>
where
    Key: Clone + Eq + std::hash::Hash,
    Value: Clone + CacheWeight,
{
    if let Some(cached) = cache
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
    {
        return Ok(cached);
    }
    let value = load()?;
    cache
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.clone(), value.clone());
    Ok(value)
}
