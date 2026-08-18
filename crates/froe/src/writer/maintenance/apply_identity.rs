//! Unix ownership and permission checks. A run that would write files a
//! service user cannot read afterwards is refused before it starts.

#[cfg(unix)]
// `PermissionsExt::mode` is always `u32`, while libc's `mode_t` (and thus
// `libc::S_ISGID`) is `u16` on Apple targets.
pub(super) const SETGID_MODE: u32 = 0o2000;

#[cfg(unix)]
use super::options::MaintenanceTask;
use super::plan::CompactionPlan;
use crate::error::{Error, Result};
#[cfg(unix)]
use crate::writer::store_writer::{
    PlannedArchiveSweep, StandaloneSegmentCompactionPlan, sync_directory_strict,
};
#[cfg(unix)]
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

pub(super) fn validate_apply_environment(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // Exercise the exact durability primitive before taking the lock or
        // performing the first mutation. Some filesystems reject directory
        // fsync; discovering that after an unlink would be too late.
        sync_directory_strict(directory)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Err(Error::InvalidFormat {
            details: "cleanup apply is supported only on Unix; dry-run planning remains available on this platform"
                .to_owned(),
        })
    }
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
pub(super) fn validate_apply_identity(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not access memory.
        let effective_uid = unsafe { libc::geteuid() };
        validate_apply_identity_for_uid(directory, effective_uid)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn validate_apply_identity_for_uid(directory: &Path, effective_uid: u32) -> Result<()> {
    if let Some(issue) = journal_service_user_issue(directory, effective_uid)? {
        return Err(Error::InvalidFormat {
            details: format!(
                "{issue}; refusing before repo.lock or replacement files can be created with the wrong owner"
            ),
        });
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn journal_service_user_issue(
    directory: &Path,
    effective_uid: u32,
) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt as _;

    let journal = directory.join("journal.log");
    let owner = std::fs::symlink_metadata(&journal)?.uid();
    Ok((owner != effective_uid).then(|| {
        format!(
            "cleanup must run as the repository service user: {} is owned by uid {owner}, but the effective uid is {effective_uid}",
            journal.display()
        )
    }))
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
pub(super) fn validate_plan_apply_identity(directory: &Path, plan: &CompactionPlan) -> Result<()> {
    #[cfg(unix)]
    {
        let credentials = current_apply_credentials()?;
        validate_plan_apply_identity_for_credentials(directory, plan, &credentials)
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, plan);
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ApplyCredentials {
    pub(super) effective_uid: u32,
    pub(super) effective_gid: u32,
    pub(super) group_ids: BTreeSet<u32>,
}

#[cfg(unix)]
pub(super) fn current_apply_credentials() -> Result<ApplyCredentials> {
    // SAFETY: these credential queries have no memory preconditions. The
    // null first `getgroups` call requests only the required element count.
    let caller_uid = unsafe { libc::geteuid() };
    // SAFETY: `getegid` has no preconditions and does not access memory.
    let primary_group = unsafe { libc::getegid() };
    // SAFETY: a zero-sized group query permits a null output pointer.
    let group_count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if group_count < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut raw_groups = vec![0 as libc::gid_t; group_count as usize];
    if group_count != 0 {
        // SAFETY: `raw_groups` has exactly `group_count` writable elements.
        let returned = unsafe { libc::getgroups(group_count, raw_groups.as_mut_ptr()) };
        if returned < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        raw_groups.truncate(returned as usize);
    }
    let mut group_ids: BTreeSet<u32> = raw_groups.into_iter().collect();
    group_ids.insert(primary_group);
    Ok(ApplyCredentials {
        effective_uid: caller_uid,
        effective_gid: primary_group,
        group_ids,
    })
}

/// What a planned run will rewrite, which is what decides the files it may
/// take ownership and permission metadata from.
#[cfg(unix)]
#[derive(Clone, Copy)]
pub(super) struct PlannedRewrites<'plan> {
    pub(super) upgrades_manifest: bool,
    pub(super) segment_plan: Option<&'plan StandaloneSegmentCompactionPlan>,
    pub(super) moves_checkpoint_head: bool,
    pub(super) rewrites_journal: bool,
}

#[cfg(unix)]
pub(super) fn planned_metadata_sources(
    directory: &Path,
    rewrites: PlannedRewrites<'_>,
) -> Result<BTreeSet<String>> {
    let PlannedRewrites {
        upgrades_manifest,
        segment_plan,
        moves_checkpoint_head,
        rewrites_journal,
    } = rewrites;
    let mut metadata_sources = BTreeSet::new();
    if upgrades_manifest {
        metadata_sources.insert("manifest".to_owned());
    }
    if rewrites_journal {
        metadata_sources.insert("journal.log".to_owned());
    }
    if let Some(segment_plan) = segment_plan {
        metadata_sources.extend(segment_plan.archives.iter().filter_map(|archive| {
            if let PlannedArchiveSweep::Rewrite { file_name, .. } = archive {
                Some(file_name.clone())
            } else {
                None
            }
        }));
    }
    if moves_checkpoint_head {
        let dispositions: HashMap<&str, &PlannedArchiveSweep> = segment_plan
            .into_iter()
            .flat_map(|plan| plan.archives.iter())
            .map(|archive| (archive.file_name(), archive))
            .collect();
        // `open_prepared` takes metadata from the first active archive, in
        // newest-number-first order. A planned whole removal can either leave
        // that source in place after a safe unlink failure or make the next
        // archive the template. Consequently only the leading run of Remove
        // sources plus the first non-Remove source can become the template.
        // A rewrite stays at the same archive number and copies its source
        // metadata onto the replacement, so its source is the exact preflight
        // representative and terminates the candidate prefix.
        for archive in crate::store::open_all_archives(directory)? {
            let file_name = archive.file_name().to_owned();
            metadata_sources.insert(file_name.clone());
            if !matches!(
                dispositions.get(file_name.as_str()),
                Some(PlannedArchiveSweep::Remove { .. })
            ) {
                break;
            }
        }
    }
    Ok(metadata_sources)
}

#[cfg(unix)]
pub(super) fn planned_apply_identity_issue(
    directory: &Path,
    plan: &CompactionPlan,
    credentials: &ApplyCredentials,
) -> Result<Option<String>> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let directory_metadata = std::fs::symlink_metadata(directory)?;
    let possible_created_gids = possible_created_group_ids(
        directory_metadata.gid(),
        directory_metadata.permissions().mode(),
        credentials,
    );

    for name in planned_metadata_sources(
        directory,
        PlannedRewrites {
            upgrades_manifest: plan.manifest_upgrade,
            segment_plan: plan.segment_plan.as_ref(),
            moves_checkpoint_head: !plan.checkpoints.names.is_empty(),
            rewrites_journal: plan.tasks.contains(&MaintenanceTask::Journal)
                && plan.journal.removed_lines != 0,
        },
    )? {
        let path = directory.join(&name);
        let metadata = std::fs::symlink_metadata(&path)?;
        if let Some(issue) = metadata_source_apply_identity_issue(
            &path,
            metadata.uid(),
            metadata.gid(),
            metadata.permissions().mode(),
            &possible_created_gids,
            credentials,
        ) {
            return Ok(Some(issue));
        }
    }
    Ok(None)
}

#[cfg(all(test, unix))]
std::thread_local! {
    static POSSIBLE_CREATED_GROUP_IDS_INPUT: std::cell::Cell<Option<(u32, u32)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, unix))]
