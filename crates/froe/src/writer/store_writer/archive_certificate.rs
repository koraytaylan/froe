//! Proving an archive's segments, graph, and binary references are
//! exactly what its index and trailers claim.

use super::providers::read_blob_identifiers;
use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::view::SegmentView;
use crate::tar_archive::archive::TarArchiveReader;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use std::collections::HashMap;
use std::sync::Arc;

/// Oak's `TarReader.sweep` for one base archive, with a precomputed
/// reclaim set from the mark phase: entries are judged and rewritten in
/// original file-position order, the generation triple comes from the
/// index entry, sub-25% savings keep the file untouched, and the graph
/// and binary-references trailers are *filtered* from the existing ones,
/// never recomputed — a raw segment scan cannot see every catalog entry,
/// and dropping one would let AEM's blob garbage collection delete a
/// still-referenced binary.
pub(super) type ExpectedGraph =
    HashMap<SegmentIdentifier, std::collections::HashSet<SegmentIdentifier>>;
pub(super) type ExpectedBinaryReferences =
    HashMap<(i32, i32, bool), HashMap<SegmentIdentifier, std::collections::HashSet<String>>>;

pub(super) fn stored_segment_generation(
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

pub(super) fn normalized_archive_graph(
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

pub(super) fn normalized_archive_binary_references(
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

pub(super) fn validate_exact_archive_trailers(
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
        // Parsed once and carried as a view. Handing the view to
        // `read_blob_identifiers` keeps the certificate reading the record
        // table that belongs to these exact bytes, and spares this pass a
        // second parse of every data segment in the store.
        let segment = SegmentView {
            structure: Arc::new(ParsedSegment::parse(identifier, bytes)?),
            bytes: bytes.into(),
        };
        let structure = &segment.structure;
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
            read_blob_identifiers(provider, &segment).map_err(|error| Error::InvalidFormat {
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
    provider: &(dyn SegmentProvider + Sync),
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
    provider: &(dyn SegmentProvider + Sync),
    archives: &[TarArchiveReader],
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<()> {
    let archives: Vec<&TarArchiveReader> = archives.iter().collect();
    certify_archives_in_parallel(provider, &archives, observer)
}

/// One certification pass shared by every worker: the archives still to be
/// claimed, and what the pass has concluded so far.
pub(super) struct ArchiveCertificationPass<'pass> {
    pub(super) provider: &'pass (dyn SegmentProvider + Sync),
    pub(super) archives: &'pass [&'pass TarArchiveReader],
    /// The next archive no worker has claimed yet.
    pub(super) next: std::sync::atomic::AtomicUsize,
    /// Archives certified, for the progress counter only.
    pub(super) certified: std::sync::atomic::AtomicUsize,
    /// Read before claiming, so workers stop starting new archives once the
    /// pass is already going to fail.
    pub(super) failed: std::sync::atomic::AtomicBool,
    /// The failure to report, with the archive's position so the lowest one
    /// wins however the workers interleave.
    pub(super) failure: std::sync::Mutex<Option<(usize, Error)>>,
}

impl ArchiveCertificationPass<'_> {
    /// Claims and certifies one archive. Returns `false` when nothing is
    /// left to claim, or when the pass has already failed.
    pub(super) fn certify_next_archive(&self) -> bool {
        use std::sync::atomic::Ordering;

        if self.failed.load(Ordering::Relaxed) {
            return false;
        }
        let position = self.next.fetch_add(1, Ordering::Relaxed);
        let Some(archive) = self.archives.get(position) else {
            return false;
        };
        match certify_active_archive(self.provider, archive) {
            Ok(()) => {
                self.certified.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => self.record_failure(position, error),
        }
        true
    }

    /// Records one archive's failure, keeping whichever is lowest-positioned.
    ///
    /// Workers reach this in whatever order they finish, so the comparison —
    /// not the arrival order — is what makes the reported failure the one a
    /// single-threaded pass would have reported.
    pub(super) fn record_failure(&self, position: usize, error: Error) {
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if failure
            .as_ref()
            .is_none_or(|(reported, _)| position < *reported)
        {
            *failure = Some((position, error));
        }
        self.failed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The failure this pass will report, if any.
    pub(super) fn reported_failure(self) -> Option<Error> {
        self.failure
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|(_, error)| error)
    }
}

/// Certifies every archive across as many threads as the host offers,
/// reporting one step whose counter advances as archives complete.
///
/// The unit of work is a whole archive, and certifying one is almost
/// entirely provider-free: a segment is proven from the bytes and record
/// table of the archive that holds it, so `provider` is consulted only to
/// follow a `0xF0`-class blob identifier out of the segment being read.
/// The locked caches behind it are therefore touched per external blob
/// identifier rather than per segment, which is what lets this scale.
///
/// Memory scales with the worker count, not the archive count: a worker
/// holds the reconstructed graph and binary-reference catalog of the one
/// archive it is certifying, so the peak is that per-archive metadata times
/// the workers, and a worker is never started for an archive that is not
/// there.
///
/// The reported failure is the lowest-positioned one, which is what a
/// single-threaded pass over this order would have reported. Positions are
/// handed out in order, so every archive before the first failure is
/// claimed, and a claimed archive is always finished — no earlier failure
/// can be missed. Archives after it may go unexamined, exactly as before.
pub(super) fn certify_archives_in_parallel(
    provider: &(dyn SegmentProvider + Sync),
    archives: &[&TarArchiveReader],
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    let pass = ArchiveCertificationPass {
        provider,
        archives,
        next: std::sync::atomic::AtomicUsize::new(0),
        certified: std::sync::atomic::AtomicUsize::new(0),
        failed: std::sync::atomic::AtomicBool::new(false),
        failure: std::sync::Mutex::new(None),
    };
    // One thread per archive at most, so a single-archive store spawns
    // nothing and runs exactly as it did before.
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(archives.len().max(1));

    crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "certifying source archives",
            crate::progress::WorkUnit::Archives,
        )
        .with_total(crate::progress::count(archives.len())),
        |observer| {
            std::thread::scope(|scope| {
                for _ in 1..workers {
                    scope.spawn(|| while pass.certify_next_archive() {});
                }
                // This thread is the last worker, and the only one that may
                // touch the observer.
                while pass.certify_next_archive() {
                    observer.step_advanced(crate::progress::count(
                        pass.certified.load(Ordering::Relaxed),
                    ));
                }
            });
            observer.step_advanced(crate::progress::count(
                pass.certified.load(Ordering::Relaxed),
            ));
            pass.reported_failure().map_or(Ok(()), Err)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::provider::SegmentProvider;
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::store_writer::providers::*;
    use std::collections::HashMap;

    use crate::writer::store_writer::test_support::*;

    /// Certification hands archives out to workers, so no position may be
    /// left unclaimed. Proven by breaking one archive at a time and
    /// requiring the pass to find it: a worker loop that skipped a position
    /// would still pass the all-healthy case, and fail here.
    #[test]
    fn parallel_certification_proves_every_archive() {
        const ARCHIVES: usize = 12;
        let (source, copies, last_payload_byte) =
            build_identical_certifiable_archives("parallel-certify-all", ARCHIVES);
        let provider = Repository::open(&source.path).expect("open provider");
        let path_of = |number: usize| copies.path.join(format!("data{number:05}a.tar"));
        let pristine = std::fs::read(path_of(0)).expect("read pristine copy");
        let open_all = || -> Vec<TarArchiveReader> {
            (0..ARCHIVES)
                .map(|number| TarArchiveReader::open(&path_of(number)).expect("open archive copy"))
                .collect()
        };

        certify_active_archives(&provider, &open_all())
            .expect("every healthy archive is certified");

        for corrupt in 0..ARCHIVES {
            let mut bytes = pristine.clone();
            bytes[last_payload_byte] ^= 0x01;
            std::fs::write(path_of(corrupt), &bytes).expect("corrupt one copy");
            let error = certify_active_archives(&provider, &open_all())
                .expect_err("the one corrupt archive must be found");
            assert!(
                error
                    .to_string()
                    .contains(&format!("data{corrupt:05}a.tar")),
                "position {corrupt} went unclaimed: {error}"
            );
            std::fs::write(path_of(corrupt), &pristine).expect("restore the copy");
        }
    }

    /// Which failure an operator is shown must not depend on how the workers
    /// interleaved: the lowest-positioned one is what a single-threaded pass
    /// over this order would have reported.
    ///
    /// Exercised through `record_failure` rather than through a corrupt
    /// multi-archive fixture, because that fixture cannot make the claim.
    /// Positions are handed out in ascending order, so the earlier archive
    /// also starts earlier and, on equal-sized archives, reliably fails
    /// first — a run reports the right archive whether or not any comparison
    /// happens. Arrival order is inverted here instead, which is the only
    /// case the comparison exists for.
    #[test]
    fn a_certification_pass_reports_its_lowest_positioned_failure() {
        static NO_SEGMENTS: std::sync::LazyLock<ArchiveSegmentsProvider<'static>> =
            std::sync::LazyLock::new(|| ArchiveSegmentsProvider {
                segments: HashMap::new(),
            });
        let failure_at = |position: usize| crate::Error::InvalidFormat {
            details: format!("archive at position {position}"),
        };
        let empty: [&TarArchiveReader; 0] = [];
        let new_pass = |provider: &'static (dyn SegmentProvider + Sync)| ArchiveCertificationPass {
            provider,
            archives: &empty,
            next: std::sync::atomic::AtomicUsize::new(0),
            certified: std::sync::atomic::AtomicUsize::new(0),
            failed: std::sync::atomic::AtomicBool::new(false),
            failure: std::sync::Mutex::new(None),
        };

        // Later position recorded first: the comparison must displace it.
        let pass = new_pass(&*NO_SEGMENTS);
        pass.record_failure(9, failure_at(9));
        pass.record_failure(3, failure_at(3));
        assert!(
            pass.reported_failure()
                .expect("a failure was recorded")
                .to_string()
                .contains("position 3")
        );

        // Ascending arrival: the first recorded is already the lowest, and
        // must not be displaced by anything later.
        let pass = new_pass(&*NO_SEGMENTS);
        pass.record_failure(3, failure_at(3));
        pass.record_failure(9, failure_at(9));
        assert!(
            pass.reported_failure()
                .expect("a failure was recorded")
                .to_string()
                .contains("position 3")
        );
    }
}
