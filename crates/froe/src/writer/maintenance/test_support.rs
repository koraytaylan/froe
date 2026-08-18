//! Fixtures and assertions shared by the maintenance test modules.

use super::options::*;
use super::plan::*;
use super::prepared::*;
use crate::checksum::crc32;
use crate::progress::{ProgressObserver, Step};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::tar_archive::archive::TarArchiveReader;
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::WritableRepository;
use crate::writer::tar_writer::TarArchiveWriter;
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
pub(super) struct TestDirectory {
    pub(super) path: std::path::PathBuf,
}
impl TestDirectory {
    pub(super) fn new(name: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "froe-cleanup-{name}-{}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self { path }
    }

    pub(super) fn repository(name: &str) -> Self {
        let directory = Self::new(name);
        WritableRepository::open(&directory.path)
            .expect("bootstrap")
            .close()
            .expect("close bootstrap");
        directory
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
#[cfg(unix)]
pub(super) fn checked_timespec_field<T>(value: i64) -> T
where
    T: TryFrom<i64>,
{
    T::try_from(value).unwrap_or_else(|_| {
        panic!("filesystem timestamp component {value} does not fit libc::timespec")
    })
}
/// The canonical repository directory of a test fixture.
///
/// Planning resolves the directory once through
/// [`canonical_repository_directory`] and reports every path relative to
/// that target, so an expectation built from the raw fixture path is not
/// comparable on a platform whose temporary directory is reached through a
/// symlink — macOS resolves `/var` to `/private/var`. Join the managed file
/// name onto this rather than canonicalizing the file itself: a managed
/// path under test may be a symlink, and following it would assert the
/// wrong name.
pub(super) fn canonical_fixture_directory(directory: &std::path::Path) -> std::path::PathBuf {
    directory
        .canonicalize()
        .expect("canonicalize the fixture directory")
}
pub(super) fn file_bytes(directory: &std::path::Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).expect("read directory") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_file() {
            files.push((
                entry.file_name(),
                std::fs::read(entry.path()).expect("read file"),
            ));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}
#[cfg(unix)]
pub(super) fn relative_path_from(
    base: &std::path::Path,
    target: &std::path::Path,
) -> std::path::PathBuf {
    let base_components: Vec<_> = base.components().collect();
    let target_components: Vec<_> = target.components().collect();
    let common = base_components
        .iter()
        .zip(&target_components)
        .take_while(|(left, right)| left == right)
        .count();
    assert!(common != 0, "absolute Unix paths share their root");

    let mut relative = std::path::PathBuf::new();
    for component in &base_components[common..] {
        assert!(matches!(component, std::path::Component::Normal(_)));
        relative.push("..");
    }
    for component in &target_components[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}
pub(super) fn file_mtimes(
    directory: &std::path::Path,
) -> Vec<(std::ffi::OsString, u64, std::time::SystemTime)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).expect("read directory") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_file() {
            let metadata = entry.metadata().expect("metadata");
            files.push((
                entry.file_name(),
                metadata.len(),
                metadata.modified().expect("mtime"),
            ));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}
pub(super) fn corrupt_first_magic(path: &std::path::Path, magic: [u8; 4]) {
    let mut bytes = std::fs::read(path).expect("read archive fixture");
    let position = bytes
        .windows(magic.len())
        .position(|window| window == magic)
        .expect("trailer magic exists");
    bytes[position] ^= 0x01;
    std::fs::write(path, bytes).expect("corrupt trailer magic");
}
pub(super) fn change_index_generation(
    path: &std::path::Path,
    identifier: SegmentIdentifier,
    generation: i32,
) {
    const TERMINATING_ZERO_BLOCKS: usize = 1024;
    const FOOTER_SIZE: usize = 16;

    let reader = TarArchiveReader::open(path).expect("open indexed archive fixture");
    let index = reader.index().expect("fixture has an index");
    let entry_size = if index.version == 2 { 33 } else { 28 };
    let entry_position = index
        .entries()
        .iter()
        .position(|entry| entry.segment_identifier == identifier)
        .expect("fixture index contains segment");
    let entry_count = index.entries().len();
    drop(reader);

    let mut bytes = std::fs::read(path).expect("read indexed archive fixture");
    let entries_end = bytes.len() - TERMINATING_ZERO_BLOCKS - FOOTER_SIZE;
    let entries_start = entries_end - entry_count * entry_size;
    let generation_start = entries_start + entry_position * entry_size + 24;
    bytes[generation_start..generation_start + 4].copy_from_slice(&generation.to_be_bytes());
    let checksum = crc32(&bytes[entries_start..entries_end]);
    bytes[entries_end..entries_end + 4].copy_from_slice(&checksum.to_be_bytes());
    std::fs::write(path, bytes).expect("write mismatched index generation");
}
pub(super) fn repack_without_graph_or_brf(
    directory: &std::path::Path,
    source_name: &str,
    target_name: &str,
) {
    let source = TarArchiveReader::open(&directory.join(source_name)).expect("source archive");
    let mut entries = source.index().expect("source index").entries().to_vec();
    entries.sort_by_key(|entry| entry.position);
    let mut target = TarArchiveWriter::new(directory, target_name);
    for entry in entries {
        target
            .write_segment(
                entry.segment_identifier,
                source
                    .segment_data(entry.segment_identifier)
                    .expect("source payload"),
                GarbageCollectionGeneration {
                    generation: entry.generation,
                    full_generation: entry.full_generation,
                    is_compacted: entry.is_compacted,
                },
                &[],
                &[],
            )
            .expect("repack segment without metadata");
    }
    target.close().expect("close repacked archive");
}
#[derive(Clone, Copy)]
pub(super) enum OmittedArchiveMetadata {
    Graph,
    BinaryReferences,
}
pub(super) fn repack_omitting_archive_metadata(
    directory: &std::path::Path,
    source_name: &str,
    omitted: OmittedArchiveMetadata,
) {
    let source_path = directory.join(source_name);
    let source = TarArchiveReader::open(&source_path).expect("source archive");
    let graph_by_source: HashMap<_, _> = source
        .segment_graph()
        .expect("source graph")
        .adjacency
        .into_iter()
        .collect();
    let mut binary_references_by_source = HashMap::new();
    for generation in source
        .binary_references()
        .expect("source binary-reference catalog")
        .generations
    {
        for (identifier, references) in generation.segments {
            assert!(
                binary_references_by_source
                    .insert(identifier, references)
                    .is_none(),
                "fixture source repeats a BRF segment"
            );
        }
    }
    let mut entries = source.index().expect("source index").entries().to_vec();
    entries.sort_by_key(|entry| entry.position);
    let temporary_name = format!("{source_name}.certificate-corrupt");
    let temporary_path = directory.join(&temporary_name);
    let mut target =
        TarArchiveWriter::new_exclusive_staged(directory, &temporary_name, source_name);
    for entry in entries {
        let references = if matches!(omitted, OmittedArchiveMetadata::Graph) {
            &[][..]
        } else {
            graph_by_source
                .get(&entry.segment_identifier)
                .map_or(&[][..], Vec::as_slice)
        };
        let binary_references = if matches!(omitted, OmittedArchiveMetadata::BinaryReferences) {
            &[][..]
        } else {
            binary_references_by_source
                .get(&entry.segment_identifier)
                .map_or(&[][..], Vec::as_slice)
        };
        target
            .write_segment(
                entry.segment_identifier,
                source
                    .segment_data(entry.segment_identifier)
                    .expect("source payload"),
                GarbageCollectionGeneration {
                    generation: entry.generation,
                    full_generation: entry.full_generation,
                    is_compacted: entry.is_compacted,
                },
                references,
                binary_references,
            )
            .expect("repack selectively omitted metadata");
    }
    target.close().expect("close corrupt repack");
    drop(source);
    std::fs::remove_file(&source_path).expect("remove original fixture archive");
    std::fs::rename(temporary_path, source_path).expect("install corrupt fixture archive");
}
pub(super) fn corrupt_segment_payload_crc(path: &std::path::Path, identifier: SegmentIdentifier) {
    let reader = TarArchiveReader::open(path).expect("open indexed archive fixture");
    let entry = *reader
        .index_entry(identifier)
        .expect("fixture index contains survivor");
    drop(reader);
    let mut bytes = std::fs::read(path).expect("read archive fixture");
    let payload_byte = entry.position as usize + entry.size as usize - 1;
    bytes[payload_byte] ^= 0x01;
    std::fs::write(path, bytes).expect("corrupt segment payload without changing its name CRC");
}
pub(super) fn write_empty_node_segment(
    store: &WritableRepository,
    generation: GarbageCollectionGeneration,
) -> crate::segment::record::RecordIdentifier {
    let mut writer = store.record_writer(generation);
    let node = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("write fixture node");
    writer.finish().expect("finish fixture segment");
    node
}
pub(super) fn rewrite_certificate_fixture(
    name: &str,
) -> (TestDirectory, String, String, SegmentIdentifier) {
    let directory = TestDirectory::repository(name);
    let store = WritableRepository::open(&directory.path).expect("open fixture writer");
    let old_generation = GarbageCollectionGeneration {
        generation: 0,
        full_generation: 0,
        is_compacted: false,
    };
    write_empty_node_segment(&store, old_generation);
    write_empty_node_segment(&store, old_generation);

    let current_generation = GarbageCollectionGeneration {
        generation: 2,
        full_generation: 2,
        is_compacted: false,
    };
    let mut survivor_writer = store.record_writer(current_generation);
    let external = survivor_writer
        .write_external_binary_identifier("source-certificate-live-external-blob")
        .expect("write external binary identifier");
    let survivor = survivor_writer
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
        .expect("write unreferenced survivor");
    survivor_writer.finish().expect("finish survivor segment");
    let content_root = write_empty_node_segment(&store, current_generation);
    let mut head_writer = store.record_writer(current_generation);
    let new_head = head_writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: content_root,
            },
            &[],
        )
        .expect("write cross-segment head");
    head_writer.finish().expect("finish head segment");
    assert!(store.compare_and_set_head(store.head(), new_head));
    store.close().expect("close fixture writer");

