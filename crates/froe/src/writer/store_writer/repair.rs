//! Rebuilding the index of an archive that has none, from a recovery
//! scan of the archive's own entries.

use super::archive_numbering::physical_archive_names;
use super::recovery::{
    any_recoverable_segment, recover_archive_number, select_writable_generation,
};
use super::startup::{RepairedArchive, existing_staging_residue, install_target_generation};
use crate::error::{Error, Result};
use crate::tar_archive::archive::TarArchiveReader;
use crate::tar_archive::file_name::{ArchiveFileName, select_newest_file_generations};
use std::fmt::Write as _;
use std::path::Path;

/// Authorizes writing version-2 data, called immediately before the first
/// rebuilt archive is installed.
///
/// The repair produces a version-2 binary-references trailer, so a
/// version-1 store has to be raised first — but only when a rebuild is
/// actually about to land, not when one is merely predicted. Expressing that
/// as a callback keeps the manifest policy in cleanup, where the plan that
/// announced it lives, while the timing stays here, where the install is.
pub(crate) trait AuthorizeVersionTwoWrite {
    /// Called once before the first install; later calls must be no-ops.
    fn authorize(&mut self) -> Result<()>;
}

/// The authorization a write session needs: none.
///
/// `WritableRepository::open` runs `check_and_update_manifest` before it
/// touches an archive, so the store is already version 2 by the time any
/// rebuild installs. Cleanup cannot do that — it may not upgrade a manifest
/// it did not plan to — which is the whole reason this is a callback.
pub(crate) struct VersionTwoAlreadyEstablished;

impl AuthorizeVersionTwoWrite for VersionTwoAlreadyEstablished {
    fn authorize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The archive file names a repair would replace, lowest number first.
///
/// The install targets specifically, which is what an ownership preflight
/// needs: those are the files `preserve_file_metadata` will try to match.
/// Unix-only for the same reason that preflight is — ownership and mode are
/// what it compares, and `preserve_file_metadata` only enforces them there.
#[cfg(unix)]
pub(crate) fn repair_target_names(directory: &Path) -> Result<Vec<String>> {
    let mut by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>> =
        std::collections::BTreeMap::new();
    for parsed in physical_archive_names(directory)? {
        by_number
            .entry(parsed.archive_number)
            .or_default()
            .push(parsed);
    }
    let mut targets = Vec::new();
    for mut generations in by_number.into_values() {
        generations.sort_by_key(|name| name.file_generation);
        let (winner, any_nonempty) = select_writable_generation(directory, &generations);
        if winner.is_some() || !any_nonempty {
            continue;
        }
        if any_recoverable_segment(directory, &generations) {
            targets.push(
                install_target_generation(directory, &generations)
                    .file_name
                    .clone(),
            );
        }
    }
    Ok(targets)
}

/// What a repair run would find: which archive numbers it can rebuild, and
/// which hold bytes no scan can read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct IndexlessSurvey {
    /// Numbers a rebuild would succeed on.
    pub(crate) repairable: usize,
    /// File names of numbers a rebuild would refuse, lowest number first.
    pub(crate) unrepairable: Vec<String>,
}

/// Surveys the archive numbers that have no valid index.
///
/// One predicate, asked once, so nothing downstream can drift from it. The
/// distinction it draws is the whole safety question of the repair task: a
/// number whose letters scan to at least one segment can be rebuilt, and one
/// whose letters scan to nothing cannot — `recover_archive_number` refuses
/// it rather than install an empty archive. Gating an irreversible step on
/// "index-less" instead of "repairable" is what let a run that repairs
/// nothing still upgrade a manifest; planning on one and gating on the other
/// is what let a doomed run pay for durable rewrites first.
pub(crate) fn survey_indexless_archive_numbers(directory: &Path) -> Result<IndexlessSurvey> {
    let mut by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>> =
        std::collections::BTreeMap::new();
    for parsed in physical_archive_names(directory)? {
        by_number
            .entry(parsed.archive_number)
            .or_default()
            .push(parsed);
    }
    let mut survey = IndexlessSurvey::default();
    for mut generations in by_number.into_values() {
        generations.sort_by_key(|name| name.file_generation);
        let (winner, any_nonempty) = select_writable_generation(directory, &generations);
        if winner.is_some() || !any_nonempty {
            continue;
        }
        if any_recoverable_segment(directory, &generations) {
            survey.repairable += 1;
        } else {
            survey.unrepairable.push(
                install_target_generation(directory, &generations)
                    .file_name
                    .clone(),
            );
        }
    }
    Ok(survey)
}

/// The refusal an unrepairable archive number earns, raised before anything
/// is authorized rather than after a rewrite has been paid for.
pub(crate) fn unrepairable_archives_refusal(unrepairable: &[String]) -> Error {
    Error::InvalidFormat {
        details: format!(
            "{} active archive(s) have no valid index and no segment any recovery scan can read: \
             {}. Repair would refuse them, so this run cannot complete however it is retried; \
             move those files aside to proceed, and keep them — they are the only copy of \
             whatever they hold",
            unrepairable.len(),
            unrepairable.join(", ")
        ),
    }
}

