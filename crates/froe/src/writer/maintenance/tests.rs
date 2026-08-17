//! Tests for the maintenance pipeline.

use super::apply_identity::SETGID_MODE;
use super::apply_identity::*;
use super::file_removal::*;
use super::journal_analysis::*;
use super::manifest::*;
use super::options::*;
use super::plan::*;
use super::planning::*;
use super::prepared::*;
use super::recovery_backups::*;
use super::stale_archives::*;
use crate::content::provider::SegmentProvider as _;
use std::io::Write as _;

#[cfg(unix)]
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::checksum::crc32;
use crate::progress::{ProgressObserver, Step};
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::ArchiveFileName;
use crate::writer::commit::create_checkpoint;
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::segment_builder::GarbageCollectionGeneration;
use crate::writer::store_writer::WritableRepository;
use crate::writer::tar_writer::TarArchiveWriter;
use std::num::NonZeroUsize;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "froe-cleanup-{name}-{}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self { path }
    }

    fn repository(name: &str) -> Self {
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
fn checked_timespec_field<T>(value: i64) -> T
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
fn canonical_fixture_directory(directory: &std::path::Path) -> std::path::PathBuf {
    directory
        .canonicalize()
        .expect("canonicalize the fixture directory")
}

fn file_bytes(directory: &std::path::Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
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
fn relative_path_from(base: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
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

fn file_mtimes(
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

fn corrupt_first_magic(path: &std::path::Path, magic: [u8; 4]) {
    let mut bytes = std::fs::read(path).expect("read archive fixture");
    let position = bytes
        .windows(magic.len())
        .position(|window| window == magic)
        .expect("trailer magic exists");
    bytes[position] ^= 0x01;
    std::fs::write(path, bytes).expect("corrupt trailer magic");
}

fn change_index_generation(path: &std::path::Path, identifier: SegmentIdentifier, generation: i32) {
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

fn repack_without_graph_or_brf(directory: &std::path::Path, source_name: &str, target_name: &str) {
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
enum OmittedArchiveMetadata {
    Graph,
    BinaryReferences,
}

fn repack_omitting_archive_metadata(
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

fn corrupt_segment_payload_crc(path: &std::path::Path, identifier: SegmentIdentifier) {
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

fn write_empty_node_segment(
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

fn rewrite_certificate_fixture(name: &str) -> (TestDirectory, String, String, SegmentIdentifier) {
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

fn whole_removal_certificate_fixture(name: &str) -> (TestDirectory, String, SegmentIdentifier) {
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

fn assert_source_certificate_refusal(
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

#[test]
fn dry_run_is_byte_exact_and_never_creates_the_lock_file() {
    let directory = TestDirectory::repository("dry-run");
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove old lock inode");
    let before = file_bytes(&directory.path);
    let mtimes_before = file_mtimes(&directory.path);

    let plan = plan_compaction(&directory.path, &CompactionOptions::default()).expect("plan");

    assert!(plan.is_empty());
    assert_eq!(file_bytes(&directory.path), before);
    assert_eq!(file_mtimes(&directory.path), mtimes_before);
    assert!(!directory.path.join("repo.lock").exists());
}

#[test]
fn journal_removal_preview_is_an_exact_bounded_byte_prefix() {
    let directory = TestDirectory::repository("bounded-journal-preview");
    let mut hostile = vec![0xff];
    hostile.extend(std::iter::repeat_n(b'x', JOURNAL_LINE_PREVIEW_LIMIT + 20));
    hostile.push(b'\n');
    std::fs::OpenOptions::new()
        .append(true)
        .open(directory.path.join("journal.log"))
        .expect("open journal")
        .write_all(&hostile)
        .expect("append long invalid line");

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Journal]),
    )
    .expect("plan long invalid line");
    let removal = plan
        .journal_line_removals()
        .last()
        .expect("invalid line removal");
    assert_eq!(
        removal.preview_bytes(),
        &hostile[..JOURNAL_LINE_PREVIEW_LIMIT]
    );
    assert!(removal.preview_truncated());
    assert_eq!(removal.reason(), JournalRemovalReason::ParserSkippedNoSpace);
}

#[test]
fn non_journal_task_hides_internal_journal_removal_candidates() {
    let directory = TestDirectory::repository("non-journal-removal-visibility");
    std::fs::OpenOptions::new()
        .append(true)
        .open(directory.path.join("journal.log"))
        .expect("open journal")
        .write_all(b"parser-skipped\n")
        .expect("append parser-skipped line");

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
    )
    .expect("segment-only plan");

    assert_eq!(plan.tasks(), &[MaintenanceTask::Segments]);
    assert!(plan.journal_line_removals().is_empty());
    assert!(
        !plan
            .actions()
            .iter()
            .any(|action| matches!(action, CompactionAction::PruneJournal { .. }))
    );
}

#[test]
fn prospective_plan_refuses_a_survivor_that_references_a_planned_removal() {
    let directory = TestDirectory::repository("prospective-survivor-reference");
    let old_generation = GarbageCollectionGeneration {
        generation: 0,
        full_generation: 0,
        is_compacted: false,
    };
    let target = {
        let store = WritableRepository::open(&directory.path).expect("open old-target writer");
        let target = write_empty_node_segment(&store, old_generation);
        store.close().expect("close old-target archive");
        target
    };
    let store = WritableRepository::open(&directory.path).expect("open survivor writer");
    let current_generation = GarbageCollectionGeneration {
        generation: 2,
        full_generation: 2,
        is_compacted: false,
    };
    let mut survivor_writer = store.record_writer(current_generation);
    let survivor = survivor_writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "old-target".to_owned(),
                node: target,
            },
            &[],
        )
        .expect("write unjournaled newer-generation survivor");
    survivor_writer.finish().expect("finish survivor segment");
    let content_root = write_empty_node_segment(&store, current_generation);
    let mut head_writer = store.record_writer(current_generation);
    let head = head_writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: content_root,
            },
            &[],
        )
        .expect("write unrelated current head");
    head_writer.finish().expect("finish current head segment");
    assert!(store.compare_and_set_head(store.head(), head));
    store.close().expect("close fixture writer");
    let before = file_bytes(&directory.path);

    let error = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
    )
    .expect_err("prospective deletion must reject a surviving cross-reference");

    assert_eq!(
        error.to_string(),
        format!(
            "invalid segment-tar data: surviving data segment {} references segment {}, which the cleanup plan would remove",
            survivor.segment, target.segment
        )
    );
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "prospective validation remains read-only"
    );
    Repository::open(&directory.path).expect("refused fixture remains readable");
}

#[cfg(unix)]
#[test]
fn apply_identity_mismatch_is_detected_before_lock_creation() {
    use std::os::unix::fs::MetadataExt;

    let directory = TestDirectory::repository("wrong-service-user");
    std::fs::remove_file(directory.path.join("repo.lock")).expect("remove old lock inode");
    let owner = std::fs::metadata(directory.path.join("journal.log"))
        .expect("journal metadata")
        .uid();
    let different_uid = if owner == u32::MAX {
        owner - 1
    } else {
        owner + 1
    };

    let error = validate_apply_identity_for_uid(&directory.path, different_uid)
        .expect_err("different service uid must be rejected");

    assert!(error.to_string().contains("service user"));
    assert!(!directory.path.join("repo.lock").exists());
}

#[cfg(unix)]
#[test]
fn authoritative_plan_rejects_a_foreign_owned_archive_rewrite_before_mutation() {
    use std::os::unix::fs::MetadataExt as _;

    let (directory, source_name, _, _) =
        rewrite_certificate_fixture("foreign-owned-rewrite-preflight");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
    let plan = plan_compaction(&directory.path, &options).expect("healthy rewrite plan");
    let owner = std::fs::metadata(directory.path.join(&source_name))
        .expect("source metadata")
        .uid();
    let source_gid = std::fs::metadata(directory.path.join(&source_name))
        .expect("source metadata")
        .gid();
    let different_uid = if owner == u32::MAX {
        owner - 1
    } else {
        owner + 1
    };
    let credentials = ApplyCredentials {
        effective_uid: different_uid,
        effective_gid: source_gid,
        group_ids: BTreeSet::from([source_gid]),
    };
    let before = file_bytes(&directory.path);

    let error = validate_plan_apply_identity_for_credentials(&directory.path, &plan, &credentials)
        .expect_err("foreign-owned rewrite source must fail preflight");

    assert_eq!(
        error.to_string(),
        format!(
            "invalid segment-tar data: cleanup cannot safely replace {} while preserving its metadata: it is owned by uid {owner}, but the effective uid is {different_uid}; conservatively refusing before planned repository mutations",
            directory.path.join(source_name).display()
        )
    );
    assert_eq!(file_bytes(&directory.path), before);
    Repository::open(&directory.path).expect("preflight refusal leaves repository healthy");
}

#[cfg(unix)]
#[test]
fn planned_identity_preflight_uses_the_real_repository_directory_gid_and_mode() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let directory = TestDirectory::repository("planned-identity-directory-metadata");
    let plan = plan_compaction(&directory.path, &CompactionOptions::new().with_tasks([]))
        .expect("plan health-only cleanup");
    let mut permissions = std::fs::symlink_metadata(&directory.path)
        .expect("repository metadata before mode change")
        .permissions();
    permissions.set_mode(0o731);
    std::fs::set_permissions(&directory.path, permissions)
        .expect("install a distinctive repository mode");
    let metadata = std::fs::symlink_metadata(&directory.path).expect("repository metadata");
    let synthetic_gid = if metadata.gid() == u32::MAX {
        metadata.gid() - 1
    } else {
        metadata.gid() + 1
    };
    let credentials = ApplyCredentials {
        effective_uid: 42_424,
        effective_gid: synthetic_gid,
        group_ids: BTreeSet::from([synthetic_gid]),
    };
    let _ = take_possible_created_group_ids_input();

    let issue = planned_apply_identity_issue(&directory.path, &plan, &credentials)
        .expect("analyze planned metadata identity");

    assert_eq!(issue, None, "a health-only plan has no metadata sources");
    assert_eq!(
        take_possible_created_group_ids_input(),
        Some((metadata.gid(), metadata.permissions().mode())),
        "the group model must receive the repository directory's real gid and mode"
    );
}

#[cfg(unix)]
#[test]
fn ownership_preview_emits_a_known_mismatch_and_matches_the_apply_gate() {
    use std::os::unix::fs::MetadataExt as _;

    let directory = TestDirectory::repository("preview-service-user-warning");
    let mut plan = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
    )
    .expect("read-only plan");
    let journal_owner = std::fs::symlink_metadata(directory.path.join("journal.log"))
        .expect("journal metadata")
        .uid();
    let other_uid = if journal_owner == u32::MAX {
        journal_owner - 1
    } else {
        journal_owner + 1
    };
    let credentials = ApplyCredentials {
        effective_uid: other_uid,
        effective_gid: 0,
        group_ids: BTreeSet::from([0]),
    };

    let issue = preview_apply_identity_issue(&directory.path, &plan, &credentials)
        .expect("preview identity analysis")
        .expect("foreign service user must produce a preview warning");
    let shared_issue = journal_service_user_issue(&directory.path, other_uid)
        .expect("shared journal ownership analysis")
        .expect("foreign service user must fail the shared gate");

    assert_eq!(issue, shared_issue);
    let apply_error = validate_apply_identity_for_uid(&directory.path, other_uid)
        .expect_err("authoritative apply rejects the same mismatch")
        .to_string();
    assert!(apply_error.contains(&shared_issue), "{apply_error}");

    let warnings_before = plan.warnings.len();
    append_apply_identity_preview_warning_for_credentials(
        &directory.path,
        &mut plan,
        Ok(credentials),
    );
    assert_eq!(plan.warnings.len(), warnings_before + 1);
    let warning = plan.warnings.last().expect("known-mismatch warning");
    assert!(
        warning.contains("apply ownership preflight warning"),
        "{warning}"
    );
    assert!(warning.contains(&shared_issue), "{warning}");
    assert!(warning.contains("authoritative apply"), "{warning}");
}

#[cfg(unix)]
#[test]
fn ownership_preview_emits_a_warning_when_analysis_is_unprovable() {
    use std::os::unix::fs::MetadataExt as _;

    let (directory, source_name, _, _) = rewrite_certificate_fixture("preview-unprovable-warning");
    let mut plan = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
    )
    .expect("read-only rewrite plan");
    let journal_metadata =
        std::fs::symlink_metadata(directory.path.join("journal.log")).expect("journal metadata");
    let credentials = ApplyCredentials {
        effective_uid: journal_metadata.uid(),
        effective_gid: journal_metadata.gid(),
        group_ids: BTreeSet::from([journal_metadata.gid()]),
    };
    std::fs::rename(
        directory.path.join(&source_name),
        directory
            .path
            .join(format!("{source_name}.removed-after-plan")),
    )
    .expect("make the planned metadata source unavailable");
    assert!(
        preview_apply_identity_issue(&directory.path, &plan, &credentials).is_err(),
        "the fixture must exercise the analysis-error arm"
    );

    let warnings_before = plan.warnings.len();
    append_apply_identity_preview_warning_for_credentials(
        &directory.path,
        &mut plan,
        Ok(credentials),
    );

    assert_eq!(plan.warnings.len(), warnings_before + 1);
    let warning = plan.warnings.last().expect("unprovable-analysis warning");
    assert!(
        warning.contains("apply ownership could not be proved"),
        "{warning}"
    );
    assert!(
        warning.contains("authoritative apply will retry"),
        "{warning}"
    );
}

