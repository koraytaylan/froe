//! Mutation-matrix tests for the metadata-aware repository snapshot helper.

mod support;

#[test]
fn snapshot_equality_detects_content_preserving_filesystem_mutations() {
    support::filesystem_snapshot::assert_snapshot_mutation_matrix("froe-cli-filesystem-snapshot");
}
