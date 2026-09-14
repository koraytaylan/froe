//! Phases 1 and 2: build the Oak-written store every later phase copies,
//! and prove froe reads it.

use super::*;

// ---------------------------------------------------------------------------
// Test phases
// ---------------------------------------------------------------------------

/// Phase 1: Generate the Oak store fixture.
///
/// Boots Sling with `TarMK`, populates content under /content/interop,
/// churns content to produce orphaned segments, and stops cleanly. The
/// resulting store is the shared fixture for all later phases.
#[test]
#[ignore = "requires podman and the apache/sling:14 image"]
pub(crate) fn generate() {
    let root = work_root();
    // Retire any pointer left by an earlier run before building the new
    // fixture. The pointer exists so a *separate* `cargo test` process can
    // find the fixture this one produced; it must never let a phase that
    // ran before `generate` in the same run silently pick up the previous
    // run's store and report a pass about bytes nobody produced today.
    let _ = std::fs::remove_file(fixture_pointer_path());
    let _ = std::fs::remove_file(digest_baseline_path());
    let volume = PodmanVolume::new("froe-interop-generate");
    eprintln!("  starting Sling on :8080");
    let sling = PodmanContainer::run_detached("froe-interop-gen", 8080, &volume.name);
    wait_for_sling(8080, "froe-interop-gen");

    eprintln!("  populating content");
    populate_content(8080);

    // The three index shapes plans 0007 onward rebuild, and which the
    // fixture otherwise lacks: the reference index holds no references at
    // all, the unique indexes cover only Sling's own system nodes, and no
    // property index was rebuilt by Oak from existing content — which is the
    // exact oracle shape plan 0007 compares against. All three go through
    // Sling, never through froe, so the bytes are authentically Oak's.
    eprintln!("  adding references, a group with members, and an index to rebuild");
    populate_references(8080);
    populate_group_with_members(8080);
    // Posted last, after the content it indexes exists, so Oak's rebuild has
    // something to find.
    populate_rebuilt_property_index(8080);

    eprintln!("  churning content to produce orphaned segments");
    churn_content(8080);

    // Pin the Oak build now, while the container is still up. The image tag
    // is mutable, so the version inside it is the only durable coordinate.
    // Record it for the run record, which is the artifact that makes a passing
    // run auditable afterwards instead of an ephemeral console line.
    let oak_version = assert_oak_build("froe-interop-gen");
    std::fs::write(work_root().join("oak-build.txt"), &oak_version)
        .expect("record the Oak build under test");

    // Record what Oak serves before froe has touched anything. Every later
    // phase compares against this, which is what turns "Sling still serves
    // the root node" into "Sling serves exactly the same tree".
    eprintln!("  recording the content baseline from Oak's own store");
    save_content_baseline(8080);

    eprintln!("  stopping Sling cleanly");
    sling.stop();

    let store = root.join("oak-store");
    eprintln!("  extracting store to {}", store.display());
    store_from_volume(&volume.name, &store);

    let entries = std::fs::read_dir(&store)
        .expect("read_dir")
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        entries
            .iter()
            .any(|n| n.starts_with("data") && n.ends_with(".tar")),
        "store has at least one data*.tar archive: {entries:?}"
    );
    assert!(
        entries.contains(&"journal.log".to_owned()),
        "store has journal.log: {entries:?}"
    );
    assert!(
        entries.contains(&"manifest".to_owned()),
        "store has manifest: {entries:?}"
    );

    OAK_STORE
        .set(store.clone())
        .expect("store the Oak store path");
    std::fs::write(fixture_pointer_path(), store.to_string_lossy().as_bytes())
        .expect("record the fixture path for later processes");

    // Before the baseline: a fixture missing one of these shapes would
    // record a digest of the wrong store, and every later phase would
    // compare correctly against bytes that prove less than they claim.
    assert_index_fixture_built(&store);

    // The baseline every later phase compares its digest against: the
    // content exactly as Oak itself wrote it, before froe has touched
    // anything. Taken here rather than per phase so that a difference
    // names the operation that introduced it.
    let baseline = digest_store(&store);
    assert!(
        baseline.lines().count() > 100,
        "the digest baseline is implausibly small, so later comparisons would be vacuous:\n\
         {baseline}"
    );
    std::fs::write(digest_baseline_path(), &baseline).expect("write the digest baseline");
    eprintln!(
        "  recorded content digest baseline: {} nodes",
        baseline.lines().count()
    );

    eprintln!("  Oak store generated at {}", store.display());
}

/// Get the shared Oak store, or panic with a clear message.
///
/// Falls back to the path `generate` recorded on disk, because the suite's
/// own wrapper script runs `generate` and the named phase in two separate
/// `cargo test` processes and a `OnceLock` does not survive that. Without
/// the fallback no phase can be re-run on its own — and a failure that
/// cannot be reproduced in isolation cannot be attributed to anything.
pub(crate) fn oak_store() -> PathBuf {
    if let Some(store) = OAK_STORE.get() {
        return store.clone();
    }
    let recorded = std::fs::read_to_string(fixture_pointer_path()).unwrap_or_default();
    let store = PathBuf::from(recorded.trim());
    assert!(
        !recorded.trim().is_empty() && store.join("journal.log").exists(),
        "Oak store not generated. Run the `generate` phase first:\n\
             cargo test -p froe-cli --features interop -- --ignored interop::generate"
    );
    store
}

