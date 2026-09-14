//! `froe index import`: installing index data built out of band.
//!
//! The flow is `froe compact`'s and `froe index reindex`'s, step for step.
//! `--dry-run` plans read-only without the lock and prints what a run would
//! do. Otherwise the command prepares under the lock, prints the plan, asks
//! while still holding the lock, applies, and prints a summary built from
//! the outcome rather than from the plan — so what the operator reads
//! afterwards is what happened, not what was intended.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use froe::writer::index::lucene_import::{
    ImportedIndex, LuceneImportOptions, LuceneImportOutcome, LuceneImportPlan, PlannedImport,
    PreparedLuceneImport, plan_lucene_import,
};

use crate::mutation::{Confirmation, PromptAnswer, confirm, report_cancelled};
use crate::output::count_noun;
use crate::progress::Reporter;

/// The parsed command line, one field per flag.
pub(crate) struct ImportCommandLine {
    pub(crate) input: PathBuf,
    pub(crate) indexes: Vec<String>,
    pub(crate) dry_run: bool,
    pub(crate) assume_yes: bool,
}

impl ImportCommandLine {
    /// The library options this command line asks for.
    fn options(&self) -> LuceneImportOptions {
        LuceneImportOptions::new(self.input.clone()).with_indexes(self.indexes.clone())
    }
}

/// Runs the command. Returns whether it succeeded.
pub(crate) fn run_import(
    repository: &Path,
    command: &ImportCommandLine,
    reporter: &Reporter,
) -> froe::Result<bool> {
    reporter.status(
        "note: the repository must be offline — no Oak instance may be running against it, and \
         the run holds the repository lock from planning through publication",
    );
    let confirmation = Confirmation::from_assume_yes_flag(command.assume_yes);
    let options = command.options();

    if command.dry_run {
        let preview = plan_lucene_import(repository, &options)?;
        // The plan is the operator's evidence: end every report before a
        // single line of it is written.
        reporter.finish();
        print_plan(&preview);
        println!("dry-run: repository was not modified");
        return Ok(true);
    }

    let prepared =
        PreparedLuceneImport::prepare_with_progress(repository, &options, &mut reporter.clone())?;
    reporter.finish();
    print_plan(prepared.plan());
    if prepared.plan().is_empty() {
        println!("nothing to do");
        return Ok(true);
    }

    let answer = confirm(
        &format!(
            "about to import {} into {}",
            count_noun(prepared.plan().imports.len() as u64, "index", "indexes"),
            crate::output::sanitize_terminal_path(&prepared.plan().directory)
        ),
        confirmation,
        reporter,
    );
    if answer != PromptAnswer::Yes {
        report_cancelled("import", answer);
        return Ok(false);
    }

    let outcome = prepared.apply_with_progress(&mut reporter.clone())?;
    reporter.finish();
    print_summary(&outcome);
    Ok(true)
}

/// Prints the plan: what each definition will receive, and what the import
/// changes about it.
fn print_plan(plan: &LuceneImportPlan) {
    println!("import plan for {}", plan.directory.display());
    println!("  from {}", plan.input.display());
    println!(
        "  the index data reflects checkpoint {}, which is the state every listed lane resumes \
         from",
        plan.checkpoint
    );
    for import in &plan.imports {
        print_planned_import(import);
    }
    if plan.imports.is_empty() {
        println!("  (the input directory holds no index this store can receive)");
    }
    for path in &plan.definitions_without_directories {
        println!(
            "  note: {path} is described in index-definitions.json but has no index directory; \
             it is ignored, as oak-run's importer ignores it"
        );
    }
    if !plan.is_empty() {
        println!(
            "  no checkpoint is released, and the index records this run replaces stay live \
             through every checkpoint that references them — reclaimed only by a later \
             `froe compact`"
        );
    }
}

/// One definition's plan lines.
fn print_planned_import(import: &PlannedImport) {
    let mut line = format!("  {}", import.path);
    let _ = write!(
        line,
        ": {}, {} from {}",
        count_noun(import.file_count as u64, "file", "files"),
        froe::format_byte_size(import.byte_count),
        import.directory.display(),
    );
    println!("{line}");
    for (jcr_name, source) in &import.mappings {
        println!("    {jcr_name} <- {}", source.display());
    }
    for (jcr_name, reason) in &import.skipped_mappings {
        println!("    {jcr_name}: skipped, {reason}");
    }
    println!(
        "    reindexCount will be set to {}, reindex cleared, corrupt and indexImportState \
         removed when present",
        import.reindex_count
    );
    println!("    :suggest-data will be absent until Oak's next suggester cycle rebuilds it");
}

/// Prints what the run actually did.
fn print_summary(outcome: &LuceneImportOutcome) {
    for index in &outcome.indexes {
        print_imported_index(index);
    }
    let mut line = format!(
        "imported {} at checkpoint {}",
        count_noun(outcome.indexes.len() as u64, "index", "indexes"),
        outcome.checkpoint,
    );
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

/// One definition's summary lines.
fn print_imported_index(index: &ImportedIndex) {
    let bytes: u64 = index.files.iter().map(|(_, length)| *length).sum();
    println!(
        "  {}: {}, {}, reindexCount {}, uid {}",
        index.path,
        count_noun(index.files.len() as u64, "file", "files"),
        froe::format_byte_size(bytes),
        index.reindex_count,
        index.unique_identifier,
    );
    if !index.dropped_hidden_children.is_empty() {
        println!(
            "    dropped {}, as Oak's own definition updater drops them",
            index.dropped_hidden_children.join(", ")
        );
    }
    for (jcr_name, reason) in &index.skipped_mappings {
        println!("    {jcr_name}: skipped, {reason}");
    }
}
