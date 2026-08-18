//! Phases 3 and 4: froe writes a checkpoint and a content subtree, and
//! Oak reads both back.

use super::*;

/// Phase 4: froe writes a checkpoint against the Oak store.
///
/// A metadata-only write-path test (logical head update). If this fails,
/// the writer's checkpoint machinery is broken, which affects cleanup's
/// expired-checkpoint test and compact's checkpoint preservation.
/// Depends on `commit` (the writer can already produce content Oak reads).
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn checkpoint() {
    let store = oak_store();

    eprintln!("  froe checkpoint create (lifetime 1000 ms)");
    let created = froe(&[
        "checkpoint",
        "create",
        store.to_str().unwrap(),
        "--lifetime-milliseconds",
        "1000",
        "--yes",
    ]);
    // Pin the identity of the checkpoint that was created. Asserting only that
    // the listing is non-empty is satisfied by the literal string "no
    // checkpoints", so it would pass even if nothing had been written.
    let name = created
        .lines()
        .find_map(|line| line.trim().strip_prefix("created checkpoint "))
        .unwrap_or_else(|| panic!("create reports the checkpoint name: {created}"))
        .trim()
        .to_owned();
    assert!(!name.is_empty(), "the created checkpoint has a name");

    eprintln!("  froe checkpoints (the new one must be listed by name)");
    let checkpoints = froe(&["checkpoints", store.to_str().unwrap()]);
    assert!(
        checkpoints.contains(&name),
        "the checkpoint {name} froe created is listed: {checkpoints}"
    );

    // Record it so the cleanup phase can prove this exact checkpoint expired
    // and was removed, rather than trusting a count.
    std::fs::write(work_root().join("expiring-checkpoint.txt"), &name)
        .expect("record the expiring checkpoint name");

    eprintln!("  checkpoint phase passed ({name})");
}

/// The node froe writes into the Oak store: a folder holding one `node`
/// child that carries a string, a long, and a boolean property. Returns the
/// folder's record, ready to be attached under `/content/interop`.
pub(crate) fn build_froe_written_folder<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
) -> RecordIdentifier {
    let title_value = writer
        .write_string("Froe-Written Node")
        .expect("title value");
    let count_value = writer.write_string("42").expect("count value");
    let active_value = writer.write_string("true").expect("active value");
    let leaf = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Zero,
            &[
                PropertyToWrite {
                    name: "jcr:title".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::Single(title_value),
                },
                PropertyToWrite {
                    name: "count".to_owned(),
                    property_type: PropertyType::Long,
                    values: PropertyValuesToWrite::Single(count_value),
                },
                PropertyToWrite {
                    name: "active".to_owned(),
                    property_type: PropertyType::Boolean,
                    values: PropertyValuesToWrite::Single(active_value),
                },
            ],
        )
        .expect("write leaf node");
    writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::One {
                name: "node".to_owned(),
                node: leaf,
            },
            &[],
        )
        .expect("write folder node")
}

/// The ancestors between the super-root and the node froe is about to add,
/// as they stand before the commit.
pub(crate) struct CommitSpine {
    pub(crate) interop: RecordIdentifier,
    pub(crate) content: RecordIdentifier,
    pub(crate) root: RecordIdentifier,
}

pub(crate) fn resolve_commit_spine(commit_store: &Path) -> CommitSpine {
    let interop_path = froe::content::path::normalized_path("/content/interop");
    let content_path = froe::content::path::normalized_path("/content");
    let root_path = froe::content::path::normalized_path("/");
    let repository = froe::store::Repository::open(commit_store).expect("open reader");
    let interop_node = repository
        .node_at_path(&interop_path)
        .expect("resolve /content/interop")
        .expect("/content/interop exists");
    let content_node = repository
        .node_at_path(&content_path)
        .expect("resolve /content")
        .expect("/content exists");
    let root_node = repository
        .node_at_path(&root_path)
        .expect("resolve /")
        .expect("/ exists");
    CommitSpine {
        interop: interop_node.record_identifier(),
        content: content_node.record_identifier(),
        root: root_node.record_identifier(),
    }
}

