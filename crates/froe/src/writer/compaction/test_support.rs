//! The store shapes the compaction tests copy: populated, checkpointed,
//! shared-subtree, wide, and randomly generated, with the assertions that
//! prove a copy reproduced one.

use crate::content::node::PropertyValues;
use crate::content::property::PropertyValue;
use crate::content::provider::SegmentProvider;
use crate::store::Repository;
use crate::writer::commit::create_checkpoint;
use crate::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use crate::writer::store_writer::WritableRepository;

pub(crate) struct TestDirectory {
    pub(crate) path: std::path::PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("froe-compaction-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn corrupt_graph_checksum(path: &std::path::Path) {
    let mut bytes = std::fs::read(path).expect("read archive");
    let mut offset = 0usize;
    while offset + 512 <= bytes.len() {
        let header = &bytes[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let name_end = header[..100]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(100);
        let name = std::str::from_utf8(&header[..name_end]).expect("UTF-8 TAR entry name");
        let size_text = std::str::from_utf8(&header[124..136])
            .expect("ASCII TAR size")
            .trim_matches(['\0', ' ']);
        let size = usize::from_str_radix(size_text, 8).expect("octal TAR size");
        if std::path::Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gph"))
        {
            let payload_end = offset + 512 + size;
            assert!(size >= 16, "graph payload includes its footer");
            bytes[payload_end - 16] ^= 0x01;
            std::fs::write(path, bytes).expect("corrupt graph checksum");
            return;
        }
        offset += 512 + size.div_ceil(512) * 512;
    }
    panic!("graph trailer not found in {}", path.display());
}

/// Builds a store with a `/content` node carrying properties and two
/// children, plus one checkpoint sharing the root.
pub(crate) fn build_populated_store(directory: &TestDirectory) {
    let store = WritableRepository::open(&directory.path).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    let first_child = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("child");
    let second_child = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("child");
    let title = writer.write_string("Compaction Test").expect("value");
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Many(vec![
                ("alpha".to_owned(), first_child),
                ("beta".to_owned(), second_child),
            ]),
            &[PropertyToWrite {
                name: "title".to_owned(),
                property_type: crate::content::property::PropertyType::String,
                values: PropertyValuesToWrite::Single(title),
            }],
        )
        .expect("content");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "content".to_owned(),
                node: content,
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
    assert!(store.compare_and_set_head(previous, head));
    create_checkpoint(
        &store,
        10_000_000,
        &[("purpose".to_owned(), "test".to_owned())],
    )
    .expect("checkpoint");
    store.close().expect("close");
}

pub(crate) fn assert_content_intact(directory: &TestDirectory) {
    let repository = Repository::open(&directory.path).expect("reader opens");
    let content = repository
        .node_at_path("/content")
        .expect("resolve")
        .expect("present");
    assert_eq!(content.child_node_count().expect("count"), 2);
    assert!(content.child_node("alpha").expect("read").is_some());
    assert!(content.child_node("beta").expect("read").is_some());
    let title = content.property("title").expect("read").expect("present");
    assert_eq!(
        title.values,
        PropertyValues::Single(PropertyValue::String("Compaction Test".to_owned()))
    );
    let checkpoints = repository.checkpoints().expect("checkpoints");
    assert_eq!(checkpoints.len(), 1, "the checkpoint survives compaction");
    let (_, checkpoint) = &checkpoints[0];
    let snapshot = checkpoint
        .child_node("root")
        .expect("read")
        .expect("snapshot");
    assert!(snapshot.child_node("content").expect("read").is_some());
}

/// Builds a store whose super-root carries a content root and three
/// checkpoints, and returns the checkpoint names in creation order.
pub(crate) fn build_store_with_checkpoints(directory: &TestDirectory) -> Vec<String> {
    build_populated_store(directory);
    let store = WritableRepository::open(&directory.path).expect("open for checkpoints");
    let mut names = Vec::new();
    for _ in 0..3 {
        names.push(create_checkpoint(&store, 60 * 60 * 1000, &[]).expect("create the checkpoint"));
    }
    store.close().expect("close after checkpoints");
    names
}

/// Every child name a node has, in stored order.
pub(crate) fn child_names(node: &crate::content::node::NodeState<'_>) -> Vec<String> {
    node.child_node_entries()
        .expect("enumerate the children")
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Copies a repository directory, so two mechanisms can be applied to the
/// same store. Checkpoint names are randomly generated, so a fixture built
/// twice is not the same fixture and the two results cannot be compared.
pub(crate) fn copy_repository(source: &std::path::Path, target: &std::path::Path) {
    std::fs::create_dir_all(target).expect("create the copy directory");
    for entry in std::fs::read_dir(source).expect("list the source store") {
        let entry = entry.expect("read the directory entry");
        if entry.file_type().expect("file type").is_file() {
            std::fs::copy(entry.path(), target.join(entry.file_name()))
                .expect("copy a repository file");
        }
    }
    // `repo.lock` is a live artefact, never part of the copy.
    let _ = std::fs::remove_file(target.join("repo.lock"));
}

/// Builds `levels` diamonds under the super-root: every level references
/// the *same* next-level node twice, so distinct nodes grow linearly
/// while distinct root-to-leaf paths grow as 2^levels.
///
/// `ballast` fresh nodes sit between the two references. They are what
/// decides whether the memo survives from the first reference to the
/// second: with `ballast` below the budget the second lookup hits, and
/// with it above, every level re-copies its whole subtree.
pub(crate) fn build_diamond_chain(directory: &TestDirectory, levels: usize, ballast: usize) {
    let store = WritableRepository::open(&directory.path).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    let mut node = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("leaf");
    for level in 0..levels {
        let mut children = vec![("a_left".to_owned(), node)];
        for index in 0..ballast {
            let value = writer
                .write_string(&format!("{level}-{index}"))
                .expect("filler value");
            let filler = writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "n".to_owned(),
                        property_type: crate::content::property::PropertyType::String,
                        values: PropertyValuesToWrite::Single(value),
                    }],
                )
                .expect("filler");
            children.push((format!("b_fill{index:04}"), filler));
        }
        children.push(("c_right".to_owned(), node));
        node = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Many(children),
                &[],
            )
            .expect("diamond");
    }
    let head = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node,
            },
            &[],
        )
        .expect("super root");
    writer.finish().expect("finish");
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
}

