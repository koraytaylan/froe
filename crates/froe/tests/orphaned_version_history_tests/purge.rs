//! The purge end to end: exact removals, every exclusion rail, the
//! checkpoint scoping, the honest estimates, and the bulk a purge
//! actually frees.

use crate::support::*;
use froe::writer::{
    ChildNodesToWrite, CompactionKind, CompactionOptions, PropertyToWrite, PropertyValuesToWrite,
    WritableRepository, compact, plan_compaction,
};
use froe::{CompactionAction, PropertyType, Repository};

/// The purge removes exactly the plain orphan: the configuration history
/// is kept with a warning, the live history survives, the emptied
/// intermediates vanish, and a repeat run converges to nothing.
#[test]
fn a_purge_removes_the_orphan_and_converges() {
    let directory = TestDirectory::new("purge");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    let plan = plan_compaction(&directory.path, &options).expect("plan the purge");
    let repeat_options = options.clone();
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories {
                histories: 1,
                nodes: 3,
                ..
            }
        )),
        "exactly the plain orphan is selected: {:?}",
        plan.actions()
    );
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("nt:configuration")),
        "the configuration exclusion is warned about: {:?}",
        plan.warnings()
    );

    compact(&directory.path, options).expect("apply the purge");

    let repository = Repository::open(&directory.path).expect("reopen after the purge");
    assert!(
        repository
            .node_at_path(&format!(
                "/jcr:system/jcr:versionStorage/ba/bb/bc/{ORPHAN_VERSIONABLE}"
            ))
            .expect("resolve the purged history")
            .is_none(),
        "the purged history must be gone from the head"
    );
    assert!(
        repository
            .node_at_path("/jcr:system/jcr:versionStorage/ba")
            .expect("resolve the emptied intermediate")
            .is_none(),
        "an intermediate whose only history was purged must be gone"
    );
    for surviving in [
        format!("/jcr:system/jcr:versionStorage/aa/ab/ac/{LIVE_VERSIONABLE}"),
        format!("/jcr:system/jcr:versionStorage/ca/cb/cc/{CONFIGURATION_VERSIONABLE}"),
        "/content/page".to_owned(),
    ] {
        assert!(
            repository
                .node_at_path(&surviving)
                .expect("resolve a survivor")
                .is_some(),
            "{surviving} must survive the purge"
        );
    }
    drop(repository);
    assert_eq!(
        oracle_orphans(&directory.path),
        vec![CONFIGURATION_VERSIONABLE.to_owned()]
    );

    let repeat = plan_compaction(&directory.path, &repeat_options).expect("plan the repeat");
    assert!(
        repeat.is_empty(),
        "with only the excluded configuration left, the repeat converges: {:?}",
        repeat.actions()
    );
    assert!(repeat.already_fully_compacted());
}

/// A REFERENCE value outside version storage naming a record inside a
/// candidate demotes it: the advisory pass fails safe.
#[test]
fn an_inbound_reference_demotes_the_candidate() {
    let directory = TestDirectory::new("reference-demotion");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: true,
            checkpoint: false,
        },
    );

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    let plan = plan_compaction(&directory.path, &options).expect("plan the demoted purge");
    assert!(
        !plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories { .. }
        )),
        "a referenced history must not be selected: {:?}",
        plan.actions()
    );
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("REFERENCE")),
        "the demotion is warned about: {:?}",
        plan.warnings()
    );
}

/// The age bound keeps a history whose newest version is younger than the
/// bound — proven with a bound far larger than the fixture's age, so the
/// test needs no clock arithmetic of its own.
#[test]
fn the_age_bound_keeps_young_histories() {
    let directory = TestDirectory::new("age-bound");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let two_centuries = std::time::Duration::from_secs(200 * 365 * 24 * 60 * 60);
    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge()
            .with_purged_history_minimum_age(two_centuries),
    )
    .expect("plan with the age bound");
    assert!(
        !plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories { .. }
        )),
        "nothing is two centuries old: {:?}",
        plan.actions()
    );
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("age bound")),
        "the age exclusion is warned about: {:?}",
        plan.warnings()
    );
}

/// A retained checkpoint's snapshot keeps its own version storage: the
/// purge removes the history from the head, and the checkpoint still
/// resolves it.
#[test]
fn a_checkpoint_snapshot_keeps_what_the_head_purges() {
    let directory = TestDirectory::new("checkpoint-scope");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: true,
        },
    );

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    compact(&directory.path, options).expect("apply the purge");

    let repository = Repository::open(&directory.path).expect("reopen after the purge");
    assert!(
        repository
            .node_at_path(&format!(
                "/jcr:system/jcr:versionStorage/ba/bb/bc/{ORPHAN_VERSIONABLE}"
            ))
            .expect("resolve the purged history")
            .is_none(),
        "the head loses the history"
    );
    let checkpoints = repository.checkpoints().expect("list checkpoints");
    assert_eq!(checkpoints.len(), 1, "the checkpoint survives the run");
    let (_, checkpoint) = &checkpoints[0];
    let mut snapshot = checkpoint
        .child_node("root")
        .expect("read the snapshot root")
        .expect("the snapshot has a root");
    for name in [
        "jcr:system",
        "jcr:versionStorage",
        "ba",
        "bb",
        "bc",
        ORPHAN_VERSIONABLE,
    ] {
        snapshot = snapshot
            .child_node(name)
            .expect("descend the snapshot")
            .unwrap_or_else(|| panic!("the checkpoint's snapshot must keep resolving {name}"));
    }
}

