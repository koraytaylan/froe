//! Mutation-matrix tests for the metadata-aware repository snapshot helper.

#[path = "support/filesystem_snapshot.rs"]
mod filesystem_snapshot;

#[test]
fn snapshot_equality_detects_content_preserving_filesystem_mutations() {
    filesystem_snapshot::assert_snapshot_mutation_matrix("froe-filesystem-snapshot");
}