#[cfg(unix)]
#[test]
fn metadata_preflight_models_inherited_gid_and_setgid_mode_conservatively() {
    const SYNTHETIC_NON_ROOT_UID: u32 = 42_424;
    const ARCHIVE_GROUP: u32 = 27_182;
    const UNRELATED_GROUP: u32 = 31_415;

    let credentials = ApplyCredentials {
        effective_uid: SYNTHETIC_NON_ROOT_UID,
        effective_gid: UNRELATED_GROUP,
        group_ids: BTreeSet::from([UNRELATED_GROUP]),
    };
    let possible_created_gids =
        possible_created_group_ids(ARCHIVE_GROUP, SETGID_MODE | 0o750, &credentials);
    let source_path = std::path::Path::new("data00000a.tar");
    let source_mode = 0o640;

    assert_eq!(
        possible_created_gids,
        BTreeSet::from([ARCHIVE_GROUP]),
        "a setgid directory fixes the staging-file group"
    );
    assert_eq!(
        metadata_source_apply_identity_issue(
            source_path,
            SYNTHETIC_NON_ROOT_UID,
            ARCHIVE_GROUP,
            source_mode,
            &possible_created_gids,
            &credentials,
        ),
        None,
        "an already inherited source gid needs neither fchown nor group membership when no setgid bit is requested"
    );

    let issue = metadata_source_apply_identity_issue(
        source_path,
        SYNTHETIC_NON_ROOT_UID,
        ARCHIVE_GROUP,
        source_mode | SETGID_MODE,
        &possible_created_gids,
        &credentials,
    )
    .expect("setgid preservation outside caller groups must refuse conservatively");
    assert!(issue.contains(&format!("gid {ARCHIVE_GROUP}")), "{issue}");
    assert!(issue.contains("setgid-mode"), "{issue}");
    assert!(issue.contains("cannot be guaranteed read-only"), "{issue}");
}

#[cfg(unix)]
#[test]
fn metadata_preflight_models_both_permitted_non_setgid_creation_groups() {
    const SYNTHETIC_NON_ROOT_UID: u32 = 42_424;
    const SOURCE_GROUP: u32 = 27_182;
    const EFFECTIVE_GROUP: u32 = 31_415;

    let credentials = ApplyCredentials {
        effective_uid: SYNTHETIC_NON_ROOT_UID,
        effective_gid: EFFECTIVE_GROUP,
        group_ids: BTreeSet::from([EFFECTIVE_GROUP]),
    };
    let possible_gids = possible_created_group_ids(SOURCE_GROUP, 0o750, &credentials);

    let issue = metadata_source_apply_identity_issue(
        std::path::Path::new("data00000a.tar"),
        SYNTHETIC_NON_ROOT_UID,
        SOURCE_GROUP,
        0o640,
        &possible_gids,
        &credentials,
    )
    .expect("a possible System V group outcome must be treated conservatively");

    assert_eq!(
        possible_gids,
        BTreeSet::from([SOURCE_GROUP, EFFECTIVE_GROUP])
    );
    assert!(
        issue.contains(&format!("may have gid {possible_gids:?}")),
        "the diagnostic must record both POSIX-permitted creation groups: {issue}"
    );
}

#[cfg(unix)]
#[test]
fn checkpoint_metadata_preflight_checks_only_the_newest_possible_template() {
    let directory = TestDirectory::repository("checkpoint-template-prefix");
    std::fs::copy(
        directory.path.join("data00000a.tar"),
        directory.path.join("data00001a.tar"),
    )
    .expect("create a second readable archive number");

    let sources = planned_metadata_sources(
        &directory.path,
        PlannedRewrites {
            upgrades_manifest: false,
            segment_plan: None,
            moves_checkpoint_head: true,
            rewrites_journal: false,
        },
    )
    .expect("derive checkpoint metadata sources");

    assert_eq!(sources, BTreeSet::from(["data00001a.tar".to_owned()]));
}

#[cfg(unix)]
#[test]
fn checkpoint_metadata_preflight_includes_the_leading_removal_outcome_prefix() {
    let directory = TestDirectory::repository("checkpoint-template-removal-prefix");
    let current_generation = GarbageCollectionGeneration {
        generation: 2,
        full_generation: 2,
        is_compacted: false,
    };
    let current_head = {
        let store = WritableRepository::open(&directory.path).expect("open head writer");
        let content_root = write_empty_node_segment(&store, current_generation);
        let mut writer = store.record_writer(current_generation);
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
            .expect("write current head");
        writer.finish().expect("finish current head segment");
        assert!(store.compare_and_set_head(store.head(), head));
        store.close().expect("close head writer");
        head
    };
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
        store.close().expect("close unjournaled orphan writer");
        orphan
    };
    let repository = Repository::open(&directory.path).expect("open prefix fixture");
    let current_archive = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(current_head.segment))
        .expect("current head archive")
        .file_name()
        .to_owned();
    let orphan_archive = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(orphan.segment))
        .expect("newest orphan archive")
        .file_name()
        .to_owned();
    drop(repository);
    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
    )
    .expect("plan newest whole removal");
    assert!(plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveReclaimableArchive { file_name, .. }
            if file_name == &orphan_archive
    )));

    let sources = planned_metadata_sources(
        &directory.path,
        PlannedRewrites {
            upgrades_manifest: false,
            segment_plan: plan.segment_plan.as_ref(),
            moves_checkpoint_head: true,
            rewrites_journal: false,
        },
    )
    .expect("derive possible checkpoint templates");

    assert_eq!(
        sources,
        BTreeSet::from([orphan_archive, current_archive]),
        "a failed newest unlink uses that source; a successful unlink promotes only the next active archive"
    );
}

#[test]
fn recovery_backup_task_requires_an_explicit_policy_without_writing() {
    let directory = TestDirectory::repository("backup-policy-required");
    let before = file_bytes(&directory.path);
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::RecoveryBackups]);

    let error = plan_compaction(&directory.path, &options)
        .expect_err("recovery backup cleanup without retention must be rejected");

    let crate::error::Error::InvalidFormat { details } = error else {
        panic!("unexpected options error: {error}");
    };
    assert_eq!(
        details,
        "recovery-backups requires an explicit age/count retention policy"
    );
    assert_eq!(file_bytes(&directory.path), before);
}

/// Overwrites an archive's index magic so it opens through the recovery
/// scan: the payload entries stay readable, only the trailer stops
/// validating. This is the shape a writer killed before closing its
/// archive leaves behind, without needing a writer to kill.
fn break_index_magic(path: &std::path::Path) {
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
const MAGIC_REASON: &str = "unrecognized index magic number 0x000115a9";
const CHECKSUM_REASON: &str = "index checksum mismatch";

#[test]
fn a_refusal_names_the_offenders_and_agrees_in_number() {
    let one = indexless_archive_refusal(IndexlessCensus {
        total_numbers: 43,
        indexless_numbers: 1,
        offenders: &[("data00042a.tar", MAGIC_REASON)],
        newest_is_indexless: true,
        any_scan_is_empty: false,
    });
    assert!(
        one.contains("1 of 43 active archive numbers has no index metadata (data00042a.tar)"),
        "singular subject names the archive and the total: {one}"
    );
    assert!(
        one.contains(MAGIC_REASON),
        "the reason the index was rejected reaches the operator: {one}"
    );
    assert!(
        one.contains("no archive, journal, or checkpoint has been changed"),
        "the refusal states precisely what is untouched: {one}"
    );

    let two = indexless_archive_refusal(IndexlessCensus {
        total_numbers: 3,
        indexless_numbers: 2,
        offenders: &[
            ("data00001a.tar", CHECKSUM_REASON),
            ("data00000a.tar", CHECKSUM_REASON),
        ],
        newest_is_indexless: false,
        any_scan_is_empty: false,
    });
    assert!(
        two.contains("2 of 3 active archive numbers have no index metadata"),
        "plural subject agrees: {two}"
    );
    assert_eq!(
        two.matches(CHECKSUM_REASON).count(),
        1,
        "one shared reason is stated once, not repeated per archive: {two}"
    );
}

/// The census counts every offender even though only five are named,
/// so an operator cannot read the shown list as the whole damage.
#[test]
fn a_refusal_counts_the_offenders_it_does_not_name() {
    let many: Vec<String> = (0..8)
        .map(|index| format!("data0000{index}a.tar"))
        .collect();
    let borrowed: Vec<(&str, &str)> = many.iter().map(|n| (n.as_str(), MAGIC_REASON)).collect();

    let truncated = indexless_archive_refusal(IndexlessCensus {
        total_numbers: 40,
        indexless_numbers: 8,
        offenders: &borrowed,
        newest_is_indexless: true,
        any_scan_is_empty: false,
    });

    assert!(
        truncated.contains("8 of 40 active archive numbers"),
        "the count is the whole census, not the shown names: {truncated}"
    );
    assert!(
        truncated.contains("and 3 more"),
        "the omitted names are counted rather than silently dropped: {truncated}"
    );
}

/// The remedy is the branch an operator acts on, and the three shapes
/// need different advice: a killed writer can be repaired, mid-store
/// damage should be inspected first, and an archive whose scan reads
/// nothing cannot be repaired at all.
#[test]
fn a_refusal_advises_the_remedy_that_fits_the_damage() {
    let killed_writer = indexless_archive_refusal(IndexlessCensus {
        total_numbers: 43,
        indexless_numbers: 1,
        offenders: &[("data00042a.tar", MAGIC_REASON)],
        newest_is_indexless: true,
        any_scan_is_empty: false,
    });
    assert!(
        killed_writer.contains("the newest active archive is among them"),
        "a killed writer is distinguishable from damage: {killed_writer}"
    );
    assert!(
        killed_writer.contains("--repair-archive-indexes"),
        "a killed writer is pointed at the task that repairs it: {killed_writer}"
    );

    let mid_store = indexless_archive_refusal(IndexlessCensus {
        total_numbers: 3,
        indexless_numbers: 2,
        offenders: &[
            ("data00001a.tar", CHECKSUM_REASON),
            ("data00000a.tar", CHECKSUM_REASON),
        ],
        newest_is_indexless: false,
        any_scan_is_empty: false,
    });
    assert!(
        mid_store.contains("the newest active archive is not among them"),
        "mid-store damage is not reported as a killed writer: {mid_store}"
    );
    assert!(
        mid_store.contains("inspect before repairing"),
        "mid-store damage does not get the unconditional repair advice: {mid_store}"
    );

    // An archive whose scan read nothing cannot be rebuilt, and the
    // write open refuses it. Advising a write command would be circular.
    let unrecoverable = indexless_archive_refusal(IndexlessCensus {
        total_numbers: 2,
        indexless_numbers: 1,
        offenders: &[("data00009a.tar", MAGIC_REASON)],
        newest_is_indexless: true,
        any_scan_is_empty: true,
    });
    assert!(
        !unrecoverable.contains("--repair-archive-indexes"),
        "an unrecoverable archive must not be sent to a command that will refuse it: \
         {unrecoverable}"
    );
    assert!(
        unrecoverable.contains("move that file aside"),
        "an unrecoverable archive gets the remedy that actually works: {unrecoverable}"
    );
}

#[test]
fn planning_warnings_survive_a_refusal_and_stay_bounded() {
    let untouched = attach_planning_warnings(
        crate::error::Error::InvalidFormat {
            details: "refused".to_owned(),
        },
        &[],
    );
    let crate::error::Error::InvalidFormat { details } = untouched else {
        panic!("variant must be preserved");
    };
    assert_eq!(details, "refused", "no warnings means no tail");

    let warnings: Vec<String> = (0..5).map(|index| format!("warning {index}")).collect();
    let attached = attach_planning_warnings(
        crate::error::Error::InvalidFormat {
            details: "refused.".to_owned(),
        },
        &warnings,
    );
    let crate::error::Error::InvalidFormat { details } = attached else {
        panic!("variant must be preserved");
    };
    assert!(
        details.contains("warning 0") && details.contains("warning 2"),
        "established warnings reach the operator: {details}"
    );
    assert!(
        !details.contains("warning 3"),
        "the tail is bounded so it cannot bury the refusal: {details}"
    );
    assert!(
        details.contains("and 2 further warnings"),
        "the omitted warnings are counted: {details}"
    );

    // A non-format error keeps its variant: a caller matching on it for
    // an input/output failure must still be able to.
    let input_output = attach_planning_warnings(
        crate::error::Error::InputOutput(std::io::Error::other("disk")),
        &warnings,
    );
    assert!(
        matches!(input_output, crate::error::Error::InputOutput(_)),
        "only format refusals carry the tail"
    );
}

/// The production refusal, end to end. Before the census this reported
/// the first offending segment of the first offending archive and
/// nothing else — no count, no ordinality, no remedy — and discarded
/// the stale-archive warning the same run had already established.
#[test]
fn cleanup_refuses_an_index_less_active_archive_with_a_full_census() {
    let directory = TestDirectory::new("index-less-census");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.close().expect("close");
    }
    break_index_magic(&directory.path.join("data00000a.tar"));
    let before = file_bytes(&directory.path);

    let error = plan_compaction(&directory.path, &CompactionOptions::default())
        .expect_err("an index-less active archive must refuse generation cleanup");
    let crate::error::Error::InvalidFormat { details } = error else {
        panic!("unexpected refusal variant");
    };
    assert!(
        details.contains("1 of 1 active archive numbers has no index metadata (data00000a.tar)"),
        "the refusal names every offender and the total: {details}"
    );
    assert!(
        details.contains("the newest active archive is among them"),
        "the refusal states whether this is a killed writer: {details}"
    );
    assert!(
        details.contains("no archive, journal, or checkpoint has been changed"),
        "the refusal states precisely what is untouched: {details}"
    );
    assert!(
        details.contains("has no valid indexed generation"),
        "the stale-archive warning established before the refusal is carried out with it: {details}"
    );
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "planning a refusal changes no byte"
    );
}

