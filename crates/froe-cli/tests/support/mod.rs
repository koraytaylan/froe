//! Shared support for `froe` command-line integration tests.

// The same snapshot utility proves read-only behaviour in both crates, and
// two copies of it could drift apart silently — weakening whichever crate
// stopped being updated. It is included from `froe`'s test tree rather than
// copied, and depends only on `std`, so nothing in `froe` leaks in with it.
#[path = "../../../froe/tests/support/filesystem_snapshot.rs"]
pub(crate) mod filesystem_snapshot;