/// A history whose `jcr:versionableUuid` does not parse shares its
/// intermediate chain with a selected orphan. The purge must remove the
/// orphan and keep the chain, because pruning it would delete the
/// unclassifiable history with it.
#[test]
fn a_malformed_sibling_under_a_shared_intermediate_survives_the_purge() {
    let directory = TestDirectory::new("malformed-sibling");
    let store = WritableRepository::open(&directory.path).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    let orphan_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: ORPHAN_VERSIONABLE,
            history_identifier: "bbbbbbbb-2222-4222-8222-999999999999",
            version_identifier: ORPHAN_VERSION,
            frozen_primary_type: "nt:unstructured",
            frozen_child: None,
        },
    );
    let malformed_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: "not-an-identifier",
            history_identifier: "dddddddd-4444-4444-8444-999999999999",
            version_identifier: "dddddddd-4444-4444-8444-aaaaaaaaaaaa",
            frozen_primary_type: "nt:unstructured",
            frozen_child: None,
        },
    );
    // Both histories under the same deepest intermediate, so the chain is
    // only removable if *everything* under it goes.
    let deepest = writer
        .write_node(
            Some("rep:versionStorage"),
            &[],
            &ChildNodesToWrite::Many(vec![
                (ORPHAN_VERSIONABLE.to_owned(), orphan_history),
                ("malformed".to_owned(), malformed_history),
            ]),
            &[],
        )
        .expect("write the deepest intermediate");
    let mut wrapped = deepest;
    let mut wrapped_name = "bc".to_owned();
    for level in ["bb", "ba"] {
        wrapped = writer
            .write_node(
                Some("rep:versionStorage"),
                &[],
                &ChildNodesToWrite::One {
                    name: wrapped_name.clone(),
                    node: wrapped,
                },
                &[],
            )
            .expect("write the intermediate");
        level.clone_into(&mut wrapped_name);
    }
    publish_version_storage(&store, writer, vec![(wrapped_name, wrapped)], Vec::new());
    store.close().expect("close the store");

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    let plan = plan_compaction(&directory.path, &options).expect("plan the purge");
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories { histories: 1, .. }
        )),
        "exactly the parseable orphan is selected: {:?}",
        plan.actions()
    );
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("do not parse")),
        "the unclassifiable history is warned about: {:?}",
        plan.warnings()
    );

    compact(&directory.path, options).expect("apply the purge");

    let repository = Repository::open(&directory.path).expect("reopen after the purge");
    assert!(
        repository
            .node_at_path(&format!(
                "/jcr:system/jcr:versionStorage/ba/bb/bc/{ORPHAN_VERSIONABLE}"
            ))
            .expect("resolve the purged history")
            .is_none(),
        "the selected orphan must be gone"
    );
    let survivor = repository
        .node_at_path("/jcr:system/jcr:versionStorage/ba/bb/bc/malformed")
        .expect("resolve the malformed history")
        .expect("the malformed history must survive: its intermediate chain was shared");
    let kept = survivor
        .properties()
        .expect("read the survivor's properties")
        .iter()
        .any(|property| property.name == "jcr:versionableUuid");
    assert!(
        kept,
        "the survivor keeps its unparseable identifier property"
    );
}

/// The copy estimate subtracts what the purge omits: planning the same
/// store with and without the purge must differ by exactly the report's
/// node-record estimate.
#[test]
fn a_purged_copy_is_predicted_cheaper_by_the_node_record_estimate() {
    let directory = TestDirectory::new("purged-copy-estimate");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let without_purge = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan without the purge");
    let with_purge = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge(),
    )
    .expect("plan with the purge");

    let estimate = with_purge
        .orphaned_version_histories()
        .node_record_bytes_estimate;
    assert!(estimate > 0, "the orphans hold node records");
    assert_eq!(
        without_purge
            .predicted_copy_output_bytes()
            .expect("the plain plan predicts a copy"),
        with_purge
            .predicted_copy_output_bytes()
            .expect("the purging plan predicts a copy")
            + estimate,
        "the purge makes the predicted copy cheaper by exactly its estimate"
    );
}