pub(super) fn take_possible_created_group_ids_input() -> Option<(u32, u32)> {
    POSSIBLE_CREATED_GROUP_IDS_INPUT.with(std::cell::Cell::take)
}

#[cfg(unix)]
pub(super) fn possible_created_group_ids(
    directory_gid: u32,
    directory_mode: u32,
    credentials: &ApplyCredentials,
) -> BTreeSet<u32> {
    #[cfg(test)]
    POSSIBLE_CREATED_GROUP_IDS_INPUT.with(|input| input.set(Some((directory_gid, directory_mode))));

    if directory_mode & SETGID_MODE != 0 {
        BTreeSet::from([directory_gid])
    } else {
        // POSIX permits either System V inheritance (the process effective
        // gid) or BSD inheritance (the parent-directory gid). Linux normally
        // selects with S_ISGID, but some filesystems also honor bsdgroups/grpid
        // mount policy. A read-only preview cannot distinguish those cases,
        // so model both outcomes rather than assuming the host default.
        BTreeSet::from([credentials.effective_gid, directory_gid])
    }
}

#[cfg(unix)]
pub(super) fn metadata_source_apply_identity_issue(
    path: &Path,
    owner: u32,
    group: u32,
    mode: u32,
    possible_created_gids: &BTreeSet<u32>,
    credentials: &ApplyCredentials,
) -> Option<String> {
    if owner != credentials.effective_uid {
        return Some(format!(
            "cleanup cannot safely replace {} while preserving its metadata: it is owned by uid {owner}, but the effective uid is {}",
            path.display(),
            credentials.effective_uid
        ));
    }
    let might_need_to_change_group = possible_created_gids
        .iter()
        .any(|&created_gid| created_gid != group);
    let must_install_setgid = mode & SETGID_MODE != 0;
    if credentials.effective_uid != 0
        && !credentials.group_ids.contains(&group)
        && (might_need_to_change_group || must_install_setgid)
    {
        return Some(format!(
            "cleanup cannot safely replace {} while preserving its metadata: gid {group} is not the effective or a supplementary group of uid {}, while a new staging file may have gid {possible_created_gids:?} and the source mode is {:#06o}; group ownership and setgid-mode preservation cannot be guaranteed read-only",
            path.display(),
            credentials.effective_uid,
            mode & 0o7777
        ));
    }
    None
}