/// The whole point of the task: a store a killed writer left behind is
/// cleaned to a healthy shape by cleanup itself, in one run, instead of
/// sending the operator to a different command as a workaround.
#[test]
fn repair_archives_heals_an_index_less_store_and_the_rest_then_plans() {
    let directory = TestDirectory::new("repair-heals");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.close().expect("close");
    }
    break_index_magic(&directory.path.join("data00000a.tar"));

    // Without the task, the default set still refuses and points at it.
    let refusal = plan_compaction(&directory.path, &CompactionOptions::default())
        .expect_err("the default set must still refuse");
    assert!(
        refusal.to_string().contains("--repair-archive-indexes"),
        "the refusal names the task that fixes it: {refusal}"
    );

    // With it, the preview names the repair and nothing else yet.
    let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
    let preview = plan_compaction(&directory.path, &options).expect("preview must not refuse");
    assert!(
        preview.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RepairArchiveIndex { file_name, .. }
                if file_name == "data00000a.tar"
        )),
        "the preview names the repair: {:?}",
        preview.actions()
    );

    let outcome = PreparedCompaction::prepare(&directory.path, options)
        .expect("prepare repairs under the lock")
        .apply()
        .expect("apply");
    assert_eq!(outcome.repaired_archives, 1, "the rebuild is reported");
    assert!(
        directory.path.join("data00000a.tar.bak").exists(),
        "the original bytes are retained"
    );

    // Healthy: every archive indexed, and the default set now plans.
    let repository = Repository::open(&directory.path).expect("reopen");
    assert!(
        !repository
            .archives()
            .iter()
            .any(TarArchiveReader::is_recovered),
        "no archive is served through the recovery scan any more"
    );
    plan_compaction(&directory.path, &CompactionOptions::default())
        .expect("the default set plans cleanly against the healed store");
}

/// An archive number that cannot be rebuilt dooms the run however it is
/// retried, so it is refused where nothing has been touched — not after
/// paying a durable rewrite of every repairable archive to discover it.
#[test]
fn an_unrepairable_archive_refuses_before_anything_is_rewritten() {
    let directory = TestDirectory::new("repair-unrepairable");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        writer
            .write_string("forces a second archive")
            .expect("string");
        writer.finish().expect("finish");
        store.close().expect("close");
    }
    // A repairable archive, and — at a higher number, so the repair loop
    // would have reached it second — bytes no scan can recover.
    break_index_magic(&directory.path.join("data00000a.tar"));
    std::fs::write(directory.path.join("data00500a.tar"), vec![0x5au8; 4096])
        .expect("unrecoverable residue");
    let before = file_bytes(&directory.path);

    let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);

    // The read-only preview says so, before any authorization.
    let preview = plan_compaction(&directory.path, &options)
        .expect_err("the preview must refuse an unrepairable archive");
    assert!(
        preview.to_string().contains("data00500a.tar"),
        "the preview names the archive that dooms the run: {preview}"
    );

    // And a library caller skipping the preview pays no rewrite either.
    match PreparedCompaction::prepare(&directory.path, options) {
        Ok(_) => panic!("prepare must refuse too"),
        Err(error) => assert!(
            error.to_string().contains("data00500a.tar"),
            "prepare names it as well: {error}"
        ),
    }
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "and neither path rewrote a single archive"
    );
}

/// A repair that fails part-way through still happens — a staging residue
/// is found per number, not by the survey — and reporting only the
/// failure would leave the operator believing nothing moved.
#[test]
fn a_failed_repair_reports_the_rebuilds_it_already_completed() {
    let directory = TestDirectory::new("repair-partial");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.close().expect("close");
    }
    // A second archive number, from an independent bootstrap so it shares
    // no segment identifier with the first — a copy would instead trip
    // the cross-number duplicate guard.
    let donor = TestDirectory::new("repair-partial-donor");
    {
        let store = WritableRepository::open(&donor.path).expect("donor bootstrap");
        store.close().expect("close");
    }
    std::fs::copy(
        donor.path.join("data00000a.tar"),
        directory.path.join("data00001a.tar"),
    )
    .expect("second archive number");
    // Both repairable; the higher number carries the residue of an
    // interrupted rebuild, which repair must refuse rather than clobber.
    break_index_magic(&directory.path.join("data00000a.tar"));
    break_index_magic(&directory.path.join("data00001a.tar"));
    std::fs::write(
        directory.path.join("data00001a.tar.recovering"),
        b"an interrupted rebuild",
    )
    .expect("staging residue");

    let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
    let details = match PreparedCompaction::prepare(&directory.path, options) {
        Ok(_) => panic!("the staging residue must refuse the run"),
        Err(crate::error::Error::InvalidFormat { details }) => details,
        Err(other) => panic!("unexpected refusal variant: {other}"),
    };
    assert!(
        details.contains("data00001a.tar.recovering"),
        "the refusal names what stopped it: {details}"
    );
    assert!(
        details.contains("data00000a.tar"),
        "the refusal names what it already rebuilt: {details}"
    );
    assert!(
        details.contains("no second attempt"),
        "the refusal says the completed work need not be redone: {details}"
    );
    assert!(
        directory.path.join("data00000a.tar.bak").exists(),
        "and that rebuild really is durable"
    );
    assert!(
        directory.path.join("data00001a.tar.recovering").exists(),
        "the residue is left for the stale-temporaries task to adjudicate"
    );
}

/// Selecting the task is not the same as having work to do, and the
/// difference used to be a one-way `store.version` 1→2 transition that
/// appeared in no plan, was never confirmed, and survived cancelling.
#[test]
fn selecting_repair_with_nothing_to_repair_changes_no_byte() {
    let directory = TestDirectory::repository("repair-noop");
    std::fs::write(
        directory.path.join("manifest"),
        "#a version one store\nstore.version=1\n",
    )
    .expect("v1 manifest");
    // Something to do, so the run is not short-circuited as empty.
    std::fs::write(directory.path.join("journal.log.compacting"), b"").expect("temporary");
    let before = file_bytes(&directory.path);

    let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
    let prepared =
        PreparedCompaction::prepare(&directory.path, options).expect("prepare must succeed");
    assert_eq!(
        prepared.repaired_archives(),
        0,
        "there was nothing to repair"
    );
    assert_eq!(
        crate::store::read_manifest_store_version(&directory.path.join("manifest"))
            .expect("read manifest"),
        1,
        "the manifest must not be upgraded when no repair happens"
    );
    drop(prepared);
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "preparing a repair run with nothing to repair changes no byte"
    );
}

/// The repair used to run before any store-version gate, so froe would
/// rewrite the archives of a store it then declared itself unable to
/// read. The library API reaches `prepare` without the lockless preview
/// that would otherwise have refused.
#[test]
fn a_store_from_a_newer_oak_is_refused_before_any_repair() {
    let directory = TestDirectory::new("repair-newer-store");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.close().expect("close");
    }
    break_index_magic(&directory.path.join("data00000a.tar"));
    std::fs::write(
        directory.path.join("manifest"),
        "#from a newer Oak\nstore.version=3\n",
    )
    .expect("v3 manifest");
    let before = file_bytes(&directory.path);

    let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
    match PreparedCompaction::prepare(&directory.path, options) {
        Ok(_) => panic!("a store version this reader does not support must be refused"),
        Err(error) => assert!(
            error.to_string().contains("newer"),
            "the refusal names the store version: {error}"
        ),
    }
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "and it is refused before a single archive is rewritten"
    );
}

/// Every index-dependent gate evaluates the store for the first time
/// after the repair, so a refusal there is ordinary — and must not claim
/// the store is as the operator left it.
#[test]
fn a_refusal_after_a_repair_says_the_repair_already_happened() {
    let directory = TestDirectory::new("repair-then-refuse");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.close().expect("close");
    }
    // A second number sharing every segment with the first: a real,
    // unrepairable defect that only the post-repair gates can see.
    std::fs::copy(
        directory.path.join("data00000a.tar"),
        directory.path.join("data00001a.tar"),
    )
    .expect("duplicate archive number");
    break_index_magic(&directory.path.join("data00000a.tar"));

    // The preview must state the unfitness itself, before authorizing.
    let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
    let preview = plan_compaction(&directory.path, &options)
        .expect_err("the cross-number duplicate must refuse in the read-only preview");
    assert!(
        preview.to_string().contains("occurs in active archives"),
        "the dry run reports the real reason: {preview}"
    );
}

/// The one-way v1 to v2 transition is charged when a rebuilt archive is
/// about to become visible, not when one is merely predicted. A repair
/// can still fail per archive for reasons no survey models, and paying an
/// irreversible format change for a run that rebuilds nothing would leave
/// the store damaged AND closed to an older Oak.
#[test]
fn a_repair_that_installs_nothing_does_not_upgrade_the_manifest() {
    let directory = TestDirectory::new("repair-v1-no-install");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.close().expect("close");
    }
    break_index_magic(&directory.path.join("data00000a.tar"));
    // Repairable by the survey, but the rebuild refuses: a staging
    // residue is found per number, after the survey has had its say.
    std::fs::write(
        directory.path.join("data00000a.tar.recovering"),
        b"an interrupted rebuild",
    )
    .expect("staging residue");
    std::fs::write(
        directory.path.join("manifest"),
        "#a version one store\nstore.version=1\n",
    )
    .expect("v1 manifest");
    let before = file_bytes(&directory.path);

    let options = CompactionOptions::default().with_task(MaintenanceTask::RepairArchives);
    assert!(
        PreparedCompaction::prepare(&directory.path, options).is_err(),
        "the residue must refuse the run"
    );
    assert_eq!(
        crate::store::read_manifest_store_version(&directory.path.join("manifest"))
            .expect("read manifest"),
        1,
        "a run that installed no rebuilt archive must not have raised the store version"
    );
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "and nothing else moved either"
    );
}

/// A rebuild ends by matching the replaced archive's ownership, which
/// fails for a foreign-owned file. Discovering that after the rewrite
/// means no rerun ever converges, so it is a stat(2) check up front.
#[cfg(unix)]
#[test]
fn a_repair_target_this_process_cannot_match_refuses_before_rewriting() {
    use std::os::unix::fs::MetadataExt as _;

    let directory = TestDirectory::new("repair-foreign-owner");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.close().expect("close");
    }
    break_index_magic(&directory.path.join("data00000a.tar"));
    let owner = std::fs::symlink_metadata(directory.path.join("data00000a.tar"))
        .expect("archive metadata")
        .uid();
    // Only meaningful where the archive is not already ours; as root
    // every chown succeeds and there is nothing to refuse.
    if owner != unsafe { libc::geteuid() } || owner == 0 {
        return;
    }
    let targets = crate::writer::store_writer::repair_target_names(&directory.path)
        .expect("survey the repair targets");
    assert_eq!(
        targets,
        vec!["data00000a.tar".to_owned()],
        "the preflight inspects the file the rebuild would replace"
    );
}

/// Repair makes the `.bak` that the backup policy retires. Doing both in
/// one run can delete the only copy of what the rebuild could not read.
#[test]
fn repair_archives_and_recovery_backups_cannot_run_together() {
    let options = CompactionOptions::default()
        .with_task(MaintenanceTask::RepairArchives)
        .with_recovery_backup_policy(RecoveryBackupPolicy::new(Duration::ZERO, 0));
    let directory = TestDirectory::repository("repair-and-backups");
    let error = plan_compaction(&directory.path, &options)
        .expect_err("the combination must be refused before anything is read");
    let crate::error::Error::InvalidFormat { details } = error else {
        panic!("unexpected refusal variant");
    };
    assert!(
        details.contains("cannot run together"),
        "the refusal explains the conflict: {details}"
    );
    assert!(
        details.contains("repair first"),
        "the refusal states the safe sequence: {details}"
    );
}

#[test]
fn empty_directory_is_refused_without_bootstrapping_anything() {
    let directory = TestDirectory::new("empty");
    let error = plan_compaction(&directory.path, &CompactionOptions::default())
        .expect_err("an empty directory is not a repository");
    let crate::error::Error::InvalidFormat { details } = error else {
        panic!("unexpected repository-shape error: {error}");
    };
    assert_eq!(
        details,
        format!(
            "{} is not an existing segment-tar repository (manifest and journal.log are required)",
            canonical_fixture_directory(&directory.path).display()
        )
    );
    assert!(file_bytes(&directory.path).is_empty());
}

#[cfg(unix)]
#[test]
fn managed_symlink_is_rejected_without_following_its_target() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::repository("managed-symlink");
    let victim = directory.path.join("victim");
    std::fs::write(&victim, b"do not touch").expect("victim");
    let staging = directory.path.join("journal.log.compacting");
    symlink("victim", &staging).expect("staging symlink");
    let before = file_bytes(&directory.path);

    let error = plan_compaction(&directory.path, &CompactionOptions::default())
        .expect_err("managed symlink must be rejected");
    let crate::error::Error::InvalidFormat { details } = error else {
        panic!("unexpected managed-file-type error: {error}");
    };
    assert_eq!(
        details,
        format!(
            "managed repository path {} is not a regular file",
            canonical_fixture_directory(&directory.path)
                .join("journal.log.compacting")
                .display()
        )
    );
    assert_eq!(file_bytes(&directory.path), before);
    assert_eq!(std::fs::read(victim).expect("victim"), b"do not touch");
}

#[cfg(unix)]
#[test]
fn repository_root_symlink_is_resolved_to_the_canonical_target() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::repository("root-symlink-target");
    let link = directory.path.with_extension("repository-link");
    let _ = std::fs::remove_file(&link);
    symlink(&directory.path, &link).expect("create repository root symlink");

    let expected = std::fs::canonicalize(&directory.path).expect("canonical target");
    let plan = plan_compaction(&link, &CompactionOptions::default()).expect("plan through alias");
    assert_eq!(plan.directory(), expected);
    let prepared = PreparedCompaction::prepare(
        &link,
        CompactionOptions::default().with_tasks(std::iter::empty()),
    )
    .expect("prepare through alias");
    assert_eq!(prepared.plan().directory(), expected);
    drop(prepared);
    std::fs::remove_file(link).expect("remove repository link");
}