/// Rebuilds the index of every archive number that has none, and does
/// nothing else.
///
/// [`initialize_archives_for_writing`] also *deletes* non-winning generation
/// letters. That deletion is cleanup's `stale-archives` task, which plans it,
/// shows it, and asks — so repair must not perform it as a side effect or it
/// would delete archives the operator never authorised, under a task that
/// only promised to repair. This is the same normalization/authorization
/// split `open_prepared` states: cleanup may only take a side effect it has
/// independently planned.
///
/// All-empty numbers are skipped rather than repaired: there is nothing to
/// rebuild from, and the zero-byte files belong to `stale-archives` too.
/// Requires only the repository lock — `recover_archive_number` reads the
/// directory and writes beside it, holding no writer state.
pub(crate) fn repair_indexless_archive_numbers(
    directory: &Path,
    observer: &mut dyn crate::progress::ProgressObserver,
    authorize: &mut dyn AuthorizeVersionTwoWrite,
) -> Result<Vec<RepairedArchive>> {
    let names = physical_archive_names(directory)?;
    // The same validation `initialize_archives_for_writing` performs before
    // it groups, and for the same reason: `data00007.tar` and
    // `data00007a.tar` both parse as number 7 generation 'a', so without this
    // they land in one group and the install target becomes whichever the
    // directory listing happened to yield first. Repairing into that is a
    // nondeterministic, irreversible rewrite of a store that the very next
    // `Repository::open` refuses outright.
    select_newest_file_generations(
        &names
            .iter()
            .map(|name| name.file_name.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut by_number: std::collections::BTreeMap<u32, Vec<ArchiveFileName>> =
        std::collections::BTreeMap::new();
    for parsed in names {
        by_number
            .entry(parsed.archive_number)
            .or_default()
            .push(parsed);
    }
    // Every archive number is examined to decide whether it needs a rebuild,
    // so the total is the whole population. The step is named for what it
    // counts: naming it for the repair would report the survey population as
    // archives repaired, which is how a single-archive rebuild came to print
    // as "535 archives".
    let total = by_number.len();
    crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "checking archive indexes for repair",
            crate::progress::WorkUnit::Archives,
        )
        .with_total(crate::progress::count(total)),
        |observer| {
            let mut repaired = Vec::new();
            let mut failures: Vec<String> = Vec::new();
            for (examined, (number, mut generations)) in by_number.into_iter().enumerate() {
                observer.step_advanced(crate::progress::count(examined));
                generations.sort_by_key(|name| name.file_generation);
                let (winner, any_nonempty) = select_writable_generation(directory, &generations);
                if winner.is_some() || !any_nonempty {
                    continue;
                }
                // Captured before the rebuild: afterwards the archive is
                // indexed and the reason no longer exists to be read.
                let reason = generations
                    .iter()
                    .rev()
                    .find_map(|candidate| {
                        TarArchiveReader::open(&directory.join(&candidate.file_name))
                            .ok()
                            .and_then(|reader| reader.recovery_reason().map(str::to_owned))
                    })
                    .unwrap_or_else(|| "the index could not be read".to_owned());
                // `recover_archive_number` unlinks its own staging file before
                // writing. A staging file that is already there is not ours:
                // it is the residue of a rebuild interrupted mid-write, which
                // cleanup's stale-temporaries task recognises, plans, and
                // deliberately *retains* unless it is provably redundant —
                // because its merged content can be the only assembled copy
                // when an install was interrupted after a letter had already
                // been retired. Repair must not delete it as a side effect of
                // a task that only promised to rebuild an index.
                if let Some(residue) = existing_staging_residue(directory, &generations) {
                    failures.push(format!(
                        "archive number {number}: {residue} is the residue of an interrupted \
                         rebuild and may hold the only assembled copy of this archive; \
                         cleanup's stale-temporaries task decides its fate, so repair will not \
                         overwrite it — move it aside to retry"
                    ));
                    continue;
                }
                // Archive numbers are independent: one that cannot be rebuilt
                // says nothing about the next, and stopping at the first would
                // hide every later problem behind one repair-and-rerun cycle
                // apiece — on a store damaged throughout, that is one full
                // planning pass per archive. Collect and continue, so the
                // operator learns the whole picture from one run.
                match recover_archive_number(directory, &generations, authorize) {
                    Ok(rebuilt) => repaired.push(RepairedArchive {
                        file_name: rebuilt.file_name().to_owned(),
                        reason,
                        bytes: rebuilt.file_size(),
                    }),
                    Err(error) => failures.push(format!("archive number {number}: {error}")),
                }
            }
            observer.step_advanced(crate::progress::count(total));
            if failures.is_empty() {
                return Ok(repaired);
            }
            Err(unfinished_repair_refusal(&repaired, &failures))
        },
    )
}

/// The refusal a partially completed repair earns.
///
/// It carries what *succeeded*, because those archives were rewritten and
/// now have `.bak` files: reporting only the failure would leave the
/// operator believing the store is as they left it. This is the same
/// obligation `attach_planning_warnings` meets for planning, applied to the
/// one mutation that happens before there is a plan to record it in.
pub(super) fn unfinished_repair_refusal(
    repaired: &[RepairedArchive],
    failures: &[String],
) -> Error {
    let mut details = format!(
        "{} of {} archive index rebuild(s) failed: {}",
        failures.len(),
        failures.len() + repaired.len(),
        failures.join("; ")
    );
    if !repaired.is_empty() {
        let names: Vec<&str> = repaired
            .iter()
            .map(|archive| archive.file_name.as_str())
            .collect();
        let _ = write!(
            details,
            ". Already rebuilt and durable, with the originals retained under `.bak` names: {}. \
             Those need no second attempt; rerunning repairs only what is left",
            names.join(", ")
        );
    }
    Error::InvalidFormat { details }
}
