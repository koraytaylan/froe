//! What a compaction plan reports about the store beyond its actions: the
//! copy's predicted cost, and the external binaries the content references.
//!
//! The external-binary figures are asserted against hand-computed
//! expectations, never against the census's own arithmetic, in the spirit of
//! the workspace's independent-implementation rule: the fixture states which
//! identifiers it wrote and what they imply, and the plan must arrive at
//! exactly that.

use froe::writer::{
    ChildNodesToWrite, CompactionKind, CompactionOptions, PropertyToWrite, PropertyValuesToWrite,
    WritableRepository, plan_compaction,
};
use froe::{PropertyType, RecordIdentifier};

/// A scratch repository directory, removed when the test drops it.
struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-plan-reporting-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test repository directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Builds a store whose content references external binaries: two nodes
/// sharing one blob, a second distinct blob, and one identifier without a
/// length suffix.
fn write_store_with_external_binaries(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    let shared = "00b6d84c92565b98a45f1bb0a9fef2eff804239ba1b96e4ae4e29e0e4222829a#1000";
    let distinct = "cafe84c92565b98a45f1bb0a9fef2eff804239ba1b96e4ae4e29e0e4222829ab#2048";
    let unmeasured = "custom-blob-store-identifier-without-length";

    let mut children = Vec::new();
    for (name, identifier) in [
        ("first", shared),
        ("second", shared),
        ("third", distinct),
        ("fourth", unmeasured),
    ] {
        let reference = writer
            .write_external_binary_identifier(identifier)
            .expect("write the external reference");
        let node = writer
            .write_node(
                Some("nt:file"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "jcr:data".to_owned(),
                    property_type: PropertyType::Binary,
                    values: PropertyValuesToWrite::Single(reference),
                }],
            )
            .expect("write the referencing node");
        children.push((name.to_owned(), node));
    }
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Many(children),
            &[],
        )
        .expect("write the content node");
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
        .expect("write the root");
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
        .expect("write the super root");
    writer.finish().expect("finish the writer");
    publish(&store, head);
    store.close().expect("close the store");
}

fn publish(store: &WritableRepository, head: RecordIdentifier) {
    let previous = store.head();
    assert!(store.compare_and_set_head(previous, head));
    store.flush().expect("flush the store");
}

/// The blob identifiers above imply exactly these figures: three distinct
/// blobs (the shared one counted once), 1000 + 2048 measured bytes, and one
/// identifier whose length cannot be known.
#[test]
fn the_plan_counts_external_binaries_by_distinct_identifier() {
    let directory = TestDirectory::new("external-binaries");
    write_store_with_external_binaries(&directory.path);

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the store");

    let footprint = plan.external_binary_footprint();
    assert_eq!(footprint.distinct_references, 3);
    assert_eq!(footprint.measured_bytes, 3048);
    assert_eq!(footprint.unmeasured_references, 1);
}

/// A copying plan predicts what the copy writes — the head closure's data
/// bytes — and a plan without a copy predicts nothing, because there is no
/// generation to pay for.
#[test]
fn the_copy_cost_is_predicted_exactly_when_a_copy_is_planned() {
    let directory = TestDirectory::new("copy-cost");
    write_store_with_external_binaries(&directory.path);

    let copying = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the copying run");
    let predicted = copying
        .predicted_copy_output_bytes()
        .expect("a copying plan predicts its output");
    assert!(
        predicted > 0,
        "the head closure holds live data, so the copy cannot be free"
    );

    let standalone = plan_compaction(&directory.path, &CompactionOptions::new())
        .expect("plan the standalone run");
    assert_eq!(standalone.predicted_copy_output_bytes(), None);
}

/// The convergence gate may only ever drop the swap itself. On a store
/// that is already fully compacted, a full-compaction plan must equal the
/// no-compaction plan in every action — the gate routes the run to the
/// standalone sweep rather than hiding garbage — and on a store that is
/// not, the copy must survive the gate untouched.
#[test]
fn the_convergence_gate_only_ever_drops_the_swap() {
    let directory = TestDirectory::new("convergence-gate");
    write_store_with_external_binaries(&directory.path);

    let fresh_with_copy = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the fresh store");
    assert!(
        !fresh_with_copy.already_fully_compacted(),
        "a store another writer produced is not already compacted"
    );
    assert_eq!(
        fresh_with_copy.effective_compaction_kind(),
        Some(CompactionKind::Full),
        "the gate must not touch a copy that has work to do"
    );

    froe::writer::compact(
        &directory.path,
        CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("compact the store once");

    let gated = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the compacted store with a copy selected");
    assert!(gated.already_fully_compacted());
    assert_eq!(gated.effective_compaction_kind(), None);
    assert_eq!(gated.predicted_copy_output_bytes(), None);

    let standalone = plan_compaction(&directory.path, &CompactionOptions::new())
        .expect("plan the compacted store without a copy");
    assert_eq!(
        gated.actions(),
        standalone.actions(),
        "a gated plan is exactly the standalone plan: the gate drops the swap and nothing else"
    );
    assert!(
        gated.is_empty(),
        "a fully compacted store leaves the standalone sweep nothing to do"
    );

    let forced = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_copy_when_already_compacted(),
    )
    .expect("plan the forced copy");
    assert!(forced.already_fully_compacted());
    assert_eq!(
        forced.effective_compaction_kind(),
        Some(CompactionKind::Full)
    );
}

/// Convergence-gate condition: the journal must be a single line naming
/// the head. A store whose segments are fully compacted but whose journal
/// still carries history is not converged — the copy runs, the history is
/// retired, and only then does the gate close.
#[test]
fn the_gate_requires_a_converged_journal() {
    let directory = TestDirectory::new("gate-journal");
    write_store_with_external_binaries(&directory.path);
    froe::writer::compact(
        &directory.path,
        CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("compact the store once");

    // Duplicate the journal's only line: the head is unchanged, but the
    // journal now carries a revision to retire.
    let journal_path = directory.path.join("journal.log");
    let journal = std::fs::read(&journal_path).expect("read the journal");
    let mut doubled = journal.clone();
    doubled.extend_from_slice(&journal);
    std::fs::write(&journal_path, doubled).expect("append the duplicate line");

    let unconverged = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the unconverged store");
    assert!(
        !unconverged.already_fully_compacted(),
        "journal history keeps the gate open"
    );
    assert_eq!(
        unconverged.effective_compaction_kind(),
        Some(CompactionKind::Full)
    );

    froe::writer::compact(
        &directory.path,
        CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("compact the history away");
    let converged = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the converged store");
    assert!(converged.already_fully_compacted());
    assert!(converged.is_empty());
}