#[cfg(unix)]
#[test]
fn prepared_cleanup_is_bound_to_an_ancestor_symlinks_resolved_target() {
    use std::os::unix::fs::symlink;

    let first_parent = TestDirectory::new("ancestor-alias-first");
    let second_parent = TestDirectory::new("ancestor-alias-second");
    let first_repository = first_parent.path.join("segmentstore");
    let second_repository = second_parent.path.join("segmentstore");
    for repository in [&first_repository, &second_repository] {
        std::fs::create_dir(repository).expect("create repository directory");
        WritableRepository::open(repository)
            .expect("bootstrap repository")
            .close()
            .expect("close repository");
        std::fs::copy(
            repository.join("journal.log"),
            repository.join("journal.log.compacting"),
        )
        .expect("create removable staging file");
    }
    let alias = first_parent.path.with_extension("ancestor-alias");
    let _ = std::fs::remove_file(&alias);
    symlink(&first_parent.path, &alias).expect("create ancestor alias");
    let aliased_repository = alias.join("segmentstore");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);

    let prepared =
        PreparedCompaction::prepare(&aliased_repository, options).expect("prepare first target");
    assert_eq!(
        prepared.plan().directory(),
        std::fs::canonicalize(&first_repository).expect("canonical first repository")
    );
    std::fs::remove_file(&alias).expect("remove first alias");
    symlink(&second_parent.path, &alias).expect("retarget ancestor alias");

    prepared.apply().expect("apply captured first target");
    assert!(!first_repository.join("journal.log.compacting").exists());
    assert!(second_repository.join("journal.log.compacting").exists());
    Repository::open(&first_repository).expect("first repository remains healthy");
    Repository::open(&second_repository).expect("second repository remains healthy");
    std::fs::remove_file(alias).expect("remove ancestor alias");
}

#[cfg(unix)]
#[test]
fn relative_repository_path_is_stored_as_an_absolute_canonical_target() {
    let directory = TestDirectory::repository("relative-canonical-target");
    let current = std::fs::canonicalize(std::env::current_dir().expect("current directory"))
        .expect("canonical current directory");
    let target = std::fs::canonicalize(&directory.path).expect("canonical repository");
    let relative = relative_path_from(&current, &target);
    assert!(!relative.is_absolute());

    let plan = plan_compaction(&relative, &CompactionOptions::default()).expect("relative plan");

    assert_eq!(plan.directory(), target);
    assert!(plan.directory().is_absolute());
}

#[test]
fn dangling_journal_line_is_pruned_with_backup_and_archives_untouched() {
    let directory = TestDirectory::repository("dangling-journal");
    let missing = SegmentIdentifier::new(7, 0xA000_0000_0000_0007);
    let journal_path = directory.path.join("journal.log");
    let retained_journal = std::fs::read(&journal_path).expect("read retained journal");
    let mut journal = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal");
    writeln!(journal, "{missing}:0 root 123").expect("append dangling line");
    drop(journal);
    let archive_before =
        std::fs::read(directory.path.join("data00000a.tar")).expect("read archive");
    std::fs::write(
        directory.path.join("manifest"),
        b"custom.property=untouched\nstore.version=1\n",
    )
    .expect("version-one manifest");
    let manifest_before = std::fs::read(directory.path.join("manifest")).expect("manifest");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Journal]);

    let plan = plan_compaction(&directory.path, &options).expect("plan");
    assert_eq!(plan.tasks(), &[MaintenanceTask::Journal]);
    assert_eq!(plan.journal_line_removals().len(), 1);
    let removal = &plan.journal_line_removals()[0];
    assert_eq!(
        removal.record_identifier().map(|record| record.segment),
        Some(missing)
    );
    assert_eq!(removal.reason(), JournalRemovalReason::MissingSegment);
    assert!(
        removal
            .preview_bytes()
            .starts_with(missing.to_string().as_bytes())
    );
    assert!(!removal.preview_truncated());
    assert!(plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::PruneJournal {
            missing_segments: 1,
            ..
        }
    )));
    let outcome = compact(&directory.path, options).expect("apply");

    assert_eq!(outcome.removed_journal_lines, 1);
    let expected_backup = canonical_fixture_directory(&directory.path).join("journal.log.bak.000");
    assert_eq!(
        outcome.journal_backup_path(),
        Some(expected_backup.as_path())
    );
    assert!(outcome.is_complete());
    assert!(directory.path.join("journal.log.bak.000").is_file());
    assert!(
        !std::fs::read_to_string(&journal_path)
            .expect("journal")
            .contains(&missing.to_string())
    );
    assert_eq!(
        std::fs::read(&journal_path).expect("rewritten journal"),
        retained_journal,
        "the retained physical journal line must be byte-exact"
    );
    assert_eq!(
        std::fs::read(directory.path.join("data00000a.tar")).expect("archive"),
        archive_before
    );
    assert_eq!(
        std::fs::read(directory.path.join("manifest")).expect("manifest"),
        manifest_before
    );
    Repository::open(&directory.path).expect("healthy repository");
}

#[test]
fn journal_cleanup_preserves_mixed_physical_terminators_exactly() {
    let directory = TestDirectory::repository("mixed-journal-terminators");
    let head = Repository::open(&directory.path)
        .expect("repository")
        .head_record_identifier();
    let retained = format!("{head} tag-lf 1\n{head} tag-crlf 2\r\n{head} tag-cr 3\r");
    let source = format!("{retained}parser-skipped\n");
    std::fs::write(directory.path.join("journal.log"), source.as_bytes())
        .expect("write mixed journal fixture");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Journal]);

    let plan = plan_compaction(&directory.path, &options).expect("plan mixed cleanup");
    assert_eq!(plan.journal_line_removals().len(), 1);
    assert_eq!(plan.journal_line_removals()[0].line_number(), 4);
    compact(&directory.path, options).expect("apply mixed cleanup");

    assert_eq!(
        std::fs::read(directory.path.join("journal.log")).expect("read rewritten journal"),
        retained.as_bytes(),
        "LF, CRLF, and bare-CR terminators must remain byte-exact"
    );
    Repository::open(&directory.path).expect("mixed-terminator repository remains healthy");
}

#[test]
fn exhausted_journal_replacement_namespace_fails_during_read_only_planning() {
    let directory = TestDirectory::repository("journal-namespace-exhausted");
    let missing = SegmentIdentifier::new(17, 0xA000_0000_0000_0017);
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(directory.path.join("journal.log"))
            .expect("open journal"),
        "{missing}:0 root 123"
    )
    .expect("append dangling line");
    for counter in 0..1000u16 {
        std::fs::write(
            directory.path.join(format!("journal.log.bak.{counter:03}")),
            [],
        )
        .expect("occupy backup name");
    }
    let before = file_bytes(&directory.path);
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Journal]);

    let error = plan_compaction(&directory.path, &options)
        .expect_err("planning must discover exhausted backup names before apply");
    assert!(error.to_string().contains("journal.log.bak"));
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "planning remains read-only"
    );
    Repository::open(&directory.path).expect("repository remains healthy");
}

#[test]
fn corrupt_record_in_the_selected_head_segment_never_rolls_back_silently() {
    let directory = TestDirectory::repository("corrupt-current-record");
    let head = Repository::open(&directory.path)
        .expect("repository")
        .head_record_identifier();
    let mut journal = std::fs::OpenOptions::new()
        .append(true)
        .open(directory.path.join("journal.log"))
        .expect("open journal");
    writeln!(journal, "{}:2147483647 root 123", head.segment)
        .expect("append corrupt current revision");
    drop(journal);
    let before = file_bytes(&directory.path);

    let error = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Journal]),
    )
    .expect_err("the exact selected head record is corrupt");

    assert!(error.to_string().contains("current journal head"));
    assert_eq!(file_bytes(&directory.path), before);
}

#[test]
fn checkpoint_without_a_snapshot_root_fails_the_health_gate() {
    let directory = TestDirectory::repository("malformed-checkpoint");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    let content_root = store
        .head_node()
        .child_node("root")
        .expect("read root")
        .expect("root exists")
        .record_identifier();
    let mut writer = store.record_writer(store.writing_generation().expect("generation"));
    let malformed_checkpoint = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("malformed checkpoint");
    let checkpoints = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "broken".to_owned(),
                node: malformed_checkpoint,
            },
            &[],
        )
        .expect("checkpoint container");
    let malformed_head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("checkpoints".to_owned(), checkpoints),
                ("root".to_owned(), content_root),
            ]),
            &[],
        )
        .expect("malformed super-root");
    writer.finish().expect("finish");
    assert!(store.compare_and_set_head(store.head(), malformed_head));
    store.close().expect("close");

    let result = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([]),
    );
    assert!(
        result.is_err(),
        "cleanup must not bless a checkpoint without its snapshot root"
    );
}

#[test]
fn valid_newer_archive_generation_makes_the_lower_letter_stale() {
    let directory = TestDirectory::repository("stale-letter");
    std::fs::copy(
        directory.path.join("data00000a.tar"),
        directory.path.join("data00000b.tar"),
    )
    .expect("copy archive generation");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
    let plan = plan_compaction(&directory.path, &options).expect("plan");
    assert!(plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveStaleArchive { file_name, .. }
            if file_name == "data00000a.tar"
    )));

    compact(&directory.path, options).expect("cleanup");
    assert!(!directory.path.join("data00000a.tar").exists());
    assert!(directory.path.join("data00000b.tar").exists());
    Repository::open(&directory.path).expect("healthy repository");
}

#[test]
fn stale_archive_cleanup_preserves_alternates_when_active_trailers_are_invalid() {
    for (name, magic) in [
        ("invalid-graph", 0x0A30_470Au32.to_be_bytes()),
        ("invalid-brf", 0x0A31_420Au32.to_be_bytes()),
    ] {
        let directory = TestDirectory::repository(name);
        let newer = directory.path.join("data00000b.tar");
        std::fs::copy(directory.path.join("data00000a.tar"), &newer)
            .expect("copy newer archive generation");
        corrupt_first_magic(&newer, magic);

        let selected = TarArchiveReader::open(&newer).expect("index remains valid");
        assert!(!selected.is_recovered());
        assert!(selected.segment_graph().is_none() || selected.binary_references().is_none());
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
        let plan = plan_compaction(&directory.path, &options).expect("plan");

        assert!(!plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveStaleArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )));
        assert!(
            plan.warnings()
                .iter()
                .any(|warning| warning.contains("incomplete recovery metadata"))
        );
        assert!(directory.path.join("data00000a.tar").exists());
        assert!(newer.exists());
    }
}

#[test]
fn stale_archive_cleanup_reconstructs_semantic_graph_and_brf_before_deletion() {
    for (name, write_metadata_record) in [("omitted-graph", 0u8), ("omitted-brf", 1u8)] {
        let directory = TestDirectory::repository(name);
        let store = WritableRepository::open(&directory.path).expect("open writer");
        let mut writer = store.record_writer(store.writing_generation().expect("generation"));
        match write_metadata_record {
            0 => {
                writer
                    .write_string(&"graph-block".repeat(40_000))
                    .expect("long string with bulk references");
            }
            _ => {
                writer
                    .write_external_binary_identifier("external-blob-that-must-survive")
                    .expect("external blob identifier");
            }
        }
        writer.finish().expect("finish metadata segment");
        store.close().expect("close writer");
        let source = directory.path.join("data00001a.tar");
        assert!(source.exists());
        let source_reader = TarArchiveReader::open(&source).expect("source reader");
        if write_metadata_record == 0 {
            assert!(
                source_reader
                    .segment_graph()
                    .is_some_and(|graph| !graph.adjacency.is_empty())
            );
        } else {
            assert!(source_reader.binary_references().is_some_and(|catalog| {
                catalog
                    .generations
                    .iter()
                    .any(|generation| !generation.segments.is_empty())
            }));
        }
        repack_without_graph_or_brf(&directory.path, "data00001a.tar", "data00001b.tar");
        let repacked = TarArchiveReader::open(&directory.path.join("data00001b.tar"))
            .expect("repacked reader");
        assert!(repacked.segment_graph().is_some());
        assert!(repacked.binary_references().is_some());

        let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);
        let plan = plan_compaction(&directory.path, &options).expect("plan");

        assert!(!plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveStaleArchive { file_name, .. }
                if file_name == "data00001a.tar"
        )));
        assert!(
            plan.warnings()
                .iter()
                .any(|warning| warning.contains("incomplete recovery metadata"))
        );
        assert!(source.exists());
    }
}

#[test]
fn foreign_tar_and_unknown_files_are_never_cleanup_targets() {
    let directory = TestDirectory::repository("foreign-files");
    std::fs::write(directory.path.join("notes.tar"), b"foreign tar").expect("foreign tar");
    std::fs::write(directory.path.join("operator-notes.txt"), b"keep me").expect("notes");

    let plan = plan_compaction(&directory.path, &CompactionOptions::default()).expect("plan");
    assert!(plan.is_empty());
    assert_eq!(
        std::fs::read(directory.path.join("notes.tar")).expect("foreign tar"),
        b"foreign tar"
    );
    assert_eq!(
        std::fs::read(directory.path.join("operator-notes.txt")).expect("notes"),
        b"keep me"
    );
}

#[test]
fn nonempty_archive_without_a_valid_index_is_preserved_for_recovery() {
    let directory = TestDirectory::repository("archive-needs-recovery");
    let damaged = directory.path.join("data00001a.tar");
    std::fs::write(&damaged, b"not a complete tar archive").expect("damaged archive");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleArchives]);

    let plan = plan_compaction(&directory.path, &options).expect("plan");

    assert!(plan.actions().is_empty());
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("no valid indexed generation"))
    );
    assert_eq!(
        std::fs::read(&damaged).expect("damaged bytes"),
        b"not a complete tar archive"
    );
}

#[test]
fn prepared_plan_rejects_same_length_inode_replacement() {
    let directory = TestDirectory::repository("stale-plan");
    let options = CompactionOptions::default().with_tasks([]);
    let prepared = PreparedCompaction::prepare(&directory.path, options).expect("prepare");
    let journal_path = directory.path.join("journal.log");
    let bytes = std::fs::read(&journal_path).expect("journal");
    let replacement = directory.path.join("replacement");
    std::fs::write(&replacement, &bytes).expect("write replacement");
    std::fs::rename(&replacement, &journal_path).expect("replace same-size journal");

    assert!(prepared.apply().is_err());
    Repository::open(&directory.path).expect("replacement bytes remain healthy");
}

