//! Which archive files a store opens and in which order: newest valid
//! generation per number, grouped so a stale letter never shadows the one
//! discovery would select.

use super::{
    Arc, BoundedCache, DiscardedProgress, Error, HashMap, ParsedSegment, Path, ProgressObserver,
    RecordIdentifier, Result, RwLock, SEGMENT_CACHE_BUDGET_BYTES, STRING_CACHE_BUDGET_BYTES,
    SegmentIdentifier, SegmentProvider, SegmentView, Step, TEMPLATE_CACHE_BUDGET_BYTES,
    TarArchiveReader, Template, WorkUnit, check_manifest, group_file_generations_newest_first,
    load_through_cache, read_string, read_template,
};

/// A read-only provider over a set of archives, without a journal or
/// resolved head. Used by journal recovery, which must read segments
/// before any head exists.
///
/// String and template caches use the same byte budgets as [`crate::store::Repository`]:
/// a miss re-decodes from the mapping, so they cannot grow with the store.
pub struct ArchiveSet {
    pub(crate) archives: Vec<TarArchiveReader>,
    pub(crate) segment_locations: HashMap<SegmentIdentifier, usize>,
    pub(crate) parsed_segment_cache: RwLock<BoundedCache<SegmentIdentifier, Arc<ParsedSegment>>>,
    pub(crate) string_cache: RwLock<BoundedCache<RecordIdentifier, Arc<str>>>,
    pub(crate) template_cache: RwLock<BoundedCache<RecordIdentifier, Arc<Template>>>,
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
            string_cache: RwLock::new(BoundedCache::new(STRING_CACHE_BUDGET_BYTES)),
            template_cache: RwLock::new(BoundedCache::new(TEMPLATE_CACHE_BUDGET_BYTES)),
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

/// Opens every archive in a directory read-only, selecting the newest
/// generation letter of each archive number. Unlike [`crate::store::Repository::open`]
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
    check_manifest(directory, ArchivePresence::of(&file_names))?;
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
pub(crate) fn open_archives_newest_valid_first(
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
pub(crate) fn open_archive_groups(
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

/// Whether the store directory holds any segment archive, which decides
/// whether a missing manifest is the legacy pre-tar format or an empty
/// store that has yet to be written to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchivePresence {
    /// At least one `.tar` archive is present.
    Present,
    /// None, so a missing manifest is not evidence of the legacy format.
    Absent,
}

impl ArchivePresence {
    /// Classifies a store from the archive file names it was opened with.
    pub(crate) fn of(archive_file_names: &[String]) -> Self {
        if archive_file_names.is_empty() {
            Self::Absent
        } else {
            Self::Present
        }
    }
}
