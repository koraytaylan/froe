//! Tests for the writable store.

use super::archive_certificate::*;
use super::archive_numbering::*;
use super::cleanup_apply::*;
use super::providers::*;
use super::reclaim::*;
use super::repository::*;
use super::session::*;
use super::sweep::*;
use super::sweep_plan::*;

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::{Arc, RwLock};

use crate::cache::BoundedCache;

use crate::content::provider::SegmentProvider;
use crate::segment::identifier::SegmentIdentifier;
#[cfg(unix)]
use crate::segment::parsed_segment::ParsedSegment;
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::store::Repository;
use crate::tar_archive::archive::TarArchiveReader;
#[cfg(unix)]
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::compaction::CompactionKind;
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::segment_builder::{GarbageCollectionGeneration, SegmentBufferBuilder};
use crate::writer::tar_writer::TarArchiveWriter;

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("froe-store-writer-{name}-{}", std::process::id()));
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
    std::fs::write(directory.path.join("manifest"), b"store.version=2\n").expect("write manifest");
}

// Low-level mark/sweep fixtures intentionally use tiny synthetic segment
// payloads and no journal. Production standalone cleanup enters through
// the repository-backed certificate wrappers; these helpers keep the
// arithmetic/ordering unit tests scoped to their primitive.
/// The rule standalone planning applies, so a test wrapper names the
/// reference generation and nothing else.
fn standalone_rule(reference: GarbageCollectionGeneration) -> ReclaimRule {
    ReclaimRule {
        reference,
        kind: CompactionKind::Full,
        retained_generations: super::RETAINED_GENERATIONS,
    }
}

fn plan_standalone_segment_cleanup(
    directory: &std::path::Path,
    reference: GarbageCollectionGeneration,
    current_head_segment: SegmentIdentifier,
    protected: &HashSet<SegmentIdentifier>,
) -> crate::error::Result<super::StandaloneSegmentCompactionPlan> {
    let archives = crate::store::open_all_archives(directory)?;
    analyze_standalone_segment_cleanup(
        directory,
        &archives,
        standalone_rule(reference),
        current_head_segment,
        protected,
        ArchiveRewritePolicy::default(),
        &mut crate::progress::DiscardedProgress,
    )
}

