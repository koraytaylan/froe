//! The `lucene_reindex` phase's reset scenario: froe resets a Lucene
//! definition on a lane whose checkpoint is gone, and Oak's own cycle
//! rebuilds it from scratch.
//!
//! A Lucene definition whose lane cannot be resolved is not rebuilt by froe
//! under `--from-head`: it is **reset**, and the claim is therefore about
//! Oak. Only Oak can make it — after froe's reset, its own cycle logs the
//! lost checkpoint, rebuilds the definition from scratch and leaves a
//! `:data`. The comparison at the end is against that second boot's own
//! lane checkpoint rather than against the phase's first oracle, because
//! the second boot's instance-keyed content makes the first comparison
//! non-deterministic.

use super::*;

/// The reset: froe resets the variant definition on a lane whose checkpoint
/// is gone, Oak rebuilds it from scratch, and froe's rebuild of the state
/// *that* cycle recorded matches it.
///
/// A Lucene definition whose lane cannot be resolved is not rebuilt by
/// froe under `--from-head`: it is **reset**, and the claim is about Oak.
/// Only Oak can make it — after froe's reset, its own cycle logs the lost
/// checkpoint, rebuilds the definition from scratch and leaves a `:data`.
/// The comparison at the end is against that second boot's own lane
/// checkpoint rather than against the first oracle, because the second
/// boot's instance-keyed content makes the first comparison
/// non-deterministic.
pub(crate) fn assert_the_lane_reset_lets_oak_rebuild_from_scratch(
    judge: &Judge,
    work: &Path,
    oak_rebuilt: &Path,
) {
    let definition = format!("/oak:index/{LUCENE_VARIANT_DEFINITION}");
    let store = work.join("reset");
    copy_store(oak_rebuilt, &store);

    let checkpoint = lane_checkpoint(&store, &definition)
        .expect("the variant definition's lane names a checkpoint after Oak's own rebuild");
    eprintln!("  removing the lane checkpoint {checkpoint}");
    froe(&[
        "checkpoint",
        "remove",
        store.to_str().expect("utf-8"),
        &checkpoint,
        "--yes",
    ]);

    let visible_before = visible_definition_line(&store, LUCENE_VARIANT_DEFINITION);
    let count_before = reindex_count(&store, &definition);
    eprintln!("  froe index reindex --yes --from-head --index {definition}");
    let summary = froe(&[
        "index",
        "reindex",
        store.to_str().expect("utf-8"),
        "--yes",
        "--from-head",
        "--index",
        &definition,
        "--binary-text",
        "marker",
        "--work-directory",
        work.to_str().expect("utf-8"),
    ]);
    eprintln!("{summary}");
    assert!(
        summary.contains("reset"),
        "the definition should have been reset rather than rebuilt: {summary}"
    );

    let hidden = hidden_children_of(&store, LUCENE_VARIANT_DEFINITION);
    assert!(
        hidden.is_empty(),
        "the reset left hidden children behind: {hidden:?}"
    );
    // The reset raises `reindex` and changes nothing else. Raising it is
    // the whole mechanism: `IndexUpdate.shouldReindex`'s other trigger
    // needs the definition to be absent from the before state, which a
    // definition the store already holds never is.
    let without_the_flag = |line: &str| -> String {
        line.split('\t')
            .filter(|field| !field.starts_with("reindex="))
            .collect::<Vec<_>>()
            .join("\t")
    };
    let visible_after = visible_definition_line(&store, LUCENE_VARIANT_DEFINITION);
    assert!(
        visible_after.contains("reindex=Boolean:true"),
        "the reset must flag the definition, or Oak rebuilds nothing: {visible_after}"
    );
    assert_eq!(
        without_the_flag(&visible_after),
        without_the_flag(&visible_before),
        "the reset changed a visible property other than the flag, which it must never do"
    );

    let extracted = oak_rebuilds_from_scratch(work, &store, &definition, count_before);
    assert_froe_matches_the_from_scratch_rebuild(judge, work, &extracted, &definition);
}

