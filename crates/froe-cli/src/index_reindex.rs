//! `froe index reindex`: rebuilding flagged indexes offline.
//!
//! The flow is `froe compact`'s, step for step. `--dry-run` plans read-only
//! without the lock and prints what a run would do. Otherwise the command
//! prepares under the lock, prints the plan, asks while still holding the
//! lock, applies, and prints a summary built from the outcome rather than
//! from the plan — so what the operator reads afterwards is what happened,
//! not what was intended.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use froe::index::lucene::documents::binaries::{BinaryTextFallback, BinaryTextPolicy};
use froe::writer::index::selection::IndexingState;
use froe::writer::index::{
    DefinitionReport, PreparedReindex, ReindexAction, ReindexOptions, ReindexOutcome, ReindexPlan,
    WorkDirectory, plan_reindex_with_progress,
};

use crate::mutation::{Confirmation, PromptAnswer, confirm, report_cancelled};
use crate::output::count_noun;
use crate::progress::Reporter;

/// One mebibyte, for the `--sort-budget-mebibytes` conversion.
const BYTES_PER_MEBIBYTE: usize = 1024 * 1024;

/// The parsed command line, one field per flag.
pub(crate) struct ReindexCommandLine {
    pub(crate) indexes: Vec<String>,
    pub(crate) dry_run: bool,
    pub(crate) assume_yes: bool,
    pub(crate) work_directory: Option<PathBuf>,
    pub(crate) from_head: bool,
    pub(crate) sort_budget_mebibytes: Option<usize>,
    /// `--binary-text`, without which no Lucene definition is rebuilt.
    pub(crate) binary_text: Option<crate::command_line::index::BinaryTextChoice>,
    /// `--pre-extracted-text-directory`, consulted before the fallback.
    pub(crate) pre_extracted_text_directory: Option<PathBuf>,
}

impl ReindexCommandLine {
    /// The library options this command line asks for.
    fn options(&self) -> ReindexOptions {
        let mut options = ReindexOptions::new()
            .with_indexes(self.indexes.clone())
            .with_from_head(self.from_head);
        if let Some(directory) = &self.work_directory {
            options = options.with_work_directory(WorkDirectory::OperatorNamed(directory.clone()));
        }
        if let Some(mebibytes) = self.sort_budget_mebibytes {
            options =
                options.with_sort_budget_bytes(mebibytes.saturating_mul(BYTES_PER_MEBIBYTE).max(1));
        }
        if let Some(choice) = self.binary_text {
            let fallback = match choice {
                crate::command_line::index::BinaryTextChoice::Marker => BinaryTextFallback::Marker,
                crate::command_line::index::BinaryTextChoice::Skip => BinaryTextFallback::Skip,
            };
            let mut policy = BinaryTextPolicy::new(fallback);
            if let Some(directory) = &self.pre_extracted_text_directory {
                policy = policy.with_pre_extracted_text_directory(directory.clone());
            }
            options = options.with_binary_text_policy(policy);
        }
        options
    }
}

/// Runs the command. Returns whether it succeeded.
pub(crate) fn run_reindex(
    repository: &Path,
    command: &ReindexCommandLine,
    reporter: &Reporter,
) -> froe::Result<bool> {
    reporter.status(
        "note: the repository must be offline — no Oak instance may be running against it, and \
         the run holds the repository lock from planning through publication",
    );
    let confirmation = Confirmation::from_assume_yes_flag(command.assume_yes);
    let options = command.options();

    if command.dry_run {
        let preview = plan_reindex_with_progress(repository, &options, &mut reporter.clone())?;
        // The plan is the operator's evidence: end every report before a
        // single line of it is written.
        reporter.finish();
        print_plan(&preview);
        println!("dry-run: repository was not modified");
        return Ok(true);
    }

    let prepared =
        PreparedReindex::prepare_with_progress(repository, options, &mut reporter.clone())?;
    reporter.finish();
    print_plan(prepared.plan());
    if prepared.plan().is_empty() {
        println!("nothing to do");
        return Ok(true);
    }

    let answer = confirm(
        &format!(
            "about to rebuild {} in {}",
            count_noun(prepared.plan().rebuild_count() as u64, "index", "indexes"),
            crate::output::sanitize_terminal_path(&prepared.plan().directory)
        ),
        confirmation,
        reporter,
    );
    if answer != PromptAnswer::Yes {
        report_cancelled("reindex", answer);
        return Ok(false);
    }

    let directory = prepared.plan().directory.clone();
    let outcome = prepared.apply_with_progress(&mut reporter.clone())?;
    reporter.finish();
    print_summary(&outcome);
    if outcome.moved_the_head() {
        report_pinning_checkpoints(&directory);
    }
    Ok(true)
}

/// Names the checkpoints that keep the replaced index records live.
///
/// The help text promises this, and it is the one thing about a reindex an
/// operator cannot work out afterwards: the old records are unreachable
/// from the head but not yet garbage, and these are the reason. Read from
/// the reopened store, so a failure to read them is reported rather than
/// failing a run that has already succeeded.
fn report_pinning_checkpoints(directory: &Path) {
    let names = match froe::Repository::open(directory).and_then(|repository| {
        repository.checkpoints().map(|checkpoints| {
            checkpoints
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        })
    }) {
        Ok(names) => names,
        Err(error) => {
            eprintln!(
                "froe: could not list the checkpoints that pin the replaced records: {error}"
            );
            return;
        }
    };
    if names.is_empty() {
        println!(
            "no checkpoint pins the replaced index records; the next `froe compact` reclaims them"
        );
        return;
    }
    println!(
        "the replaced index records stay live through {}: {}. They are reclaimed only by a \
         `froe compact` run after those are released.",
        count_noun(names.len() as u64, "checkpoint", "checkpoints"),
        names.join(", "),
    );
}

