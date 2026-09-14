//! The open protocol, and the confirmation window it holds open.
//!
//! Identical to plan 0007's, step for step, because the protocol is the
//! store's rather than the operation's: the shape check and the two pre-lock
//! apply-identity gates, the lock, both again, a replan from disk, the
//! fingerprint, the certified archive number and the metadata-source gate.
//! `apply` rechecks the fingerprint and the path identity and only then
//! opens through `open_prepared`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::progress::{DiscardedProgress, ProgressObserver};
use crate::store::Repository;
use crate::writer::index::lucene_import::plan::{
    LuceneImportOptions, LuceneImportPlan, build_plan,
};
use crate::writer::maintenance::{
    DirectoryFingerprint, canonical_repository_directory, directory_fingerprint,
    validate_apply_environment, validate_apply_identity, validate_metadata_source_apply_identity,
    validate_repository_shape,
};
use crate::writer::repository_lock::RepositoryLock;

/// An import that has taken the lock and is waiting to be confirmed.
pub struct PreparedLuceneImport {
    pub(crate) directory: PathBuf,
    pub(crate) plan: LuceneImportPlan,
    pub(crate) fingerprint: DirectoryFingerprint,
    pub(crate) certified_archive_number: u32,
    pub(crate) repository_lock: Arc<RepositoryLock>,
}

impl PreparedLuceneImport {
    /// Takes the lock and plans under it.
    pub fn prepare(directory: &Path, options: &LuceneImportOptions) -> Result<Self> {
        Self::prepare_with_progress(directory, options, &mut DiscardedProgress)
    }

    /// Prepares exactly as [`PreparedLuceneImport::prepare`] does, reporting
    /// each step.
    pub fn prepare_with_progress(
        directory: &Path,
        options: &LuceneImportOptions,
        observer: &mut dyn ProgressObserver,
    ) -> Result<Self> {
        let _ = observer;
        let directory = canonical_repository_directory(directory)?;
        validate_repository_shape(&directory)?;
        validate_apply_environment(&directory)?;
        validate_apply_identity(&directory)?;

        let repository_lock = Arc::new(RepositoryLock::acquire(&directory)?);
        // The path may have changed between the lockless checks and the
        // lock, so every one of them runs again against the locked state.
        validate_repository_shape(&directory)?;
        validate_apply_environment(&directory)?;
        validate_apply_identity(&directory)?;
        repository_lock.validate_path_identity(&directory)?;

        // Replanned from disk: no record identity from a lockless plan
        // survives this point.
        let repository = Repository::open(&directory)?;
        let plan = build_plan(&directory, &repository, options)?;
        let fingerprint = directory_fingerprint(&directory)?;
        let certified_archive_number =
            crate::writer::store_writer::next_cleanup_archive_number(&directory)?;
        // Last, and before anything is written: `preserve_file_metadata`
        // runs inside `flush`, so a store this gate would refuse fails only
        // after every record is on disk without it.
        validate_metadata_source_apply_identity(&directory)?;

        drop(repository);
        Ok(Self {
            directory,
            plan,
            fingerprint,
            certified_archive_number,
            repository_lock,
        })
    }

    /// What the run will do.
    #[must_use]
    pub fn plan(&self) -> &LuceneImportPlan {
        &self.plan
    }

    /// The facts that must still hold when the operator confirms.
    ///
    /// The fingerprint skips `repo.lock` by design — the run creates it — so
    /// only the identity check catches a lock file replaced during the
    /// window.
    pub(crate) fn recheck_before_mutation(&self) -> Result<()> {
        let current = directory_fingerprint(&self.directory)?;
        if current != self.fingerprint {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} changed between planning and applying; refusing before any index \
                     record is written",
                    self.directory.display()
                ),
            });
        }
        self.repository_lock.validate_path_identity(&self.directory)
    }

    /// Applies the prepared import.
    pub fn apply(self) -> Result<super::apply::LuceneImportOutcome> {
        super::apply::apply_prepared(&self, &mut DiscardedProgress)
    }

    /// Applies exactly as [`PreparedLuceneImport::apply`] does, reporting
    /// each step. Reporting cannot alter the mutation sequence.
    pub fn apply_with_progress(
        self,
        observer: &mut dyn ProgressObserver,
    ) -> Result<super::apply::LuceneImportOutcome> {
        super::apply::apply_prepared(&self, observer)
    }
}

/// Plans, prepares and applies in one call.
pub fn lucene_import(
    directory: &Path,
    options: &LuceneImportOptions,
) -> Result<super::apply::LuceneImportOutcome> {
    lucene_import_with_progress(directory, options, &mut DiscardedProgress)
}

/// The observed twin of [`lucene_import`].
pub fn lucene_import_with_progress(
    directory: &Path,
    options: &LuceneImportOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<super::apply::LuceneImportOutcome> {
    PreparedLuceneImport::prepare_with_progress(directory, options, observer)?
        .apply_with_progress(observer)
}

#[cfg(test)]
mod tests {
    use super::PreparedLuceneImport;
    use crate::writer::index::lucene_import::plan::LuceneImportOptions;
    use crate::writer::maintenance::gate_observation::{recorded, reset};
    use crate::writer::store_writer::WritableRepository;

    /// An import's `prepare` runs the same open protocol compaction's does.
    ///
    /// The third user of task 0715's seam. A `prepare` that dropped a gate
    /// would plan correctly and apply correctly against a healthy store, and
    /// only fail to refuse an unhealthy one — an absence nothing downstream
    /// can observe.
    #[test]
    fn a_prepare_runs_the_whole_open_protocol() {
        let path = std::env::temp_dir().join(format!(
            "froe-lucene-import-gates-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        WritableRepository::open(&path)
            .expect("bootstrap")
            .close()
            .expect("close the bootstrap");

        // An empty input directory: the protocol runs before the plan reads
        // it, so the refusal proves the gates ran even though the plan then
        // fails.
        let input = path.join("input");
        std::fs::create_dir_all(&input).expect("create the input directory");

        reset();
        let _ = PreparedLuceneImport::prepare(&path, &LuceneImportOptions::new(input));

        let calls = recorded();
        let gates: Vec<&str> = calls.iter().map(|(gate, _)| *gate).collect();
        for expected in [
            "validate_repository_shape",
            "validate_apply_environment",
            "validate_apply_identity",
        ] {
            assert!(
                gates.contains(&expected),
                "{expected} was not called during prepare: {gates:?}"
            );
            assert!(
                gates.iter().filter(|gate| **gate == expected).count() >= 2,
                "{expected} must run before and again after the lock: {gates:?}"
            );
        }

        let canonical = std::fs::canonicalize(&path).expect("canonicalize");
        for (gate, seen) in &calls {
            assert_eq!(
                seen,
                &canonical,
                "{gate} received {} rather than the canonicalized directory",
                seen.display()
            );
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}
