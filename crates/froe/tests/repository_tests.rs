//! End-to-end tests: open synthetic repositories written by the
//! independent encoder in `support` and read them back through the public
//! API.

#![allow(
    unreachable_pub,
    reason = "test binaries have no external interface; pub only means module-visible"
)]

mod support;

use froe::content::{ChildNodeArity, PropertyType, PropertyValue, PropertyValues};
use froe::store::Repository;
use support::{
    ArchiveBuilder, MapEntryFixture, SegmentBuilder, TYPE_LIST_BUCKET, TYPE_NODE, TYPE_TEMPLATE,
    TYPE_VALUE, TestDirectory, build_child_map, data_segment_uuid, format_uuid,
    record_identifier_bytes, string_record, write_repository,
};

/// The number of children under `/content`; above 32 so the child map is
/// stored as a branch record with leaf buckets.
const CONTENT_CHILD_COUNT: usize = 40;

/// String record numbers in the values segment.
mod value_records {
    pub const ROOT_NAME: u32 = 1;
    pub const CHECKPOINTS_NAME: u32 = 2;
    pub const CONTENT_NAME: u32 = 3;
    pub const EMPTY_NAME: u32 = 4;
    pub const TITLE_NAME: u32 = 5;
    pub const COUNT_NAME: u32 = 6;
    pub const ACTIVE_NAME: u32 = 7;
    pub const TITLE_VALUE: u32 = 8;
    pub const COUNT_VALUE: u32 = 9;
    pub const ACTIVE_VALUE: u32 = 10;
    pub const PRIMARY_TYPE_VALUE: u32 = 11;
    pub const CREATED_NAME: u32 = 12;
    pub const TIMESTAMP_NAME: u32 = 13;
    pub const CREATED_VALUE: u32 = 14;
    pub const TIMESTAMP_VALUE: u32 = 15;
    pub const CHECKPOINT_NAME: u32 = 16;
    /// Child names occupy records 20 through `20 + CONTENT_CHILD_COUNT - 1`.
    pub const FIRST_CHILD_NAME: u32 = 20;
}

/// Node and structure record numbers in the tree segment.
mod tree_records {
    pub const TEMPLATE_EMPTY: u32 = 1;
    pub const TEMPLATE_CONTENT: u32 = 2;
    pub const PROPERTY_NAME_BUCKET: u32 = 3;
    pub const TEMPLATE_ROOT: u32 = 4;
    pub const TEMPLATE_SUPER_ROOT: u32 = 5;
    pub const TEMPLATE_CHECKPOINT: u32 = 6;
    pub const TEMPLATE_CHECKPOINTS_PARENT: u32 = 7;
    pub const CHECKPOINT_PROPERTY_NAME_BUCKET: u32 = 8;
    /// Child maps and child nodes are allocated dynamically from 100.
    pub const DYNAMIC_START: u32 = 100;
}

/// The two segments of the synthetic repository plus the journal line for
/// its head.
struct SyntheticRepository {
    values_segment: (support::SegmentUuid, Vec<u8>),
    tree_segment: (support::SegmentUuid, Vec<u8>),
    journal_line: String,
}