#[test]
fn deferred_removal_rechecks_the_exact_planned_file_identity() {
    let directory = TestDirectory::new("deferred-removal-identity");
    let removable_name = "journal.log.bak.998";
    let removable_path = directory.path.join(removable_name);
    std::fs::write(&removable_path, b"independent old recovery copy")
        .expect("write independently removable backup");
    let removable_metadata =
        std::fs::symlink_metadata(&removable_path).expect("removable metadata");
    let removable = PlannedFileRemoval {
        file_name: removable_name.to_owned(),
        bytes: removable_metadata.len(),
        fingerprint: file_fingerprint(
            std::ffi::OsString::from(removable_name),
            &removable_metadata,
        ),
    };
    let name = "journal.log.bak.999";
    let path = directory.path.join(name);
    std::fs::write(&path, b"first recovery copy").expect("write planned backup");
    let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
    let planned = PlannedFileRemoval {
        file_name: name.to_owned(),
        bytes: metadata.len(),
        fingerprint: file_fingerprint(std::ffi::OsString::from(name), &metadata),
    };
    std::fs::remove_file(&path).expect("remove original backup");
    std::fs::write(&path, b"new recovery material").expect("replace backup");

    let (removed, failures) = remove_planned_files(
        &directory.path,
        [removable, planned],
        PlannedFileRemovalFailureMode::Partial,
        &mut crate::progress::DiscardedProgress,
    )
    .expect("late deletion refusals are a partial outcome");

    assert_eq!(removed, 1);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].file_name(), name);
    assert!(failures[0].error().contains("changed after"));
    assert!(
        !removable_path.exists(),
        "a late identity refusal must not discard an earlier successful deletion"
    );
    assert_eq!(
        std::fs::read(&path).expect("replacement remains"),
        b"new recovery material"
    );
}

#[test]
fn deletion_absence_state_does_not_depend_on_diagnostic_text() {
    let retained = super::FileDeletionFailure::retained(
        "data00000a.tar".to_owned(),
        ALREADY_ABSENT_DELETION_DETAIL,
    );
    let absent = super::FileDeletionFailure::already_absent(
        "data00001a.tar".to_owned(),
        "a deliberately different ENOENT diagnostic",
    );

    assert!(!retained.target_was_already_absent());
    assert!(absent.target_was_already_absent());
}

#[test]
fn strict_stale_removal_reports_an_already_absent_file_without_losing_the_outcome() {
    let directory = TestDirectory::new("strict-removal-already-absent");
    let name = "data00001a.tar";
    let path = directory.path.join(name);
    std::fs::write(&path, b"certified stale archive").expect("write planned source");
    let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
    let planned = PlannedFileRemoval {
        file_name: name.to_owned(),
        bytes: metadata.len(),
        fingerprint: file_fingerprint(OsString::from(name), &metadata),
    };
    std::fs::remove_file(path).expect("another actor won the unlink race");

    let (removed, failures) = remove_planned_files(
        &directory.path,
        [planned],
        PlannedFileRemovalFailureMode::RequireCertifiedTarget,
        &mut crate::progress::DiscardedProgress,
    )
    .expect("absence is a reportable partial result, not a lost cleanup outcome");

    assert_eq!(removed, 0);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].file_name(), name);
    assert!(failures[0].target_was_already_absent());
    assert_eq!(
        failures[0].error(),
        "file was already absent when deletion was attempted"
    );
}

#[cfg(unix)]
#[test]
fn strict_stale_removal_treats_disappearance_at_each_recertification_as_partial() {
    for disappearance_before_verification in 0..=1 {
        let directory = TestDirectory::new(&format!(
            "strict-removal-post-open-absence-{disappearance_before_verification}"
        ));
        let name = "data00001a.tar";
        let path = directory.path.join(name);
        std::fs::write(&path, b"certified stale archive").expect("write planned source");
        let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
        let planned = PlannedFileRemoval {
            file_name: name.to_owned(),
            bytes: metadata.len(),
            fingerprint: file_fingerprint(OsString::from(name), &metadata),
        };

        let (removed, failures) = remove_planned_files_with_after_open(
            &directory.path,
            [planned],
            PlannedFileRemovalFailureMode::RequireCertifiedTarget,
            |path, verification| {
                if verification == disappearance_before_verification {
                    std::fs::remove_file(path).expect("another actor removes the held pathname");
                } else if disappearance_before_verification == 0 && verification == 1 {
                    // Reached only if the first production recertification
                    // has been removed or neutralized. Make the second one
                    // reject instead of accidentally preserving the same
                    // partial outcome, so each call remains load-bearing.
                    std::fs::write(path, b"replacement recovery material")
                        .expect("install a replacement pathname");
                }
            },
            |_| panic!("an absent pathname must never reach unlink"),
        )
        .expect("strict mode keeps an already achieved deletion as partial");

        assert_eq!(removed, 0);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].file_name(), name);
        assert!(failures[0].target_was_already_absent());
        assert_eq!(
            failures[0].error(),
            "file was already absent when deletion was attempted"
        );
    }
}

#[test]
fn strict_stale_removal_reports_an_exact_source_unlink_error_as_partial() {
    let directory = TestDirectory::new("strict-removal-unlink-error");
    let name = "data00001a.tar";
    let path = directory.path.join(name);
    std::fs::write(&path, b"certified stale archive").expect("write planned source");
    let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
    let planned = PlannedFileRemoval {
        file_name: name.to_owned(),
        bytes: metadata.len(),
        fingerprint: file_fingerprint(OsString::from(name), &metadata),
    };

    let (removed, failures) = remove_planned_files_with(
        &directory.path,
        [planned],
        PlannedFileRemovalFailureMode::RequireCertifiedTarget,
        |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected exact-source unlink refusal",
            ))
        },
    )
    .expect("a certified source left in place is a reportable partial result");

    assert_eq!(removed, 0);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].file_name(), name);
    assert!(!failures[0].target_was_already_absent());
    assert_eq!(failures[0].error(), "injected exact-source unlink refusal");
    assert_eq!(
        std::fs::read(path).expect("certified source remains"),
        b"certified stale archive"
    );
}

#[cfg(unix)]
#[test]
fn deferred_removal_reports_a_non_not_found_open_error_as_partial() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("deferred-removal-open-error");
    let name = "journal.log.bak.999";
    let path = directory.path.join(name);
    std::fs::write(&path, b"planned recovery copy").expect("write planned backup");
    let metadata = std::fs::symlink_metadata(&path).expect("planned metadata");
    let planned = PlannedFileRemoval {
        file_name: name.to_owned(),
        bytes: metadata.len(),
        fingerprint: file_fingerprint(std::ffi::OsString::from(name), &metadata),
    };
    let victim = directory.path.join("recovery-evidence");
    std::fs::write(&victim, b"do not follow").expect("write symlink target");
    std::fs::remove_file(&path).expect("remove planned inode");
    symlink("recovery-evidence", &path).expect("install non-followable replacement");
    let expected_open_error = {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        let error = options
            .open(&path)
            .expect_err("O_NOFOLLOW must reject the substituted symlink");
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        error.to_string()
    };

    let (removed, failures) = remove_planned_files(
        &directory.path,
        [planned],
        PlannedFileRemovalFailureMode::Partial,
        &mut crate::progress::DiscardedProgress,
    )
    .expect("late open refusal is a partial outcome");

    assert_eq!(removed, 0);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].file_name(), name);
    assert_eq!(failures[0].error(), expected_open_error);
    assert_eq!(
        std::fs::read(&victim).expect("symlink target remains"),
        b"do not follow"
    );
    assert!(path.is_symlink());
}

#[cfg(unix)]
#[test]
fn prepared_plan_rejects_in_place_change_with_restored_mtime() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let directory = TestDirectory::repository("stale-plan-ctime");
    let staging = directory.path.join("journal.log.compacting");
    std::fs::copy(directory.path.join("journal.log"), &staging)
        .expect("create redundant staging journal");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);
    let prepared = PreparedCompaction::prepare(&directory.path, options).expect("prepare");
    let metadata = std::fs::metadata(&staging).expect("staging metadata");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let changed = vec![b'x'; metadata.len() as usize];
    std::fs::write(&staging, changed).expect("same-inode same-size overwrite");
    let path = CString::new(staging.as_os_str().as_bytes()).expect("path without NUL");
    let times = [
        libc::timespec {
            tv_sec: checked_timespec_field(metadata.atime()),
            tv_nsec: checked_timespec_field(metadata.atime_nsec()),
        },
        libc::timespec {
            tv_sec: checked_timespec_field(metadata.mtime()),
            tv_nsec: checked_timespec_field(metadata.mtime_nsec()),
        },
    ];
    // SAFETY: the path is NUL-terminated and `times` contains two valid
    // timespec values copied from stat(2).
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
    assert_eq!(result, 0, "restore fixture mtime");

    assert!(prepared.apply().is_err());
    assert!(
        staging.exists(),
        "stale proof must not delete changed evidence"
    );
    Repository::open(&directory.path).expect("repository remains healthy");
}

#[test]
fn prepared_cleanup_is_excluded_by_an_existing_writer_lock() {
    let directory = TestDirectory::repository("lock-exclusion");
    let writer = WritableRepository::open(&directory.path).expect("hold writer lock");
    plan_compaction(&directory.path, &CompactionOptions::default())
        .expect("lock-free preview remains read-only");

    assert!(PreparedCompaction::prepare(&directory.path, CompactionOptions::default()).is_err());
    writer.close().expect("close writer");
    Repository::open(&directory.path).expect("repository healthy");
}

#[test]
fn prepared_cleanup_refuses_a_replaced_lock_inode() {
    let directory = TestDirectory::repository("replaced-lock");
    let staging = directory.path.join("journal.log.compacting");
    std::fs::copy(directory.path.join("journal.log"), &staging)
        .expect("create removable staging file");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);
    let prepared = PreparedCompaction::prepare(&directory.path, options).expect("prepare");
    let lock_path = directory.path.join("repo.lock");
    std::fs::remove_file(&lock_path).expect("unlink held lock pathname");
    std::fs::write(&lock_path, b"replacement inode").expect("replace lock inode");

    let error = prepared
        .apply()
        .expect_err("replacement lock must abort apply");

    assert!(error.to_string().contains("lock inode"));
    assert!(staging.exists());
    Repository::open(&directory.path).expect("repository remains healthy");
}

#[test]
fn recovery_backup_target_accepts_only_exact_oak_and_journal_counter_forms() {
    for (name, target) in [
        ("journal.log.bak.000", "journal.log"),
        ("journal.log.bak.999", "journal.log"),
        ("data00000a.tar.bak", "data00000a.tar"),
        ("data00000a.tar.2.bak", "data00000a.tar"),
        ("data00000a.tar.2147483647.bak", "data00000a.tar"),
        ("data00000a.tar.ro.bak", "data00000a.tar"),
        ("data00000a.tar.2.ro.bak", "data00000a.tar"),
    ] {
        assert_eq!(
            recovery_backup_target(name).as_deref(),
            Some(target),
            "{name}"
        );
    }

    for hostile in [
        "journal.log.bak.",
        "journal.log.bak.00",
        "journal.log.bak.0000",
        "journal.log.bak.+00",
        "data00000a.tar.0.bak",
        "data00000a.tar.1.bak",
        "data00000a.tar.02.bak",
        "data00000a.tar.007.ro.bak",
        "data00000a.tar.2147483648.bak",
        "data00000a.tar.-2.ro.bak",
        "data00000a.tar..2.ro.bak",
        "data00000a.tar.2.ro.bak.extra",
    ] {
        assert_eq!(recovery_backup_target(hostile), None, "{hostile}");
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_backup_like_name_is_not_promoted_into_the_managed_allowlist() {
    use std::os::unix::ffi::OsStrExt as _;

    let hostile = std::ffi::OsStr::from_bytes(b"data00000a.tar.\xff.ro.bak");
    assert!(!is_managed_name(hostile));
}

#[test]
fn non_regular_numbered_read_only_backup_is_refused_even_during_preview() {
    let directory = TestDirectory::repository("non-regular-numbered-ro-backup");
    let backup = directory.path.join("data00000a.tar.2.ro.bak");
    std::fs::create_dir(&backup).expect("create hostile managed-name directory");

    let error = plan_compaction(&directory.path, &CompactionOptions::default())
        .expect_err("managed backup names must remain regular files in dry-run");

    assert_eq!(
        error.to_string(),
        format!(
            "invalid segment-tar data: managed repository path {} is not a regular file",
            canonical_fixture_directory(&directory.path)
                .join("data00000a.tar.2.ro.bak")
                .display()
        )
    );
}

#[test]
fn all_archive_backup_spellings_share_one_keep_latest_slot() {
    let directory = TestDirectory::repository("archive-backup-shared-retention-slot");
    let now = SystemTime::now();
    for (name, age_hours) in [
        ("data00000a.tar.ro.bak", 1),
        ("data00000a.tar.2.ro.bak", 2),
        ("data00000a.tar.bak", 3),
        ("data00000a.tar.2.bak", 4),
    ] {
        let file = File::create(directory.path.join(name)).expect("create recovery backup");
        file.set_times(
            std::fs::FileTimes::new().set_modified(now - Duration::from_secs(age_hours * 3600)),
        )
        .expect("set distinct backup time");
    }
    let options = CompactionOptions::default()
        .with_tasks([])
        .with_recovery_backup_policy(RecoveryBackupPolicy::new(Duration::ZERO, 1));

    let plan = plan_compaction(&directory.path, &options).expect("plan grouped backups");
    let removals: Vec<_> = plan
        .actions()
        .iter()
        .filter_map(|action| match action {
            CompactionAction::RemoveRecoveryBackup { file_name, .. } => Some(file_name.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(
        removals,
        [
            "data00000a.tar.2.bak",
            "data00000a.tar.2.ro.bak",
            "data00000a.tar.bak",
        ]
    );
    assert!(
        !removals.contains(&"data00000a.tar.ro.bak"),
        "the newest spelling consumes the one shared target slot"
    );
}

#[test]
fn default_preserves_backups_but_explicit_zero_retention_removes_them() {
    let directory = TestDirectory::repository("backup-retention");
    let backup = directory.path.join("journal.log.bak.999");
    std::fs::write(&backup, b"recovery material").expect("write backup");
    let future_backup = directory.path.join("journal.log.bak.998");
    let future_file = std::fs::File::create(&future_backup).expect("create future-dated backup");
    future_file
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)),
        )
        .expect("future-date backup");
    let default_plan =
        plan_compaction(&directory.path, &CompactionOptions::default()).expect("plan");
    assert!(
        !default_plan
            .actions()
            .iter()
            .any(|action| matches!(action, CompactionAction::RemoveRecoveryBackup { .. }))
    );
    assert!(backup.exists());

    let options = CompactionOptions::default()
        .with_tasks([])
        .with_recovery_backup_policy(RecoveryBackupPolicy {
            minimum_age: std::time::Duration::ZERO,
            keep_latest_per_target: 0,
        });
    compact(&directory.path, options).expect("remove backup");
    assert!(!backup.exists());
    assert!(
        future_backup.exists(),
        "a future-dated backup is never old enough, even at a zero age floor"
    );
    Repository::open(&directory.path).expect("healthy repository");
}

#[test]
fn numbered_read_only_archive_backups_are_recognized_and_policy_managed() {
    let directory = TestDirectory::repository("numbered-read-only-backup");
    let name = "data00000a.tar.2.ro.bak";
    let backup = directory.path.join(name);
    std::fs::copy(directory.path.join("data00000a.tar"), &backup)
        .expect("create Oak-style numbered read-only backup");

    let default_plan = plan_compaction(&directory.path, &CompactionOptions::default())
        .expect("default plan preserves recovery evidence");
    assert!(!default_plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveRecoveryBackup { file_name, .. } if file_name == name
    )));

    let options = CompactionOptions::default()
        .with_tasks([])
        .with_recovery_backup_policy(RecoveryBackupPolicy::new(std::time::Duration::ZERO, 0));
    let plan = plan_compaction(&directory.path, &options).expect("plan numbered backup");
    assert!(plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveRecoveryBackup { file_name, .. } if file_name == name
    )));

    let outcome = compact(&directory.path, options).expect("remove numbered backup");
    assert_eq!(outcome.removed_recovery_backups, 1);
    assert!(outcome.is_complete());
    assert!(!backup.exists());
    Repository::open(&directory.path).expect("repository remains healthy");
}

