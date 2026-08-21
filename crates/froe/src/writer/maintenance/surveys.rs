//! Read-only surveys a caller can take before deciding what a run should
//! do: which archives need an index repair, and what recovery backups a
//! removal would consider.
//!
//! Both surveys answer in a couple of seconds what the full planning walk
//! answers in minutes, so an interactive caller can ask its questions
//! before the store is opened for planning rather than after.

use super::planning::{canonical_repository_directory, validate_repository_shape};
use super::recovery_backups::recovery_backup_target;
use crate::error::Result;
use std::path::Path;

/// What a read-only look at the archive indexes established: the archives
/// an authorized repair would rebuild, and the ones nothing can.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArchiveIndexSurvey {
    /// Install-target file names of active archive numbers that have no
    /// valid index but whose entries a recovery scan can read — what an
    /// authorized repair rebuilds, retaining the originals under `.bak`
    /// names.
    pub repairable: Vec<String>,
    /// File names of active archive numbers that have no valid index and
    /// no segment any recovery scan can read — residue no repair can
    /// rebuild, which a maintenance run refuses outright.
    pub unrepairable: Vec<String>,
}

impl ArchiveIndexSurvey {
    /// Whether any active archive lacks a usable index, repairably or not.
    /// Planning past this state needs either an authorized repair or an
    /// operator moving the unrepairable files aside.
    #[must_use]
    pub fn any_archive_lacks_an_index(&self) -> bool {
        !self.repairable.is_empty() || !self.unrepairable.is_empty()
    }
}

/// Surveys the archive indexes read-only, without taking the repository
/// lock or changing a byte. The distinction between repairable and
/// unrepairable is the one predicate a repair run itself gates on.
pub fn survey_archive_indexes(directory: &Path) -> Result<ArchiveIndexSurvey> {
    let directory = canonical_repository_directory(directory)?;
    validate_repository_shape(&directory)?;
    let survey = crate::writer::store_writer::survey_indexless_archive_numbers(&directory)?;
    Ok(ArchiveIndexSurvey {
        repairable: survey.repairable,
        unrepairable: survey.unrepairable,
    })
}

/// The recovery backups sitting in a repository directory: every
/// `journal.log.bak.NNN` and archive `.bak` spelling froe manages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryBackupSurvey {
    /// Recovery backup files present.
    pub files: u64,
    /// Bytes those files hold.
    pub bytes: u64,
}

/// Surveys the recovery backups read-only, without taking the repository
/// lock or changing a byte. This is the population an enabled
/// recovery-backup removal would apply its age/count policy to.
pub fn survey_recovery_backups(directory: &Path) -> Result<RecoveryBackupSurvey> {
    let directory = canonical_repository_directory(directory)?;
    validate_repository_shape(&directory)?;
    let mut survey = RecoveryBackupSurvey::default();
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if recovery_backup_target(&name).is_none() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        survey.files += 1;
        survey.bytes = survey.bytes.saturating_add(metadata.len());
    }
    Ok(survey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::maintenance::test_support::*;

    #[test]
    fn a_healthy_store_surveys_clean() {
        let directory = TestDirectory::repository("survey-clean");
        let indexes = survey_archive_indexes(&directory.path).expect("survey indexes");
        assert!(!indexes.any_archive_lacks_an_index());
        assert!(indexes.repairable.is_empty());
        assert!(indexes.unrepairable.is_empty());
        let backups = survey_recovery_backups(&directory.path).expect("survey backups");
        assert_eq!(backups, RecoveryBackupSurvey { files: 0, bytes: 0 });
    }

    #[test]
    fn a_broken_index_and_a_backup_are_both_surveyed_without_writing() {
        let directory = TestDirectory::repository("survey-damage-and-backups");
        break_index_magic(&directory.path.join("data00000a.tar"));
        std::fs::write(directory.path.join("journal.log.bak.000"), b"seven b")
            .expect("write recovery backup");
        std::fs::write(directory.path.join("data00000a.tar.bak"), b"12 bytes....")
            .expect("write archive recovery backup");
        let before = file_bytes(&directory.path);

        let indexes = survey_archive_indexes(&directory.path).expect("survey indexes");
        assert!(indexes.any_archive_lacks_an_index());
        assert_eq!(indexes.repairable, ["data00000a.tar"]);
        assert!(indexes.unrepairable.is_empty());
        let backups = survey_recovery_backups(&directory.path).expect("survey backups");
        assert_eq!(backups.files, 2);
        assert_eq!(backups.bytes, 19);

        assert_eq!(
            file_bytes(&directory.path),
            before,
            "both surveys must be strictly read-only"
        );
    }

    #[test]
    fn an_unreadable_indexless_archive_is_surveyed_as_unrepairable() {
        let directory = TestDirectory::repository("survey-unrepairable");
        std::fs::write(directory.path.join("data00500a.tar"), vec![0x5au8; 4096])
            .expect("write unrecoverable residue");

        let indexes = survey_archive_indexes(&directory.path).expect("survey indexes");
        assert!(indexes.any_archive_lacks_an_index());
        assert!(indexes.repairable.is_empty());
        assert_eq!(indexes.unrepairable, ["data00500a.tar"]);
    }

    #[test]
    fn a_missing_directory_is_reported_as_not_a_repository() {
        let missing =
            std::env::temp_dir().join(format!("froe-survey-absent-{}", std::process::id()));
        let error = survey_archive_indexes(&missing).expect_err("absent directory must refuse");
        assert!(
            error.to_string().contains("is not a repository directory"),
            "unexpected refusal: {error}"
        );
    }
}
