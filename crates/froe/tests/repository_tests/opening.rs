//! What opening a store does to it: selecting the newest generation,
//! recovering an archive with no index, rewinding a journal past missing
//! segments, and refusing what it cannot read.

use super::*;

#[test]
pub(crate) fn spreads_segments_across_archives_and_selects_newest_generation() {
    let directory = TestDirectory::new("multiple-archives");
    let repository_data = build_synthetic_repository();

    // The values segment lives in archive 0, the tree segment in archive 1.
    let mut first_archive = ArchiveBuilder::new();
    first_archive.add_segment(
        repository_data.values_segment.0,
        repository_data.values_segment.1.clone(),
    );
    let mut second_archive = ArchiveBuilder::new();
    second_archive.add_segment(
        repository_data.tree_segment.0,
        repository_data.tree_segment.1.clone(),
    );

    // A stale generation `a` of archive 0 exists with garbage content;
    // only generation `b` may be opened.
    write_repository(
        &directory.path,
        &[
            ("data00000a.tar".to_owned(), vec![0xFFu8; 4096]),
            (
                "data00000b.tar".to_owned(),
                first_archive.build("data00000b.tar"),
            ),
            (
                "data00001a.tar".to_owned(),
                second_archive.build("data00001a.tar"),
            ),
        ],
        std::slice::from_ref(&repository_data.journal_line),
    );

    let repository = Repository::open(&directory.path).expect("open repository");
    assert_eq!(repository.archives().len(), 2);
    assert_eq!(
        repository.archives()[0].file_name(),
        "data00001a.tar",
        "newest first"
    );
    assert_eq!(repository.archives()[1].file_name(), "data00000b.tar");

    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    assert_eq!(
        content.child_node_count().expect("count"),
        CONTENT_CHILD_COUNT as u64
    );
}

/// Every file in a directory with its full content — the read-only
/// invariant check: an open must neither create, nor delete, nor modify
/// anything, not even with a same-length rewrite.
pub(crate) fn directory_snapshot(
    path: &std::path::Path,
) -> std::collections::BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(path)
        .expect("list directory")
        .map(|entry| {
            let entry = entry.expect("directory entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("read file"),
            )
        })
        .collect()
}

#[test]
pub(crate) fn recovers_archives_without_an_index() {
    let directory = TestDirectory::new("recovers-without-index");
    let repository_data = build_synthetic_repository();

    // The tree segment's archive has no index — like the archive a live
    // repository is currently writing.
    let mut indexed_archive = ArchiveBuilder::new();
    indexed_archive.add_segment(
        repository_data.values_segment.0,
        repository_data.values_segment.1.clone(),
    );
    let mut live_archive = ArchiveBuilder::new().without_index();
    live_archive.add_segment(
        repository_data.tree_segment.0,
        repository_data.tree_segment.1.clone(),
    );

    write_repository(
        &directory.path,
        &[
            (
                "data00000a.tar".to_owned(),
                indexed_archive.build("data00000a.tar"),
            ),
            (
                "data00001a.tar".to_owned(),
                live_archive.build("data00001a.tar"),
            ),
        ],
        std::slice::from_ref(&repository_data.journal_line),
    );

    // This is the exact scenario where Java's read-only open writes a
    // `.ro.bak` recovery file; froe promises the recovery stays in
    // memory — the directory must be untouched, and there must be no
    // lock file.
    let snapshot_before = directory_snapshot(&directory.path);
    let repository = Repository::open(&directory.path).expect("open repository");
    assert!(repository.archives()[0].is_recovered());
    assert!(!repository.archives()[1].is_recovered());

    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    assert_eq!(
        content
            .property("title")
            .expect("read")
            .expect("present")
            .values,
        PropertyValues::Single(PropertyValue::String("Hello World".to_owned()))
    );
    drop(repository);
    assert_eq!(
        directory_snapshot(&directory.path),
        snapshot_before,
        "a read-only open must not create, delete, or modify any file"
    );
    assert!(
        !directory.path.join("repo.lock").exists(),
        "a read-only open must never touch the repository lock"
    );
}