/// Boots Sling on the reset store, waits for Oak's own from-scratch
/// rebuild, and extracts the result.
fn oak_rebuilds_from_scratch(
    work: &Path,
    store: &Path,
    definition: &str,
    count_before: i64,
) -> PathBuf {
    eprintln!("  booting Sling so Oak's own lane rebuilds the definition from scratch");
    let volume = PodmanVolume::new("froe-lucene-reset-volume");
    let bootstrap =
        PodmanContainer::run_detached("froe-lucene-reset-bootstrap", RESET_PORT, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-lucene-reset", RESET_PORT, &volume.name);
    wait_for_sling(RESET_PORT, "froe-lucene-reset");

    wait_for_the_log_line(
        "froe-lucene-reset",
        "Failed to retrieve previously indexed checkpoint",
    );
    wait_for_the_log_line(
        "froe-lucene-reset",
        &format!("Reindexing will be performed for following indexes: [{definition}]"),
    );
    // The lane clears the flag and advances the count in the commit that
    // finishes the rebuild, so waiting on both is waiting for the rebuild
    // to have landed rather than to have started.
    let after = sling::sling_wait_until_reindexed(RESET_PORT, definition, count_before);
    assert_eq!(
        after,
        count_before + 1,
        "Oak's own from-scratch rebuild advanced reindexCount by more than one, so the lane \
         never settled and the extracted index is whichever cycle happened to be last"
    );
    drop(sling);

    let extracted = work.join("reset-extracted");
    store_from_volume(&volume.name, &extracted);
    let rebuilt = froe(&["node", extracted.to_str().expect("utf-8"), definition]);
    assert!(
        rebuilt.contains("child             :data"),
        "Oak's rebuild left no :data, so froe's reset removed an index nothing \
         restored:\n{rebuilt}"
    );
    extracted
}

/// froe rebuilds the same definition from the lane checkpoint that cycle
/// recorded, and the two enumerate identically.
fn assert_froe_matches_the_from_scratch_rebuild(
    judge: &Judge,
    work: &Path,
    extracted: &Path,
    definition: &str,
) {
    let name = definition
        .rsplit_once('/')
        .expect("a definition path names a definition")
        .1
        .to_owned();
    let froe_store = work.join("reset-froe");
    copy_store(extracted, &froe_store);
    reset_to_what_oak_started_from(&froe_store, extracted, std::slice::from_ref(&name));
    froe(&[
        "index",
        "reindex",
        froe_store.to_str().expect("utf-8"),
        "--yes",
        "--index",
        definition,
        "--binary-text",
        "marker",
        "--work-directory",
        work.to_str().expect("utf-8"),
    ]);

    let oak_dump = work.join("reset-oak-dump");
    let froe_dump = work.join("reset-froe-dump");
    for (store, into) in [(extracted, &oak_dump), (&froe_store, &froe_dump)] {
        froe(&[
            "index",
            "dump",
            store.to_str().expect("utf-8"),
            "--index",
            definition,
            "--output",
            into.to_str().expect("utf-8"),
        ]);
    }
    let _ = assert_every_definition_enumerates_identically(
        judge,
        &Comparison {
            store: extracted,
            oak_dump: &oak_dump,
            froe_dump: &froe_dump,
            work: &work.join("reset-enumerations"),
            definitions: std::slice::from_ref(&definition.to_owned()),
        },
    );
    eprintln!("    the reset scenario ended in Oak's own from-scratch rebuild, matched");
}

/// Waits for one line to appear in a container's log.
fn wait_for_the_log_line(container: &str, line: &str) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        if container_logs(container).contains(line) {
            return;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("Oak never logged {line:?} in {container}");
}

/// The checkpoint a definition's lane names, if any.
fn lane_checkpoint(store: &Path, definition: &str) -> Option<String> {
    let repository = froe::Repository::open(store).expect("open the store");
    let root = repository.content_root().expect("content root");
    let lanes = froe::index::lanes::AsyncLanes::read(&root).expect("read /:async");
    let node = repository.node_at_path(definition).ok()??;
    let model = froe::index::IndexDefinition::read(&node, definition).ok()?;
    let lane = model.lane.as_deref()?;
    lanes.lane(lane).and_then(|lane| lane.checkpoint.clone())
}

/// A definition's `reindexCount`.
fn reindex_count(store: &Path, definition: &str) -> i64 {
    let repository = froe::Repository::open(store).expect("open the store");
    let node = repository
        .node_at_path(definition)
        .expect("resolve the definition")
        .expect("the definition is in the store");
    froe::index::IndexDefinition::read(&node, definition)
        .expect("model the definition")
        .reindex
        .count
}

/// The definition node's own digest line.
fn visible_definition_line(store: &Path, name: &str) -> String {
    let path = format!("/oak:index/{name}");
    froe_tolerating_dangling_lane_checkpoints(&["digest", store.to_str().expect("utf-8")])
        .lines()
        .find(|line| line.starts_with(&format!("{path}\t")) || *line == path)
        .unwrap_or("")
        .to_owned()
}

/// A definition's hidden children, rendered.
fn hidden_children_of(store: &Path, name: &str) -> Vec<String> {
    let prefix = format!("/oak:index/{name}/:");
    froe_tolerating_dangling_lane_checkpoints(&["digest", store.to_str().expect("utf-8")])
        .lines()
        .filter(|line| line.starts_with(&prefix))
        .map(str::to_owned)
        .collect()
}
