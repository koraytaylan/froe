//! The repositories these tests read: assembled record by record from the
//! independent test encoder, so a reader is checked against bytes froe's
//! own writer never produced.

use super::*;

/// Builds one node record: its own stable identifier, its template, and
/// whatever extra slots that template declares.
pub(crate) type NodeRecordWriter<'writer> = dyn Fn(u32, u32, &[Vec<u8>]) -> Vec<u8> + 'writer;

/// The number of children under `/content`; above 32 so the child map is
/// stored as a branch record with leaf buckets.
pub(crate) const CONTENT_CHILD_COUNT: usize = 40;

/// String record numbers in the values segment.
pub(crate) mod value_records {
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
pub(crate) mod tree_records {
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
pub(crate) struct SyntheticRepository {
    pub(crate) values_segment: (support::SegmentUuid, Vec<u8>),
    pub(crate) tree_segment: (support::SegmentUuid, Vec<u8>),
    pub(crate) journal_line: String,
}

/// Every string the tree refers to, in one segment of its own.
pub(crate) fn build_values_segment(
    values_uuid: support::SegmentUuid,
) -> (SegmentBuilder, Vec<String>) {
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

    (values, child_names)
}

/// Every template the synthetic tree's nodes point at.
///
/// Property names sit in the mandatory on-disk order — sorted by signed
/// Java `String.hashCode` — which is what makes this fixture a valid
/// stand-in for one Oak wrote.
pub(crate) fn add_tree_templates(
    tree: &mut SegmentBuilder,
    value_identifier: &dyn Fn(u32) -> Vec<u8>,
    own_identifier: &dyn Fn(u32) -> Vec<u8>,
) {
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
}

/// The forty child nodes under /content and the branch map that indexes
/// them — above 32 entries, so the map is a branch with leaf buckets.
pub(crate) fn add_content_children(
    tree: &mut SegmentBuilder,
    child_names: &[String],
    value_identifier: &dyn Fn(u32) -> Vec<u8>,
    own_identifier: &dyn Fn(u32) -> Vec<u8>,
    node_record: &NodeRecordWriter<'_>,
    allocate: &mut impl FnMut() -> u32,
) -> (Vec<u32>, u32) {
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
    let content_child_map = build_child_map(tree, allocate, &map_entries);
    (child_node_records, content_child_map)
}

/// The checkpoints container and the one checkpoint under it.
///
/// Its root child shares the live content root record, which is what
/// makes a checkpoint cheap in Oak and what the reader must not mistake
/// for two separate trees.
pub(crate) fn add_checkpoint_subtree(
    tree: &mut SegmentBuilder,
    root_node: u32,
    value_identifier: &dyn Fn(u32) -> Vec<u8>,
    own_identifier: &dyn Fn(u32) -> Vec<u8>,
    node_record: &NodeRecordWriter<'_>,
    allocate: &mut impl FnMut() -> u32,
) -> u32 {
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

    checkpoints_parent_node
}

/// /content with its properties and children, /empty beside it, and the
/// content root that holds both.
pub(crate) fn add_content_root(
    tree: &mut SegmentBuilder,
    content_child_map: u32,
    value_identifier: &dyn Fn(u32) -> Vec<u8>,
    own_identifier: &dyn Fn(u32) -> Vec<u8>,
    node_record: &NodeRecordWriter<'_>,
    allocate: &mut impl FnMut() -> u32,
) -> u32 {
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
        tree,
        allocate,
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

    root_node
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
pub(crate) fn build_synthetic_repository() -> SyntheticRepository {
    let values_uuid = data_segment_uuid(0x0002);
    let tree_uuid = data_segment_uuid(0x0001);

    let (values, child_names) = build_values_segment(values_uuid);

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

    add_tree_templates(&mut tree, &value_identifier, &own_identifier);

    // A node record: stable identifier (self), template, extra slots.
    let node_record = |own_record_number: u32, template: u32, slots: &[Vec<u8>]| {
        let mut bytes = record_identifier_bytes(0, own_record_number);
        bytes.extend(record_identifier_bytes(0, template));
        for slot in slots {
            bytes.extend_from_slice(slot);
        }
        bytes
    };

    let (_, content_child_map) = add_content_children(
        &mut tree,
        &child_names,
        &value_identifier,
        &own_identifier,
        &node_record,
        &mut allocate,
    );

    let root_node = add_content_root(
        &mut tree,
        content_child_map,
        &value_identifier,
        &own_identifier,
        &node_record,
        &mut allocate,
    );

    let checkpoints_parent_node = add_checkpoint_subtree(
        &mut tree,
        root_node,
        &value_identifier,
        &own_identifier,
        &node_record,
        &mut allocate,
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
pub(crate) fn write_single_archive_repository(directory: &TestDirectory) -> SyntheticRepository {
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

/// Whether the fixture includes the segment holding the string record a
/// cross-segment blob identifier points at.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StringSegment {
    /// Included, so the identifier resolves.
    Present,
    /// Omitted, so the identifier dangles and recovery must fail closed.
    Absent,
}

/// Builds a two-segment archive fixture where a large (`0xF0`-class)
/// external blob identifier in one segment points at a string record in
/// the *other* segment — the layout the production writer emits when a
/// segment boundary falls between the two records. `string_segment`
/// controls whether the string-bearing segment is actually included.
pub(crate) fn cross_segment_blob_archive(string_segment_presence: StringSegment) -> Vec<u8> {
    let identifier_holder = data_segment_uuid(0x51);
    let string_holder = data_segment_uuid(0x52);

    let mut string_segment = SegmentBuilder::new(string_holder);
    string_segment.add_record(
        0,
        TYPE_VALUE,
        string_record("blob-identifier-in-another-segment"),
    );

    let mut identifier_segment = SegmentBuilder::new(identifier_holder);
    let reference = identifier_segment.add_referenced_segment(string_holder);
    let mut blob_identifier_record = vec![0xF0u8];
    blob_identifier_record.extend(record_identifier_bytes(reference, 0));
    identifier_segment.add_record(
        0,
        support::TYPE_EXTERNAL_BLOB_IDENTIFIER,
        blob_identifier_record,
    );

    let mut archive = ArchiveBuilder::new().without_index();
    if string_segment_presence == StringSegment::Present {
        archive.add_segment(string_holder, string_segment.build());
    }
    archive.add_segment(identifier_holder, identifier_segment.build());
    archive.build("data00001a.tar")
}

/// Writes the synthetic content repository plus the cross-segment blob
/// archive (which has no index, so a write open must recover it).
pub(crate) fn write_repository_with_blob_archive(
    directory: &TestDirectory,
    string_segment_presence: StringSegment,
) {
    let repository = build_synthetic_repository();
    let mut content_archive = ArchiveBuilder::new();
    content_archive.add_segment(
        repository.values_segment.0,
        repository.values_segment.1.clone(),
    );
    content_archive.add_segment(repository.tree_segment.0, repository.tree_segment.1.clone());
    write_repository(
        &directory.path,
        &[
            (
                "data00000a.tar".to_owned(),
                content_archive.build("data00000a.tar"),
            ),
            (
                "data00001a.tar".to_owned(),
                cross_segment_blob_archive(string_segment_presence),
            ),
        ],
        std::slice::from_ref(&repository.journal_line),
    );
}
