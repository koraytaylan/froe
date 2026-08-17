//! Unix ownership and permission checks. A run that would write files a
//! service user cannot read afterwards is refused before it starts.

#[cfg(unix)]
// `PermissionsExt::mode` is always `u32`, while libc's `mode_t` (and thus
// `libc::S_ISGID`) is `u16` on Apple targets.
pub(super) const SETGID_MODE: u32 = 0o2000;

use super::options::MaintenanceTask;
use super::plan::CompactionPlan;
use crate::error::{Error, Result};
use crate::writer::store_writer::{
    PlannedArchiveSweep, StandaloneSegmentCompactionPlan, sync_directory_strict,
};
use std::collections::{BTreeSet, HashMap};
#[cfg(unix)]
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
