//! Detection held to the independent oracle, and the classification
//! regressions adversarial review proved out: a shared record must
//! not orphan a live history, and the external census covers version
//! storage.

use crate::support::*;
use froe::PropertyType;
use froe::writer::{
    ChildNodesToWrite, CompactionKind, CompactionOptions, PropertyToWrite, PropertyValuesToWrite,
    WritableRepository, plan_compaction,
};

/// The planner's orphan count must equal the oracle's, on the same store.
#[test]
fn detection_agrees_with_the_independent_oracle() {
    let directory = TestDirectory::new("oracle");
    write_store(
        &directory.path,
        &Fixture {
            reference_into_orphan: false,
            checkpoint: false,
        },
    );

    let orphans = oracle_orphans(&directory.path);
    assert_eq!(
        orphans,
        vec![
            ORPHAN_VERSIONABLE.to_owned(),
            CONFIGURATION_VERSIONABLE.to_owned()
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>(),
        "the oracle itself must find exactly the two orphans"
    );

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the store");
    let report = plan.orphaned_version_histories();
    assert_eq!(report.orphaned_histories, orphans.len() as u64);
    assert_eq!(
        report.orphaned_nodes, 6,
        "two orphan histories of three nodes each"
    );
    assert_eq!(report.malformed_identifiers, 0);
}

/// A record reachable both under live content and inside a frozen node
/// must keep its history live. This is the regression the walk order
/// exists to prevent: when version storage was certified before the
/// content tree, the shared record was memo-skipped out of the live walk,
/// its `jcr:uuid` never matched, and the live history read as an orphan —
/// which a purge would then have removed.
#[test]
fn a_record_shared_into_a_frozen_subtree_stays_live() {
    let directory = TestDirectory::new("shared-frozen-record");
    let store = WritableRepository::open(&directory.path).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    // The live page first, so the same record can be attached in both
    // places: under the content tree and inside the live history's frozen
    // node.
    let page_properties = vec![string_property(
        &mut writer,
        "jcr:uuid",
        PropertyType::String,
        LIVE_VERSIONABLE,
    )];
    let live_page = writer
        .write_node(
            Some("nt:unstructured"),
            &["mix:versionable".to_owned()],
            &ChildNodesToWrite::Zero,
            &page_properties,
        )
        .expect("write the live page");

    let live_history = write_history(
        &mut writer,
        &HistoryToWrite {
            versionable: LIVE_VERSIONABLE,
            history_identifier: "aaaaaaaa-1111-4111-8111-999999999999",
            version_identifier: "aaaaaaaa-1111-4111-8111-888888888888",
            frozen_primary_type: "nt:unstructured",
            frozen_child: Some(("sharedPage", live_page)),
        },
    );
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
    let (live_name, live_wrapped) = wrap_in_intermediates(
        &mut writer,
        "aa",
        "ab",
        "ac",
        LIVE_VERSIONABLE,
        live_history,
    );
    let (orphan_name, orphan_wrapped) = wrap_in_intermediates(
        &mut writer,
        "ba",
        "bb",
        "bc",
        ORPHAN_VERSIONABLE,
        orphan_history,
    );
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::One {
                name: "page".to_owned(),
                node: live_page,
            },
            &[],
        )
        .expect("write the content");
    publish_version_storage(
        &store,
        writer,
        vec![(live_name, live_wrapped), (orphan_name, orphan_wrapped)],
        vec![("content".to_owned(), content)],
    );
    store.close().expect("close the store");

    assert_eq!(
        oracle_orphans(&directory.path),
        vec![ORPHAN_VERSIONABLE.to_owned()],
        "the oracle sees exactly the deliberate orphan"
    );
    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the store");
    let report = plan.orphaned_version_histories();
    assert_eq!(
        report.orphaned_histories, 1,
        "the live history shares a record into its frozen node and must not read as orphaned"
    );
}

/// External binaries referenced only inside version storage are part of
/// the head's footprint. The census must count them even though the
/// version-storage walk is the only walk that certifies those records —
/// a blob held solely by old versions is exactly the blob-store bloat the
/// footprint exists to explain.
#[test]
fn the_external_footprint_covers_version_storage() {
    let directory = TestDirectory::new("external-in-history");
    let store = WritableRepository::open(&directory.path).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    let history_only_blob = writer
        .write_external_binary_identifier(
            "cafe84c92565b98a45f1bb0a9fef2eff804239ba1b96e4ae4e29e0e4222829ab#2048",
        )
        .expect("write the history-only reference");
    let file = writer
        .write_node(
            Some("nt:file"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(history_only_blob),
            }],
        )
        .expect("write the frozen file");
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
    let live_blob = writer
        .write_external_binary_identifier(
            "00b6d84c92565b98a45f1bb0a9fef2eff804239ba1b96e4ae4e29e0e4222829a#1000",
        )
        .expect("write the live reference");
    let live_file = writer
        .write_node(
            Some("nt:file"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(live_blob),
            }],
        )
        .expect("write the live file");
    publish_version_storage(
        &store,
        writer,
        vec![(orphan_name, orphan_wrapped)],
        vec![("attachment".to_owned(), live_file)],
    );
    store.close().expect("close the store");

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::new().with_compaction(CompactionKind::Full),
    )
    .expect("plan the store");
    let footprint = plan.external_binary_footprint();
    assert_eq!(
        footprint.distinct_references, 2,
        "one live blob and one referenced only inside the history"
    );
    assert_eq!(footprint.measured_bytes, 3048);
    assert_eq!(footprint.unmeasured_references, 0);
    assert_eq!(
        plan.orphaned_version_histories().external_references,
        1,
        "the orphan report still attributes the history's own reference"
    );
}
