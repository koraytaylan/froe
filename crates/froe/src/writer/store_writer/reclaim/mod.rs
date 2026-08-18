//! Oak's mark phase: which segments a generation predicate reclaims,
//! and which archives are worth rewriting once it has run.

use super::sweep::{is_reclaimable, next_archive_staging_name};
use super::sweep_plan::{PlannedArchiveSweep, StandaloneSegmentCompactionPlan};
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::parsed_segment::ParsedSegment;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::compaction::CompactionKind;
use crate::writer::segment_builder::GarbageCollectionGeneration;
use std::collections::HashMap;
use std::path::Path;

mod gate;

pub use gate::*;

pub(super) fn analyze_standalone_segment_cleanup(
    directory: &Path,
    archives: &[TarArchiveReader],
    rule: ReclaimRule,
    current_head_segment: SegmentIdentifier,
    protected: &std::collections::HashSet<SegmentIdentifier>,
    rewrite_policy: ArchiveRewritePolicy,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> Result<StandaloneSegmentCompactionPlan> {
    reject_duplicate_active_segments(archives)?;

    let mut references = std::collections::HashSet::new();
    let mut reclaimable = std::collections::HashSet::new();
    let policy = ReclaimPolicy {
        rule,
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
        if let Some(planned) = plan_archive_sweep(
            directory,
            archive,
            &reclaimable,
            rewrite_policy,
            &std::collections::HashSet::new(),
        )? {
            planned_archives.push(planned);
        }
    }
    planned_archives.sort_by(|left, right| left.file_name().cmp(right.file_name()));

    Ok(StandaloneSegmentCompactionPlan {
        archives: planned_archives,
        marked_segments: reclaimable.len(),
        reclaimable,
    })
}

pub(super) fn reject_duplicate_active_segments(archives: &[TarArchiveReader]) -> Result<()> {
    unique_active_segment_locations(archives).map(|_| ())
}

pub(super) fn unique_active_segment_locations(
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

/// Plans one archive's sweep.
///
/// `absent_names` are archive file names this same run has already committed
/// to unlink but has not unlinked yet. Both obstacles this function reads off
/// the live directory — an alternate generation letter that a whole-file
/// removal would promote, and an occupied rewrite target — must treat those
/// names as gone, or a plan built before the removals and a replan built after
/// them would disagree about archives the run never mentioned. Every applying
/// call passes an empty set, because by then the removals really have happened.
/// The sweep a post-compaction reclaim pass will perform, planned read-only.
///
/// Mirrors `reclaim_old_generations_with`'s mark exactly: the same
/// `mark_one_archive` over the same base archives in the same order, with the
/// dangling-future rule disabled and no protected set — because the run it
/// predicts has just published a compacted head, so nothing is dangling and
/// nothing is vetoed. What it cannot observe directly is the reference set the
/// copy's own session archives would seed, so the caller supplies it:
/// `seed_references` are the pre-existing bulk segments the copy will
/// reference where they lie.
///
/// `absent_names` are archives this run removes before it copies, so the
/// prediction and the replan agree about a namespace the prediction can see
/// but the replan will not.
pub(crate) fn predict_post_compaction_reclamation(
    directory: &Path,
    repository: &crate::store::Repository,
    rule: ReclaimRule,
    seed_references: &std::collections::HashSet<SegmentIdentifier>,
    rewrite_policy: ArchiveRewritePolicy,
    absent_names: &std::collections::HashSet<String>,
) -> Result<StandaloneSegmentCompactionPlan> {
    let protected = std::collections::HashSet::new();
    let policy = ReclaimPolicy {
        rule,
        protected_data_segments: &protected,
    };
    let mut references = seed_references.clone();
    let mut reclaimable = std::collections::HashSet::new();
    // No dangling-future root: the run being predicted commits its head before
    // it sweeps, so every compacted entry it will see belongs at or before it.
    let mut ahead_of_root = None;
    for archive in repository.archives() {
        if absent_names.contains(archive.file_name()) {
            continue;
        }
        mark_one_archive(
            archive,
            policy,
            &mut references,
            &mut reclaimable,
            &mut ahead_of_root,
        )?;
    }

    let mut planned_archives = Vec::new();
    for archive in repository.archives() {
        if absent_names.contains(archive.file_name()) {
            continue;
        }
        if let Some(planned) = plan_archive_sweep(
            directory,
            archive,
            &reclaimable,
            rewrite_policy,
            absent_names,
        )? {
            planned_archives.push(planned);
        }
    }
    planned_archives.sort_by(|left, right| left.file_name().cmp(right.file_name()));
    Ok(StandaloneSegmentCompactionPlan {
        archives: planned_archives,
        marked_segments: reclaimable.len(),
        reclaimable,
    })
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
/// The generation predicate one maintenance run applies, everywhere.
///
/// Built once per run and passed by value to the mark phase and to the
/// head-safety guard, so the two can never read different values. They used to
/// read the same pair of constants from five hundred lines apart, which was a
/// coincidence rather than a guarantee — and the moment the retention value
/// became a per-run quantity, that coincidence would have converted a refusal
/// into the silent deletion of head-reachable data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReclaimRule {
    /// The generation every candidate is judged against.
    pub(crate) reference: GarbageCollectionGeneration,
    /// Which of Oak's two generation predicates judges each candidate.
    pub(crate) kind: CompactionKind,
    /// How many generations behind the reference survive.
    pub(crate) retained_generations: i32,
}

#[derive(Clone, Copy)]
pub(super) struct ReclaimPolicy<'protected> {
    pub(super) rule: ReclaimRule,
    pub(super) protected_data_segments: &'protected std::collections::HashSet<SegmentIdentifier>,
}

pub(super) fn mark_one_archive(
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
                    policy.rule.reference,
                    generation,
                    policy.rule.kind,
                    policy.rule.retained_generations,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::record::{RecordIdentifier, RecordType};
    use crate::store::Repository;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::compaction::CompactionKind;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::segment_builder::SegmentBufferBuilder;
    use crate::writer::store_writer::repository::*;
    use crate::writer::store_writer::test_support::*;
    use crate::writer::tar_writer::TarArchiveWriter;
    use std::collections::HashSet;

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
            rule: ReclaimRule {
                reference,
                kind: CompactionKind::Full,
                retained_generations: 1,
            },
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
            rule: ReclaimRule {
                reference: current,
                kind: CompactionKind::Full,
                retained_generations: 2,
            },
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
            standalone_rule(generation(4, 4, true)),
            missing_root,
            &HashSet::new(),
            ArchiveRewritePolicy::default(),
            &mut crate::progress::DiscardedProgress,
        )
        .expect_err("missing root must refuse cleanup");
        assert!(error.to_string().contains("was not encountered"));
        assert!(directory.path.join("data00000a.tar").exists());
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

        let plan = plan_cleanup_from_directory(&directory.path, current, root, &HashSet::new())
            .expect("plan");
        assert!(!plan.reclaimable_segments().contains(&bulk));
        assert_eq!(plan.marked_segments, 0);
        assert!(plan.archives.is_empty());
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
            plan_cleanup_from_directory(&directory.path, reference, duplicate, &HashSet::new())
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

        let plan = plan_cleanup_from_directory(&directory.path, reference, root, &HashSet::new())
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

        let error = plan_cleanup_from_directory(&directory.path, reference, root, &HashSet::new())
            .expect_err("malformed kept data cannot safely propagate references");
        assert!(error.to_string().contains("magic bytes"));
        assert_eq!(std::fs::read(path).expect("read after refusal"), before);
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
            .reclaim_old_generations(reference, CompactionKind::Full)
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
            .reclaim_old_generations(generation(0, 0, false), CompactionKind::Tail)
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
            .reclaim_old_generations(generation, CompactionKind::Tail)
            .expect("reclaim ignores the unrelated file");
        store.close().expect("close");
        assert!(
            directory.path.join("notes.tar").exists(),
            "the unrelated file is left untouched"
        );
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
        assert!(store.compare_and_set_head(store.head(), invalid_head));
        store
            .flush()
            .expect("normal commit exposes the deliberately invalid test head");

        let error = store
            .reclaim_old_generations(reference, CompactionKind::Full)
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
}