/// Convergence-gate condition: a selected purge forces the copy. The same
/// store that gates an ordinary full run must not gate one that omits
/// content, or the purge would silently never happen.
#[test]
fn a_selected_purge_forces_the_copy_through_the_gate() {
    let directory = TestDirectory::new("purge-through-gate");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    compact(
        &directory.path,
        CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("compact without the purge");

    let gated = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the plain repeat");
    assert!(
        gated.already_fully_compacted(),
        "without a purge the compacted store gates the copy"
    );
    assert_eq!(gated.effective_compaction_kind(), None);

    let purging = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge(),
    )
    .expect("plan the purging repeat");
    assert!(
        !purging.already_fully_compacted(),
        "a selected purge means the run has content work"
    );
    assert_eq!(
        purging.effective_compaction_kind(),
        Some(CompactionKind::Full)
    );
    assert!(
        purging.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories { .. }
        )),
        "the purge is in the plan: {:?}",
        purging.actions()
    );
}

/// A purge whose selection is empty must change nothing about the plan:
/// action for action, it is the plan the same options produce without the
/// purge flag.
#[test]
fn an_empty_selection_leaves_the_plan_identical() {
    let directory = TestDirectory::new("empty-selection");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let two_centuries = std::time::Duration::from_secs(200 * 365 * 24 * 60 * 60);
    let without_purge = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan without the purge");
    let empty_purge = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge()
            .with_purged_history_minimum_age(two_centuries),
    )
    .expect("plan the age-emptied purge");

    assert_eq!(
        without_purge.actions(),
        empty_purge.actions(),
        "an empty selection is the plain plan, byte for byte"
    );
    assert_eq!(
        without_purge.predicted_copy_output_bytes(),
        empty_purge.predicted_copy_output_bytes(),
        "nothing is omitted, so nothing is subtracted"
    );
}

/// With a checkpoint retained, both the purge action and the report say
/// so: the operator reading either sees that released bulk may stay
/// pinned until the checkpoint expires.
#[test]
fn a_retained_checkpoint_is_reported_on_the_purge_and_the_report() {
    let directory = TestDirectory::new("checkpoint-caveat");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: true,
        },
    );

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new()
            .with_compaction(CompactionKind::Full)
            .with_orphaned_version_history_purge(),
    )
    .expect("plan the scoped purge");
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::PurgeOrphanedVersionHistories {
                retained_checkpoints: 1,
                ..
            }
        )),
        "the action names the retained checkpoint: {:?}",
        plan.actions()
    );
    assert_eq!(
        plan.orphaned_version_histories().retained_checkpoints,
        1,
        "the report carries the same caveat"
    );
}

/// Bulk the purge releases is real: an orphan holding a large inline
/// binary reports released bulk, and the store shrinks by at least that
/// much once the purge lands.
#[test]
fn released_bulk_is_reported_and_actually_freed() {
    let directory = TestDirectory::new("released-bulk");
    let store = WritableRepository::open(&directory.path).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    // Large enough that the binary cannot live inline in a data segment
    // and must spill into bulk blocks.
    let large_binary = writer
        .write_binary_content(&vec![0xAB_u8; 600 * 1024])
        .expect("write the large binary");
    let file = writer
        .write_node(
            Some("nt:file"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(large_binary),
            }],
        )
        .expect("write the file node");
    let orphan_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: ORPHAN_VERSIONABLE,
            history_identifier: "bbbbbbbb-2222-4222-8222-999999999999",
            version_identifier: ORPHAN_VERSION,
            frozen_primary_type: "nt:unstructured",
            frozen_child: Some(("file", file)),
        },
    );
    let (orphan_name, orphan_wrapped) = wrap_in_intermediates(
        &mut writer,
        "ba",
        "bb",
        "bc",
        ORPHAN_VERSIONABLE,
        orphan_history,
    );
    publish_version_storage(
        &store,
        writer,
        vec![(orphan_name, orphan_wrapped)],
        Vec::new(),
    );
    store.close().expect("close the store");

    let directory_bytes = |path: &std::path::Path| -> u64 {
        std::fs::read_dir(path)
            .expect("list the store")
            .map(|entry| {
                entry
                    .expect("read an entry")
                    .metadata()
                    .expect("stat")
                    .len()
            })
            .sum()
    };
    let before = directory_bytes(&directory.path);

    let options = CompactionOptions::new()
        .with_compaction(CompactionKind::Full)
        .with_orphaned_version_history_purge();
    let plan = plan_compaction(&directory.path, &options).expect("plan the purge");
    let report = plan.orphaned_version_histories();
    assert!(
        report.released_bulk_segments > 0,
        "a 600 KiB binary must occupy bulk segments"
    );
    assert!(report.released_bulk_bytes > 0);
    assert_eq!(
        report.retained_checkpoints, 0,
        "no checkpoints, so nothing can pin the released blocks"
    );

    compact(&directory.path, options).expect("apply the purge");
    let after = directory_bytes(&directory.path);
    // The reported figure is an upper bound in general (a record shared
    // into kept content retains its blocks), but this fixture shares
    // nothing across histories, so here the ceiling is attained and the
    // store must shrink by at least that much.
    assert!(
        before.saturating_sub(after) >= report.released_bulk_bytes,
        "the store must shrink by at least the released bulk: before {before}, after {after}, released {}",
        report.released_bulk_bytes
    );
}