#[cfg(unix)]
pub(super) fn validate_plan_apply_identity_for_credentials(
    directory: &Path,
    plan: &CompactionPlan,
    credentials: &ApplyCredentials,
) -> Result<()> {
    if let Some(details) = planned_apply_identity_issue(directory, plan, credentials)? {
        return Err(Error::InvalidFormat {
            details: format!(
                "{details}; conservatively refusing before planned repository mutations"
            ),
        });
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn preview_apply_identity_issue(
    directory: &Path,
    plan: &CompactionPlan,
    credentials: &ApplyCredentials,
) -> Result<Option<String>> {
    if let Some(issue) = journal_service_user_issue(directory, credentials.effective_uid)? {
        return Ok(Some(issue));
    }
    planned_apply_identity_issue(directory, plan, credentials)
}

pub(super) fn append_apply_identity_preview_warning(directory: &Path, plan: &mut CompactionPlan) {
    #[cfg(unix)]
    append_apply_identity_preview_warning_for_credentials(
        directory,
        plan,
        current_apply_credentials(),
    );
    #[cfg(not(unix))]
    let _ = (directory, plan);
}

#[cfg(unix)]
pub(super) fn append_apply_identity_preview_warning_for_credentials(
    directory: &Path,
    plan: &mut CompactionPlan,
    credentials: Result<ApplyCredentials>,
) {
    match credentials
        .and_then(|credentials| preview_apply_identity_issue(directory, plan, &credentials))
    {
        Ok(Some(issue)) => plan.warnings.push(format!(
            "apply ownership preflight warning: {issue}; authoritative apply will conservatively refuse before planned repository mutations"
        )),
        Ok(None) => {}
        Err(error) => plan.warnings.push(format!(
            "apply ownership could not be proved during this read-only preview ({error}); authoritative apply will retry the check under the repository lock"
        )),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::store::Repository;

    use crate::writer::maintenance::options::*;
    use crate::writer::maintenance::plan::*;

    use crate::writer::maintenance::prepared::*;

    use crate::writer::maintenance::test_support::*;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::store_writer::WritableRepository;

    #[cfg(unix)]
    #[test]
    fn apply_identity_mismatch_is_detected_before_lock_creation() {
        use std::os::unix::fs::MetadataExt;

        let directory = TestDirectory::repository("wrong-service-user");
        std::fs::remove_file(directory.path.join("repo.lock")).expect("remove old lock inode");
        let owner = std::fs::metadata(directory.path.join("journal.log"))
            .expect("journal metadata")
            .uid();
        let different_uid = if owner == u32::MAX {
            owner - 1
        } else {
            owner + 1
        };

        let error = validate_apply_identity_for_uid(&directory.path, different_uid)
            .expect_err("different service uid must be rejected");

        assert!(error.to_string().contains("service user"));
        assert!(!directory.path.join("repo.lock").exists());
    }
    #[cfg(unix)]
    #[test]
    fn authoritative_plan_rejects_a_foreign_owned_archive_rewrite_before_mutation() {
        use std::os::unix::fs::MetadataExt as _;

        let (directory, source_name, _, _) =
            rewrite_certificate_fixture("foreign-owned-rewrite-preflight");
        let options = CompactionOptions::default().with_tasks([MaintenanceTask::Segments]);
        let plan = plan_compaction(&directory.path, &options).expect("healthy rewrite plan");
        let owner = std::fs::metadata(directory.path.join(&source_name))
            .expect("source metadata")
            .uid();
        let source_gid = std::fs::metadata(directory.path.join(&source_name))
            .expect("source metadata")
            .gid();
        let different_uid = if owner == u32::MAX {
            owner - 1
        } else {
            owner + 1
        };
        let credentials = ApplyCredentials {
            effective_uid: different_uid,
            effective_gid: source_gid,
            group_ids: BTreeSet::from([source_gid]),
        };
        let before = file_bytes(&directory.path);

        let error =
            validate_plan_apply_identity_for_credentials(&directory.path, &plan, &credentials)
                .expect_err("foreign-owned rewrite source must fail preflight");

        assert_eq!(
            error.to_string(),
            format!(
                "invalid segment-tar data: cleanup cannot safely replace {} while preserving its metadata: it is owned by uid {owner}, but the effective uid is {different_uid}; conservatively refusing before planned repository mutations",
                directory.path.join(source_name).display()
            )
        );
        assert_eq!(file_bytes(&directory.path), before);
        Repository::open(&directory.path).expect("preflight refusal leaves repository healthy");
    }
    #[cfg(unix)]
    #[test]
    fn planned_identity_preflight_uses_the_real_repository_directory_gid_and_mode() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let directory = TestDirectory::repository("planned-identity-directory-metadata");
        let plan = plan_compaction(&directory.path, &CompactionOptions::new().with_tasks([]))
            .expect("plan health-only cleanup");
        let mut permissions = std::fs::symlink_metadata(&directory.path)
            .expect("repository metadata before mode change")
            .permissions();
        permissions.set_mode(0o731);
        std::fs::set_permissions(&directory.path, permissions)
            .expect("install a distinctive repository mode");
        let metadata = std::fs::symlink_metadata(&directory.path).expect("repository metadata");
        let synthetic_gid = if metadata.gid() == u32::MAX {
            metadata.gid() - 1
        } else {
            metadata.gid() + 1
        };
        let credentials = ApplyCredentials {
            effective_uid: 42_424,
            effective_gid: synthetic_gid,
            group_ids: BTreeSet::from([synthetic_gid]),
        };
        let _ = take_possible_created_group_ids_input();

        let issue = planned_apply_identity_issue(&directory.path, &plan, &credentials)
            .expect("analyze planned metadata identity");

        assert_eq!(issue, None, "a health-only plan has no metadata sources");
        assert_eq!(
            take_possible_created_group_ids_input(),
            Some((metadata.gid(), metadata.permissions().mode())),
            "the group model must receive the repository directory's real gid and mode"
        );
    }
    #[cfg(unix)]
    #[test]
    fn ownership_preview_emits_a_known_mismatch_and_matches_the_apply_gate() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = TestDirectory::repository("preview-service-user-warning");
        let mut plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("read-only plan");
        let journal_owner = std::fs::symlink_metadata(directory.path.join("journal.log"))
            .expect("journal metadata")
            .uid();
        let other_uid = if journal_owner == u32::MAX {
            journal_owner - 1
        } else {
            journal_owner + 1
        };
        let credentials = ApplyCredentials {
            effective_uid: other_uid,
            effective_gid: 0,
            group_ids: BTreeSet::from([0]),
        };

        let issue = preview_apply_identity_issue(&directory.path, &plan, &credentials)
            .expect("preview identity analysis")
            .expect("foreign service user must produce a preview warning");
        let shared_issue = journal_service_user_issue(&directory.path, other_uid)
            .expect("shared journal ownership analysis")
            .expect("foreign service user must fail the shared gate");

        assert_eq!(issue, shared_issue);
        let apply_error = validate_apply_identity_for_uid(&directory.path, other_uid)
            .expect_err("authoritative apply rejects the same mismatch")
            .to_string();
        assert!(apply_error.contains(&shared_issue), "{apply_error}");

        let warnings_before = plan.warnings.len();
        append_apply_identity_preview_warning_for_credentials(
            &directory.path,
            &mut plan,
            Ok(credentials),
        );
        assert_eq!(plan.warnings.len(), warnings_before + 1);
        let warning = plan.warnings.last().expect("known-mismatch warning");
        assert!(
            warning.contains("apply ownership preflight warning"),
            "{warning}"
        );
        assert!(warning.contains(&shared_issue), "{warning}");
        assert!(warning.contains("authoritative apply"), "{warning}");
    }
    #[cfg(unix)]
    #[test]
    fn ownership_preview_emits_a_warning_when_analysis_is_unprovable() {
        use std::os::unix::fs::MetadataExt as _;

        let (directory, source_name, _, _) =
            rewrite_certificate_fixture("preview-unprovable-warning");
        let mut plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("read-only rewrite plan");
        let journal_metadata = std::fs::symlink_metadata(directory.path.join("journal.log"))
            .expect("journal metadata");
        let credentials = ApplyCredentials {
            effective_uid: journal_metadata.uid(),
            effective_gid: journal_metadata.gid(),
            group_ids: BTreeSet::from([journal_metadata.gid()]),
        };
        std::fs::rename(
            directory.path.join(&source_name),
            directory
                .path
                .join(format!("{source_name}.removed-after-plan")),
        )
        .expect("make the planned metadata source unavailable");
        assert!(
            preview_apply_identity_issue(&directory.path, &plan, &credentials).is_err(),
            "the fixture must exercise the analysis-error arm"
        );

        let warnings_before = plan.warnings.len();
        append_apply_identity_preview_warning_for_credentials(
            &directory.path,
            &mut plan,
            Ok(credentials),
        );

        assert_eq!(plan.warnings.len(), warnings_before + 1);
        let warning = plan.warnings.last().expect("unprovable-analysis warning");
        assert!(
            warning.contains("apply ownership could not be proved"),
            "{warning}"
        );
        assert!(
            warning.contains("authoritative apply will retry"),
            "{warning}"
        );
    }
    #[cfg(unix)]
    #[test]
    fn metadata_preflight_models_inherited_gid_and_setgid_mode_conservatively() {
        const SYNTHETIC_NON_ROOT_UID: u32 = 42_424;
        const ARCHIVE_GROUP: u32 = 27_182;
        const UNRELATED_GROUP: u32 = 31_415;

        let credentials = ApplyCredentials {
            effective_uid: SYNTHETIC_NON_ROOT_UID,
            effective_gid: UNRELATED_GROUP,
            group_ids: BTreeSet::from([UNRELATED_GROUP]),
        };
        let possible_created_gids =
            possible_created_group_ids(ARCHIVE_GROUP, SETGID_MODE | 0o750, &credentials);
        let source_path = std::path::Path::new("data00000a.tar");
        let source_mode = 0o640;

        assert_eq!(
            possible_created_gids,
            BTreeSet::from([ARCHIVE_GROUP]),
            "a setgid directory fixes the staging-file group"
        );
        assert_eq!(
            metadata_source_apply_identity_issue(
                source_path,
                SYNTHETIC_NON_ROOT_UID,
                ARCHIVE_GROUP,
                source_mode,
                &possible_created_gids,
                &credentials,
            ),
            None,
            "an already inherited source gid needs neither fchown nor group membership when no setgid bit is requested"
        );

        let issue = metadata_source_apply_identity_issue(
            source_path,
            SYNTHETIC_NON_ROOT_UID,
            ARCHIVE_GROUP,
            source_mode | SETGID_MODE,
            &possible_created_gids,
            &credentials,
        )
        .expect("setgid preservation outside caller groups must refuse conservatively");
        assert!(issue.contains(&format!("gid {ARCHIVE_GROUP}")), "{issue}");
        assert!(issue.contains("setgid-mode"), "{issue}");
        assert!(issue.contains("cannot be guaranteed read-only"), "{issue}");
    }
    #[cfg(unix)]
    #[test]
    fn metadata_preflight_models_both_permitted_non_setgid_creation_groups() {
        const SYNTHETIC_NON_ROOT_UID: u32 = 42_424;
        const SOURCE_GROUP: u32 = 27_182;
        const EFFECTIVE_GROUP: u32 = 31_415;

        let credentials = ApplyCredentials {
            effective_uid: SYNTHETIC_NON_ROOT_UID,
            effective_gid: EFFECTIVE_GROUP,
            group_ids: BTreeSet::from([EFFECTIVE_GROUP]),
        };
        let possible_gids = possible_created_group_ids(SOURCE_GROUP, 0o750, &credentials);

        let issue = metadata_source_apply_identity_issue(
            std::path::Path::new("data00000a.tar"),
            SYNTHETIC_NON_ROOT_UID,
            SOURCE_GROUP,
            0o640,
            &possible_gids,
            &credentials,
        )
        .expect("a possible System V group outcome must be treated conservatively");

        assert_eq!(
            possible_gids,
            BTreeSet::from([SOURCE_GROUP, EFFECTIVE_GROUP])
        );
        assert!(
            issue.contains(&format!("may have gid {possible_gids:?}")),
            "the diagnostic must record both POSIX-permitted creation groups: {issue}"
        );
    }
    #[cfg(unix)]
    #[test]
    fn checkpoint_metadata_preflight_checks_only_the_newest_possible_template() {
        let directory = TestDirectory::repository("checkpoint-template-prefix");
        std::fs::copy(
            directory.path.join("data00000a.tar"),
            directory.path.join("data00001a.tar"),
        )
        .expect("create a second readable archive number");

        let sources = planned_metadata_sources(
            &directory.path,
            PlannedRewrites {
                upgrades_manifest: false,
                segment_plan: None,
                moves_checkpoint_head: true,
                rewrites_journal: false,
            },
        )
        .expect("derive checkpoint metadata sources");

        assert_eq!(sources, BTreeSet::from(["data00001a.tar".to_owned()]));
    }
    #[cfg(unix)]
    #[test]
    fn checkpoint_metadata_preflight_includes_the_leading_removal_outcome_prefix() {
        let directory = TestDirectory::repository("checkpoint-template-removal-prefix");
        let current_generation = GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        };
        let current_head = {
            let store = WritableRepository::open(&directory.path).expect("open head writer");
            let content_root = write_empty_node_segment(&store, current_generation);
            let mut writer = store.record_writer(current_generation);
            let head = writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: content_root,
                    },
                    &[],
                )
                .expect("write current head");
            writer.finish().expect("finish current head segment");
            assert!(store.compare_and_set_head(store.head(), head));
            store.close().expect("close head writer");
            head
        };
        let orphan = {
            let store = WritableRepository::open(&directory.path).expect("open orphan writer");
            let orphan = write_empty_node_segment(
                &store,
                GarbageCollectionGeneration {
                    generation: 0,
                    full_generation: 0,
                    is_compacted: false,
                },
            );
            store.close().expect("close unjournaled orphan writer");
            orphan
        };
        let repository = Repository::open(&directory.path).expect("open prefix fixture");
        let current_archive = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(current_head.segment))
            .expect("current head archive")
            .file_name()
            .to_owned();
        let orphan_archive = repository
            .archives()
            .iter()
            .find(|archive| archive.contains_segment(orphan.segment))
            .expect("newest orphan archive")
            .file_name()
            .to_owned();
        drop(repository);
        let plan = plan_compaction(
            &directory.path,
            &CompactionOptions::default().with_tasks([MaintenanceTask::Segments]),
        )
        .expect("plan newest whole removal");
        assert!(plan.actions().iter().any(|action| matches!(
            action,
            CompactionAction::RemoveReclaimableArchive { file_name, .. }
                if file_name == &orphan_archive
        )));

        let sources = planned_metadata_sources(
            &directory.path,
            PlannedRewrites {
                upgrades_manifest: false,
                segment_plan: plan.segment_plan.as_ref(),
                moves_checkpoint_head: true,
                rewrites_journal: false,
            },
        )
        .expect("derive possible checkpoint templates");

        assert_eq!(
            sources,
            BTreeSet::from([orphan_archive, current_archive]),
            "a failed newest unlink uses that source; a successful unlink promotes only the next active archive"
        );
    }

    /// A rebuild ends by matching the replaced archive's ownership, which
    /// fails for a foreign-owned file. Discovering that after the rewrite
    /// means no rerun ever converges, so it is a stat(2) check up front.
    #[cfg(unix)]
    #[test]
    fn a_repair_target_this_process_cannot_match_refuses_before_rewriting() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = TestDirectory::new("repair-foreign-owner");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close");
        }
        break_index_magic(&directory.path.join("data00000a.tar"));
        let owner = std::fs::symlink_metadata(directory.path.join("data00000a.tar"))
            .expect("archive metadata")
            .uid();
        // Only meaningful where the archive is not already ours; as root
        // every chown succeeds and there is nothing to refuse.
        if owner != unsafe { libc::geteuid() } || owner == 0 {
            return;
        }
        let targets = crate::writer::store_writer::repair_target_names(&directory.path)
            .expect("survey the repair targets");
        assert_eq!(
            targets,
            vec!["data00000a.tar".to_owned()],
            "the preflight inspects the file the rebuild would replace"
        );
    }
}