#[test]
pub(crate) fn journal_rewinds_past_revisions_with_missing_segments() {
    let directory = TestDirectory::new("journal-rewind");
    let repository_data = build_synthetic_repository();
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(
        repository_data.values_segment.0,
        repository_data.values_segment.1.clone(),
    );
    archive.add_segment(
        repository_data.tree_segment.0,
        repository_data.tree_segment.1.clone(),
    );

    // The newest journal line references a segment that no archive holds;
    // the reader must fall back to the older valid line.
    let missing_revision = "99999999-9999-4999-a999-999999999999:123 root 1800000000000".to_owned();
    write_repository(
        &directory.path,
        &[("data00000a.tar".to_owned(), archive.build("data00000a.tar"))],
        &[repository_data.journal_line.clone(), missing_revision],
    );

    let repository = Repository::open(&directory.path).expect("open repository");
    assert_eq!(repository.journal_entries().len(), 2);
    assert!(
        repository
            .node_at_path("/content")
            .expect("resolve")
            .is_some()
    );
}

#[test]
pub(crate) fn rejects_stores_that_cannot_be_opened() {
    // A directory with archives but no manifest is the legacy format.
    let legacy = TestDirectory::new("legacy-store");
    let repository_data = build_synthetic_repository();
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(
        repository_data.values_segment.0,
        repository_data.values_segment.1.clone(),
    );
    std::fs::write(
        legacy.path.join("data00000a.tar"),
        archive.build("data00000a.tar"),
    )
    .expect("write archive");
    std::fs::write(legacy.path.join("journal.log"), "").expect("write journal");
    assert!(Repository::open(&legacy.path).is_err());

    // A store version above 2 is newer than this reader.
    let too_new = TestDirectory::new("too-new-store");
    write_repository(&too_new.path, &[], &[]);
    std::fs::write(too_new.path.join("manifest"), "store.version=3\n").expect("write manifest");
    assert!(Repository::open(&too_new.path).is_err());

    // An empty journal cannot provide a head.
    let empty_journal = TestDirectory::new("empty-journal");
    write_repository(&empty_journal.path, &[], &[]);
    assert!(Repository::open(&empty_journal.path).is_err());

    // A missing directory cannot be opened.
    assert!(Repository::open(std::path::Path::new("/nonexistent-froe-repository")).is_err());
}

#[test]
pub(crate) fn write_open_recovery_resolves_cross_segment_blob_identifiers() {
    let directory = TestDirectory::new("recovery-blob-catalog");
    write_repository_with_blob_archive(&directory, StringSegment::Present);

    // The write open recovers the index-less archive; the rebuilt binary
    // references catalog must contain the cross-segment identifier —
    // dropping it would let AEM's blob garbage collection delete the
    // referenced binary.
    let store =
        froe::writer::store_writer::WritableRepository::open(&directory.path).expect("open");
    store.close().expect("close");

    assert!(
        directory.path.join("data00001a.tar.bak").exists(),
        "the original archive is retired to a backup name"
    );
    let repository = Repository::open(&directory.path).expect("reader");
    let recovered = repository
        .archives()
        .iter()
        .find(|archive| archive.file_name() == "data00001a.tar")
        .expect("recovered archive present");
    assert!(
        !recovered.is_recovered(),
        "the rebuilt archive has a valid index"
    );
    let catalog = recovered
        .binary_references()
        .expect("the rebuilt archive has a catalog");
    let identifiers: Vec<&str> = catalog
        .generations
        .iter()
        .flat_map(|generation| generation.segments.iter())
        .flat_map(|(_, references)| references.iter().map(String::as_str))
        .collect();
    assert_eq!(identifiers, ["blob-identifier-in-another-segment"]);
}

#[test]
pub(crate) fn write_open_recovery_fails_closed_on_unresolvable_blob_identifiers() {
    let directory = TestDirectory::new("recovery-blob-fail-closed");
    write_repository_with_blob_archive(&directory, StringSegment::Absent);

    let error = froe::writer::store_writer::WritableRepository::open(&directory.path);
    assert!(
        error.is_err(),
        "recovery must refuse to publish an incomplete blob catalog"
    );
    // The failure leaves every original archive untouched: nothing was
    // renamed, deleted, or replaced.
    assert!(directory.path.join("data00000a.tar").exists());
    assert!(directory.path.join("data00001a.tar").exists());
    assert!(!directory.path.join("data00001a.tar.bak").exists());
    // The store still opens read-only (recovery stays in memory there).
    Repository::open(&directory.path).expect("read-only open still works");
}
