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
//! can safely open a live repository or a backup. It implements
//! [`SegmentProvider`] with bounded caches for parsed segments, strings,
//! and templates — the hot metadata of any traversal.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::content::node::NodeState;
use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::journal::{JournalEntry, read_journal};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::RecordIdentifier;
use crate::segment::view::SegmentView;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::select_newest_file_generations;

/// The highest store version this reader understands
/// (`store.version` in the manifest; 2 since Oak 1.8).
const MAXIMUM_STORE_VERSION: i64 = 2;

/// Bounded cache capacities. Parsed segment structures are the largest
/// entries (their record tables), so their cap is the smallest.
const SEGMENT_CACHE_CAPACITY: usize = 4096;
const STRING_CACHE_CAPACITY: usize = 65_536;
const TEMPLATE_CACHE_CAPACITY: usize = 16_384;

/// A read-only segment store repository.
pub struct Repository {
    directory: PathBuf,
    /// Open archives, newest archive number first — the probe order for
    /// segments duplicated across archives.
    archives: Vec<TarArchiveReader>,
    /// Segment identifier to position in [`Self::archives`], resolved
    /// newest-archive-wins.
    segment_locations: HashMap<SegmentIdentifier, usize>,
    /// Journal entries, newest first.
    journal_entries: Vec<JournalEntry>,
    /// The resolved head: the record identifier of the super-root node.
    head_record_identifier: RecordIdentifier,
    parsed_segment_cache: RwLock<BoundedCache<SegmentIdentifier, Arc<ParsedSegment>>>,
    string_cache: RwLock<BoundedCache<RecordIdentifier, Arc<str>>>,
    template_cache: RwLock<BoundedCache<RecordIdentifier, Arc<Template>>>,
}

impl Repository {
    /// Opens the segment store in `directory` read-only.
    pub fn open(directory: &Path) -> Result<Self> {
        if !directory.is_dir() {
            return Err(Error::InvalidFormat {
                details: format!("{} is not a directory", directory.display()),
            });
        }

        let archive_file_names = list_archive_file_names(directory)?;
        check_manifest(directory, !archive_file_names.is_empty())?;

        let selected = select_newest_file_generations(&archive_file_names)?;
        let mut archives = Vec::with_capacity(selected.len());
        for archive_file_name in &selected {
            archives.push(TarArchiveReader::open(
                &directory.join(&archive_file_name.file_name),
            )?);
        }

        let mut segment_locations = HashMap::new();
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
            parsed_segment_cache: RwLock::new(BoundedCache::new(SEGMENT_CACHE_CAPACITY)),
            string_cache: RwLock::new(BoundedCache::new(STRING_CACHE_CAPACITY)),
            template_cache: RwLock::new(BoundedCache::new(TEMPLATE_CACHE_CAPACITY)),
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

    /// All segment identifiers, in archive probe order.
    pub fn segment_identifiers(&self) -> impl Iterator<Item = SegmentIdentifier> + '_ {
        self.archives
            .iter()
            .flat_map(TarArchiveReader::segment_identifiers)
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
        let archive_position = *self
            .segment_locations
            .get(&segment_identifier)
            .ok_or(Error::SegmentNotFound { segment_identifier })?;
        let bytes = self.archives[archive_position]
            .segment_data(segment_identifier)
            .ok_or(Error::SegmentNotFound { segment_identifier })?;

        if let Some(structure) = self
            .parsed_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&segment_identifier)
        {
            return Ok(SegmentView {
                structure,
                bytes: bytes.into(),
            });
        }
        let structure = Arc::new(ParsedSegment::parse(segment_identifier, bytes)?);
        self.parsed_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(segment_identifier, Arc::clone(&structure));
        Ok(SegmentView {
            structure,
            bytes: bytes.into(),
        })
    }

    fn string(&self, record_identifier: RecordIdentifier) -> Result<Arc<str>> {
        if let Some(cached) = self
            .string_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&record_identifier)
        {
            return Ok(cached);
        }
        let value: Arc<str> = Arc::from(read_string(self, record_identifier)?);
        self.string_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(record_identifier, Arc::clone(&value));
        Ok(value)
    }

    fn template(&self, record_identifier: RecordIdentifier) -> Result<Arc<Template>> {
        if let Some(cached) = self
            .template_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&record_identifier)
        {
            return Ok(cached);
        }
        let template = Arc::new(read_template(self, record_identifier)?);
        self.template_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(record_identifier, Arc::clone(&template));
        Ok(template)
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