/// Prints the plan: one line per definition, then the warnings and the work
/// directory the run will spill into.
fn print_plan(plan: &ReindexPlan) {
    println!("reindex plan for {}", plan.directory.display());
    for action in &plan.actions {
        println!("  {}", render_action(action));
    }
    if plan.actions.is_empty() {
        println!("  (no definition is flagged for reindex)");
    }
    for warning in &plan.warnings {
        println!("  warning: {warning}");
    }
    // A **proxy**, and the word is deliberate: for a Lucene definition it
    // rests on two byte totals a counting walk can produce without
    // analyzing anything, and `docs/index.md` §5.3 records the basis.
    println!(
        "  work directory {} ({} as a proxy for one definition's spill)",
        plan.work_directory.display(),
        froe::format_byte_size(plan.work_directory_estimate_bytes),
    );
    if !plan.is_empty() {
        println!(
            "  the index records this run replaces stay live through every checkpoint that \
             references them, and are reclaimed only by a later `froe compact`"
        );
    }
}

/// One plan line.
fn render_action(action: &ReindexAction) -> String {
    match action {
        ReindexAction::Rebuild {
            path,
            state,
            entries,
            entry_bytes,
        } => {
            let mut line = format!("rebuild {path} from {}", render_state(state));
            let _ = write!(
                line,
                ": {}, {} to sort",
                count_noun(*entries, "entry", "entries"),
                froe::format_byte_size(*entry_bytes),
            );
            line
        }
        ReindexAction::RebuildLucene {
            path,
            state,
            rules,
            documents,
            stored_bytes,
            indexed_bytes,
            binary_text_policy,
        } => {
            let mut line = format!("rebuild {path} from {}", render_state(state));
            let _ = write!(
                line,
                ": {}, {}, {} stored and {} indexed, binary text {binary_text_policy}",
                count_noun(*rules as u64, "indexing rule", "indexing rules"),
                count_noun(*documents, "document", "documents"),
                froe::format_byte_size(*stored_bytes),
                froe::format_byte_size(*indexed_bytes),
            );
            line
        }
        ReindexAction::Reset {
            path,
            lane,
            hidden_children,
        } => format!(
            "reset {path}: lane {lane} cannot be resolved, so {} removed for Oak's own \
             replay to rebuild",
            if hidden_children.is_empty() {
                "nothing".to_owned()
            } else {
                hidden_children.join(", ")
            }
        ),
        ReindexAction::NothingToDo { path, reason } => {
            format!("nothing to do for {path}: {reason}")
        }
        // `ReindexAction` is `#[non_exhaustive]`: a later froe writes
        // variants this one cannot name, and saying so beats saying nothing.
        other => format!(
            "{}: an action this froe version cannot describe",
            other.path()
        ),
    }
}

/// Which state a rebuild reads.
fn render_state(state: &IndexingState) -> String {
    match state {
        IndexingState::Head => "the head".to_owned(),
        IndexingState::LaneCheckpoint { lane, checkpoint } => {
            format!("lane {lane}'s checkpoint {checkpoint}")
        }
        IndexingState::ResetForReplay { lane } => {
            format!("lane {lane}, which cannot be resolved")
        }
    }
}

/// Prints what the run actually did.
fn print_summary(outcome: &ReindexOutcome) {
    let mut rebuilt = 0u64;
    let mut reset = 0u64;
    let mut nothing = 0u64;
    for (path, report) in &outcome.definitions {
        match report {
            DefinitionReport::Rebuilt {
                entries,
                distinct_keys,
                nodes_written,
            } => {
                rebuilt += 1;
                println!(
                    "  {path}: {}, {}, {}",
                    count_noun(*entries, "entry", "entries"),
                    count_noun(*distinct_keys, "distinct key", "distinct keys"),
                    count_noun(*nodes_written, "index node", "index nodes"),
                );
            }
            DefinitionReport::RebuiltIndex {
                documents,
                nodes_visited,
                files,
                segment_bytes,
            } => {
                rebuilt += 1;
                println!(
                    "  {path}: {}, {} visited, {} in {}",
                    count_noun(*documents, "document", "documents"),
                    count_noun(*nodes_visited, "node", "nodes"),
                    froe::format_byte_size(*segment_bytes),
                    count_noun(files.len() as u64, "index file", "index files"),
                );
            }
            DefinitionReport::Reset {
                removed_hidden_children,
                retained_hidden_children,
            } => {
                reset += 1;
                let mut line = format!(
                    "  {path}: reset, removed {}",
                    removed_hidden_children.join(", ")
                );
                if !retained_hidden_children.is_empty() {
                    let _ = write!(line, ", kept {}", retained_hidden_children.join(", "));
                }
                println!("{line}");
            }
            DefinitionReport::NothingToDo { reason } => {
                nothing += 1;
                println!("  {path}: nothing to do: {reason}");
            }
            other => println!("  {path}: {other:?}"),
        }
    }

    let mut line = format!("reindexed {}", count_noun(rebuilt, "index", "indexes"));
    if reset > 0 {
        let _ = write!(line, ", reset {reset}");
    }
    if nothing > 0 {
        let _ = write!(line, ", {nothing} with nothing to do");
    }
    if outcome.moved_the_head() {
        let _ = write!(
            line,
            "; head {} -> {}",
            outcome.head_before, outcome.head_after
        );
    } else {
        line.push_str("; the head did not move");
    }
    println!("{line}");
}
