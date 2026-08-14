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

use crate::cache::BoundedCache;
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

/// The highest store version this reader understands
/// (`store.version` in the manifest; 2 since Oak 1.8).
const MAXIMUM_STORE_VERSION: i64 = 2;

/// Cache budgets, in bytes.
///
/// Bytes rather than entries: a parsed segment's resident size follows the
/// number of records the segment happens to hold, which spans two orders of
/// magnitude across real stores. The previous entry caps held about 120 MB
/// of parsed segments on a typical AEM store and about 1.4 GB on a dense
/// one — the same configuration, an order of magnitude apart. These figures
/// are what the process actually holds, whatever the segments look like.
pub(crate) const SEGMENT_CACHE_BUDGET_BYTES: usize = 192 * 1024 * 1024;
const STRING_CACHE_BUDGET_BYTES: usize = 48 * 1024 * 1024;
const TEMPLATE_CACHE_BUDGET_BYTES: usize = 48 * 1024 * 1024;

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
        check_manifest(directory, !archive_file_names.is_empty())?;

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
        let expected_segments: usize = archives.iter().map(TarArchiveReader::segment_count).sum();
        let mut segment_locations = HashMap::with_capacity(expected_segments);
        for (position, archive) in archives.iter().enumerate() {
            for identifier in archive.segment_identifiers() {
                segment_locations.entry(identifier).or_insert(position);
            }
        }
        Self {
            archives,
            segment_locations,
            parsed_segment_cache: RwLock::new(BoundedCache::new(SEGMENT_CACHE_BUDGET_BYTES)),
        }
    }

    /// Every segment identifier across the archives, duplicates included:
    /// one segment served by two archives is yielded once per archive.
    pub fn segment_identifiers(&self) -> impl Iterator<Item = SegmentIdentifier> + '_ {
        self.archives
            .iter()
            .flat_map(TarArchiveReader::segment_identifiers)
    }

    /// Every segment identifier exactly once, in archive probe order.
    ///
    /// A scan over the whole store that must not process a segment twice can
    /// use this instead of accumulating its own seen-set. The location map
    /// that decides which archive owns a duplicate is already built, so
    /// deduplicating here costs nothing, where the caller would have paid
    /// per-segment — or worse, per-record — for the same answer.
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

    /// How many identifiers [`ArchiveSet::segment_identifiers`] yields.
    /// A segment present in more than one archive is counted once per
    /// archive, exactly as the iterator yields it, so this is the total a
    /// progress report over that iteration counts up to — not the number
    /// of distinct segments.
    #[must_use]
    pub fn segment_identifier_count(&self) -> usize {
        self.archives
            .iter()
            .map(TarArchiveReader::segment_count)
            .sum()
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
/// this needs no journal, so recovery can read segments before a head
/// exists — but the manifest is validated exactly like Java's read-only
/// store open: a legacy or newer-versioned store gets the categorical
/// refusal, never segment-level parse noise.
pub fn open_all_archives(directory: &Path) -> Result<Vec<TarArchiveReader>> {
    open_all_archives_with_progress(directory, &mut DiscardedProgress)
}

/// Opens every archive in a directory read-only, exactly like
/// [`open_all_archives`], reporting the scan to `observer`.
pub fn open_all_archives_with_progress(
    directory: &Path,
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<TarArchiveReader>> {
    let file_names = list_archive_file_names(directory)?;
    check_manifest(directory, !file_names.is_empty())?;
    open_archives_newest_valid_first(directory, &file_names, observer)
}

/// Opens one archive per number read-only: the highest generation letter
/// whose *index is valid* wins — a partial next-letter file left by an
/// interrupted sweep must never shadow the complete previous letter
/// beside it. This is Java's *read-write* selection rule (its read-only
/// open considers only the highest letter and recover-scans it); froe
/// deliberately uses the stricter rule on the read side too, as it never
/// serves fewer segments. When no letter of a number has a valid index,
/// the recovered in-memory views of every letter are served, newest
/// letter first, so all segments stay reachable through the probe order.
/// Zero-length files are skipped: a live writer creates its next archive
/// lazily, and the empty file is that creation's race window.
fn open_archives_newest_valid_first(
    directory: &Path,
    file_names: &[String],
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<TarArchiveReader>> {
    let groups = group_file_generations_newest_first(file_names)?;
    crate::progress::observe(
        observer,
        &Step::new("opening archives", WorkUnit::Archives)
            .with_total(crate::progress::count(groups.len())),
        |observer| open_archive_groups(directory, groups, observer),
    )
}

/// Opens the winning archive of each generation group, reporting one
/// completed archive at a time.
fn open_archive_groups(
    directory: &Path,
    groups: Vec<Vec<crate::tar_archive::file_name::ArchiveFileName>>,
    observer: &mut dyn ProgressObserver,
) -> Result<Vec<TarArchiveReader>> {
    let mut archives = Vec::new();
    let group_count = groups.len();
    for (opened, group) in groups.into_iter().enumerate() {
        // Items *completed*: the archive being opened is not one of them
        // until its turn ends, so a one-archive store does not sit at
        // 100% for the whole open.
        observer.step_advanced(crate::progress::count(opened));
        let mut recovered: Vec<TarArchiveReader> = Vec::new();
        let mut winner: Option<TarArchiveReader> = None;
        let mut first_error: Option<Error> = None;
        for candidate in &group {
            let path = directory.join(&candidate.file_name);
            if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == 0) {
                continue;
            }
            match TarArchiveReader::open(&path) {
                Ok(reader) if !reader.is_recovered() => {
                    winner = Some(reader);
                    break;
                }
                Ok(reader) => recovered.push(reader),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(winner) = winner {
            archives.push(winner);
        } else if !recovered.is_empty() {
            archives.extend(recovered);
        } else if let Some(error) = first_error {
            observer.step_advanced(crate::progress::count(opened));
            return Err(error);
        }
    }
    observer.step_advanced(crate::progress::count(group_count));
    Ok(archives)
}

/// Lists the file names ending in `.tar` in the repository directory.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the Java reader filters archives with a case-sensitive \".tar\" suffix"
)]
pub(crate) fn list_archive_file_names(directory: &Path) -> Result<Vec<String>> {
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

/// Reads the `store.version` key from the manifest, a Java properties file.
/// Logical-line continuation, separators, and character escapes follow
/// `Properties.load(Reader)`, including its last-duplicate-wins behavior. The
/// version is parsed as a Java `int`; an absent or unparseable value defaults
/// to the maximum supported version, like the Java reader.
pub(crate) fn read_manifest_store_version(manifest_path: &Path) -> Result<i64> {
    let content = std::fs::read_to_string(manifest_path)?;
    parse_manifest_store_version(&content)
}

fn parse_manifest_store_version(content: &str) -> Result<i64> {
    let mut version = MAXIMUM_STORE_VERSION;
    let store_version_key = java_property_ascii("store.version");
    for (key, value) in parse_java_properties(content)? {
        if key != store_version_key {
            continue;
        }
        version = parse_java_i32(&value).map_or(MAXIMUM_STORE_VERSION, i64::from);
    }
    Ok(version)
}

/// Decodes the key/value entries produced by `Properties.load(Reader)`. Java
/// strings are represented as UTF-16 code units so even escaped surrogate code
/// units can be retained without manufacturing invalid Rust `char`s.
fn parse_java_properties(content: &str) -> Result<Vec<(Vec<u16>, Vec<u16>)>> {
    let mut properties = Vec::new();
    for line in java_property_logical_lines(content) {
        let (key, value) = split_java_property(&line);
        properties.push((
            decode_java_property_component(key)?,
            decode_java_property_component(value)?,
        ));
    }
    Ok(properties)
}

fn java_property_logical_lines(content: &str) -> Vec<Vec<char>> {
    let characters: Vec<char> = content.chars().collect();
    let mut lines = Vec::new();
    let mut logical = Vec::new();
    let mut continuation = false;
    let mut cursor = 0usize;

    while cursor < characters.len() {
        let start = cursor;
        while cursor < characters.len() && !matches!(characters[cursor], '\r' | '\n') {
            cursor += 1;
        }
        let line_terminated = cursor < characters.len();
        let end = cursor;
        if line_terminated {
            let terminator = characters[cursor];
            cursor += 1;
            if terminator == '\r' && cursor < characters.len() && characters[cursor] == '\n' {
                cursor += 1;
            }
        }

        let natural = &characters[start..end];
        let first_content = natural
            .iter()
            .position(|character| !is_java_property_whitespace(*character))
            .unwrap_or(natural.len());
        let natural = &natural[first_content..];
        if !continuation && logical.is_empty() {
            if natural.is_empty() {
                continue;
            }
            if natural
                .first()
                .is_some_and(|character| matches!(*character, '#' | '!'))
            {
                continue;
            }
        }

        let trailing_backslashes = natural
            .iter()
            .rev()
            .take_while(|character| **character == '\\')
            .count();
        let continues = trailing_backslashes % 2 == 1;
        let append_end = natural.len() - usize::from(continues);
        logical.extend_from_slice(&natural[..append_end]);
        if continues && line_terminated {
            continuation = true;
            continue;
        }

        if !logical.is_empty() {
            lines.push(std::mem::take(&mut logical));
        } else if continues && !line_terminated {
            // LineReader tests for an empty buffer before removing the final
            // continuation marker, so a lone backslash at EOF produces one
            // empty-key/empty-value property.
            lines.push(Vec::new());
        }
        continuation = false;
    }

    // Java removes an odd terminal backslash and returns the accumulated line
    // when EOF follows a continuation marker without another physical line.
    if continuation || !logical.is_empty() {
        lines.push(logical);
    }
    lines
}

fn split_java_property(line: &[char]) -> (&[char], &[char]) {
    let mut key_length = 0usize;
    let mut value_start = line.len();
    let mut has_separator = false;
    let mut preceding_backslash = false;
    while key_length < line.len() {
        let character = line[key_length];
        if matches!(character, '=' | ':') && !preceding_backslash {
            value_start = key_length + 1;
            has_separator = true;
            break;
        }
        if is_java_property_whitespace(character) && !preceding_backslash {
            value_start = key_length + 1;
            break;
        }
        if character == '\\' {
            preceding_backslash = !preceding_backslash;
        } else {
            preceding_backslash = false;
        }
        key_length += 1;
    }

    while value_start < line.len() {
        let character = line[value_start];
        if !is_java_property_whitespace(character) {
            if !has_separator && matches!(character, '=' | ':') {
                has_separator = true;
            } else {
                break;
            }
        }
        value_start += 1;
    }
    (&line[..key_length], &line[value_start..])
}

fn decode_java_property_component(component: &[char]) -> Result<Vec<u16>> {
    let mut decoded = Vec::with_capacity(component.len());
    let mut cursor = 0usize;
    while cursor < component.len() {
        let character = component[cursor];
        cursor += 1;
        if character != '\\' {
            push_java_property_character(&mut decoded, character);
            continue;
        }
        let Some(&escaped) = component.get(cursor) else {
            return Err(Error::InvalidFormat {
                details: "malformed trailing escape in manifest properties".to_owned(),
            });
        };
        cursor += 1;
        match escaped {
            't' => decoded.push('\t' as u16),
            'n' => decoded.push('\n' as u16),
            'r' => decoded.push('\r' as u16),
            'f' => decoded.push('\u{c}' as u16),
            'u' => {
                let mut value = 0u32;
                for _ in 0..4 {
                    let Some(&digit) = component.get(cursor) else {
                        return Err(malformed_java_unicode_escape());
                    };
                    cursor += 1;
                    let Some(digit) = java_hex_digit(digit) else {
                        return Err(malformed_java_unicode_escape());
                    };
                    value = value * 16 + digit;
                }
                decoded.push(value as u16);
            }
            _ => push_java_property_character(&mut decoded, escaped),
        }
    }
    Ok(decoded)
}

fn push_java_property_character(decoded: &mut Vec<u16>, character: char) {
    let mut encoded = [0u16; 2];
    decoded.extend_from_slice(character.encode_utf16(&mut encoded));
}

fn malformed_java_unicode_escape() -> Error {
    Error::InvalidFormat {
        details: "malformed \\uXXXX escape in manifest properties".to_owned(),
    }
}

fn java_hex_digit(character: char) -> Option<u32> {
    match character {
        '0'..='9' => Some(character as u32 - '0' as u32),
        'a'..='f' => Some(character as u32 - 'a' as u32 + 10),
        'A'..='F' => Some(character as u32 - 'A' as u32 + 10),
        _ => None,
    }
}

fn java_property_ascii(value: &str) -> Vec<u16> {
    value.bytes().map(u16::from).collect()
}

fn parse_java_i32(value: &[u16]) -> Option<i32> {
    let (&first, _) = value.split_first()?;
    let (negative, mut cursor, limit) = match first {
        character if character == u16::from(b'-') => (true, 1, i32::MIN),
        character if character == u16::from(b'+') => (false, 1, -i32::MAX),
        _ => (false, 0, -i32::MAX),
    };
    if cursor == value.len() {
        return None;
    }

    // Accumulate negatively, like Integer.parseInt, so MIN remains representable.
    let multiplication_limit = limit / 10;
    let mut result = 0i32;
    while cursor < value.len() {
        let digit = java_decimal_digit(value[cursor])?;
        if result < multiplication_limit {
            return None;
        }
        result *= 10;
        if result < limit + digit {
            return None;
        }
        result -= digit;
        cursor += 1;
    }
    Some(if negative { result } else { -result })
}

/// The zero code unit of every BMP `Nd` block recognized by
/// `Character.digit(char, 10)`. Its letter-to-digit cases cannot produce a
/// value below ten and therefore do not apply at radix ten.
const JAVA_BMP_DECIMAL_ZEROES: [u16; 37] = [
    0x0030, 0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6, 0x0c66, 0x0ce6,
    0x0d66, 0x0de6, 0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80,
    0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
    0xff10,
];

fn java_decimal_digit(character: u16) -> Option<i32> {
    JAVA_BMP_DECIMAL_ZEROES.iter().find_map(|&zero| {
        let digit = character.wrapping_sub(zero);
        (digit < 10).then(|| i32::from(digit))
    })
}

fn is_java_property_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\u{c}')
}

/// A cache bounded by entry count with first-in-first-out eviction.
///
/// Parsed segment structures, strings, and templates are all cheap to
/// re-create from the memory-mapped archives, so simple eviction beats
/// the bookkeeping cost of recency tracking here.
#[cfg(test)]
mod tests {
    use super::{
        MAXIMUM_STORE_VERSION, parse_java_i32, parse_java_properties, parse_manifest_store_version,
    };

    fn java_units(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn manifest_properties_accept_java_line_terminators_and_last_duplicate_wins() {
        let manifest = concat!(
            "\u{c}! ignored comment\r",
            "store.version=0\r\n",
            "store.version : 1\n",
        );

        assert_eq!(parse_manifest_store_version(manifest).unwrap(), 1);
    }

    #[test]
    fn manifest_properties_continue_odd_backslashes_and_skip_leading_whitespace() {
        assert_eq!(
            parse_manifest_store_version("store.version=\\\n 1").unwrap(),
            1,
        );

        for terminator in ["\n", "\r", "\r\n"] {
            let manifest = format!("store.version=\\{terminator} \t\u{c}1");
            assert_eq!(
                parse_manifest_store_version(&manifest).unwrap(),
                1,
                "terminator {terminator:?}",
            );
        }

        let properties = parse_java_properties("key=value\\\\\nnext=entry").unwrap();
        assert_eq!(properties[0].1, java_units("value\\"));
        assert_eq!(properties[1].0, java_units("next"));

        let three_backslashes = format!("key=value{}\n  continued", "\\".repeat(3));
        let properties = parse_java_properties(&three_backslashes).unwrap();
        assert_eq!(properties[0].1, java_units("value\\continued"));

        assert_eq!(
            parse_java_properties("\\\n").unwrap(),
            [(Vec::new(), Vec::new())],
            "a continued zero-length logical line at EOF is an empty Java property",
        );
    }

    #[test]
    fn manifest_properties_preserve_java_terminal_backslash_eof_behavior() {
        assert_eq!(
            parse_java_properties("\\").unwrap(),
            vec![(Vec::new(), Vec::new())],
        );
        assert_eq!(
            parse_java_properties("key=\\").unwrap(),
            vec![(java_units("key"), Vec::new())],
        );
        assert_eq!(
            parse_manifest_store_version("store.version=\\").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
    }

    #[test]
    fn manifest_properties_decode_escaped_keys_separators_and_characters() {
        let properties = parse_java_properties(r"escaped\ key\:\=\\tail=\t\n\r\f\\\u0031").unwrap();

        assert_eq!(properties.len(), 1);
        assert_eq!(properties[0].0, java_units("escaped key:=\\tail"));
        assert_eq!(properties[0].1, java_units("\t\n\r\u{c}\\1"));
        assert_eq!(
            parse_manifest_store_version(r"store\.version=\u0031").unwrap(),
            1,
        );
        assert_eq!(
            parse_manifest_store_version(r"store\u002eversion=\u0031").unwrap(),
            1,
        );
    }

    #[test]
    fn manifest_properties_use_maximum_for_absent_or_last_unparseable_version() {
        assert_eq!(
            parse_manifest_store_version("unrelated=value\n").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=1\nstore.version=invalid\n").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=invalid\nstore.version=1\n").unwrap(),
            1,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=1 \n").unwrap(),
            MAXIMUM_STORE_VERSION,
            "Java Integer.parseInt does not trim the decoded value",
        );
        assert_eq!(
            parse_manifest_store_version("store.version=2147483648\n").unwrap(),
            MAXIMUM_STORE_VERSION,
            "values outside a Java int are unparseable",
        );
        assert_eq!(
            parse_manifest_store_version("store.version=-2147483648\n").unwrap(),
            i64::from(i32::MIN),
        );
    }

    #[test]
    fn java_i32_accepts_bmp_decimal_digits_and_checks_signed_overflow() {
        assert_eq!(parse_java_i32(&java_units("١")), Some(1));
        assert_eq!(parse_java_i32(&java_units("２")), Some(2));
        assert_eq!(parse_java_i32(&java_units("١2३")), Some(123));
        assert_eq!(parse_java_i32(&java_units("+١")), Some(1));
        assert_eq!(parse_java_i32(&java_units("-٢")), Some(-2));
        assert_eq!(parse_java_i32(&java_units("٢١٤٧٤٨٣٦٤٧")), Some(i32::MAX));
        assert_eq!(parse_java_i32(&java_units("٢١٤٧٤٨٣٦٤٨")), None);
        assert_eq!(parse_java_i32(&java_units("-٢١٤٧٤٨٣٦٤٨")), Some(i32::MIN));
        assert_eq!(parse_java_i32(&java_units("-٢١٤٧٤٨٣٦٤٩")), None);
        assert_eq!(
            parse_manifest_store_version(r"store.version=\u0661").unwrap(),
            1,
        );
    }

    #[test]
    fn manifest_properties_reject_malformed_unicode_escapes() {
        assert!(parse_manifest_store_version(r"store.version=\u12x4").is_err());
        assert!(parse_manifest_store_version(r"unrelated=\u123").is_err());
    }
}
