//! Segment providers over archive sets, and the certificates proving a
//! reopened source still holds what the plan measured.

use super::archive_certificate::certify_active_archive;
use crate::content::provider::SegmentProvider;
use crate::content::template::{Template, read_template};
use crate::content::value::read_string;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::segment::view::SegmentView;
use crate::tar_archive::archive::TarArchiveReader;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use std::collections::HashMap;
use std::sync::Arc;

/// The graph and binary-references trailers of an archive being swept,
/// filtered to the surviving segments — Oak filters the existing
/// trailers, never recomputes them; only a missing trailer falls back to
/// a per-segment header scan.
pub(super) struct FilteredTrailers {
    pub(super) graph_present: bool,
    /// The surviving catalog entries with their original generation
    /// triples, which the swept archive preserves verbatim; `None` when
    /// the original archive had no readable catalog.
    pub(super) catalog: Option<Vec<(GarbageCollectionGeneration, SegmentIdentifier, Vec<String>)>>,
    pub(super) graph_by_source: HashMap<SegmentIdentifier, Vec<SegmentIdentifier>>,
}

impl FilteredTrailers {
    pub(super) fn from_archive(
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
    pub(super) fn for_segment(
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
                let segment = provider.segment(identifier)?;
                read_blob_identifiers(provider, &segment).map_err(|error| Error::InvalidFormat {
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
pub(super) fn archive_segments_provider<'archives>(
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
pub(super) fn seed_references_from_archive(
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

/// Whether opening a fresh base-source provider must also derive the full
/// certificate for every base archive it serves.
#[derive(Clone, Copy)]
pub(super) enum BaseSourceCertification {
    /// Prove every base archive before returning the provider.
    Derive,
    /// The caller holds a [`CertifiedReclaimSources`] covering exactly these
    /// archives, taken under the lock still held, and has mutated none of
    /// them since.
    AlreadyProven,
}

/// Proof that every base archive of one store was certified under the
/// repository lock the holder still holds.
///
/// Returned by [`WritableRepository::preflight_reclaim_sources_with_progress`]
/// and accepted by
/// [`WritableRepository::reclaim_old_generations_from_sources`]. Compaction
/// certifies its sources before allocating the compacted copy and reclaims
/// from the same sources afterwards; without this the reclaim pass re-derived
/// the identical certificate over the identical bytes, parsing and hashing the
/// whole store a second time within one locked run. Nothing froe does between
/// those two points writes to a base archive: the deep copy only appends new
/// archives.
///
/// The proof names the archives it covers rather than asserting a bare fact,
/// and reclamation compares that set against its own base archives, so it can
/// only excuse work it actually did. A set that has shifted — an archive
/// renamed, added, or retired since — fails the comparison and the full
/// certificate is derived again.
///
/// What this never stands in for is the certificate immediately before a
/// mutation. Each archive that will change on disk is certified again in
/// `sweep_one_archive`, through a no-follow descriptor bound to the exact
/// inode about to be acted on, and its sweep plan is re-derived from those
/// fresh bytes and compared against the planned one.
pub(crate) struct CertifiedReclaimSources {
    pub(super) base_names: std::collections::HashSet<String>,
}

impl CertifiedReclaimSources {
    /// Whether this proof covers exactly `base_names` — no archive missing
    /// from what was certified, and none certified that is no longer a base
    /// archive.
    pub(super) fn certifies_exactly(&self, base_names: &std::collections::HashSet<String>) -> bool {
        self.base_names == *base_names
    }
}

/// Extracts every external blob identifier recorded in one segment,
/// resolving large (`0xF0`-class) identifiers through `provider`. Fails
/// when any identifier cannot be resolved: a rebuilt catalog missing an
/// entry would let AEM's blob garbage collection delete a binary that is
/// still referenced, so callers that *publish* the catalog must fail
/// closed instead.
///
/// The segment is taken as a resolved [`SegmentView`] rather than resolved
/// again here. Every caller already holds one, so re-resolving cost a second
/// parse of the same bytes on each certified segment; taking the view also
/// makes it structural, rather than a property of the provider passed, that
/// the record table read is the one belonging to these payload bytes.
pub(crate) fn read_blob_identifiers(
    provider: &dyn SegmentProvider,
    segment: &SegmentView<'_>,
) -> Result<Vec<String>> {
    let mut identifiers = Vec::new();
    for entry in segment.structure.record_table() {
        if entry.record_type() != Some(RecordType::ExternalBlobIdentifier) {
            continue;
        }
        let head = segment.read_u8(entry.record_number, 0)?;
        if head & 0xF0 == 0xE0 {
            let stored = segment.read_u16(entry.record_number, 0)?;
            let length = usize::from(stored & 0x0FFF);
            let reference_bytes = segment.read_bytes(entry.record_number, 2, length)?;
            identifiers.push(String::from_utf8_lossy(reference_bytes).into_owned());
        } else if head & 0xF8 == 0xF0 {
            let string_identifier = segment.read_record_identifier(entry.record_number, 1, 0)?;
            identifiers.push(read_string(provider, string_identifier)?);
        }
    }
    Ok(identifiers)
}

/// Certifies one freshly reopened source without resolving any UUID that it
/// contains through an older repository mapping. References to segments in
/// other archives still delegate to the complete provider captured by the
/// caller.
pub(super) fn certify_reopened_active_archive(
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
/// certification: a `0xF0`-class blob identifier resolves through a record
/// identifier that may name the very segment being inspected, and delegating
/// that UUID would read the string out of stale payload bytes. The segment
/// under inspection no longer reaches the provider at all — it is passed to
/// `read_blob_identifiers` as an already-resolved view — but every UUID it
/// *references* still does, so the shadowing stays load-bearing.
pub(super) struct ReopenedSourceProvider<'source, 'fallback> {
    pub(super) source: ArchiveSegmentsProvider<'source>,
    pub(super) fallback: &'fallback dyn SegmentProvider,
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
pub(super) struct ArchiveSegmentsProvider<'bytes> {
    pub(super) segments: HashMap<SegmentIdentifier, (Arc<ParsedSegment>, &'bytes [u8])>,
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
    use super::*;

    use crate::writer::store_writer::test_support::*;
    use crate::writer::tar_writer::TarArchiveWriter;
    use std::collections::HashSet;

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

        let plan = plan_cleanup_from_directory(&directory.path, current, root, &HashSet::new())
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
    /// The proof names what it covers, so it cannot excuse work it did not
    /// do. A base archive set that has gained, lost, or exchanged a name
    /// since the certificate was taken is not the set it proved.
    #[test]
    fn a_reclaim_proof_covers_only_the_sources_it_named() {
        let proved: std::collections::HashSet<String> =
            ["data00000a.tar".to_owned(), "data00001a.tar".to_owned()]
                .into_iter()
                .collect();
        let proof = CertifiedReclaimSources {
            base_names: proved.clone(),
        };
        assert!(proof.certifies_exactly(&proved));

        for divergent in [
            // One retired since.
            vec!["data00000a.tar"],
            // One added since.
            vec!["data00000a.tar", "data00001a.tar", "data00002a.tar"],
            // One rewritten to the next generation letter since.
            vec!["data00000a.tar", "data00001b.tar"],
            // Nothing left.
            vec![],
        ] {
            let current: std::collections::HashSet<String> =
                divergent.iter().map(|name| (*name).to_owned()).collect();
            assert!(
                !proof.certifies_exactly(&current),
                "a proof of {proved:?} must not cover {current:?}"
            );
        }
    }
}