/// Builds a repository with this content tree:
///
/// ```text
/// (super-root)
/// ├─ root
/// │   ├─ content        properties: title, count, active; 40 children
/// │   │   └─ child-00 … child-39
/// │   └─ empty
/// └─ checkpoints
///     └─ cp-one         created/timestamp properties, root → shared with /
/// ```
#[allow(
    clippy::too_many_lines,
    reason = "one linear fixture description reads better unsplit"
)]
fn build_synthetic_repository() -> SyntheticRepository {
    let values_uuid = data_segment_uuid(0x0002);
    let tree_uuid = data_segment_uuid(0x0001);

    // --- The values segment: every string used by the tree. ---
    let mut values = SegmentBuilder::new(values_uuid);
    let strings = [
        (value_records::ROOT_NAME, "root"),
        (value_records::CHECKPOINTS_NAME, "checkpoints"),
        (value_records::CONTENT_NAME, "content"),
        (value_records::EMPTY_NAME, "empty"),
        (value_records::TITLE_NAME, "title"),
        (value_records::COUNT_NAME, "count"),
        (value_records::ACTIVE_NAME, "active"),
        (value_records::TITLE_VALUE, "Hello World"),
        (value_records::COUNT_VALUE, "42"),
        (value_records::ACTIVE_VALUE, "true"),
        (value_records::PRIMARY_TYPE_VALUE, "nt:unstructured"),
        (value_records::CREATED_NAME, "created"),
        (value_records::TIMESTAMP_NAME, "timestamp"),
        (value_records::CREATED_VALUE, "1700000000000"),
        (value_records::TIMESTAMP_VALUE, "9999999999999"),
        (value_records::CHECKPOINT_NAME, "cp-one"),
    ];
    for (record_number, text) in strings {
        values.add_record(record_number, TYPE_VALUE, string_record(text));
    }
    let child_names: Vec<String> = (0..CONTENT_CHILD_COUNT)
        .map(|index| format!("child-{index:02}"))
        .collect();
    for (index, name) in child_names.iter().enumerate() {
        values.add_record(
            value_records::FIRST_CHILD_NAME + index as u32,
            TYPE_VALUE,
            string_record(name),
        );
    }

    // --- The tree segment: templates, maps, and nodes. ---
    let mut tree = SegmentBuilder::new(tree_uuid);
    let values_reference = tree.add_referenced_segment(values_uuid);
    let value_identifier =
        |record_number: u32| record_identifier_bytes(values_reference, record_number);
    let own_identifier = |record_number: u32| record_identifier_bytes(0, record_number);

    let mut next_dynamic = tree_records::DYNAMIC_START;
    let mut allocate = move || {
        let allocated = next_dynamic;
        next_dynamic += 1;
        allocated
    };

    // Template of the empty leaf nodes: primary type, zero children.
    let mut template_empty = ((1u32 << 31) | (1 << 29)).to_be_bytes().to_vec();
    template_empty.extend(value_identifier(value_records::PRIMARY_TYPE_VALUE));
    tree.add_record(tree_records::TEMPLATE_EMPTY, TYPE_TEMPLATE, template_empty);

    // Template of /content: primary type, many children, three properties
    // in the mandatory on-disk order — sorted by signed Java
    // `String.hashCode`, so active (-1422950650) before count (94851343)
    // before title (110371416) — with types BOOLEAN, LONG, STRING.
    let mut property_name_bucket = Vec::new();
    property_name_bucket.extend(value_identifier(value_records::ACTIVE_NAME));
    property_name_bucket.extend(value_identifier(value_records::COUNT_NAME));
    property_name_bucket.extend(value_identifier(value_records::TITLE_NAME));
    tree.add_record(
        tree_records::PROPERTY_NAME_BUCKET,
        TYPE_LIST_BUCKET,
        property_name_bucket,
    );
    let mut template_content = ((1u32 << 31) | (1 << 28) | 3).to_be_bytes().to_vec();
    template_content.extend(value_identifier(value_records::PRIMARY_TYPE_VALUE));
    template_content.extend(own_identifier(tree_records::PROPERTY_NAME_BUCKET));
    template_content.extend([6u8, 3, 1]);
    tree.add_record(
        tree_records::TEMPLATE_CONTENT,
        TYPE_TEMPLATE,
        template_content,
    );

    // Templates with many children and nothing else (root, super-root).
    let many_children_template = (1u32 << 28).to_be_bytes().to_vec();
    tree.add_record(
        tree_records::TEMPLATE_ROOT,
        TYPE_TEMPLATE,
        many_children_template.clone(),
    );
    tree.add_record(
        tree_records::TEMPLATE_SUPER_ROOT,
        TYPE_TEMPLATE,
        many_children_template,
    );

    // Checkpoint template: single child "root", properties created and
    // timestamp (both LONG).
    // On-disk order by signed hash: timestamp (55126294) before
    // created (1028554472).
    let mut checkpoint_property_names = Vec::new();
    checkpoint_property_names.extend(value_identifier(value_records::TIMESTAMP_NAME));
    checkpoint_property_names.extend(value_identifier(value_records::CREATED_NAME));
    tree.add_record(
        tree_records::CHECKPOINT_PROPERTY_NAME_BUCKET,
        TYPE_LIST_BUCKET,
        checkpoint_property_names,
    );
    let mut template_checkpoint = 2u32.to_be_bytes().to_vec();
    template_checkpoint.extend(value_identifier(value_records::ROOT_NAME));
    template_checkpoint.extend(own_identifier(
        tree_records::CHECKPOINT_PROPERTY_NAME_BUCKET,
    ));
    template_checkpoint.extend([3u8, 3]);
    tree.add_record(
        tree_records::TEMPLATE_CHECKPOINT,
        TYPE_TEMPLATE,
        template_checkpoint,
    );

    // Checkpoints-parent template: single child "cp-one".
    let mut template_checkpoints_parent = 0u32.to_be_bytes().to_vec();
    template_checkpoints_parent.extend(value_identifier(value_records::CHECKPOINT_NAME));
    tree.add_record(
        tree_records::TEMPLATE_CHECKPOINTS_PARENT,
        TYPE_TEMPLATE,
        template_checkpoints_parent,
    );

    // A node record: stable identifier (self), template, extra slots.
    let node_record = |own_record_number: u32, template: u32, slots: &[Vec<u8>]| {
        let mut bytes = record_identifier_bytes(0, own_record_number);
        bytes.extend(record_identifier_bytes(0, template));
        for slot in slots {
            bytes.extend_from_slice(slot);
        }
        bytes
    };

    // The empty child nodes.
    let mut child_node_records = Vec::with_capacity(CONTENT_CHILD_COUNT);
    for _ in 0..CONTENT_CHILD_COUNT {
        let record_number = allocate();
        tree.add_record(
            record_number,
            TYPE_NODE,
            node_record(record_number, tree_records::TEMPLATE_EMPTY, &[]),
        );
        child_node_records.push(record_number);
    }

    // The child map of /content (40 entries: a branch with leaf buckets).
    let map_entries: Vec<MapEntryFixture> = child_names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            (
                name.clone(),
                value_identifier(value_records::FIRST_CHILD_NAME + index as u32),
                own_identifier(child_node_records[index]),
            )
        })
        .collect();
    let content_child_map = build_child_map(&mut tree, &mut allocate, &map_entries);

    // /content: property values bucket, then the node.
    let mut content_values_bucket = Vec::new();
    content_values_bucket.extend(value_identifier(value_records::ACTIVE_VALUE));
    content_values_bucket.extend(value_identifier(value_records::COUNT_VALUE));
    content_values_bucket.extend(value_identifier(value_records::TITLE_VALUE));
    let content_values_record = allocate();
    tree.add_record(
        content_values_record,
        TYPE_LIST_BUCKET,
        content_values_bucket,
    );
    let content_node = allocate();
    tree.add_record(
        content_node,
        TYPE_NODE,
        node_record(
            content_node,
            tree_records::TEMPLATE_CONTENT,
            &[
                own_identifier(content_child_map),
                own_identifier(content_values_record),
            ],
        ),
    );

    // /empty.
    let empty_node = allocate();
    tree.add_record(
        empty_node,
        TYPE_NODE,
        node_record(empty_node, tree_records::TEMPLATE_EMPTY, &[]),
    );

    // The content root with children content and empty.
    let root_map = build_child_map(
        &mut tree,
        &mut allocate,
        &[
            (
                "content".to_owned(),
                value_identifier(value_records::CONTENT_NAME),
                own_identifier(content_node),
            ),
            (
                "empty".to_owned(),
                value_identifier(value_records::EMPTY_NAME),
                own_identifier(empty_node),
            ),
        ],
    );
    let root_node = allocate();
    tree.add_record(
        root_node,
        TYPE_NODE,
        node_record(
            root_node,
            tree_records::TEMPLATE_ROOT,
            &[own_identifier(root_map)],
        ),
    );

    // The checkpoint: its root child SHARES the content root record.
    let mut checkpoint_values_bucket = Vec::new();
    checkpoint_values_bucket.extend(value_identifier(value_records::TIMESTAMP_VALUE));
    checkpoint_values_bucket.extend(value_identifier(value_records::CREATED_VALUE));
    let checkpoint_values_record = allocate();
    tree.add_record(
        checkpoint_values_record,
        TYPE_LIST_BUCKET,
        checkpoint_values_bucket,
    );
    let checkpoint_node = allocate();
    tree.add_record(
        checkpoint_node,
        TYPE_NODE,
        node_record(
            checkpoint_node,
            tree_records::TEMPLATE_CHECKPOINT,
            &[
                own_identifier(root_node),
                own_identifier(checkpoint_values_record),
            ],
        ),
    );
    let checkpoints_parent_node = allocate();
    tree.add_record(
        checkpoints_parent_node,
        TYPE_NODE,
        node_record(
            checkpoints_parent_node,
            tree_records::TEMPLATE_CHECKPOINTS_PARENT,
            &[own_identifier(checkpoint_node)],
        ),
    );

    // The super-root with children root and checkpoints.
    let super_root_map = build_child_map(
        &mut tree,
        &mut allocate,
        &[
            (
                "root".to_owned(),
                value_identifier(value_records::ROOT_NAME),
                own_identifier(root_node),
            ),
            (
                "checkpoints".to_owned(),
                value_identifier(value_records::CHECKPOINTS_NAME),
                own_identifier(checkpoints_parent_node),
            ),
        ],
    );
    let super_root_node = allocate();
    tree.add_record(
        super_root_node,
        TYPE_NODE,
        node_record(
            super_root_node,
            tree_records::TEMPLATE_SUPER_ROOT,
            &[own_identifier(super_root_map)],
        ),
    );

    let journal_line = format!(
        "{}:{super_root_node} root 1700000000000",
        format_uuid(tree_uuid)
    );
    SyntheticRepository {
        values_segment: (values_uuid, values.build()),
        tree_segment: (tree_uuid, tree.build()),
        journal_line,
    }
}

