//! Reading a store: walking the content tree, resolving paths however
//! they are spelled, and the typed properties and checkpoints it holds.

use super::*;

#[test]
pub(crate) fn traverses_the_content_tree() {
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
pub(crate) fn reads_typed_properties() {
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
pub(crate) fn reads_checkpoints_sharing_records_with_the_head() {
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
pub(crate) fn resolves_an_existing_path_however_it_is_spelled() {
    let directory = TestDirectory::new("resolves-existing-paths");
    write_single_archive_repository(&directory);
    let repository = Repository::open(&directory.path).expect("open repository");

    for path in ["/", "/content", "content/", "/content/child-05"] {
        assert!(
            repository.node_at_path(path).expect("resolve").is_some(),
            "{path} names a node in the fixture, with or without surrounding slashes"
        );
    }
}

#[test]
pub(crate) fn resolves_a_path_with_no_node_to_none_rather_than_an_error() {
    let directory = TestDirectory::new("resolves-missing-paths");
    write_single_archive_repository(&directory);
    let repository = Repository::open(&directory.path).expect("open repository");

    for path in ["/missing", "/content/missing"] {
        assert!(
            repository.node_at_path(path).expect("resolve").is_none(),
            "{path} names no node, which is an absence and not a failure"
        );
    }
}

#[test]
pub(crate) fn stable_identifiers_use_the_journal_record_form() {
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

/// Writes the synthetic repository to the directory named by the
/// `FROE_EXAMPLE_REPOSITORY_PATH` environment variable, for manually
/// exercising the command line against real files. Ignored in normal test
/// runs.
#[test]
#[ignore = "development utility, run explicitly with --ignored"]
pub(crate) fn write_example_repository_for_manual_smoke_testing() {
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