/// Rewrites the spine from `/content/interop` up to the super-root so the
/// new `folder` is reachable, and returns the new super-root. Each step
/// feeds the next: a rewritten node has a new record, so its parent must be
/// rewritten to point at it.
pub(crate) fn rewrite_commit_spine<Sink: SegmentSink>(
    writable: &WritableRepository,
    writer: &mut RecordWriter<Sink>,
    head: RecordIdentifier,
    spine: &CommitSpine,
    folder: RecordIdentifier,
) -> RecordIdentifier {
    // 1. Rewrite /content/interop to add the new child.
    let mut edits = froe::writer::commit::ChildEdits::new();
    edits.insert("froe-written".to_owned(), Some(folder));
    let new_interop = rewrite_node_with_child_edits(writable, writer, Some(spine.interop), &edits)
        .expect("rewrite interop node");

    // 2. Rewrite /content to point at the new /content/interop.
    let mut content_edits = froe::writer::commit::ChildEdits::new();
    content_edits.insert("interop".to_owned(), Some(new_interop));
    let new_content =
        rewrite_node_with_child_edits(writable, writer, Some(spine.content), &content_edits)
            .expect("rewrite /content");

    // 3. Rewrite / (root) to point its content child at the new /content.
    let mut root_content_edits = froe::writer::commit::ChildEdits::new();
    root_content_edits.insert("content".to_owned(), Some(new_content));
    let new_root =
        rewrite_node_with_child_edits(writable, writer, Some(spine.root), &root_content_edits)
            .expect("rewrite root");

    // 4. Rewrite the super-root to point its `root` child at the new root.
    let mut super_root_edits = froe::writer::commit::ChildEdits::new();
    super_root_edits.insert("root".to_owned(), Some(new_root));
    rewrite_node_with_child_edits(writable, writer, Some(head), &super_root_edits)
        .expect("rewrite super-root")
}

/// froe reads back the subtree it just wrote, through its own CLI.
pub(crate) fn assert_froe_reads_its_own_writes(commit_store: &Path) {
    eprintln!("  froe tree /content/interop/froe-written (depth 2)");
    let tree = froe(&[
        "tree",
        commit_store.to_str().unwrap(),
        "/content/interop/froe-written",
        "--depth",
        "2",
    ]);
    assert!(
        tree.contains("nt:unstructured"),
        "tree shows the node: {tree}"
    );

    eprintln!("  froe node /content/interop/froe-written/node");
    let node = froe(&[
        "node",
        commit_store.to_str().unwrap(),
        "/content/interop/froe-written/node",
    ]);
    assert!(
        node.contains("Froe-Written Node"),
        "froe reads the title property: {node}"
    );
    assert!(
        node.contains("count") && node.contains("Long"),
        "froe reads the long property: {node}"
    );
    assert!(
        node.contains("active") && node.contains("Boolean"),
        "froe reads the boolean property: {node}"
    );
}