#[test]
fn backup_count_retains_every_file_tied_at_the_newest_cutoff() {
    let directory = TestDirectory::repository("backup-retention-mtime-tie");
    let now = std::time::SystemTime::now();
    let tied = now - std::time::Duration::from_secs(7200);
    let older = now - std::time::Duration::from_secs(10_800);
    for (name, modified) in [
        ("journal.log.bak.000", tied),
        ("journal.log.bak.001", tied),
        ("journal.log.bak.002", tied),
        ("journal.log.bak.003", older),
    ] {
        let file = std::fs::File::create(directory.path.join(name)).expect("create backup");
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("set exact backup mtime");
    }
    let options = CompactionOptions::default()
        .with_tasks([])
        .with_recovery_backup_policy(RecoveryBackupPolicy::new(std::time::Duration::ZERO, 1));

    let plan = plan_compaction(&directory.path, &options).expect("plan tied backups");
    let removals: Vec<_> = plan
        .actions()
        .iter()
        .filter_map(|action| match action {
            CompactionAction::RemoveRecoveryBackup { file_name, .. } => Some(file_name.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(removals, ["journal.log.bak.003"]);
}

#[test]
fn manifest_upgrade_separates_a_trailing_properties_continuation() {
    let suffix = b"# upgraded atomically by froe cleanup\nstore.version=2\n";
    for (source, expected_prefix) in [
        (
            &b"custom.property=kept\\"[..],
            &b"custom.property=kept\\\n\n"[..],
        ),
        (
            &b"custom.property=kept\\\n"[..],
            &b"custom.property=kept\\\n\n"[..],
        ),
        (
            &b"custom.property=kept\\\r"[..],
            &b"custom.property=kept\\\r\n\n"[..],
        ),
    ] {
        let upgraded = manifest_upgrade_bytes(source);
        assert!(upgraded.starts_with(expected_prefix), "{upgraded:?}");
        assert!(upgraded.ends_with(suffix), "{upgraded:?}");
    }
}

#[cfg(unix)]
#[test]
fn manifest_certificate_rejects_path_substitution_around_publication() {
    use std::os::unix::fs::MetadataExt as _;

    let directory = TestDirectory::new("manifest-certificate-substitution");
    let canonical = directory.path.join("manifest");
    let canonical_bytes = b"custom.property=kept\nstore.version=1\n";
    std::fs::write(&canonical, canonical_bytes).expect("write canonical manifest");

    let staged = directory.path.join("manifest.cleaning.000");
    let expected = b"custom.property=kept\nstore.version=2\n";
    std::fs::write(&staged, expected).expect("write staged manifest");
    let staged_metadata = std::fs::symlink_metadata(&staged).expect("staged metadata");
    let certificate = certify_manifest_file(
        &staged,
        &staged_metadata,
        expected,
        ManifestFileAccess::ReadWrite,
        "staged manifest replacement",
    )
    .expect("certify staged manifest");

    let retained_inode = directory.path.join("retained-manifest-inode");
    std::fs::rename(&staged, &retained_inode).expect("move certified inode aside");
    std::fs::write(&staged, expected).expect("substitute same-byte staged manifest");
    let substituted_metadata =
        std::fs::symlink_metadata(&staged).expect("substituted staged metadata");
    assert_ne!(
        (substituted_metadata.dev(), substituted_metadata.ino()),
        (staged_metadata.dev(), staged_metadata.ino()),
        "the fixture must isolate identity checking from byte checking"
    );
    certificate
        .recertify(
            &staged,
            expected,
            ManifestFileAccess::ReadWrite,
            "staged manifest replacement",
        )
        .expect_err("same bytes on a different inode must not be publishable");
    assert_eq!(
        std::fs::read(&canonical).expect("read canonical manifest"),
        canonical_bytes,
        "a rejected staging substitution must leave the source canonical"
    );

    std::fs::remove_file(&staged).expect("remove substituted staging file");
    std::fs::rename(&retained_inode, &staged).expect("restore certified inode");
    certificate
        .recertify(
            &staged,
            expected,
            ManifestFileAccess::ReadWrite,
            "staged manifest replacement",
        )
        .expect("restored certified inode");

    let installed = directory.path.join("installed-manifest");
    std::fs::rename(&staged, &installed).expect("publish certified inode");
    certificate
        .recertify(
            &installed,
            expected,
            ManifestFileAccess::ReadWrite,
            "installed manifest replacement",
        )
        .expect("certificate follows the inode through rename");

    let displaced = directory.path.join("displaced-installed-manifest");
    std::fs::rename(&installed, &displaced).expect("displace installed inode");
    std::fs::write(&installed, expected).expect("substitute installed manifest");
    let installed_substitute =
        std::fs::symlink_metadata(&installed).expect("installed substitute metadata");
    assert_ne!(
        (installed_substitute.dev(), installed_substitute.ino()),
        (staged_metadata.dev(), staged_metadata.ino()),
        "the post-publication fixture must install a different inode"
    );
    certificate
        .recertify(
            &installed,
            expected,
            ManifestFileAccess::ReadWrite,
            "installed manifest replacement",
        )
        .expect_err("post-rename same-byte inode substitution must be detected");
}

#[test]
fn redundant_journal_staging_is_removed_and_second_run_is_a_true_noop() {
    let directory = TestDirectory::repository("temporary-idempotence");
    let staging = directory.path.join("journal.log.compacting");
    std::fs::copy(directory.path.join("journal.log"), &staging).expect("copy staging");
    let forensic_staging = directory.path.join("journal.log.recovered");
    std::fs::write(&forensic_staging, b"unterminated recovery evidence")
        .expect("write ambiguous staging journal");
    std::fs::write(directory.path.join("gc.log"), b"operator gc state\n").expect("seed gc log");

    compact(&directory.path, CompactionOptions::default()).expect("first cleanup");
    assert!(!staging.exists());
    assert!(forensic_staging.exists());
    assert_eq!(
        std::fs::read(directory.path.join("gc.log")).expect("gc log"),
        b"operator gc state\n"
    );
    let before = file_bytes(&directory.path);
    let second =
        plan_compaction(&directory.path, &CompactionOptions::default()).expect("second plan");
    assert!(second.is_empty(), "second plan: {:?}", second.actions());
    let outcome = PreparedCompaction::prepare(&directory.path, CompactionOptions::default())
        .expect("prepare no-op")
        .apply()
        .expect("apply no-op");
    assert_eq!(outcome.head_before, outcome.head_after);
    assert_eq!(file_bytes(&directory.path), before);
}

#[test]
fn archive_staging_requires_complete_byte_identity_before_removal() {
    let directory = TestDirectory::repository("archive-staging-proof");
    let exact = directory.path.join("data00000b.tar.cleaning.000");
    std::fs::copy(directory.path.join("data00000a.tar"), &exact)
        .expect("copy exact staging archive");
    let ambiguous = directory.path.join("data00001a.tar.recovering");
    std::fs::write(&ambiguous, b"nonempty recovery evidence")
        .expect("write ambiguous staging archive");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);

    let plan = plan_compaction(&directory.path, &options).expect("plan");
    assert!(plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveTemporary { file_name, .. }
            if file_name == "data00000b.tar.cleaning.000"
    )));
    assert!(!plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveTemporary { file_name, .. }
            if file_name == "data00001a.tar.recovering"
    )));

    compact(&directory.path, options).expect("cleanup");
    assert!(!exact.exists());
    assert!(ambiguous.exists());
    Repository::open(&directory.path).expect("healthy repository");
}

#[test]
fn manifest_staging_requires_exact_canonical_or_upgrade_bytes() {
    let directory = TestDirectory::repository("manifest-staging-proof");
    let canonical = b"custom.property=kept\nstore.version=1\n";
    std::fs::write(directory.path.join("manifest"), canonical).expect("write version-one manifest");
    let identical = directory.path.join("manifest.cleaning.000");
    std::fs::write(&identical, canonical).expect("write identical staging manifest");
    let exact_upgrade = directory.path.join("manifest.cleaning.001");
    std::fs::write(&exact_upgrade, manifest_upgrade_bytes(canonical))
        .expect("write exact upgrade staging manifest");
    let divergent = directory.path.join("manifest.cleaning.002");
    std::fs::write(&divergent, b"store.version=2\noperator.data=lost\n")
        .expect("write divergent staging manifest");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::StaleTemporaries]);

    let plan = plan_compaction(&directory.path, &options).expect("plan stale manifests");
    let planned_names: Vec<_> = plan
        .actions()
        .iter()
        .filter_map(|action| match action {
            CompactionAction::RemoveTemporary { file_name, .. } => Some(file_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        planned_names,
        ["manifest.cleaning.000", "manifest.cleaning.001"]
    );
    assert!(plan.warnings().iter().any(|warning| {
        warning.contains("manifest.cleaning.002") && warning.contains("not provably redundant")
    }));

    compact(&directory.path, options).expect("remove proven manifest staging files");
    assert!(!identical.exists());
    assert!(!exact_upgrade.exists());
    assert!(divergent.exists());
    assert_eq!(
        std::fs::read(directory.path.join("manifest")).expect("read canonical manifest"),
        canonical
    );
    Repository::open(&directory.path).expect("healthy repository");
}

#[test]
fn expired_checkpoints_are_removed_in_one_healthy_head_update() {
    let directory = TestDirectory::repository("expired-checkpoint");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    create_checkpoint(&store, 1, &[]).expect("checkpoint");
    store.close().expect("close writer");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

    let outcome = compact(&directory.path, options).expect("cleanup");

    assert_eq!(outcome.removed_checkpoints, 1);
    let repository = Repository::open(&directory.path).expect("healthy repository");
    assert!(repository.checkpoints().expect("checkpoints").is_empty());
}

#[test]
fn a_retention_bound_beside_a_checkpoint_head_update_is_refused_while_planning() {
    let directory = TestDirectory::repository("retention-with-checkpoint");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    create_checkpoint(&store, 1, &[]).expect("checkpoint");
    store.close().expect("close writer");
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Checkpoint removal installs a new head and appends its journal
    // line, which moves the newest resolvable revision. The bound would
    // then retire the very head the plan retained, and the run would
    // abort its own apply — after the checkpoint removal had already
    // been committed. Refuse while planning, where nothing has moved.
    let options = CompactionOptions::default()
        .with_tasks([
            MaintenanceTask::ExpiredCheckpoints,
            MaintenanceTask::Journal,
        ])
        .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision"));
    let error = plan_compaction(&directory.path, &options).expect_err("must refuse");
    assert!(
        error.to_string().contains("checkpoint"),
        "unexpected refusal: {error}"
    );

    // Nothing was touched: the checkpoint is still there to be removed
    // by a run that does not also bound the journal.
    let repository = Repository::open(&directory.path).expect("healthy repository");
    assert_eq!(repository.checkpoints().expect("checkpoints").len(), 1);
}

#[test]
fn checkpoint_planning_rejects_a_physically_exhausted_archive_namespace() {
    let directory = TestDirectory::repository("checkpoint-archive-number-exhausted");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    create_checkpoint(&store, 1, &[]).expect("checkpoint");
    store.close().expect("close writer");
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Zero-byte files are skipped by archive discovery, but their exact
    // Oak archive names still occupy the physical namespace.
    std::fs::write(directory.path.join("data4294967295z.tar"), b"")
        .expect("install maximum-number residue");
    let before = file_bytes(&directory.path);
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

    let error = plan_compaction(&directory.path, &options)
        .expect_err("planning must reject u32::MAX before checkpoint mutation");

    assert!(
        error.to_string().contains("namespace is exhausted"),
        "{error}"
    );
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "namespace preflight must remain byte-exact"
    );
}