/// Writes the synthetic repository with both segments in one archive.
fn write_single_archive_repository(directory: &TestDirectory) -> SyntheticRepository {
    let repository = build_synthetic_repository();
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(
        repository.values_segment.0,
        repository.values_segment.1.clone(),
    );
    archive.add_segment(repository.tree_segment.0, repository.tree_segment.1.clone());
    write_repository(
        &directory.path,
        &[("data00000a.tar".to_owned(), archive.build("data00000a.tar"))],
        std::slice::from_ref(&repository.journal_line),
    );
    repository
}

#[test]
fn traverses_the_content_tree() {
    let directory = TestDirectory::new("traverses-content-tree");
    write_single_archive_repository(&directory);
    let repository = Repository::open(&directory.path).expect("open repository");

    assert_eq!(repository.segment_count(), 2);
    assert_eq!(repository.archives().len(), 1);
    assert!(!repository.archives()[0].is_recovered());

    let content_root = repository.content_root().expect("content root");
    let child_names: Vec<String> = content_root
        .child_node_entries()
        .expect("children")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(child_names.contains(&"content".to_owned()));
    assert!(child_names.contains(&"empty".to_owned()));

    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    assert_eq!(
        content.child_node_count().expect("count"),
        CONTENT_CHILD_COUNT as u64
    );

    // Every child resolves by name through the branch map.
    for index in 0..CONTENT_CHILD_COUNT {
        let name = format!("child-{index:02}");
        let child = content.child_node(&name).expect("lookup").expect("present");
        let template = child.template().expect("template");
        assert_eq!(template.primary_type.as_deref(), Some("nt:unstructured"));
        assert_eq!(template.child_arity, ChildNodeArity::Zero);
    }
    assert!(content.child_node("child-99").expect("lookup").is_none());

    // Enumerated entries cover all children exactly once.
    let mut enumerated: Vec<String> = content
        .child_node_entries()
        .expect("entries")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    enumerated.sort();
    let expected: Vec<String> = (0..CONTENT_CHILD_COUNT)
        .map(|index| format!("child-{index:02}"))
        .collect();
    assert_eq!(enumerated, expected);
}