/// The exact number of distinct node records reachable from `root` — the
/// figure `compacted_nodes` is supposed to equal.
pub(crate) fn distinct_reachable_nodes(
    provider: &dyn SegmentProvider,
    root: crate::segment::record::RecordIdentifier,
) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![root];
    while let Some(record) = pending.pop() {
        if !seen.insert(record) {
            continue;
        }
        let node = crate::content::node::NodeState::new(provider, record);
        for (_, child) in node.child_node_entries().expect("children") {
            pending.push(child.record_identifier());
        }
    }
    seen.len()
}

/// A wide, shallow tree of roughly `fanout * fanout` leaves.
pub(crate) fn build_wide_store(directory: &TestDirectory, fanout: usize) {
    let store = WritableRepository::open(&directory.path).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    let mut branches = Vec::with_capacity(fanout);
    for branch in 0..fanout {
        let mut leaves = Vec::with_capacity(fanout);
        for leaf in 0..fanout {
            let value = writer
                .write_string(&format!("{branch}-{leaf}"))
                .expect("leaf value");
            let node = writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "n".to_owned(),
                        property_type: crate::content::property::PropertyType::String,
                        values: PropertyValuesToWrite::Single(value),
                    }],
                )
                .expect("leaf");
            leaves.push((format!("leaf{leaf:05}"), node));
        }
        let node = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Many(leaves),
                &[],
            )
            .expect("branch");
        branches.push((format!("branch{branch:05}"), node));
    }
    let root = writer
        .write_node(None, &[], &ChildNodesToWrite::Many(branches), &[])
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
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
}

pub(crate) fn resident_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("statm");
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .expect("resident field")
        .parse()
        .expect("page count");
    pages * 4096
}

/// A deterministic generator, so a failure names a seed that reproduces
/// it. Nothing in the crate needs randomness, so this stays local.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub(crate) fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next() % bound as u64) as usize
    }
}

/// Builds a random acyclic content graph. Every node draws its children
/// from the nodes already written, which is what the segment format
/// guarantees anyway (a record only references earlier records), so the
/// result is a legal DAG with arbitrary sharing.
pub(crate) fn build_random_dag(directory: &TestDirectory, seed: u64) {
    let mut rng = Rng(seed | 1);
    let store = WritableRepository::open(&directory.path).expect("bootstrap");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);

    // Most shapes stay small so many of them run; every twentieth is big
    // enough that `RewrittenNodes` crosses at least one growth (its first
    // is at 717 entries), so rehashing is exercised end to end and not
    // only by the table's own test.
    let node_count = if seed.is_multiple_of(20) {
        800 + rng.below(1700)
    } else {
        8 + rng.below(60)
    };
    let mut written = Vec::with_capacity(node_count);
    for index in 0..node_count {
        let child_count = if written.is_empty() { 0 } else { rng.below(5) };
        let mut children = Vec::with_capacity(child_count);
        for child in 0..child_count {
            // Draw with replacement, so the same record can be referenced
            // several times from one parent and from many parents.
            let picked = written[rng.below(written.len())];
            children.push((format!("c{child:03}"), picked));
        }
        let value = writer
            .write_string(&format!("seed{seed}-node{index}"))
            .expect("value");
        let node = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &match children.len() {
                    0 => ChildNodesToWrite::Zero,
                    1 => {
                        let (name, node) = children.into_iter().next().expect("one child");
                        ChildNodesToWrite::One { name, node }
                    }
                    _ => ChildNodesToWrite::Many(children),
                },
                &[PropertyToWrite {
                    name: "n".to_owned(),
                    property_type: crate::content::property::PropertyType::String,
                    values: PropertyValuesToWrite::Single(value),
                }],
            )
            .expect("node");
        written.push(node);
    }
    let root = *written.last().expect("at least one node");
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
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
}

/// Every data segment's referenced segments must resolve — the sweep
/// must never delete a bulk segment a kept data segment points at.
pub(crate) fn assert_no_dangling_segment_references(directory: &TestDirectory) {
    let repository = Repository::open(&directory.path).expect("reader opens");
    for segment_identifier in repository.segment_identifiers() {
        if segment_identifier.is_bulk_segment() {
            continue;
        }
        let view = repository
            .segment(segment_identifier)
            .expect("data segment readable");
        for referenced in &view.structure.referenced_segments {
            assert!(
                repository.contains_segment(*referenced),
                "kept data segment {segment_identifier} references missing segment \
                 {referenced}"
            );
        }
    }
}