/// The recorded digest of the pristine Oak-written store.
pub(crate) fn digest_baseline_path() -> PathBuf {
    work_root().join("content-digest-baseline.txt")
}

/// The baseline digest, or a clear message about which phase records it.
pub(crate) fn digest_baseline() -> String {
    std::fs::read_to_string(digest_baseline_path()).expect(
        "read the content digest baseline; the generate phase records it and every later \
         phase compares against it",
    )
}

/// Phase 2: froe reads the Oak-written store.
///
/// If this fails, froe cannot read Oak's format and no write-path
/// verification is meaningful — there is no way to confirm that froe's
/// output is correct without a working reader.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn read() {
    let store = oak_store();
    eprintln!("  froe summary");
    let summary = froe(&["summary", store.to_str().unwrap()]);
    assert!(summary.contains("archives"), "summary has archives line");
    assert!(summary.contains("segments"), "summary has segments line");
    assert!(summary.contains("head"), "summary has head line");

    eprintln!("  froe tree /content/interop (depth 3)");
    let tree = froe(&[
        "tree",
        store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "3",
    ]);
    assert!(tree.contains("sling:Folder"), "tree shows sling:Folder");
    assert!(
        tree.contains("sling:OrderedFolder"),
        "tree shows OrderedFolder"
    );
    assert!(tree.contains("nt:file"), "tree shows nt:file");

    eprintln!("  froe check (expect exit 0)");
    assert_check_passes_at_head(&store, "read");

    // The digest `generate` recorded, re-derived in this process. It
    // proves the rendering is reproducible across runs — without which
    // every later comparison would report differences that mean nothing —
    // and that reading the pristine store is itself stable.
    eprintln!("  content digest reproduces the baseline recorded at generate");
    assert_digest_delta(
        &digest_baseline(),
        &digest_store(&store),
        ExpectedDigestDelta::None,
        "read",
    );

    eprintln!("  froe search-nodes");
    let search = froe(&[
        "search-nodes",
        store.to_str().unwrap(),
        "--has-property",
        "jcr:primaryType",
        "--value",
        "jcr:primaryType=sling:OrderedFolder",
        "--limit",
        "5",
    ]);
    assert!(!search.trim().is_empty(), "search found at least one node");

    eprintln!("  froe export (json-lines)");
    let export_path = work_root().join("oak-export.jsonl");
    // `froe export` never overwrites, by design. Without clearing the path
    // the phase passes once and then fails on every later run against the
    // same `FROE_INTEROP_WORK_ROOT` — which is precisely the mode used when
    // pointing the suite at a large fixture that is expensive to rebuild.
    let _ = std::fs::remove_file(&export_path);
    froe(&[
        "export",
        store.to_str().unwrap(),
        "--path",
        "/content/interop",
        "--output",
        export_path.to_str().unwrap(),
        "--quiet",
    ]);
    let export = std::fs::read_to_string(&export_path).expect("read export");
    assert!(!export.is_empty(), "export produced output");

    eprintln!("  read phase passed");
}

/// Asserts every index shape `generate` added is in the extracted store.
///
/// Through `froe node` and the content API rather than through anything this
/// plan built: the assertion has to be able to fail when a *reader* is wrong,
/// and a check written against the reader under test cannot do that. It runs
/// before the digest baseline is recorded, so a fixture that silently lost a
/// shape fails here rather than three phases later.
pub(crate) fn assert_index_fixture_built(store: &Path) {
    let store_path = store.to_str().expect("the store path is UTF-8");

    // The reference index, both hidden children, each with at least one
    // entry. An empty reference index proves nothing about a reader of one.
    for child in [":references", ":weakreferences"] {
        let path = format!("/oak:index/reference/{child}");
        let tree = froe(&["tree", store_path, &path, "--depth", "2"]);
        let entries = tree.lines().count();
        assert!(
            entries > 1,
            "{path} holds no entry, so the reference index in the fixture is empty:\n{tree}"
        );
    }

    // The group's multi-valued `rep:members` under a `rep:MemberReferences`
    // node, which is what `repMembers` indexes.
    let members = froe(&[
        "tree",
        store_path,
        "/oak:index/repMembers/:index",
        "--depth",
        "3",
    ]);
    assert!(
        members.lines().count() > 1,
        "the repMembers index holds no entry, so the fixture has no group \
         membership to rebuild:\n{members}"
    );

    // The property index Oak rebuilt from existing content. It cannot still
    // be flagged: collecting the editors clears the flag in the same commit.
    let definition = froe(&["node", store_path, "/oak:index/interopTitle"]);
    assert!(
        definition.contains("reindex <Boolean> = false"),
        "the definition is still flagged for reindexing, which Oak clears in \
         the commit that registers the editor:\n{definition}"
    );
    assert!(
        definition.contains("reindexCount <Long> = 1"),
        "the definition was not reindexed exactly once:\n{definition}"
    );
    let storage = froe(&[
        "tree",
        store_path,
        "/oak:index/interopTitle/:index",
        "--depth",
        "2",
    ]);
    assert!(
        storage.lines().count() > 1,
        "the rebuilt index has an empty :index, so Oak found no content to \
         index and the oracle would compare two empty trees:\n{storage}"
    );
    eprintln!("  index fixture: references, group members and a rebuilt property index");
}