/// A commit is the one operation that is *supposed* to change content,
/// which makes "did it change anything else?" the question worth asking. A
/// node record rewritten on the path from the root to the new subtree that
/// lost a property, or a re-rendered value, would be invisible to every
/// other assertion in this phase.
///
/// Only added paths are expected: a digest line carries a node's own
/// properties, so an ancestor gaining a child does not change its line.
pub(crate) fn assert_commit_only_added_nodes(digest_before: &str, commit_store: &Path) {
    eprintln!("  content digest after the commit");
    let before_nodes = digest_nodes(digest_before);
    let after_digest = digest_store(commit_store);
    let after_nodes = digest_nodes(&after_digest);
    let unexpected: Vec<&str> = before_nodes
        .iter()
        .filter(|(path, properties)| after_nodes.get(*path) != Some(properties))
        .map(|(path, _)| *path)
        .collect();
    assert!(
        unexpected.is_empty(),
        "commit: {} node(s) that existed before the commit changed or disappeared, and a \
         commit must only add:\n{}",
        unexpected.len(),
        unexpected
            .iter()
            .take(20)
            .map(|path| format!("  {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let added: Vec<&str> = after_nodes
        .keys()
        .filter(|path| !before_nodes.contains_key(*path))
        .copied()
        .collect();
    assert!(
        !added.is_empty()
            && added
                .iter()
                .all(|path| path.starts_with("/content/interop/froe-written")),
        "commit: the added nodes are exactly the froe-written subtree, nothing else: {added:?}"
    );
    eprintln!(
        "  commit added {} nodes and changed nothing else",
        added.len()
    );
}

/// Phase 3: froe adds nodes with typed properties to the content tree.
///
/// Uses the library's commit API (`rewrite_node_with_child_edits`) to
/// add a subtree under `/content/interop/froe-written` with string, long,
/// and boolean properties. Then boots Sling against the modified store
/// and verifies Oak reads the froe-written nodes back correctly.
///
/// If this fails, froe cannot write content that Oak reads — the core
/// interop claim. There is no point testing checkpoint, compact, cleanup,
/// backup, or recover if the writer cannot produce content Oak reads.
/// Depends on `read` (to verify).
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn commit() {
    let store = oak_store();
    let commit_store = work_root().join("commit-store");
    eprintln!("  copying store to {}", commit_store.display());
    copy_store(&store, &commit_store);
    let digest_before = digest_store(&commit_store);

    eprintln!("  opening store for writing");
    let writable = WritableRepository::open(&commit_store).expect("open writable");
    let generation = writable.writing_generation().expect("generation");
    let mut writer = writable.record_writer(generation);

    let folder = build_froe_written_folder(&mut writer);

    // Commit: add the new folder as a child of /content/interop. The Oak
    // super-root has children `root` (the content tree root, `/`) and
    // `checkpoints`, so the spine to rewrite is
    //
    //   super-root → root (/) → content → interop → [froe-written]
    let head = writable.head();
    let spine = resolve_commit_spine(&commit_store);
    let new_super_root = rewrite_commit_spine(&writable, &mut writer, head, &spine, folder);

    writer.finish().expect("finish writer");
    assert!(
        writable.compare_and_set_head(head, new_super_root),
        "head CAS succeeded (single-writer, no contention)"
    );
    writable.flush().expect("flush");
    writable.close().expect("close");

    eprintln!("  froe-written nodes committed");

    assert_froe_reads_its_own_writes(&commit_store);
    assert_commit_only_added_nodes(&digest_before, &commit_store);

    // Boot Sling against the froe-written store and verify Oak reads the
    // froe-written nodes.
    eprintln!("  booting Sling against the froe-written store");
    let volume = PodmanVolume::new("froe-interop-commit");
    let bootstrap = PodmanContainer::run_detached("froe-commit-bootstrap", 8084, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&commit_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-commit-verify", 8084, &volume.name);
    wait_for_sling(8084, "froe-commit-verify");

    // The phase that carries the core interop claim had no repair-marker
    // scan at all, so a run where Oak rebuilt an index or skipped an
    // archive on its way to serving the node would have passed. Reading
    // the node back proves nothing if Oak had to reconstruct the store to
    // do it. The baseline comparison is deliberately absent instead: this
    // phase *adds* content, so the pristine fingerprint would rightly
    // differ, and the digest assertion below is what covers it.
    assert_oak_consumed_store_as_written("froe-commit-verify", "commit");

    eprintln!("  fetching froe-written node from Sling");
    let sling_node = Command::new("curl")
        .args([
            "-s",
            "-u",
            "admin:admin",
            "http://localhost:8084/content/interop/froe-written/node.tidy.json",
        ])
        .output()
        .expect("curl froe-written node");
    let sling_response = String::from_utf8_lossy(&sling_node.stdout);
    assert!(
        sling_response.contains("Froe-Written Node"),
        "Sling reads the froe-written title: {sling_response}"
    );

    drop(sling);
    eprintln!("  commit phase passed");
}