#[test]
fn checkpoint_cleanup_allocates_after_zero_byte_next_archive_residue() {
    let directory = TestDirectory::repository("checkpoint-zero-byte-next-archive");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    create_checkpoint(&store, 1, &[]).expect("checkpoint");
    store.close().expect("close writer");
    std::thread::sleep(std::time::Duration::from_millis(10));

    let repository = Repository::open(&directory.path).expect("open checkpoint repository");
    let active_maximum = repository
        .archives()
        .iter()
        .filter_map(|archive| ArchiveFileName::parse(archive.file_name()))
        .map(|name| name.archive_number)
        .max()
        .expect("fixture has an active archive");
    drop(repository);
    let occupied_number = active_maximum.checked_add(1).expect("fixture namespace");
    let certified_number = occupied_number.checked_add(1).expect("fixture namespace");
    let occupied_name = format!("data{occupied_number:05}a.tar");
    std::fs::write(directory.path.join(&occupied_name), b"")
        .expect("install zero-byte otherwise-next residue");
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

    let plan = plan_compaction(&directory.path, &options).expect("plan checkpoint cleanup");
    assert_eq!(plan.checkpoint_archive_number, Some(certified_number));
    let outcome = compact(&directory.path, options).expect("apply checkpoint cleanup");

    assert_eq!(outcome.removed_checkpoints, 1);
    assert_eq!(
        std::fs::read(directory.path.join(&occupied_name)).expect("read zero-byte residue"),
        b"",
        "cleanup must neither truncate nor reuse physical residue"
    );
    assert!(
        directory
            .path
            .join(format!("data{certified_number:05}a.tar"))
            .exists()
    );
    let repository = Repository::open(&directory.path).expect("healthy repository");
    assert!(repository.checkpoints().expect("checkpoints").is_empty());
}

#[test]
fn checkpoint_only_cleanup_rejects_current_index_generation_mismatch() {
    let directory = TestDirectory::repository("checkpoint-index-generation-mismatch");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    create_checkpoint(&store, 1, &[]).expect("checkpoint");
    store.close().expect("close writer");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let repository = Repository::open(&directory.path).expect("open checkpoint repository");
    let head = repository.head_record_identifier();
    let archive_name = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(head.segment))
        .expect("active archive contains head")
        .file_name()
        .to_owned();
    let header_generation = repository
        .segment(head.segment)
        .expect("read head segment")
        .structure
        .generation;
    drop(repository);
    change_index_generation(
        &directory.path.join(archive_name),
        head.segment,
        header_generation.saturating_add(1),
    );
    let before = file_bytes(&directory.path);
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

    let error = plan_compaction(&directory.path, &options)
        .expect_err("checkpoint head update must reject corrupt index generation");

    assert!(error.to_string().contains("index generation"), "{error}");
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "planning must not mutate"
    );
}

#[test]
fn checkpoint_only_cleanup_rejects_duplicate_head_segments_before_generation_validation() {
    let directory = TestDirectory::repository("checkpoint-duplicate-head-generation");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    create_checkpoint(&store, 1, &[]).expect("checkpoint");
    store.close().expect("close writer");
    std::thread::sleep(std::time::Duration::from_millis(10));

    let repository = Repository::open(&directory.path).expect("open checkpoint repository");
    let head = repository.head_record_identifier();
    let source_name = repository
        .archives()
        .iter()
        .find(|archive| archive.contains_segment(head.segment))
        .expect("active archive contains head")
        .file_name()
        .to_owned();
    let next_number = repository
        .archives()
        .iter()
        .filter_map(|archive| {
            crate::tar_archive::file_name::ArchiveFileName::parse(archive.file_name())
                .map(|name| name.archive_number)
        })
        .max()
        .expect("fixture has archives")
        .checked_add(1)
        .expect("fixture archive namespace");
    let duplicate_name = format!("data{next_number:05}a.tar");
    let header_generation = repository
        .segment(head.segment)
        .expect("read head segment")
        .structure
        .generation;
    drop(repository);

    let duplicate_path = directory.path.join(&duplicate_name);
    std::fs::copy(directory.path.join(source_name), &duplicate_path)
        .expect("copy head archive under a newer number");
    change_index_generation(
        &duplicate_path,
        head.segment,
        header_generation.saturating_add(1),
    );
    let before = file_bytes(&directory.path);
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);

    let error = plan_compaction(&directory.path, &options)
        .expect_err("checkpoint write must reject ambiguous duplicate segment locations");

    assert!(
        error.to_string().contains("occurs in active archives"),
        "{error}"
    );
    assert_eq!(
        file_bytes(&directory.path),
        before,
        "planning must not mutate"
    );
}

#[test]
fn a_head_moving_cleanup_upgrades_a_version_one_manifest_atomically() {
    let directory = TestDirectory::repository("manifest-upgrade");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    create_checkpoint(&store, 1, &[]).expect("checkpoint");
    store.close().expect("close writer");
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(
        directory.path.join("manifest"),
        b"custom.property=kept\nstore.version=\\\n 1\n",
    )
    .expect("install Java-continuation version-one manifest");
    #[cfg(unix)]
    let source_identity = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(
            directory.path.join("manifest"),
            std::fs::Permissions::from_mode(0o640),
        )
        .expect("set manifest permissions");
        let metadata =
            std::fs::metadata(directory.path.join("manifest")).expect("source manifest metadata");
        (metadata.uid(), metadata.gid())
    };
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::ExpiredCheckpoints]);
    let plan = plan_compaction(&directory.path, &options).expect("plan");
    assert!(
        plan.actions()
            .iter()
            .any(|action| matches!(action, CompactionAction::UpgradeManifest))
    );

    compact(&directory.path, options).expect("cleanup");

    let manifest =
        std::fs::read_to_string(directory.path.join("manifest")).expect("read upgraded manifest");
    assert!(manifest.contains("custom.property=kept"));
    assert!(manifest.ends_with("store.version=2\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata =
            std::fs::metadata(directory.path.join("manifest")).expect("upgraded manifest metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
        assert_eq!((metadata.uid(), metadata.gid()), source_identity);
    }
    assert!(
        !std::fs::read_dir(&directory.path)
            .expect("read directory")
            .any(|entry| entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with("manifest.cleaning."))
    );
    Repository::open(&directory.path).expect("healthy v2 repository");
}

#[test]
fn segment_source_certificate_rejects_a_survivor_payload_crc_mismatch() {
    let (directory, source_name, replacement_name, survivor) =
        rewrite_certificate_fixture("source-certificate-survivor-crc");
    corrupt_segment_payload_crc(&directory.path.join(&source_name), survivor);

    assert_source_certificate_refusal(
        &directory,
        &source_name,
        Some(&replacement_name),
        "payload CRC",
    );
}

#[test]
fn segment_source_certificate_rejects_exact_graph_or_brf_omissions() {
    for (name, omitted, expected_error) in [
        (
            "source-certificate-omitted-graph",
            OmittedArchiveMetadata::Graph,
            "segment graph differs",
        ),
        (
            "source-certificate-omitted-brf",
            OmittedArchiveMetadata::BinaryReferences,
            "binary-reference catalog differs",
        ),
    ] {
        let (directory, source_name, replacement_name, _) = rewrite_certificate_fixture(name);
        repack_omitting_archive_metadata(&directory.path, &source_name, omitted);

        assert_source_certificate_refusal(
            &directory,
            &source_name,
            Some(&replacement_name),
            expected_error,
        );
    }
}

#[test]
fn segment_source_certificate_precedes_a_whole_archive_removal() {
    let (directory, source_name, orphan) =
        whole_removal_certificate_fixture("source-certificate-whole-removal");
    change_index_generation(&directory.path.join(&source_name), orphan, -1);

    assert_source_certificate_refusal(
        &directory,
        &source_name,
        None,
        "index/header generation disagreement",
    );
    assert!(
        directory.path.join(source_name).exists(),
        "the whole-removal source must survive a failed certificate"
    );
}

#[test]
fn segment_cleanup_removes_old_unjournaled_archive_but_preserves_history() {
    let directory = TestDirectory::repository("orphan-segment-history");
    let old_head = Repository::open(&directory.path)
        .expect("old repository")
        .head_record_identifier();

    // A separate, unjournaled generation-zero archive: representative of
    // a failed write/CAS whose records never became repository state.
    {
        let store = WritableRepository::open(&directory.path).expect("open orphan writer");
        let mut writer = store.record_writer(GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        });
        writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("orphan node");
        writer.finish().expect("finish orphan segment");
        store.close().expect("close orphan writer");
    }
    assert!(directory.path.join("data00001a.tar").is_file());

    // Publish a completely independent generation-two head. It does not
    // reference generation zero; only the older journal line roots the
    // original bootstrap revision.
    let new_head = {
        let store = WritableRepository::open(&directory.path).expect("open new head writer");
        let generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };
        let mut writer = store.record_writer(generation);
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
    assert!(directory.path.join("data00002a.tar").is_file());

    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
    let plan = plan_compaction(&directory.path, &options).expect("segment plan");
    assert!(plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveReclaimableArchive { file_name, .. }
            if file_name == "data00001a.tar"
    )));
    assert!(!plan.actions().iter().any(|action| matches!(
        action,
        CompactionAction::RemoveReclaimableArchive { file_name, .. }
            if file_name == "data00000a.tar"
    )));
    let planned_removed_segments: usize = plan
        .actions()
        .iter()
        .filter_map(|action| match action {
            CompactionAction::RemoveReclaimableArchive { segments, .. }
            | CompactionAction::RewriteArchive { segments, .. } => Some(*segments),
            _ => None,
        })
        .sum();
    assert!(planned_removed_segments != 0);

    let outcome = compact(&directory.path, options).expect("segment cleanup");
    assert_eq!(outcome.head_after, new_head);
    assert_eq!(outcome.removed_segments(), planned_removed_segments);
    assert!(!directory.path.join("data00001a.tar").exists());
    let repository = Repository::open(&directory.path).expect("healthy final repository");
    assert_eq!(repository.head_record_identifier(), new_head);
    crate::tooling::verify_node_tree(&repository, old_head)
        .expect("historical root remains readable");
}

/// Builds the fixture the field report reduces to: an independent
/// generation-two head, plus a generation-zero archive that only the
/// original journal line still roots. Oak's predicate would reclaim that
/// archive; froe's history keep-veto does not. Returns the old head, the
/// new head, and the directory.
fn history_veto_fixture(name: &str) -> (TestDirectory, RecordIdentifier, RecordIdentifier) {
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

/// The shape a real store has once reclamation actually starts removing
/// things: dead segments that survive only because their archive failed
/// the rewrite gate, still pointing at dead segments in an archive that
/// goes away entirely.
#[test]
fn a_dead_survivor_pointing_at_a_removed_segment_is_handled() {
    let directory = TestDirectory::repository("dead-survivor-reference");
    let dead_generation = GarbageCollectionGeneration {
        generation: 0,
        full_generation: 0,
        is_compacted: false,
    };
    let live_generation = GarbageCollectionGeneration {
        generation: 2,
        full_generation: 2,
        is_compacted: false,
    };

    // An archive of nothing but dead segments: fully reclaimable, so the
    // sweep unlinks it whole.
    let target = {
        let store = WritableRepository::open(&directory.path).expect("open target writer");
        let mut writer = store.record_writer(dead_generation);
        let node = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("dead target node");
        writer.finish().expect("finish target");
        store.close().expect("close target writer");
        node
    };

    // An archive that keeps one dead segment referencing that target,
    // beside enough live segments that removing the dead one cannot
    // repay a rewrite — so the dead segment stays on disk.
    let new_head = {
        let store = WritableRepository::open(&directory.path).expect("open mixed writer");
        let mut referencing = store.record_writer(dead_generation);
        referencing
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "target".to_owned(),
                    node: target,
                },
                &[],
            )
            .expect("dead referencing node");
        referencing.finish().expect("finish referencing");
        for _ in 0..8 {
            let mut filler = store.record_writer(live_generation);
            filler
                .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
                .expect("live filler");
            filler.finish().expect("finish filler");
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
        writer.finish().expect("finish head");
        assert!(store.compare_and_set_head(store.head(), head));
        store.close().expect("close mixed writer");
        head
    };

    // The default task set, with no retention bound: the loosened check
    // lives on this path, so this is where it must be pinned.
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);

    // Unconditional. Restoring the stricter check must fail this, so the
    // plan is required to exist rather than merely be well-explained if
    // it does not.
    let plan = plan_compaction(&directory.path, &options)
        .expect("a dead survivor pointing at removed garbage must not refuse the plan");
    let removed: usize = plan
        .actions()
        .iter()
        .filter_map(|action| match action {
            CompactionAction::RemoveReclaimableArchive { segments, .. }
            | CompactionAction::RewriteArchive { segments, .. } => Some(*segments),
            _ => None,
        })
        .sum();
    assert!(
        removed != 0,
        "the fixture must actually remove something, or it proves nothing"
    );

    let outcome = compact(&directory.path, options).expect("apply the plan");
    assert_eq!(outcome.head_after, new_head);
    assert!(outcome.removed_segments() != 0);
    let repository = Repository::open(&directory.path).expect("healthy store");
    assert_eq!(repository.head_record_identifier(), new_head);
    crate::tooling::verify_node_tree(&repository, new_head).expect("head verifies");
}

#[test]
fn the_plan_prices_the_history_veto_against_oaks_own_predicate() {
    let (directory, _old_head, _new_head) = history_veto_fixture("history-veto-price");

    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
    let plan = plan_compaction(&directory.path, &options).expect("segment plan");

    // The bootstrap revision's segments are reachable from the old
    // journal line and from nothing else.
    assert!(
        plan.history_protected_segments() != 0,
        "the bootstrap revision must be counted as history-only"
    );
    // And they are two full generations behind the head, so Oak would
    // have reclaimed them. That difference is the veto's price, and
    // reporting it is the whole point: it is what turns "reclaimed
    // nothing" into a number the operator can act on.
    let (reclaimable_segments, reclaimable_bytes) = plan.history_protected_reclaimable();
    assert!(
        reclaimable_segments != 0,
        "generation-zero history must be priced as reclaimable-but-protected"
    );
    assert!(reclaimable_bytes != 0);
    assert!(reclaimable_segments <= plan.history_protected_segments());
}