#[test]
fn reads_typed_properties() {
    let directory = TestDirectory::new("reads-typed-properties");
    write_single_archive_repository(&directory);
    let repository = Repository::open(&directory.path).expect("open repository");
    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");

    let properties = content.properties().expect("properties");
    let names: Vec<&str> = properties
        .iter()
        .map(|property| property.name.as_str())
        .collect();
    // Stored order is the template's on-disk order: sorted by signed Java
    // `String.hashCode`, negative hashes ("active") first.
    assert_eq!(names, ["jcr:primaryType", "active", "count", "title"]);

    let title = content.property("title").expect("read").expect("present");
    assert_eq!(title.property_type, PropertyType::String);
    assert_eq!(
        title.values,
        PropertyValues::Single(PropertyValue::String("Hello World".to_owned()))
    );

    let count = content.property("count").expect("read").expect("present");
    assert_eq!(
        count.values,
        PropertyValues::Single(PropertyValue::Long(42))
    );

    let active = content.property("active").expect("read").expect("present");
    assert_eq!(
        active.values,
        PropertyValues::Single(PropertyValue::Boolean(true))
    );

    let primary_type = content
        .property("jcr:primaryType")
        .expect("read")
        .expect("present");
    assert_eq!(
        primary_type.values,
        PropertyValues::Single(PropertyValue::Name("nt:unstructured".to_owned()))
    );

    let empty = repository
        .node_at_path("/empty")
        .expect("resolve")
        .expect("present");
    assert_eq!(
        empty.properties().expect("properties").len(),
        1,
        "only jcr:primaryType"
    );
    assert_eq!(empty.child_node_count().expect("count"), 0);
}

