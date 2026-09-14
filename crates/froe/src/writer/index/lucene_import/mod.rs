//! `froe index import`: a Lucene index built out of band, into a stopped
//! store.
//!
//! The operation follows plan 0007's open protocol exactly — the shape
//! check and the two pre-lock apply-identity gates before and again after
//! the lock, the path-identity check, a replan, the fingerprint, the
//! certified archive number and the metadata-source gate — so what is new
//! here is not the publication discipline but *what may be published*.
//!
//! **The state rule replaces oak-run's bring-up-to-date.** oak-run runs
//! four numbered steps on a live store: switch the definition's lane to
//! `temp-<lane>`, import the data, bring the index up to date from the
//! indexed checkpoint to the lane's current one with Lucene's own writer,
//! and release the checkpoint. The third step needs an index writer froe
//! does not have until plan 0009, and the lane switch and the revert inside
//! it exist only because another indexer is running.
//!
//! Offline, with the head frozen under `repo.lock`, the equivalent
//! guarantee is a **precondition** rather than a repair: the directory's
//! `indexer-info.properties` names the checkpoint it reflects, and froe
//! requires that checkpoint's root record to be, by identity, the root of
//! the selected definition's lane checkpoint. When they match, no catch-up
//! is owed and the lane's next cycle continues correctly from its own
//! checkpoint. When they do not, the import is refused with **both roots
//! named**.
//!
//! Three recorded departures from the importer froe replaces:
//!
//! * **No checkpoint is released.** The only checkpoint the rule accepts is
//!   the lane's, which the lane owns.
//! * **`:suggest-data` is never imported.** Oak's own Lucene writer rebuilds
//!   the suggestions whenever their `lastUpdated` is missing.
//! * **`refresh` is not copied from the file.** froe refreshes the stored
//!   definition by cloning it, which is what the refresh means.

pub mod apply;
pub mod drift;
pub(crate) mod materialize;
pub mod plan;
pub mod prepared;

pub use apply::{ImportedIndex, LuceneImportOutcome};
pub use drift::{DriftVerdict, REWRITTEN_PROPERTY_NAMES};
pub use plan::{LuceneImportOptions, LuceneImportPlan, PlannedImport, plan_lucene_import};
pub use prepared::{PreparedLuceneImport, lucene_import, lucene_import_with_progress};
