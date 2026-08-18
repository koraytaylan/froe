//! Fixtures and assertions shared by the store-writer test modules.

use super::archive_certificate::*;
use super::archive_numbering::*;
use super::cleanup_apply::*;
use super::providers::*;
use super::reclaim::*;
use super::repository::*;
#[cfg(unix)]
use crate::content::provider::SegmentProvider;
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::{RecordIdentifier, RecordType};
use crate::store::Repository;
use crate::tar_archive::archive::TarArchiveReader;
use crate::writer::compaction::CompactionKind;
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::repository_lock::RepositoryLock;
use crate::writer::segment_builder::{GarbageCollectionGeneration, SegmentBufferBuilder};
use crate::writer::tar_writer::TarArchiveWriter;
use std::collections::HashSet;
use std::sync::Arc;

pub(super) struct TestDirectory {
    pub(super) path: std::path::PathBuf,
}
impl TestDirectory {
    pub(super) fn new(name: &str) -> Self {
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
pub(super) fn open_prepared_store(
    directory: &std::path::Path,
    repository_lock: Arc<RepositoryLock>,
) -> WritableRepository {
    let certified =
        next_cleanup_archive_number(directory).expect("certify next physical archive number");
    WritableRepository::open_prepared(directory, repository_lock, certified)
        .expect("prepared writer")
}
#[derive(Clone)]
pub(super) struct TestArchiveEntry {
    pub(super) identifier: SegmentIdentifier,
    pub(super) content: Vec<u8>,
    pub(super) generation: GarbageCollectionGeneration,
    pub(super) references: Vec<SegmentIdentifier>,
    pub(super) binary_references: Vec<String>,
}
impl TestArchiveEntry {
    pub(super) fn new(
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

    pub(super) fn referencing(mut self, references: &[SegmentIdentifier]) -> Self {
        self.references.extend_from_slice(references);
        self
    }
}
pub(super) fn data_identifier(seed: u64) -> SegmentIdentifier {
    SegmentIdentifier::new(seed, 0xA000_0000_0000_0000 | seed)
}
pub(super) fn bulk_identifier(seed: u64) -> SegmentIdentifier {
    SegmentIdentifier::new(seed, 0xB000_0000_0000_0000 | seed)
}
pub(super) fn non_data_identifier(seed: u64) -> SegmentIdentifier {
    SegmentIdentifier::new(seed, 0xC000_0000_0000_0000 | seed)
}
pub(super) const fn generation(
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
pub(super) fn write_test_archive(
    directory: &TestDirectory,
    name: &str,
    entries: &[TestArchiveEntry],
) {
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
pub(super) fn write_manifest(directory: &TestDirectory) {
    std::fs::write(directory.path.join("manifest"), b"store.version=2\n").expect("write manifest");
}
// Low-level mark/sweep fixtures intentionally use tiny synthetic segment
// payloads and no journal. Production standalone cleanup enters through
// the repository-backed certificate wrappers; these helpers keep the
// arithmetic/ordering unit tests scoped to their primitive.
/// The rule standalone planning applies, so a test wrapper names the
/// reference generation and nothing else.
pub(super) fn standalone_rule(reference: GarbageCollectionGeneration) -> ReclaimRule {
    ReclaimRule {
        reference,
        kind: CompactionKind::Full,
        retained_generations: super::RETAINED_GENERATIONS,
    }
}
pub(super) fn plan_cleanup_from_directory(
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
pub(super) fn apply_cleanup_from_directory(
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
pub(super) enum OmittedSessionTrailer {
    Graph,
    BinaryReferences,
}
pub(super) fn write_session_semantic_fixture(
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
pub(super) fn rewrite_session_archive_omitting_trailer(
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
pub(super) fn rewrite_session_archive_in_order(
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
pub(super) fn truncate_archive_before_trailers(directory: &TestDirectory, name: &str) {
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

/// Builds a store whose base archive holds one dead generation-zero
/// segment beside live head-generation ones, opens it for a session at the
/// given target generation, and returns the writable store plus the
/// certified sources a reclaim pass needs.
pub(super) fn reclaimable_base_fixture(name: &str) -> (TestDirectory, GarbageCollectionGeneration) {
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
/// Builds a store whose base archives hold a bulk segment, persists
/// one session data segment at `session_generation` referencing that
/// bulk segment, reclaims at generation 2, and asserts the bulk
/// segment survives.
pub(super) fn assert_session_reference_keeps_base_bulk_alive(name: &str, session_generation: i32) {
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
/// Rewrites a finalized session archive so one segment carries payload
/// bytes the session never wrote, with a self-consistent tar entry.
///
/// Self-consistent is the point: the entry name's CRC matches the bytes
/// beside it, so the archive's own structural validation is satisfied.
/// Only the checksum the session recorded at write time can tell that
/// the payload is not the one it produced.
pub(super) fn rewrite_session_archive_with_foreign_payload(
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
pub(super) fn assert_prepared_session_trailer_omission_fails_closed(
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
pub(super) fn assert_postcomp_session_trailer_omission_fails_closed(
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
/// Builds `count` byte-identical certifiable archives in their own
/// directory, named as consecutive archive numbers, and returns the
/// directory beside the payload offset that carries the last byte of the
/// head segment — the byte a caller flips to break one copy's CRC.
pub(super) fn build_identical_certifiable_archives(
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