#[test]
fn reads_checkpoints_sharing_records_with_the_head() {
    let directory = TestDirectory::new("reads-checkpoints");
    write_single_archive_repository(&directory);
    let repository = Repository::open(&directory.path).expect("open repository");

    let checkpoints = repository.checkpoints().expect("checkpoints");
    assert_eq!(checkpoints.len(), 1);
    let (name, checkpoint) = &checkpoints[0];
    assert_eq!(name, "cp-one");

    let created = checkpoint
        .property("created")
        .expect("read")
        .expect("present");
    assert_eq!(
        created.values,
        PropertyValues::Single(PropertyValue::Long(1_700_000_000_000))
    );

    let checkpoint_root = checkpoint
        .child_node("root")
        .expect("read")
        .expect("present");
    let live_root = repository.content_root().expect("content root");
    assert_eq!(
        checkpoint_root.record_identifier(),
        live_root.record_identifier(),
        "the checkpoint's root shares the live root's record"
    );
}

#[test]
fn resolves_paths() {
    let directory = TestDirectory::new("resolves-paths");
    write_single_archive_repository(&directory);
    let repository = Repository::open(&directory.path).expect("open repository");

    assert!(repository.node_at_path("/").expect("resolve").is_some());
    assert!(
        repository
            .node_at_path("/content")
            .expect("resolve")
            .is_some()
    );
    assert!(
        repository
            .node_at_path("content/")
            .expect("resolve")
            .is_some()
    );
    assert!(
        repository
            .node_at_path("/content/child-05")
            .expect("resolve")
            .is_some()
    );
    assert!(
        repository
            .node_at_path("/missing")
            .expect("resolve")
            .is_none()
    );
    assert!(
        repository
            .node_at_path("/content/missing")
            .expect("resolve")
            .is_none()
    );
}

#[test]
fn spreads_segments_across_archives_and_selects_newest_generation() {
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
fn directory_snapshot(path: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
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
fn recovers_archives_without_an_index() {
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
fn journal_rewinds_past_revisions_with_missing_segments() {
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
fn rejects_stores_that_cannot_be_opened() {
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

/// Writes the synthetic repository to the directory named by the
/// `FROE_EXAMPLE_REPOSITORY_PATH` environment variable, for manually
/// exercising the command line against real files. Ignored in normal test
/// runs.
#[test]
#[ignore = "development utility, run explicitly with --ignored"]
fn write_example_repository_for_manual_smoke_testing() {
    let Ok(target) = std::env::var("FROE_EXAMPLE_REPOSITORY_PATH") else {
        return;
    };
    let target = std::path::PathBuf::from(target);
    std::fs::create_dir_all(&target).expect("create example directory");
    let repository = build_synthetic_repository();
    let mut archive = ArchiveBuilder::new();
    archive.add_segment(
        repository.values_segment.0,
        repository.values_segment.1.clone(),
    );
    archive.add_segment(repository.tree_segment.0, repository.tree_segment.1.clone());
    write_repository(
        &target,
        &[("data00000a.tar".to_owned(), archive.build("data00000a.tar"))],
        std::slice::from_ref(&repository.journal_line),
    );
}

#[test]
fn stable_identifiers_use_the_journal_record_form() {
    let directory = TestDirectory::new("stable-identifiers");
    write_single_archive_repository(&directory);
    let repository = Repository::open(&directory.path).expect("open repository");
    let head = repository.head();
    let stable = head.stable_identifier().expect("stable identifier");
    let head_identifier = repository.head_record_identifier();
    assert_eq!(
        stable,
        format!(
            "{}:{}",
            head_identifier.segment, head_identifier.record_number
        )
    );
}
