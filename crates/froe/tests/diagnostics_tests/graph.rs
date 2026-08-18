//! The segment graph a run reports: trusted and totalized when the stored
//! one is valid, reconstructed from reference tables when it is not.

use super::*;

#[test]
pub(crate) fn corrupt_graph_is_reconstructed_and_does_not_hide_content_attribution() {
    let fixture = write_diagnostic_fixture("corrupt-debug-graph", GraphFixture::Corrupt);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");

    assert_eq!(report.state, ArchiveDebugState::Active);
    let graph = report.graph.as_ref().expect("reconstructed graph");
    assert_eq!(graph.origin, ArchiveGraphOrigin::Reconstructed);
    assert_eq!(graph.rows.len(), 1);
    assert_eq!(graph.rows[0].segment_identifier, fixture.data_identifier);
    assert_eq!(
        graph.rows[0].references,
        ArchiveGraphReferences::Available(fixture.bulk_identifiers.to_vec())
    );
    assert!(
        report
            .references
            .iter()
            .any(|reference| matches!(reference, ArchivePathReference::Node { .. })),
        "path scan remains useful even when the optional graph is corrupt"
    );
}

#[test]
pub(crate) fn reconstructed_graph_validates_headers_before_reserving_raw_edge_counts() {
    let fixture = write_diagnostic_fixture(
        "corrupt-debug-graph-header",
        GraphFixture::CorruptWithInvalidHeader,
    );
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let mut options = ArchiveDebugOptions::default();
    options.maximum_graph_edges = fixture.bulk_identifiers.len();
    let report = debug_archive_with_options(&repository, DATA_ARCHIVE, options)
        .expect("invalid header is an unavailable row, not a graph-budget refusal");

    let graph = report.graph.expect("reconstructed graph");
    assert_eq!(graph.origin, ArchiveGraphOrigin::Reconstructed);
    assert_eq!(graph.rows.len(), 2);
    assert_eq!(
        graph.rows[0].references,
        ArchiveGraphReferences::Available(fixture.bulk_identifiers.to_vec())
    );
    let invalid_identifier = fixture
        .invalid_graph_identifier
        .expect("fixture includes an invalid data segment");
    let invalid_row = graph
        .rows
        .iter()
        .find(|row| row.segment_identifier == invalid_identifier)
        .expect("invalid row remains totalized");
    assert!(matches!(
        &invalid_row.references,
        ArchiveGraphReferences::Unavailable { details }
            if details.contains("declares -1 segment references")
    ));
}

#[test]
pub(crate) fn crc_valid_nonempty_stored_graph_uses_oak_set_order_and_last_source_row() {
    let fixture = write_diagnostic_fixture("stored-debug-graph", GraphFixture::ValidNonempty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");

    let graph = report.graph.as_ref().expect("stored graph");
    assert_eq!(graph.origin, ArchiveGraphOrigin::Stored);
    assert_eq!(graph.rows.len(), 2, "one total row per archive segment");
    assert_eq!(graph.rows[0].segment_identifier, fixture.data_identifier);
    assert_eq!(
        graph.rows[0].references,
        ArchiveGraphReferences::Available(vec![
            fixture.bulk_identifiers[1],
            fixture.bulk_identifiers[3],
        ]),
        "the duplicate source's last row wins, targets deduplicate and sort"
    );
    assert_eq!(
        graph.rows[1].segment_identifier,
        fixture
            .graph_empty_identifier
            .expect("fixture has an unmentioned archive segment")
    );
    assert_eq!(
        graph.rows[1].references,
        ArchiveGraphReferences::Available(Vec::new()),
        "an archive segment absent from the stored graph receives an empty row"
    );

    let complete_work = report.work.consumed_work_units;
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = complete_work - 1;
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units,
            attempted_work_units,
        }) if maximum_work_units == complete_work - 1
            && attempted_work_units == complete_work
    ));
}

#[test]
pub(crate) fn stored_graph_rows_and_edges_have_independent_typed_caps() {
    let fixture = write_diagnostic_fixture("stored-debug-graph-caps", GraphFixture::ValidNonempty);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");

    let mut row_options = ArchiveDebugOptions::default();
    row_options.maximum_graph_rows = 1;
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, row_options),
        Err(ArchiveDebugError::GraphBudgetExceeded {
            maximum_graph_rows: 1,
            attempted_graph_rows: 2,
            ..
        })
    ));

    let mut edge_options = ArchiveDebugOptions::default();
    edge_options.maximum_graph_edges = 2;
    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, edge_options),
        Err(ArchiveDebugError::GraphBudgetExceeded {
            maximum_graph_edges: 2,
            attempted_graph_edges: 3,
            ..
        })
    ));
}

#[test]
pub(crate) fn missing_graph_is_reconstructed_from_recovered_archive_bytes() {
    let fixture = write_diagnostic_fixture("missing-debug-graph", GraphFixture::Missing);
    let before = directory_snapshot(&fixture.directory.path);
    let repository = Repository::open(&fixture.directory.path).expect("open recovered repository");
    let report = debug_archive(&repository, DATA_ARCHIVE).expect("debug report");

    let graph = report.graph.as_ref().expect("reconstructed graph");
    assert_eq!(graph.origin, ArchiveGraphOrigin::Reconstructed);
    assert_eq!(graph.rows.len(), 1);
    assert_eq!(
        graph.rows[0].references,
        ArchiveGraphReferences::Available(fixture.bulk_identifiers.to_vec())
    );
    drop(repository);
    assert_eq!(directory_snapshot(&fixture.directory.path), before);
    assert!(!fixture.directory.path.join("repo.lock").exists());
}

#[test]
pub(crate) fn reconstructed_graph_charges_dense_segment_bytes_before_parsing() {
    let fixture = write_diagnostic_fixture("missing-debug-graph-work", GraphFixture::Missing);
    let repository = Repository::open(&fixture.directory.path).expect("open repository");
    let data_bytes = repository
        .archives()
        .iter()
        .find(|archive| archive.file_name() == DATA_ARCHIVE)
        .and_then(|archive| archive.segment_data(fixture.data_identifier))
        .expect("independent data segment");
    assert_eq!(data_bytes.len(), 2_096);
    // The independently encoded tree consumes 1,099 units before graph
    // reconstruction; graph selection and its row cost two more. Reserving
    // the complete 2,096-byte segment therefore attempts unit 3,197 before
    // parsing. Removing the byte charge makes this absolute threshold pass.
    let mut options = ArchiveDebugOptions::default();
    options.maximum_work_units = 3_196;

    assert!(matches!(
        debug_archive_with_options(&repository, DATA_ARCHIVE, options),
        Err(ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: 3_196,
            attempted_work_units: 3_197,
        })
    ));
}