/// A read-only provider over a set of archives, without a journal or
/// resolved head. Used by journal recovery, which must read segments
/// before any head exists.
pub struct ArchiveSet {
    archives: Vec<TarArchiveReader>,
    segment_locations: HashMap<SegmentIdentifier, usize>,
    parsed_segment_cache: RwLock<BoundedCache<SegmentIdentifier, Arc<ParsedSegment>>>,
}

impl ArchiveSet {
    /// Wraps a set of already-opened archives.
    #[must_use]
    pub fn new(archives: Vec<TarArchiveReader>) -> Self {
        let mut segment_locations = HashMap::new();
        for (position, archive) in archives.iter().enumerate() {
            for identifier in archive.segment_identifiers() {
                segment_locations.entry(identifier).or_insert(position);
            }
        }
        Self {
            archives,
            segment_locations,
            parsed_segment_cache: RwLock::new(BoundedCache::new(SEGMENT_CACHE_CAPACITY)),
        }
    }

    /// Every segment identifier across the archives.
    pub fn segment_identifiers(&self) -> impl Iterator<Item = SegmentIdentifier> + '_ {
        self.archives
            .iter()
            .flat_map(TarArchiveReader::segment_identifiers)
    }
}

impl SegmentProvider for ArchiveSet {
    fn segment(&self, segment_identifier: SegmentIdentifier) -> Result<SegmentView<'_>> {
        let archive_position = *self
            .segment_locations
            .get(&segment_identifier)
            .ok_or(Error::SegmentNotFound { segment_identifier })?;
        let bytes = self.archives[archive_position]
            .segment_data(segment_identifier)
            .ok_or(Error::SegmentNotFound { segment_identifier })?;
        if let Some(structure) = self
            .parsed_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&segment_identifier)
        {
            return Ok(SegmentView {
                structure,
                bytes: bytes.into(),
            });
        }
        let structure = Arc::new(ParsedSegment::parse(segment_identifier, bytes)?);
        self.parsed_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(segment_identifier, Arc::clone(&structure));
        Ok(SegmentView {
            structure,
            bytes: bytes.into(),
        })
    }

    fn string(&self, record_identifier: RecordIdentifier) -> Result<Arc<str>> {
        read_string(self, record_identifier).map(Arc::from)
    }

    fn template(&self, record_identifier: RecordIdentifier) -> Result<Arc<Template>> {
        read_template(self, record_identifier).map(Arc::new)
    }
}

/// Opens every archive in a directory read-only, selecting the newest
/// generation letter of each archive number. Unlike [`Repository::open`]
/// this needs no journal or manifest, so recovery can read segments
/// before a head exists.
pub fn open_all_archives(directory: &Path) -> Result<Vec<TarArchiveReader>> {
    let file_names = list_archive_file_names(directory)?;
    let selected = select_newest_file_generations(&file_names)?;
    let mut archives = Vec::with_capacity(selected.len());
    for archive_file_name in &selected {
        archives.push(TarArchiveReader::open(
            &directory.join(&archive_file_name.file_name),
        )?);
    }
    Ok(archives)
}

/// Lists the file names ending in `.tar` in the repository directory.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the Java reader filters archives with a case-sensitive \".tar\" suffix"
)]
fn list_archive_file_names(directory: &Path) -> Result<Vec<String>> {
    let mut file_names = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let file_name = entry?.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.ends_with(".tar") {
            file_names.push(file_name.to_owned());
        }
    }
    Ok(file_names)
}