    let repository = Repository::open(&directory.path).expect("open healthy fixture");
    let source_name = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(survivor.segment))
        .expect("session archive contains survivor")
        .file_name()
        .to_owned();
    drop(repository);
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
    let plan = plan_compaction(&directory.path, &options).expect("healthy rewrite plan");
    let replacement_name = plan
        .actions()
        .iter()
        .find_map(|action| match action {
            CompactionAction::RewriteArchive {
                file_name,
                replacement_name,
                ..
            } if file_name == &source_name => Some(replacement_name.clone()),
            _ => None,
        })
        .expect("fixture produces an actionable rewrite");
    (directory, source_name, replacement_name, survivor.segment)
}
pub(super) fn whole_removal_certificate_fixture(
    name: &str,
) -> (TestDirectory, String, SegmentIdentifier) {
    let directory = TestDirectory::repository(name);
    let orphan = {
        let store = WritableRepository::open(&directory.path).expect("open orphan writer");
        let orphan = write_empty_node_segment(
            &store,
            GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            },
        );
        store.close().expect("close orphan writer");
        orphan
    };
    {
        let store = WritableRepository::open(&directory.path).expect("open head writer");
        let generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };
        let content_root = write_empty_node_segment(&store, generation);
        let mut writer = store.record_writer(generation);
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("write new head");
        writer.finish().expect("finish new head segment");
        assert!(store.compare_and_set_head(store.head(), head));
        store.close().expect("close head writer");
    }
    let repository = Repository::open(&directory.path).expect("open healthy fixture");
    let source_name = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(orphan.segment))
        .expect("orphan archive")
        .file_name()
        .to_owned();
    drop(repository);
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
    let plan = plan_compaction(&directory.path, &options).expect("healthy removal plan");
    assert!(plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveReclaimableArchive { file_name, .. }
            if file_name == &source_name
    )));
    (directory, source_name, orphan.segment)
}
pub(super) fn assert_source_certificate_refusal(
    directory: &TestDirectory,
    source_name: &str,
    replacement_name: Option<&str>,
    expected_error: &str,
) {
    let source_path = directory.path.join(source_name);
    let source_before = std::fs::read(&source_path).expect("read corrupt source");
    let journal_before = std::fs::read(directory.path.join("journal.log")).expect("journal");
    let before = file_bytes(&directory.path);
    for options in [
        CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        CompactionOptions::default(),
    ] {
        let error = plan_compaction(&directory.path, &options)
            .expect_err("read-only planning must reject an uncertified active archive");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected certificate error: {error}"
        );
        assert_eq!(
            file_bytes(&directory.path),
            before,
            "planning mutated files"
        );
    }

    let error = compact(
        &directory.path,
        CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
    )
    .expect_err("locked replan must reject an uncertified active archive");
    assert!(
        error.to_string().contains(expected_error),
        "unexpected locked certificate error: {error}"
    );
    assert_eq!(
        std::fs::read(&source_path).expect("source remains"),
        source_before,
        "cleanup changed the uncertified source"
    );
    assert_eq!(
        std::fs::read(directory.path.join("journal.log")).expect("journal remains"),
        journal_before,
        "cleanup changed the journal before source certification"
    );
    if let Some(replacement_name) = replacement_name {
        assert!(
            !directory.path.join(replacement_name).exists(),
            "cleanup published a replacement before source certification"
        );
    }
}
/// Overwrites an archive's index magic so it opens through the recovery
/// scan: the payload entries stay readable, only the trailer stops
/// validating. This is the shape a writer killed before closing its
/// archive leaves behind, without needing a writer to kill.
pub(super) fn break_index_magic(path: &std::path::Path) {
    use std::io::{Seek as _, SeekFrom};

    let length = std::fs::metadata(path).expect("archive metadata").len();
    // Fully qualified: the module's `OpenOptions` import is Unix-only,
    // and this helper is not.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open archive for damage");
    file.seek(SeekFrom::Start(length - 1028))
        .expect("seek to the index magic");
    file.write_all(&[0xde, 0xad, 0xbe, 0xef])
        .expect("overwrite the index magic");
}
/// The reason strings both tests build their censuses from.
pub(super) const MAGIC_REASON: &str = "unrecognized index magic number 0x000115a9";
pub(super) const CHECKSUM_REASON: &str = "index checksum mismatch";
/// Builds the fixture the field report reduces to: an independent
/// generation-two head, plus a generation-zero archive that only the
/// original journal line still roots. Oak's predicate would reclaim that
/// archive; froe's history keep-veto does not. Returns the old head, the
/// new head, and the directory.
pub(super) fn history_veto_fixture(
    name: &str,
) -> (TestDirectory, RecordIdentifier, RecordIdentifier) {
    let directory = TestDirectory::repository(name);
    let old_head = Repository::open(&directory.path)
        .expect("old repository")
        .head_record_identifier();
    let new_head = {
        let store = WritableRepository::open(&directory.path).expect("open new head writer");
        let mut writer = store.record_writer(GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        });
        let root = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("new content root");
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
            .expect("new super root");
        writer.finish().expect("finish new head");
        assert!(store.compare_and_set_head(store.head(), head));
        store.close().expect("close new head writer");
        head
    };
    (directory, old_head, new_head)
}
/// One archive holding a single dead generation-zero segment beside
/// enough live head-generation segments that removing the dead one
/// frees less than a quarter of the file. This is the field report in
/// miniature: real garbage, correctly identified, and — under Oak's
/// heuristic — declined by every run forever.
pub(super) fn sub_gate_garbage_fixture(name: &str) -> (TestDirectory, RecordIdentifier) {
    let directory = TestDirectory::repository(name);
    let new_head = {
        let store = WritableRepository::open(&directory.path).expect("open writer");
        let mut dead = store.record_writer(GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        });
        dead.write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("dead node");
        dead.finish().expect("finish dead segment");

        let live_generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };
        for _ in 0..8 {
            let mut live = store.record_writer(live_generation);
            live.write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("live node");
            live.finish().expect("finish live segment");
        }
        let mut writer = store.record_writer(live_generation);
        let root = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("content root");
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
        writer.finish().expect("finish head segment");
        assert!(store.compare_and_set_head(store.head(), head));
        store.close().expect("close writer");
        head
    };
    (directory, new_head)
}
/// Records the step names an operation reports, so a test can assert
/// what the operator was told froe was doing.
pub(super) struct StepNameObserver {
    pub(super) names: Vec<String>,
}
impl ProgressObserver for StepNameObserver {
    fn step_began(&mut self, step: &Step<'_>) {
        self.names.push(step.description().to_owned());
    }

    fn step_advanced(&mut self, _completed: u64) {}

    fn step_ended(&mut self) {}
}