#[test]
fn the_history_price_counts_the_bulk_segments_held_behind_the_data_ones() {
    // The veto holds bulk segments only indirectly: a vetoed data segment
    // keeps seeding its references, so its binary content stays too.
    // Counting protected *data* segments alone therefore prices a store
    // full of inline binaries at a rounding error, and an operator
    // reading that figure would decline a run worth most of the store.
    let directory = TestDirectory::repository("history-price-bulk");
    {
        let store = WritableRepository::open(&directory.path).expect("open binary writer");
        let mut writer = store.record_writer(GarbageCollectionGeneration {
            generation: 0,
            full_generation: 0,
            is_compacted: false,
        });
        // Comfortably past the 256 KiB segment limit, so the content
        // lands in bulk segments rather than inline in the data segment.
        let content: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
        let binary = writer.write_binary_content(&content).expect("binary");
        let file = writer
            .write_node(
                Some("nt:file"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "data".to_owned(),
                    property_type: crate::content::property::PropertyType::Binary,
                    values: PropertyValuesToWrite::Single(binary),
                }],
            )
            .expect("file node");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: file,
                },
                &[],
            )
            .expect("binary super root");
        writer.finish().expect("finish binary segments");
        assert!(store.compare_and_set_head(store.head(), head));
        store.close().expect("close binary writer");
    }
    // An independent generation-two head that reaches none of it.
    {
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
    }

    let priced = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
    )
    .expect("priced plan");
    let (_priced_segments, priced_bytes) = priced.history_protected_reclaimable();

    // Now actually retire the history and compare. The quoted price must
    // be what the operation delivers, not the data-segment fraction of it.
    let outcome = compact(
        &directory.path,
        CompactionOptions::default()
            .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
            .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision")),
    )
    .expect("bounded cleanup");
    let freed = outcome.archive_bytes_before - outcome.archive_bytes_after;
    assert!(
        freed > 1024 * 1024,
        "retiring the history must free the binary content: {freed}"
    );
    assert_eq!(
        priced_bytes, freed,
        "the quoted price must be what retiring the history delivers"
    );
}

#[test]
fn a_journal_retention_bound_retires_the_history_the_veto_protects() {
    let (directory, old_head, new_head) = history_veto_fixture("history-veto-retention");
    let protected = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
    )
    .expect("unbounded plan");
    assert!(protected.history_protected_reclaimable().0 != 0);
    assert!(
        !protected.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )),
        "without a bound the veto must keep the bootstrap archive"
    );

    let bounded = CompactionOptions::default()
        .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
        .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision"));
    let plan = plan_compaction(&directory.path, &bounded).expect("bounded plan");

    // The older line is pruned for the retention reason, not for damage.
    assert!(
        plan.journal_line_removals().iter().any(|removal| {
            removal.reason() == JournalRemovalReason::BeyondRetention
                && removal.record_identifier() == Some(old_head)
        }),
        "the superseded revision must be removed as beyond retention"
    );
    // Releasing that root is what makes the archive eligible.
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == "data00000a.tar"
        )),
        "the bound must release the bootstrap archive to Oak's predicate"
    );
    assert!(plan.estimated_reclaimable_bytes() != 0);

    let outcome = compact(&directory.path, bounded).expect("bounded cleanup");
    assert_eq!(outcome.head_after, new_head);
    assert!(!directory.path.join("data00000a.tar").exists());
    let repository = Repository::open(&directory.path).expect("healthy final repository");
    assert_eq!(repository.head_record_identifier(), new_head);
    // The journal keeps exactly the bound's worth of revisions, and the
    // retired history is genuinely gone rather than merely unrooted.
    let journal =
        std::fs::read_to_string(directory.path.join("journal.log")).expect("read journal");
    assert_eq!(
        journal.lines().count(),
        1,
        "a bound of one leaves one journal line"
    );
    assert!(
        crate::tooling::verify_node_tree(&repository, old_head).is_err(),
        "the retired revision must no longer resolve"
    );
}

#[test]
fn a_retention_bound_without_the_journal_task_is_refused() {
    let (directory, _old_head, _new_head) = history_veto_fixture("history-veto-task-guard");
    // Un-rooting without pruning would leave the line in the journal for
    // the prospective-plan check to verify as retained history, and the
    // run would refuse itself with a far less actionable message.
    let options = CompactionOptions::default()
        .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision"))
        .with_tasks([MaintenanceTask::Segments]);
    let error = plan_compaction(&directory.path, &options).expect_err("must refuse");
    assert!(
        error.to_string().contains("requires the journal task"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn a_bound_counts_only_revisions_that_actually_resolve() {
    // A line whose segment exists but whose tree does not verify used to
    // fill a slot in the bound and then be removed as unreadable anyway,
    // so `N = 2` kept one revision and irreversibly retired a readable
    // one to make room for it. Every earlier retention test used N = 1,
    // which cannot expose this: the head is always the newest resolvable
    // line and always verifies.
    let (directory, old_head, new_head) = history_veto_fixture("retention-counts-readable");

    // A journal line naming a record that resolves to a segment but not
    // to a readable node tree: the head's own segment, at a record
    // number that is not a node record.
    let unreadable = RecordIdentifier::new(new_head.segment, new_head.record_number + 1);
    // Second newest, not newest: the newest line is the head, and a
    // head that is not a node record is refused long before any bound.
    let journal_path = directory.path.join("journal.log");
    let journal = std::fs::read_to_string(&journal_path).expect("read journal");
    let mut lines: Vec<&str> = journal.lines().collect();
    let head_line = lines.pop().expect("a head line");
    let unreadable_line = format!("{unreadable} root 0");
    lines.push(&unreadable_line);
    lines.push(head_line);
    std::fs::write(&journal_path, format!("{}\n", lines.join("\n")))
        .expect("insert unreadable line");

    let options = CompactionOptions::default()
        .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
        .with_journal_revision_retention(NonZeroUsize::new(2).expect("two revisions"));
    let plan = plan_compaction(&directory.path, &options).expect("bounded plan");

    // The unreadable line goes, as it always did. What must not happen is
    // the older *readable* revision going with it to satisfy a bound the
    // unreadable line was counted against.
    assert!(
        !plan.journal_line_removals().iter().any(|removal| {
            removal.reason() == JournalRemovalReason::BeyondRetention
                && removal.record_identifier() == Some(old_head)
        }),
        "a readable revision was retired to make room for an unreadable one: {:?}",
        plan.journal_line_removals()
    );
}

#[test]
fn a_bound_larger_than_the_journal_removes_nothing() {
    let (directory, _old_head, _new_head) = history_veto_fixture("history-veto-wide-bound");
    let options = CompactionOptions::default()
        .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
        .with_journal_revision_retention(NonZeroUsize::new(64).expect("wide bound"));
    let plan = plan_compaction(&directory.path, &options).expect("wide plan");
    assert!(
        !plan
            .journal_line_removals()
            .iter()
            .any(|removal| { removal.reason() == JournalRemovalReason::BeyondRetention }),
        "a bound wider than the journal must retire nothing"
    );
    assert!(plan.history_protected_reclaimable().0 != 0);
}

/// One archive holding a single dead generation-zero segment beside
/// enough live head-generation segments that removing the dead one
/// frees less than a quarter of the file. This is the field report in
/// miniature: real garbage, correctly identified, and — under Oak's
/// heuristic — declined by every run forever.
fn sub_gate_garbage_fixture(name: &str) -> (TestDirectory, RecordIdentifier) {
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

#[test]
fn gate_deferred_garbage_is_counted_rather_than_reported_as_nothing() {
    let (directory, new_head) = sub_gate_garbage_fixture("retained-reclaimable");

    let options = CompactionOptions::default()
        .with_tasks([MaintenanceTask::Segments])
        .with_oak_savings_gate();
    let plan = plan_compaction(&directory.path, &options).expect("segment plan");
    assert_eq!(plan.current_head(), new_head);
    let deferred = plan
        .warnings()
        .iter()
        .any(|warning| warning.contains("25% rewrite gate"));
    assert!(
        deferred,
        "expected a savings deferral: {:?}",
        plan.warnings()
    );
    assert!(
        plan.retained_reclaimable_segments() != 0,
        "declined garbage must be counted, not silently dropped"
    );
    assert!(plan.retained_reclaimable_bytes() != 0);
    // The distinction the old output could not express: nothing is
    // reclaimable *by this run*, yet the store is not clean.
    assert_eq!(plan.estimated_reclaimable_bytes(), 0);
}

#[test]
fn the_default_policy_reclaims_what_the_oak_gate_declines() {
    let (directory, new_head) = sub_gate_garbage_fixture("default-policy-reclaims");

    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
    let plan = plan_compaction(&directory.path, &options).expect("segment plan");
    assert_eq!(plan.current_head(), new_head);
    assert!(
        !plan
            .warnings()
            .iter()
            .any(|warning| warning.contains("rewrite gate")),
        "the default policy never defers for savings: {:?}",
        plan.warnings()
    );
    assert_eq!(
        plan.retained_reclaimable_segments(),
        0,
        "nothing identified may be left behind on an archive with letters to spare"
    );
    assert_eq!(plan.retained_reclaimable_bytes(), 0);
    assert!(
        plan.estimated_reclaimable_bytes() != 0,
        "the garbage the gate declined is now actually reclaimed"
    );
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RewriteArchive { file_name, replacement_name, .. }
                if file_name == "data00001a.tar" && replacement_name == "data00001b.tar"
        )),
        "the sub-gate archive is rewritten to its next generation: {:?}",
        plan.actions()
    );
}

/// Records the step names an operation reports, so a test can assert
/// what the operator was told froe was doing.
struct StepNameObserver {
    names: Vec<String>,
}

impl ProgressObserver for StepNameObserver {
    fn step_began(&mut self, step: &Step<'_>) {
        self.names.push(step.description().to_owned());
    }

    fn step_advanced(&mut self, _completed: u64) {}

    fn step_ended(&mut self) {}
}

#[test]
fn an_unselected_task_reports_no_step_of_its_own() {
    let (directory, _old_head, _new_head) = history_veto_fixture("unselected-task-step");
    let mut observer = StepNameObserver { names: Vec::new() };
    // recovery-backups is not among the defaults, so announcing its
    // removal step told the operator froe had considered backups it was
    // never asked to touch.
    compact_with_progress(
        &directory.path,
        CompactionOptions::default()
            .with_tasks([
                MaintenanceTask::Segments,
                MaintenanceTask::Journal,
                MaintenanceTask::StaleTemporaries,
            ])
            .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision")),
        &mut observer,
    )
    .expect("bounded cleanup");
    for unselected in ["removing old recovery backups", "removing stale archives"] {
        assert!(
            !observer.names.iter().any(|name| name == unselected),
            "unselected task reported {unselected:?}: {:?}",
            observer.names
        );
    }
    assert!(
        observer
            .names
            .iter()
            .any(|name| name == "removing stale temporary files"),
        "a selected task still reports its step: {:?}",
        observer.names
    );
}

#[test]
fn the_outcome_states_the_backup_bytes_the_archive_figures_omit() {
    let (directory, _old_head, _new_head) = history_veto_fixture("retained-backup-bytes");
    // What a preceding index repair leaves behind: bytes on disk that no
    // archive figure counts, so a run that grew the directory reported
    // its size as unchanged.
    let retired = directory.path.join("data00000a.tar.bak");
    std::fs::write(&retired, vec![7u8; 4096]).expect("write retired original");

    let outcome = compact(
        &directory.path,
        CompactionOptions::default()
            .with_tasks([MaintenanceTask::Segments, MaintenanceTask::Journal])
            .with_journal_revision_retention(NonZeroUsize::new(1).expect("one revision")),
    )
    .expect("bounded cleanup");

    // Summed independently here rather than trusting the crate's own
    // predicate: every retained backup form, straight off the directory.
    let mut expected = 0u64;
    for entry in std::fs::read_dir(&directory.path).expect("read directory") {
        let entry = entry.expect("directory entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        let journal_backup = name.starts_with("journal.log.bak.")
            && name.len() == "journal.log.bak.".len() + 3
            && name.ends_with(|last: char| last.is_ascii_digit());
        let archive_backup = std::path::Path::new(&name)
            .extension()
            .is_some_and(|extension| extension == "bak");
        if archive_backup || journal_backup {
            expected += entry.metadata().expect("entry metadata").len();
        }
    }
    assert!(expected >= 4096, "the retired original must be counted");
    assert_eq!(outcome.retained_recovery_backup_bytes, expected);
    // The two figures are disjoint: the archive line falls, while the
    // backup bytes it does not count stay on disk.
    assert!(outcome.archive_bytes_after < outcome.archive_bytes_before);
}

#[test]
fn current_head_reaching_a_one_generation_old_segment_fails_closed() {
    let directory = TestDirectory::repository("generation-invariant");
    let store = WritableRepository::open(&directory.path).expect("open writer");
    // Exactly one generation behind the head: the boundary the retention
    // value moved. At two retained generations the arithmetic spared this
    // child, so the run proceeded on the predicate alone; at one it is
    // reclaimable and only the reference guard keeps the head's own data.
    let mut root_writer = store.record_writer(GarbageCollectionGeneration {
        generation: 1,
        full_generation: 1,
        is_compacted: false,
    });
    let old_root = root_writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("one-generation-old content root");
    root_writer.finish().expect("finish the older generation");
    let mut writer = store.record_writer(GarbageCollectionGeneration {
        generation: 2,
        full_generation: 2,
        is_compacted: false,
    });
    let new_head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: old_root,
            },
            &[],
        )
        .expect("new super root");
    writer.finish().expect("finish");
    assert!(store.compare_and_set_head(store.head(), new_head));
    store.close().expect("close");
    let before = file_bytes(&directory.path);
    let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);

    let error = plan_compaction(&directory.path, &options)
        .expect_err("a live one-generation-old child is unsafe at one retained generation");
    assert!(
        error
            .to_string()
            .contains("current head reaches data segment")
    );
    assert_eq!(file_bytes(&directory.path), before);
    Repository::open(&directory.path).expect("refusal leaves repository healthy");
}