fn apply_standalone_segment_cleanup(
    directory: &std::path::Path,
    reference: GarbageCollectionGeneration,
    current_head_segment: SegmentIdentifier,
    protected: &HashSet<SegmentIdentifier>,
    expected: Option<&super::StandaloneSegmentCompactionPlan>,
) -> crate::error::Result<(
    super::StandaloneSegmentCompactionPlan,
    super::StandaloneSegmentCompactionOutcome,
)> {
    let archives = crate::store::open_all_archives(directory)?;
    apply_standalone_segment_cleanup_from_archives(
        directory,
        &archives,
        None,
        standalone_rule(reference),
        current_head_segment,
        protected,
        ArchiveRewritePolicy::default(),
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
    // Read through the provider: the session keeps locators, not
    // payloads, so the bytes come from the archive that holds them.
    let view = store.segment(target).expect("target belongs to session");
    let structure = Arc::clone(&view.structure);
    let bytes = view.bytes.to_vec();
    let generation = stored_segment_generation(target, &structure);
    let mut references = structure.referenced_segments.clone();
    let mut binary_references =
        read_blob_identifiers(store, &view).expect("reconstruct fixture BRF");
    match omitted {
        OmittedSessionTrailer::Graph => references.clear(),
        OmittedSessionTrailer::BinaryReferences => binary_references.clear(),
    }

    std::fs::remove_file(store.directory.join(&file_name)).expect("remove complete session TAR");
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
        let view = store
            .segment(*identifier)
            .expect("ordered segment belongs to session");
        let structure = Arc::clone(&view.structure);
        let bytes = view.bytes.to_vec();
        let binary_references =
            read_blob_identifiers(store, &view).expect("reconstruct fixture BRF");
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
    assert!(is_reclaimable(
        reference,
        generation(10, 6, true),
        CompactionKind::Full,
        2
    ));
    assert!(!is_reclaimable(
        reference,
        generation(10, 7, true),
        CompactionKind::Full,
        2
    ));
    // Generation age is an alternate path only for non-compacted data.
    assert!(is_reclaimable(
        reference,
        generation(8, 7, false),
        CompactionKind::Full,
        2
    ));
    assert!(!is_reclaimable(
        reference,
        generation(8, 7, true),
        CompactionKind::Full,
        2
    ));
    assert!(!is_reclaimable(
        reference,
        generation(9, 7, false),
        CompactionKind::Full,
        2
    ));

    // Java subtraction wraps in signed i32 arithmetic. These pairs
    // straddle the boundary and distinguish a wrapped delta of 1 from 2.
    let wrapping_reference = generation(i32::MIN, i32::MIN, false);
    assert!(!is_reclaimable(
        wrapping_reference,
        generation(i32::MAX, i32::MAX, false),
        CompactionKind::Full,
        2
    ));
    assert!(is_reclaimable(
        wrapping_reference,
        generation(i32::MAX - 1, i32::MAX - 1, false),
        CompactionKind::Full,
        2
    ));
    assert!(!is_reclaimable(
        generation(i32::MAX, i32::MAX, false),
        generation(i32::MIN, i32::MIN, false),
        CompactionKind::Full,
        2
    ));
}

#[test]
fn post_compaction_reclaimer_still_retains_exactly_one_generation() {
    let reference = generation(5, 5, true);
    assert!(is_reclaimable(
        reference,
        generation(4, 4, true),
        CompactionKind::Full,
        1
    ));
    assert!(!is_reclaimable(
        reference,
        generation(5, 5, true),
        CompactionKind::Full,
        1
    ));
    assert!(is_reclaimable(
        reference,
        generation(4, 5, false),
        CompactionKind::Tail,
        1
    ));
    assert!(!is_reclaimable(
        reference,
        generation(4, 5, true),
        CompactionKind::Tail,
        1
    ));
}

/// The boundary the retention value actually moves, and the store shape
/// that sits on it: a head Oak tail-compacted to `(1,0,compacted)` over
/// generation-zero data segments it still reaches. At two retained
/// generations those segments are spared by arithmetic; at one they are
/// reclaimable, and only `validate_reclaim_reference_invariant` stands
/// between the head and its own data.
#[test]
fn one_retained_generation_reclaims_what_two_spared() {
    let tail_compacted_head = generation(1, 0, true);
    let untouched_tail = generation(0, 0, false);
    assert!(is_reclaimable(
        tail_compacted_head,
        untouched_tail,
        CompactionKind::Full,
        1
    ));
    assert!(!is_reclaimable(
        tail_compacted_head,
        untouched_tail,
        CompactionKind::Full,
        2
    ));
    assert_eq!(
        super::RETAINED_GENERATIONS,
        1,
        "the run's own retention value is the one this boundary describes"
    );
}

#[test]
fn the_oak_savings_gate_defers_at_exactly_twenty_five_percent() {
    let directory = TestDirectory::new("savings-gate");
    let entries: Vec<_> = (1..=4)
        .map(|seed| TestArchiveEntry::new(data_identifier(seed), 1, generation(0, 0, false)))
        .collect();
    write_test_archive(&directory, "data00000a.tar", &entries);
    let reader =
        TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open archive");

    let exactly_one_quarter = HashSet::from([entries[0].identifier]);
    let exact = plan_archive_sweep(
        &directory.path,
        &reader,
        &exactly_one_quarter,
        ArchiveRewritePolicy::OakSavingsGate,
        &std::collections::HashSet::new(),
    )
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
    let rewrite = plan_archive_sweep(
        &directory.path,
        &reader,
        &more_than_one_quarter,
        ArchiveRewritePolicy::OakSavingsGate,
        &std::collections::HashSet::new(),
    )
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
fn the_default_policy_rewrites_what_the_oak_savings_gate_defers() {
    let directory = TestDirectory::new("savings-gate-default");
    let entries: Vec<_> = (1..=4)
        .map(|seed| TestArchiveEntry::new(data_identifier(seed), 1, generation(0, 0, false)))
        .collect();
    write_test_archive(&directory, "data00000a.tar", &entries);
    let reader =
        TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open archive");

    // The same one-in-four reclaimable set the gate defers above.
    let exactly_one_quarter = HashSet::from([entries[0].identifier]);
    let planned = plan_archive_sweep(
        &directory.path,
        &reader,
        &exactly_one_quarter,
        ArchiveRewritePolicy::EveryReclaimableArchive,
        &std::collections::HashSet::new(),
    )
    .expect("plan exact threshold")
    .expect("one segment is eligible");
    assert!(
        matches!(
            planned,
            PlannedArchiveSweep::Rewrite {
                segment_count: 1,
                eligible_entry_bytes: 1024,
                ref replacement_name,
                ..
            } if replacement_name == "data00000b.tar"
        ),
        "the default policy rewrites regardless of how little it frees, got {planned:?}"
    );
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

/// A stale archive letter this same run has already committed to unlink
/// must not block the sweep of the archive it shadows. Planning before the
/// removals and replanning after them would otherwise disagree about an
/// archive the run never named — which is a plan authorizing one mutation
/// and an apply performing another.
#[test]
fn a_sweep_plan_treats_a_pending_stale_removal_as_already_absent() {
    let directory = TestDirectory::new("pending-stale-removal");
    let old_one = data_identifier(210);
    let old_two = data_identifier(211);
    write_test_archive(
        &directory,
        "data00000a.tar",
        &[
            TestArchiveEntry::new(old_one, 1, generation(0, 0, false)),
            TestArchiveEntry::new(old_two, 1, generation(0, 0, false)),
        ],
    );
    // The superseded-letter condition a stale-archive removal retires.
    std::fs::write(
        directory.path.join("data00000b.tar"),
        b"stale letter this run removes first",
    )
    .expect("write the stale letter");
    let reader =
        TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open source");
    let cleaned = HashSet::from([old_one, old_two]);

    let blocked = plan_archive_sweep(
        &directory.path,
        &reader,
        &cleaned,
        ArchiveRewritePolicy::default(),
        &std::collections::HashSet::new(),
    )
    .expect("plan against the live namespace")
    .expect("every entry is reclaimable");
    assert!(
        matches!(
            blocked,
            PlannedArchiveSweep::BlockedByOccupiedGeneration {
                ref occupied_name,
                ..
            } if occupied_name == "data00000b.tar"
        ),
        "the live namespace still holds the stale letter, so the removal is blocked: {blocked:?}"
    );

    let absent = HashSet::from(["data00000b.tar".to_owned()]);
    let unblocked = plan_archive_sweep(
        &directory.path,
        &reader,
        &cleaned,
        ArchiveRewritePolicy::default(),
        &absent,
    )
    .expect("plan against the namespace this run will leave behind")
    .expect("every entry is reclaimable");
    assert!(
        matches!(
            unblocked,
            PlannedArchiveSweep::Remove {
                segment_count: 2,
                ..
            }
        ),
        "a letter this run removes first cannot block the whole-file removal: {unblocked:?}"
    );
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
    std::fs::write(directory.path.join("data00000b.tar"), occupied).expect("write occupied target");
    let reader =
        TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open source");
    let cleaned = HashSet::from([old_one, old_two]);

    let planned = plan_archive_sweep(
        &directory.path,
        &reader,
        &cleaned,
        ArchiveRewritePolicy::default(),
        &std::collections::HashSet::new(),
    )
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
        ArchiveRewritePolicy::default(),
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
    let error = plan_archive_sweep(
        &directory.path,
        &reader,
        &HashSet::from([old_one, old_two]),
        ArchiveRewritePolicy::default(),
        &std::collections::HashSet::new(),
    )
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
        std::fs::read(directory.path.join("data00000b.tar.cleaning.000")).expect("first residue"),
        0u16.to_be_bytes()
    );
    assert_eq!(
        std::fs::read(directory.path.join("data00000b.tar.cleaning.999")).expect("last residue"),
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
        plan_archive_sweep(
            &directory.path,
            &reader,
            &cleaned,
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
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
        ArchiveRewritePolicy::default(),
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
        plan_archive_sweep(
            &directory.path,
            &active,
            &cleaned,
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
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
        ArchiveRewritePolicy::default(),
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
        plan_archive_sweep(
            &directory.path,
            &reader,
            &cleaned,
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
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
        ArchiveRewritePolicy::default(),
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

    let swept =
        TarArchiveReader::open(&directory.path.join("data00001b.tar")).expect("open swept archive");
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
    // Generation `z` is the deferral the default policy still produces:
    // the `a`–`z` namespace is a format limit, not an economic choice, so
    // this archive keeps its reclaimable target on disk however little
    // rewriting it would free.
    write_test_archive(
        &directory,
        "data00000z.tar",
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
        PlannedArchiveSweep::DeferredAtLastGeneration { file_name, .. }
            if file_name == "data00000z.tar"
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
        directory.path.join("data00000z.tar").exists(),
        "the deferred target remains physically available"
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
    let after_plan = |plan: &super::StandaloneSegmentCompactionPlan| {
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
    let (plan, outcome) = apply_standalone_segment_cleanup_from_archives(
        &directory.path,
        &archives,
        None,
        standalone_rule(reference),
        root,
        &HashSet::new(),
        ArchiveRewritePolicy::default(),
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
        std::fs::read(directory.path.join("data00000b.tar")).expect("read occupied replacement"),
        occupied,
        "an unrelated occupied generation must not be credited as cleanup output"
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture, two sequenced sweeps, and the assertions that separate their outcomes belong in one place"
)]
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
        plan_archive_sweep(
            &directory.path,
            &first,
            &reclaimable,
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
        .expect("initial first plan")
        .expect("first archive is initially actionable"),
        PlannedArchiveSweep::Rewrite { .. }
    ));
    assert!(matches!(
        plan_archive_sweep(
            &directory.path,
            &second,
            &reclaimable,
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
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
        ArchiveRewritePolicy::default(),
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
        ArchiveRewritePolicy::default(),
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

    let plan = plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
        .expect("plan");
    apply_standalone_segment_cleanup(
        &directory.path,
        reference,
        root,
        &HashSet::new(),
        Some(&plan),
    )
    .expect("apply");

    let swept =
        TarArchiveReader::open(&directory.path.join("data00000b.tar")).expect("open swept archive");
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

    let plan = plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
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
    let replacement_metadata = std::fs::metadata(&replacement_path).expect("replacement metadata");
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

    let plan = plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
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

    let error = plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
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

    let plan = plan_standalone_segment_cleanup(&directory.path, reference, root, &HashSet::new())
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
        assert!(store.compare_and_set_head(previous, head));
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
            bytes[position + 2..position + 2 + SWAPPED_BLOB.len()].copy_from_slice(SWAPPED_BLOB);
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
    // The certificate reconstructs from the payload bytes of the archive
    // it was handed, so an inline (`0xE0`-class) identifier is caught
    // whatever provider is passed — a stale one no longer masks it. The
    // provider still resolves every UUID the segment *references*, which
    // is what the reopened-source shadowing below remains needed for.
    let stale_provider_error = certify_active_archive(&stale_repository, &reopened)
        .expect_err("the reopened payload is certified against itself, not the stale mapping");
    assert!(
        stale_provider_error.to_string().contains("catalog differs"),
        "{stale_provider_error}"
    );

    let cleaned: HashSet<_> = source
        .segment_identifiers()
        .filter(|identifier| *identifier != blob_segment)
        .collect();
    assert!(
        !cleaned.is_empty(),
        "the fixture must request a partial rewrite"
    );
    assert!(matches!(
        plan_archive_sweep(
            &directory.path,
            source,
            &cleaned,
            ArchiveRewritePolicy::default(),
            &std::collections::HashSet::new(),
        )
        .expect("source sweep plan"),
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
        ArchiveRewritePolicy::default(),
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

/// Builds a store whose base archive holds one dead generation-zero
/// segment beside live head-generation ones, opens it for a session at the
/// given target generation, and returns the writable store plus the
/// certified sources a reclaim pass needs.
fn reclaimable_base_fixture(name: &str) -> (TestDirectory, GarbageCollectionGeneration) {
    let directory = TestDirectory::new(name);
    let store = WritableRepository::open(&directory.path).expect("open the fixture writer");
    let mut dead = store.record_writer(generation(0, 0, false));
    dead.write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("write the dead node");
    dead.finish().expect("finish the dead segment");

    let live = generation(2, 2, true);
    // Enough live segments beside the dead ones that dropping them frees
    // well under a quarter of the archive — which is the only shape where
    // Oak's savings heuristic and the default policy disagree, and so the
    // only shape that can tell whether the policy reached the sweep.
    for _ in 0..8 {
        let mut filler = store.record_writer(live);
        filler
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("write a live filler node");
        filler.finish().expect("finish the filler segment");
    }
    let mut writer = store.record_writer(live);
    let root = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("write the content root");
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
        .expect("write the super root");
    writer.finish().expect("finish the live segment");
    assert!(store.compare_and_set_head(store.head(), head));
    store.flush().expect("flush");
    store.close().expect("close the fixture writer");
    (directory, live)
}

/// The authorization the post-compaction sweep never had: a confirmed plan
/// it must match before it unlinks anything. A disagreement is answered by
/// refusing, not by explaining afterwards.
#[test]
fn a_post_compaction_sweep_refuses_an_unconfirmed_disposition_before_it_unlinks() {
    let (directory, live) = reclaimable_base_fixture("sweep-authorization");
    let before: Vec<String> =
        crate::store::list_archive_file_names(&directory.path).expect("list the archives before");
    assert!(!before.is_empty());

    // A plan naming a disposition this store cannot produce.
    let impossible = StandaloneSegmentCompactionPlan {
        archives: vec![PlannedArchiveSweep::Remove {
            file_name: "data09999a.tar".to_owned(),
            segment_count: 1,
            file_bytes: 1,
        }],
        marked_segments: 1,
        reclaimable: std::collections::HashSet::new(),
    };

    let mut store = WritableRepository::open(&directory.path).expect("open for the sweep");
    let error = store
        .reclaim_old_generations_with(GenerationReclaimRequest {
            rule: ReclaimRule {
                reference: live,
                kind: CompactionKind::Full,
                retained_generations: RETAINED_GENERATIONS,
            },
            rewrite_policy: ArchiveRewritePolicy::EveryReclaimableArchive,
            certified_sources: None,
            expected: Some(&impossible),
        })
        .expect_err("an unconfirmed disposition must be refused");
    store.close().expect("close after the refusal");

    assert!(
        error.to_string().contains("changed after confirmation"),
        "the refusal names its reason: {error}"
    );
    assert_eq!(
        crate::store::list_archive_file_names(&directory.path).expect("list the archives after"),
        before,
        "a refused sweep unlinks nothing"
    );
}

/// The rewrite policy now reaches the code that acts on it. Before this,
/// a run under Oak's savings heuristic predicted a deferral and then
/// rewrote the archive anyway.
#[test]
fn the_savings_gate_reaches_the_post_compaction_sweep() {
    for (policy, expect_unchanged) in [
        (ArchiveRewritePolicy::OakSavingsGate, true),
        (ArchiveRewritePolicy::EveryReclaimableArchive, false),
    ] {
        let (directory, live) = reclaimable_base_fixture(&format!("sweep-policy-{policy:?}"));
        let before = crate::store::list_archive_file_names(&directory.path).expect("list");

        let mut store = WritableRepository::open(&directory.path).expect("open for the sweep");
        let outcome = store
            .reclaim_old_generations_with(GenerationReclaimRequest {
                rule: ReclaimRule {
                    reference: live,
                    kind: CompactionKind::Full,
                    retained_generations: RETAINED_GENERATIONS,
                },
                rewrite_policy: policy,
                certified_sources: None,
                expected: None,
            })
            .expect("the sweep runs");
        store.close().expect("close after the sweep");

        let after = crate::store::list_archive_file_names(&directory.path).expect("list");
        if expect_unchanged {
            assert_eq!(
                after, before,
                "Oak's heuristic leaves an archive it will not repay: {outcome:?}"
            );
            assert_eq!(outcome, SegmentSweepOutcome::default());
        } else {
            assert_ne!(
                after, before,
                "the default policy reclaims what the heuristic declined"
            );
            assert!(
                outcome.removed_archives + outcome.rewritten_archives > 0,
                "and reports what it did: {outcome:?}"
            );
        }
    }
}

#[test]
fn reclaim_marks_session_archives_so_referenced_base_bulk_survives() {
    assert_session_reference_keeps_base_bulk_alive("session-mark", 2);
}

/// The regression guard for the defect that made `compact` unusable: a
/// session that keeps what it writes.
///
/// This asserts the property directly rather than a consequence of it.
/// Writing more segments must not make the session hold more payload —
/// if it does, this fails no matter which structure grew or why. A test
/// that only checked "the store is still correct" would have passed
/// throughout the entire period the bug existed.
#[test]
fn writing_more_segments_does_not_make_a_session_hold_more_bytes() {
    /// A ceiling far below what this many segments occupy, so eviction
    /// is doing the bounding rather than the fixture simply being
    /// smaller than the default budget.
    const TEST_BUDGET_BYTES: usize = 4096;

    fn payload_bytes_held_after(directory: &TestDirectory, segments: usize) -> usize {
        let mut store = WritableRepository::open(&directory.path).expect("open");
        store.session_segment_cache = RwLock::new(BoundedCache::new(TEST_BUDGET_BYTES));
        let generation = store.writing_generation().expect("generation");
        for _ in 0..segments {
            let mut writer = store.record_writer(generation);
            writer
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("node");
            writer.finish().expect("finish");
        }
        let held = store
            .session_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .used_bytes();
        let locators = store
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(locators, segments, "every write must be locatable");
        store.close().expect("close");
        held
    }

    // A budget small enough that even this fixture's tiny segments
    // overflow it, so the ceiling is what limits residency rather than
    // the fixture being smaller than the default budget.
    let few = TestDirectory::new("session-growth-few");
    WritableRepository::open(&few.path)
        .expect("bootstrap")
        .close()
        .expect("close bootstrap");
    let many = TestDirectory::new("session-growth-many");
    WritableRepository::open(&many.path)
        .expect("bootstrap")
        .close()
        .expect("close bootstrap");

    let after_few = payload_bytes_held_after(&few, 8);
    let after_many = payload_bytes_held_after(&many, 64);

    // Eight times the writes, and the payload held must not grow with
    // them. The locator count does grow — that is metadata, and the
    // assertion above pins it — but bytes must not.
    assert!(
        after_many <= TEST_BUDGET_BYTES,
        "payload residency exceeded the ceiling: {after_many} bytes held \
         against a {TEST_BUDGET_BYTES}-byte budget"
    );
    assert!(
        after_few <= TEST_BUDGET_BYTES,
        "payload residency exceeded the ceiling: {after_few} bytes"
    );
    // And the production budget is a constant of the archive size, not
    // of the store: nothing here may make it a function of content.
    assert_eq!(
        session_cache_budget_bytes(DEFAULT_MAXIMUM_ARCHIVE_SIZE),
        (DEFAULT_MAXIMUM_ARCHIVE_SIZE as usize) * 2
    );
}

/// Guards the locator against regrowing into what it replaced.
///
/// `session_segments` holds one entry per segment for the life of the
/// session, so anything heap-owning added to its value type is once
/// again a per-segment allocation across the whole store. Keeping the
/// type `Copy` and small is what makes that map metadata rather than a
/// second copy of the repository.
#[test]
fn a_session_locator_owns_no_heap_and_stays_small() {
    // `Copy` cannot be derived for a type owning heap, so binding this
    // is a compile-time proof that no String, Vec, or Arc crept in.
    fn assert_copy<Type: Copy>() {}
    assert_copy::<SessionSegment>();
    assert!(
        std::mem::size_of::<SessionSegment>() <= 24,
        "SessionSegment grew to {} bytes; a field that scales per segment \
         belongs on disk or in a bounded cache, not here",
        std::mem::size_of::<SessionSegment>()
    );
}

#[test]
fn a_session_serves_its_own_segments_from_disk_when_nothing_is_cached() {
    let directory = TestDirectory::new("session-reread");
    let mut store = WritableRepository::open(&directory.path).expect("open");
    // Rotate on every segment, and cache nothing at all. Between them
    // these force the two read-back paths the session now depends on:
    // rotated archives through their mappings, and the archive still
    // open through the writer's positional read. The session retains no
    // payload, so if either path were wrong these reads would fail.
    store.maximum_archive_size = 1;
    store.session_segment_cache = RwLock::new(BoundedCache::new(0));

    let generation = store.writing_generation().expect("generation");
    let mut written = Vec::new();
    for _ in 0..4 {
        let mut writer = store.record_writer(generation);
        let node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("node");
        writer.finish().expect("finish");
        written.push(node.segment);
    }

    assert!(
        store
            .session_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "a zero-budget cache retains no payload"
    );
    for identifier in &written {
        let view = store.segment(*identifier).expect("session segment rereads");
        assert_eq!(view.structure.identifier, *identifier);
        assert!(!view.bytes.is_empty());
        assert!(
            store.segment_generation(*identifier).is_some(),
            "the locator answers the generation without a read"
        );
        assert!(store.contains_segment(*identifier));
    }
    store.close().expect("close");
    Repository::open(&directory.path).expect("store is healthy");
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
        SegmentBufferBuilder::write_record_identifier_bytes(reference, 0, &mut identifier_bytes);
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
            .reclaim_old_generations(reference_generation, CompactionKind::Tail)
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
        .reclaim_old_generations(generation, CompactionKind::Tail)
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
    assert!(store.compare_and_set_head(durable_head, invalid_head));

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

/// Rewrites a finalized session archive so one segment carries payload
/// bytes the session never wrote, with a self-consistent tar entry.
///
/// Self-consistent is the point: the entry name's CRC matches the bytes
/// beside it, so the archive's own structural validation is satisfied.
/// Only the checksum the session recorded at write time can tell that
/// the payload is not the one it produced.
fn rewrite_session_archive_with_foreign_payload(
    store: &WritableRepository,
    target: SegmentIdentifier,
) {
    let file_name = crate::store::list_archive_file_names(&store.directory)
        .expect("list archives")
        .into_iter()
        .find(|file_name| {
            TarArchiveReader::open(&store.directory.join(file_name))
                .is_ok_and(|archive| archive.contains_segment(target))
        })
        .expect("archive containing target session segment");
    let view = store.segment(target).expect("target belongs to session");
    let structure = Arc::clone(&view.structure);
    let mut bytes = view.bytes.to_vec();
    let generation = stored_segment_generation(target, &structure);
    let binary_references = read_blob_identifiers(store, &view).expect("reconstruct fixture BRF");
    // Flip a payload byte well past the header the parser needs.
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;

    std::fs::remove_file(store.directory.join(&file_name)).expect("remove session TAR");
    let mut writer = TarArchiveWriter::new(&store.directory, &file_name);
    writer
        .write_segment(
            target,
            &bytes,
            generation,
            &structure.referenced_segments,
            &binary_references,
        )
        .expect("write foreign payload");
    writer.close().expect("finalize foreign payload");
}

/// The regression for the session payload certificate.
///
/// The certificate used to compare the archive against a retained copy
/// of every byte the session wrote. It now compares the checksum the
/// session recorded against the archive's own tar entry name, which the
/// archive separately proves against its payload. This asserts the
/// changed mechanism still refuses a payload the session did not write —
/// and refuses it before the journal moves.
#[test]
fn a_session_payload_the_writer_never_produced_fails_closed() {
    let directory = TestDirectory::new("session-foreign-payload");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.close().expect("close bootstrap");
    }
    let journal_path = directory.path.join("journal.log");
    let journal_before = std::fs::read(&journal_path).expect("journal before");

    let repository_lock =
        Arc::new(RepositoryLock::acquire(&directory.path).expect("maintenance lock"));
    let mut store = open_prepared_store(&directory.path, Arc::clone(&repository_lock));
    store.maximum_archive_size = 1;
    let previous = store.head();
    let generation = store.writing_generation().expect("generation");
    let (head, child) = write_session_semantic_fixture(&store, generation);
    rewrite_session_archive_with_foreign_payload(&store, child.segment);
    assert!(store.compare_and_set_head(previous, head));

    let error = store
        .flush()
        .expect_err("the payload certificate must precede the journal append");
    assert!(
        error.to_string().contains("changed the payload of segment"),
        "unexpected validation error: {error}"
    );
    assert_eq!(
        std::fs::read(&journal_path).expect("journal after refusal"),
        journal_before,
        "a payload the session never wrote cannot reach the journal"
    );
    drop(store);
    drop(repository_lock);
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
    assert!(store.compare_and_set_head(previous, head));

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
        .map(|write| (write.archive_file_name.to_string(), write.identifier))
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
    assert!(store.compare_and_set_head(previous, head));

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

    let first = directory
        .path
        .join(recorded_writes[0].archive_file_name.as_ref());
    let second = directory
        .path
        .join(recorded_writes[1].archive_file_name.as_ref());
    let temporary = directory.path.join("session-boundary-swap.tmp");
    std::fs::rename(&first, &temporary).expect("move first archive aside");
    std::fs::rename(&second, &first).expect("move second into first boundary");
    std::fs::rename(&temporary, &second).expect("move first into second boundary");
    assert!(store.compare_and_set_head(previous, head));

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

    assert!(store.compare_and_set_head(durable_head, head));
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
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("commit compacted fixture head");
    let journal_before = std::fs::read(&journal_path).expect("committed journal");
    let target = match omitted {
        OmittedSessionTrailer::Graph => head.segment,
        OmittedSessionTrailer::BinaryReferences => child.segment,
    };
    rewrite_session_archive_omitting_trailer(&store, target, omitted);

    let error = store
        .reclaim_old_generations(reference, CompactionKind::Full)
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
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("commit compacted fixture head");
    store
        .finalized_session_semantic_validations
        .store(0, Ordering::Relaxed);

    store
        .reclaim_old_generations(reference, CompactionKind::Full)
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
    assert!(store.compare_and_set_head(previous, head));
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
    assert!(store.compare_and_set_head(previous_head, new_head));
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
    let journal_before = std::fs::read(directory.path.join("journal.log")).expect("journal before");

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
        let certified = next_cleanup_archive_number(&directory.path).expect("initial certificate");
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
    let journal_before = std::fs::read(directory.path.join("journal.log")).expect("journal before");

    let mut store = WritableRepository::open(&directory.path).expect("open corrupt-indexed base");
    let error = store
        .reclaim_old_generations(generation(2, 2, true), CompactionKind::Full)
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

/// A caller's proof excuses re-deriving the bulk certificate, never the
/// certificate that guards a mutation. This is the same corrupt source as
/// the test above, reclaimed with a proof naming exactly these archives —
/// the strongest thing a caller can present, and what compaction presents
/// after certifying them before its deep copy. The corruption must still
/// be refused, by the per-archive certificate `sweep_one_archive` derives
/// through a fresh descriptor, and nothing may be mutated on the way.
#[test]
fn a_reclaim_proof_never_lets_a_corrupt_source_reach_a_mutation() {
    let directory = TestDirectory::new("postcomp-proven-source-still-certified");
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
    let journal_before = std::fs::read(directory.path.join("journal.log")).expect("journal before");

    let mut store = WritableRepository::open(&directory.path).expect("open corrupt-indexed base");
    // The proof a successful preflight would have returned, presented
    // after the bytes it covered were changed underneath it.
    let proof = CertifiedReclaimSources {
        base_names: store.base_archive_names(),
    };
    assert!(
        proof.certifies_exactly(&store.base_archive_names()),
        "the fixture must present a proof the skip actually accepts"
    );

    let error = store
        .reclaim_old_generations_with(GenerationReclaimRequest {
            rule: ReclaimRule {
                reference: generation(2, 2, true),
                kind: CompactionKind::Full,
                retained_generations: RETAINED_GENERATIONS,
            },
            rewrite_policy: ArchiveRewritePolicy::EveryReclaimableArchive,
            certified_sources: Some(&proof),
            expected: None,
        })
        .expect_err("a proven source is still certified at its mutation boundary");

    assert!(error.to_string().contains("payload CRC"), "{error}");
    assert_eq!(
        std::fs::read(&base_path).expect("base after refusal"),
        corrupt_base,
        "a skipped bulk pass must not let the sweep rewrite its corrupt source"
    );
    assert_eq!(
        std::fs::read(directory.path.join("journal.log")).expect("journal after refusal"),
        journal_before,
        "a skipped bulk pass must not let the sweep change the journal"
    );
    assert!(!directory.path.join("data00000b.tar").exists());
}

/// Builds `count` byte-identical certifiable archives in their own
/// directory, named as consecutive archive numbers, and returns the
/// directory beside the payload offset that carries the last byte of the
/// head segment — the byte a caller flips to break one copy's CRC.
fn build_identical_certifiable_archives(
    name: &str,
    count: usize,
) -> (TestDirectory, TestDirectory, usize) {
    let source = TestDirectory::new(&format!("{name}-source"));
    {
        let store = WritableRepository::open(&source.path).expect("bootstrap");
        store.close().expect("close bootstrap");
    }
    let repository = Repository::open(&source.path).expect("open healthy source");
    let head = repository.head_record_identifier();
    let entry = *repository
        .archives()
        .iter()
        .find_map(|archive| archive.index_entry(head.segment))
        .expect("head index entry");
    drop(repository);
    let last_payload_byte = entry.position as usize + entry.size as usize - 1;

    let copies = TestDirectory::new(&format!("{name}-copies"));
    let bytes = std::fs::read(source.path.join("data00000a.tar")).expect("read source archive");
    for number in 0..count {
        std::fs::write(copies.path.join(format!("data{number:05}a.tar")), &bytes)
            .expect("write archive copy");
    }
    (source, copies, last_payload_byte)
}

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

    certify_active_archives(&provider, &open_all()).expect("every healthy archive is certified");

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
        assert!(
            store.compare_and_set_head(previous, head),
            "compare and set succeeds"
        );
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