/// Validates the `manifest` file with read-only semantics: never writes,
/// accepts store versions 1 and 2, and rejects a store that has archives
/// but no manifest (that is the legacy pre-tar format).
pub(crate) fn check_manifest(directory: &Path, archives_exist: bool) -> Result<()> {
    let manifest_path = directory.join("manifest");
    if !manifest_path.exists() {
        if archives_exist {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} has segment archives but no manifest; this is the legacy \
                     oak-segment format, not segment-tar",
                    directory.display()
                ),
            });
        }
        return Ok(());
    }
    let store_version = read_manifest_store_version(&manifest_path)?;
    if store_version <= 0 {
        return Err(Error::InvalidFormat {
            details: format!("invalid store version {store_version} in manifest"),
        });
    }
    if store_version > MAXIMUM_STORE_VERSION {
        return Err(Error::InvalidFormat {
            details: format!(
                "store version {store_version} is newer than this reader supports \
                 (up to {MAXIMUM_STORE_VERSION})"
            ),
        });
    }
    Ok(())
}

/// Reads the `store.version` key from the manifest, a Java properties
/// file. Only the subset of the properties syntax that this file uses is
/// implemented: comments, blank lines, and `key=value` / `key:value` /
/// `key value` pairs without escape sequences — including Java's rule
/// that whitespace after the key may be followed by one optional `=` or
/// `:` before the value. The version is parsed as a Java `int`; an
/// absent or unparseable value defaults to the maximum supported
/// version, like the Java reader.
fn read_manifest_store_version(manifest_path: &Path) -> Result<i64> {
    let content = std::fs::read_to_string(manifest_path)?;
    for line in content.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let Some(separator) = line.find(['=', ':', ' ', '\t']) else {
            continue;
        };
        if line[..separator].trim() != "store.version" {
            continue;
        }
        let mut value = &line[separator + 1..];
        if line.as_bytes()[separator] == b' ' || line.as_bytes()[separator] == b'\t' {
            // Java properties: whitespace after the key, then one
            // optional '=' or ':', then the value.
            let after_whitespace = value.trim_start_matches([' ', '\t']);
            value = after_whitespace
                .strip_prefix(['=', ':'])
                .unwrap_or(after_whitespace);
        }
        let parsed: i64 = value
            .trim()
            .parse::<i32>()
            .map_or(MAXIMUM_STORE_VERSION, i64::from);
        return Ok(parsed);
    }
    Ok(MAXIMUM_STORE_VERSION)
}

/// A cache bounded by entry count with first-in-first-out eviction.
///
/// Parsed segment structures, strings, and templates are all cheap to
/// re-create from the memory-mapped archives, so simple eviction beats
/// the bookkeeping cost of recency tracking here.
struct BoundedCache<Key, Value> {
    entries: HashMap<Key, Value>,
    insertion_order: VecDeque<Key>,
    capacity: usize,
}

impl<Key: Eq + Hash + Clone, Value: Clone> BoundedCache<Key, Value> {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity.min(1024)),
            insertion_order: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
        }
    }

    fn get(&self, key: &Key) -> Option<Value> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: Key, value: Value) {
        if self.entries.insert(key.clone(), value).is_none() {
            self.insertion_order.push_back(key);
            while self.insertion_order.len() > self.capacity {
                if let Some(oldest) = self.insertion_order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BoundedCache;

    #[test]
    fn bounded_cache_evicts_oldest_entries() {
        let mut cache: BoundedCache<u32, u32> = BoundedCache::new(2);
        cache.insert(1, 10);
        cache.insert(2, 20);
        assert_eq!(cache.get(&1), Some(10));
        cache.insert(3, 30);
        assert_eq!(cache.get(&1), None, "the oldest entry is evicted");
        assert_eq!(cache.get(&2), Some(20));
        assert_eq!(cache.get(&3), Some(30));
    }

    #[test]
    fn bounded_cache_reinsertion_does_not_duplicate() {
        let mut cache: BoundedCache<u32, u32> = BoundedCache::new(2);
        cache.insert(1, 10);
        cache.insert(1, 11);
        cache.insert(2, 20);
        assert_eq!(cache.get(&1), Some(11), "reinsertion updates the value");
        cache.insert(3, 30);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.get(&2), Some(20));
    }
}
