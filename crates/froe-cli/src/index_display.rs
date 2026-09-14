//! `froe index`: listing, dumping and checking Oak's indexes, read-only.
//!
//! Every function here opens the store the way `froe summary` does — no
//! lock, no manifest write, no file created — and renders under the stream
//! contract of `docs/cli-output.md`: data on standard output, progress and
//! warnings on standard error.
//!
//! The exit codes `check` produces are the contract oak-run's
//! `--index-consistency-check` never had, and a runbook needs. They live in
//! [`CheckOutcome`] rather than being computed at the call site, because the
//! rule that decides between 0 and 4 is easy to get backwards: a definition
//! whose *type* has no applicable check — the counter, the disabled, the
//! Elasticsearch and the unknown — must not reach 4, or no unnarrowed run
//! could ever exit 0, every store carrying a counter. Only an index that
//! *has* a check which could not be *run* does.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use froe::index::counter::{CountBound, NodeCountEstimate, estimated_node_count};
use froe::index::definitions_json::{ChildFilter, RenderOptions, render};
use froe::index::inventory::{IndexInfo, IndexInventory};
use froe::index::lanes::{AsyncLanes, CheckState, checked_state_root};
use froe::index::property::consistency::{
    EntryCheckBudget, NodeCheckBudget, PropertyIndexReport, check as check_property_index,
};
use froe::index::{IndexType, IndexWarning, lucene};
use froe::progress::ProgressObserver as _;
use froe::store::Repository;

use crate::output::{sanitize_terminal_text, write_diagnostic_handling_observed_broken_pipe};
use crate::progress::{Reporter, format_count};

/// How many faults of one kind a verdict names before it summarizes the
/// rest. A wholly stale index would otherwise print a line per entry.
const FAULT_REPORT_LIMIT: usize = 10;

/// The slack a derived node budget carries over the counter's own estimate.
///
/// The counter is a *sampling* estimator: it records a path only once per
/// `resolution` insertions, so its number runs low on a freshly-written
/// subtree and drifts with deletions. Nothing in this
/// repository measures how far off it runs on a real store, and a factor
/// stated as though it had been measured would be worse than none — so this
/// is deliberately generous rather than tight. Its job is to stop a walk
/// that has clearly gone wrong (a path filter that matched the whole store
/// when it was meant to match a branch), not to be a performance budget.
const NODE_BUDGET_FACTOR: u64 = 8;

/// The floor a derived node budget never goes below, so a small or
/// newly-created index is never refused for being small.
const NODE_BUDGET_FLOOR: u64 = 100_000;

/// The progress step the check opens. The inventory opens `inventorying
/// indexes` itself, so this covers only the checking that follows.
const CHECK_STEP: &str = "checking indexes";

/// How many index entries a check examines before refusing. Entries are
/// cheap — one node each — and the number only has to bound a walk that has
/// gone wrong.
const ENTRY_BUDGET: u64 = 50_000_000;

/// What `froe index check` concluded, and the exit code it maps to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CheckOutcome {
    /// Every index with an applicable check ran and was consistent.
    Consistent,
    /// At least one index is inconsistent.
    Inconsistent,
    /// None is inconsistent, but at least one index that has an applicable
    /// check could not be run.
    NotRun,
}

impl CheckOutcome {
    /// The process exit code. 3 and 4 rather than 1 or 2, because the binary
    /// already returns 1 for a runtime failure and leaves 2 to the argument
    /// parser: an inconsistent index must not look like a store that would
    /// not open.
    pub(crate) fn exit_code(self) -> u8 {
        match self {
            CheckOutcome::Consistent => 0,
            CheckOutcome::Inconsistent => 3,
            CheckOutcome::NotRun => 4,
        }
    }
}

/// Whether a subcommand reproduces Oak's refusal when the index path service
/// cannot enumerate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PathService {
    /// `definitions` and `check`: Oak's own printer and checker walk
    /// `IndexPathService.getIndexPaths()`, which throws when
    /// `/oak:index/nodetype` is absent or does not read as a `property`
    /// index. Falling back silently to the root-level definitions would make
    /// froe's output differ from Oak's on exactly the store where an
    /// operator most needs them to agree.
    Required,
    /// `list`: a listing is froe's own, not a reproduction of an Oak
    /// printer, and an operator whose nodetype index is broken is precisely
    /// the one who needs to see what definitions the store holds. It reports
    /// what it could enumerate and warns about what it could not.
    Optional,
}

/// Collects the inventory, narrowed to `requested` when it is not empty.
///
/// A requested path naming no definition is a typed refusal naming the path,
/// for all three subcommands: silently listing nothing would read as an
/// index that exists and is empty.
fn narrowed_inventory(
    repository: &Repository,
    requested: &[String],
    path_service: PathService,
    reporter: &Reporter,
) -> froe::Result<IndexInventory> {
    let super_root = repository.head();

    // A caller who named the paths never consults the path service, so its
    // nodetype precondition is never evaluated — which is how oak-run behaves
    // when it is given `--index-paths`.
    if !requested.is_empty() {
        let inventory = IndexInventory::collect_selected(
            repository,
            &super_root,
            requested,
            &mut reporter.clone(),
        )
        .map_err(index_error_to_store_error)?;
        for path in requested {
            let known = inventory
                .indexes
                .iter()
                .any(|index| &index.path == path && index.model_error.is_none());
            if !known {
                return Err(froe::Error::InvalidFormat {
                    details: format!("no index definition at {}", sanitize_terminal_text(path)),
                });
            }
        }
        return Ok(inventory);
    }

    let inventory =
        IndexInventory::collect_with_progress(repository, &super_root, &mut reporter.clone())
            .map_err(index_error_to_store_error)?;
    if path_service == PathService::Required
        && let Some(warning) = inventory.warnings.iter().find(|warning| {
            matches!(
                warning,
                IndexWarning::NonRootDefinitionsNotEnumerated { .. }
            )
        })
    {
        return Err(froe::Error::InvalidFormat {
            details: format!(
                "{}; Oak's index path service refuses this store, so this command does too — \
                 name the definitions with --index to bypass it, as oak-run's --index-paths does",
                sanitize_terminal_text(&warning.to_string())
            ),
        });
    }
    Ok(inventory)
}

fn index_error_to_store_error(error: froe::index::IndexError) -> froe::Error {
    match error {
        froe::index::IndexError::Record(source) => source,
        other => froe::Error::InvalidFormat {
            details: other.to_string(),
        },
    }
}

/// `froe index list`.
pub(crate) fn print_index_list(
    repository: &Repository,
    requested: &[String],
    reporter: &Reporter,
) -> froe::Result<()> {
    let inventory = narrowed_inventory(repository, requested, PathService::Optional, reporter)?;
    let mut rendered = String::new();
    for index in &inventory.indexes {
        rendered.push_str(&render_index_row(index));
    }
    let stdout = std::io::stdout();
    let mut locked = stdout.lock();
    write_diagnostic_handling_observed_broken_pipe(&mut locked, |output| {
        output.write_all(rendered.as_bytes())?;
        Ok(())
    })?;

    let mut report = String::new();
    for index in &inventory.indexes {
        for warning in &index.warnings {
            let _ = writeln!(
                report,
                "  {} {}",
                sanitize_terminal_text(&index.path),
                sanitize_terminal_text(&warning.to_string())
            );
        }
        if let Some(error) = &index.model_error {
            let _ = writeln!(
                report,
                "  {} could not be read: {}",
                sanitize_terminal_text(&index.path),
                sanitize_terminal_text(error)
            );
        }
    }
    for warning in &inventory.warnings {
        let _ = writeln!(report, "  {}", sanitize_terminal_text(&warning.to_string()));
    }
    if !report.is_empty() {
        eprint!("warnings:\n{report}");
    }
    Ok(())
}

/// One index's line. Deliberately one line per index rather than a wide
/// table: a definition path is long, and a table that wraps is worse to read
/// than a list.
///
/// Every field is written through [`field`], so the name column is one width
/// and a value is always separated from its name by at least two spaces.
/// That is not only for the eye: the interop phase parses this output to
/// compare it against Oak's own index printer, and a field whose value ran
/// into its name would be parsed as a name.
fn render_index_row(index: &IndexInfo) -> String {
    let mut row = format!("{}\n", sanitize_terminal_text(&index.path));
    field(
        &mut row,
        "type",
        &index.index_type().map_or_else(
            || "unreadable".to_owned(),
            |kind| kind.stored_name().to_owned(),
        ),
    );
    let definition = index.definition.as_ref();
    if let Some(lane) = definition.and_then(|definition| definition.lane.as_deref()) {
        field(&mut row, "lane", &sanitize_terminal_text(lane));
    }
    if let Some(checkpoint) = &index.lane_checkpoint {
        field(
            &mut row,
            "lane checkpoint",
            &format!(
                "{}{}",
                sanitize_terminal_text(checkpoint),
                if index.lane_checkpoint_dangling {
                    "  (dangling)"
                } else {
                    ""
                }
            ),
        );
    }
    if let Some(indexed_up_to) = &index.indexed_up_to {
        field(
            &mut row,
            "indexed up to",
            &sanitize_terminal_text(indexed_up_to),
        );
    }
    if let Some(definition) = definition {
        field(
            &mut row,
            "reindex",
            &format!(
                "{} (count {})",
                definition.reindex.flagged, definition.reindex.count
            ),
        );
    }
    render_sizes_and_estimates(&mut row, index);
    field(
        &mut row,
        "hidden mount",
        &index.hidden_children.has_mount.to_string(),
    );
    field(
        &mut row,
        "property index",
        &index.hidden_children.has_property_index.to_string(),
    );
    field(
        &mut row,
        "definition drift",
        &index.definition_changed.to_string(),
    );
    if index.definition_changed {
        let _ = writeln!(
            row,
            "  {:<FIELD_NAME_WIDTH$}{} paths differ from the stored clone",
            "",
            format_count(index.definition_diff.len() as u64)
        );
    }
    row
}

/// The measured half of a row: what the storage weighs and what the type's
/// own information provider estimates about it.
///
/// Each is printed only when there is one, because an absent value is not a
/// zero — no `:suggest-data` child is not a suggester of size zero, and a
/// type with no information provider has no estimate rather than an estimate
/// of nothing.
fn render_sizes_and_estimates(row: &mut String, index: &IndexInfo) {
    if let Some(size) = index.size_in_bytes {
        field(
            row,
            "size",
            &format!("{} ({size})", froe::format_byte_size(size)),
        );
    }
    if let Some(size) = index.suggest_size_in_bytes {
        field(
            row,
            "suggest size",
            &format!("{} ({size})", froe::format_byte_size(size)),
        );
    }
    if let Some(entries) = index.estimated_entry_count {
        field(row, "estimated entries", &format_count(entries));
    }
    if let Some(estimate) = index.estimated_node_count {
        field(row, "estimated nodes", &render_node_estimate(estimate));
    }
    if index.approximate_counters > 0 {
        field(
            row,
            "counters",
            &format_count(index.approximate_counters as u64),
        );
    }
    if !index.lucene_files.is_empty() {
        field(
            row,
            "lucene files",
            &format_count(index.lucene_files.len() as u64),
        );
    }
}

/// The width the field-name column is padded to, chosen so the longest name
/// still leaves two spaces before its value.
const FIELD_NAME_WIDTH: usize = 19;

fn field(row: &mut String, name: &str, value: &str) {
    let _ = writeln!(row, "  {name:<FIELD_NAME_WIDTH$}{value}");
}

fn render_node_estimate(estimate: NodeCountEstimate) -> String {
    match estimate {
        NodeCountEstimate::Unknown => "unknown (no counter index)".to_owned(),
        NodeCountEstimate::Fallback => "unknown (the counter never sampled this path)".to_owned(),
        NodeCountEstimate::Count(count) => format_count(count),
    }
}

/// `froe index definitions`.
pub(crate) fn print_index_definitions(
    repository: &Repository,
    repository_path: &Path,
    output_path: Option<&Path>,
    requested: &[String],
    reporter: &Reporter,
) -> froe::Result<()> {
    let inventory = narrowed_inventory(repository, requested, PathService::Required, reporter)?;
    let mut definitions = Vec::with_capacity(inventory.indexes.len());
    for index in &inventory.indexes {
        let Some(node) = repository.node_at_path(&index.path)? else {
            continue;
        };
        definitions.push((index.path.clone(), node));
    }
    let rendered = render(
        repository,
        &definitions,
        RenderOptions {
            child_filter: ChildFilter::Printer,
            ..RenderOptions::default()
        },
    )
    .map_err(index_error_to_store_error)?;

    // Never an existing file, and never a file inside the store: a
    // definitions file is an artifact an operator keeps and re-imports,
    // which is why this refuses where `froe digest --output` truncates.
    if let Some(path) = output_path {
        let mut file = froe_export::output_file::create_export_output(repository_path, path)?;
        file.write_all(rendered.as_bytes())?;
        file.flush()?;
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut locked = stdout.lock();
    write_diagnostic_handling_observed_broken_pipe(&mut locked, |output| {
        output.write_all(rendered.as_bytes())?;
        // A terminal wants a final newline where Oak's printer ends at the
        // closing brace. `--output` writes the printer's bytes exactly, so
        // the file a later import reads is byte-identical to Oak's.
        output.write_all(b"\n")?;
        Ok(())
    })
}

/// What one definition's check concluded, apart from what it printed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// The check ran and the index agreed with the state it indexes.
    Consistent,
    /// The check ran and found faults.
    Inconsistent,
    /// The index has an applicable check that could not be run.
    NotRun,
    /// The index's *type* has no applicable check. Deliberately its own
    /// verdict rather than `NotRun`: see [`CheckOutcome`].
    NotApplicable,
}

/// `froe index check`.
pub(crate) fn check_indexes(
    repository: &Repository,
    requested: &[String],
    reporter: &Reporter,
) -> froe::Result<CheckOutcome> {
    let inventory = narrowed_inventory(repository, requested, PathService::Required, reporter)?;
    let super_root = repository.head();
    let content_root = repository.content_root()?;
    let lanes = AsyncLanes::read(&content_root).map_err(index_error_to_store_error)?;

    let mut any_inconsistent = false;
    let mut any_not_run = false;
    let mut rendered = String::new();
    // The step is opened here, where the checking happens; the inventory
    // opens its own. A function that reports owns its step, so a command
    // never wraps one around a call that opens one of its own.
    let mut observer = reporter.clone();
    observer.step_began(
        &froe::Step::new(CHECK_STEP, froe::WorkUnit::Nodes)
            .with_total(inventory.indexes.len() as u64),
    );
    for (position, index) in inventory.indexes.iter().enumerate() {
        observer.step_advanced(position as u64);
        let (verdict, text) = check_one(repository, &super_root, &content_root, &lanes, index)?;
        match verdict {
            Verdict::Inconsistent => any_inconsistent = true,
            Verdict::NotRun => any_not_run = true,
            Verdict::Consistent | Verdict::NotApplicable => {}
        }
        rendered.push_str(&text);
    }
    observer.step_advanced(inventory.indexes.len() as u64);
    observer.step_ended();

    let stdout = std::io::stdout();
    let mut locked = stdout.lock();
    write_diagnostic_handling_observed_broken_pipe(&mut locked, |output| {
        output.write_all(rendered.as_bytes())?;
        Ok(())
    })?;

    Ok(if any_inconsistent {
        CheckOutcome::Inconsistent
    } else if any_not_run {
        CheckOutcome::NotRun
    } else {
        CheckOutcome::Consistent
    })
}

/// One definition's verdict and the lines that say why.
fn check_one(
    repository: &Repository,
    super_root: &froe::content::node::NodeState<'_>,
    content_root: &froe::content::node::NodeState<'_>,
    lanes: &AsyncLanes,
    index: &IndexInfo,
) -> froe::Result<(Verdict, String)> {
    let path = sanitize_terminal_text(&index.path);
    let Some(definition) = index.definition.as_ref() else {
        return Ok((
            Verdict::NotRun,
            format!(
                "{path}: not checked — the definition could not be read: {}\n",
                index
                    .model_error
                    .as_deref()
                    .map_or_else(String::new, sanitize_terminal_text)
            ),
        ));
    };
    let Some(kind) = definition.index_type.as_ref() else {
        return Ok((
            Verdict::NotApplicable,
            format!("{path}: no applicable check — Oak's indexer skips this definition\n"),
        ));
    };
    match kind {
        IndexType::Property | IndexType::Reference => check_property_family(
            repository,
            super_root,
            content_root,
            lanes,
            index,
            definition,
        ),
        IndexType::Lucene => check_lucene(repository, index),
        // Reported, and deliberately *not* `NotRun`: these have no
        // applicable check at all, and every store carries a counter.
        IndexType::Counter
        | IndexType::Disabled { .. }
        | IndexType::Elasticsearch
        | IndexType::Ordered
        | IndexType::Unknown(_) => Ok((
            Verdict::NotApplicable,
            format!(
                "{path}: no applicable check for type {}\n",
                kind.stored_name()
            ),
        )),
    }
}

fn check_property_family(
    repository: &Repository,
    super_root: &froe::content::node::NodeState<'_>,
    content_root: &froe::content::node::NodeState<'_>,
    lanes: &AsyncLanes,
    index: &IndexInfo,
    definition: &froe::index::IndexDefinition,
) -> froe::Result<(Verdict, String)> {
    let path = sanitize_terminal_text(&index.path);
    let state =
        checked_state_root(super_root, lanes, definition).map_err(index_error_to_store_error)?;
    if let CheckState::Uncheckable { reason } = &state {
        return Ok((
            Verdict::NotRun,
            format!("{path}: not checked — {}\n", sanitize_terminal_text(reason)),
        ));
    }
    let Some(state_root) = state.root() else {
        unreachable!("only the uncheckable variant has no root")
    };
    let Some(definition_node) = repository.node_at_path(&index.path)? else {
        return Ok((Verdict::NotRun, vanished(&path)));
    };
    let budget = derived_node_budget(content_root, definition)?;
    match check_property_index(
        &definition_node,
        definition,
        state_root,
        EntryCheckBudget::of_entries(ENTRY_BUDGET),
        budget,
    ) {
        Ok(report) => {
            // `has_definite_faults`, not `is_consistent`: a node the
            // definition covers that no entry names may be an entry the
            // index lost or a node Oak never indexed, and the check cannot
            // tell which. The verdict line reports the count either way.
            let verdict = if report.has_definite_faults() {
                Verdict::Inconsistent
            } else {
                Verdict::Consistent
            };
            Ok((verdict, render_property_verdict(&path, &state, &report)))
        }
        // A budget refusal is `NotRun` rather than a failure: the index may
        // be perfectly correct, and the remedy is to narrow `--index` past
        // it rather than to reindex.
        Err(error) => Ok((
            Verdict::NotRun,
            format!(
                "{path}: not checked — {}\n",
                sanitize_terminal_text(&error.to_string())
            ),
        )),
    }
}

/// The Lucene level-1 pass, which is deliberately independent of the lane:
/// it reads the definition subtree at the head and needs no lane state, so a
/// dangling lane never makes a Lucene definition uncheckable.
fn check_lucene(repository: &Repository, index: &IndexInfo) -> froe::Result<(Verdict, String)> {
    let path = sanitize_terminal_text(&index.path);
    let Some(definition_node) = repository.node_at_path(&index.path)? else {
        return Ok((Verdict::NotRun, vanished(&path)));
    };
    let report = lucene::check::check_blobs(repository, &definition_node)
        .map_err(index_error_to_store_error)?;
    let verdict = if report.is_consistent() {
        Verdict::Consistent
    } else {
        Verdict::Inconsistent
    };
    Ok((verdict, render_lucene_verdict(&path, &report)))
}

fn vanished(path: &str) -> String {
    format!(
        "{path}: not checked — the definition node vanished between the listing and the check\n"
    )
}

/// The node budget for one definition, derived from the counter's estimates
/// over its include set rather than taken from a flag.
///
/// Two cases take the unbudgeted form instead of a limit, because the
/// estimate is not a count: the store has no counter index at all
/// (`Unknown`), and an include path the sampling counter never recorded
/// (`Fallback`). A limit invented for either would refuse a healthy store at
/// a number nothing justifies.
fn derived_node_budget(
    content_root: &froe::content::node::NodeState<'_>,
    definition: &froe::index::IndexDefinition,
) -> froe::Result<NodeCheckBudget> {
    // The reference index's editor is built with no path filter, so its
    // `includedPaths` bound nothing: the walk covers the store.
    let bounded_by_the_filter = definition.index_type.as_ref() != Some(&IndexType::Reference)
        && !definition.path_filter.include_paths().is_empty();
    let include_paths: Vec<&str> = if bounded_by_the_filter {
        definition
            .path_filter
            .include_paths()
            .iter()
            .map(String::as_str)
            .collect()
    } else {
        vec!["/"]
    };

    let mut total: u64 = 0;
    for path in include_paths {
        let estimate = estimated_node_count(content_root, path, CountBound::Maximum)
            .map_err(index_error_to_store_error)?;
        match estimate {
            NodeCountEstimate::Unknown | NodeCountEstimate::Fallback => {
                return Ok(NodeCheckBudget::unlimited());
            }
            NodeCountEstimate::Count(count) => total = total.saturating_add(count),
        }
    }
    Ok(NodeCheckBudget::of_nodes(
        total
            .saturating_mul(NODE_BUDGET_FACTOR)
            .max(NODE_BUDGET_FLOOR),
    ))
}

fn render_property_verdict(
    path: &str,
    state: &CheckState<'_>,
    report: &PropertyIndexReport,
) -> String {
    let against = match state {
        CheckState::Head(_) => "the head".to_owned(),
        CheckState::LaneCheckpoint {
            lane, checkpoint, ..
        } => format!(
            "lane {} at checkpoint {}",
            sanitize_terminal_text(lane),
            sanitize_terminal_text(checkpoint)
        ),
        CheckState::Uncheckable { .. } => unreachable!("an uncheckable state has no report"),
    };
    let scale = format!(
        "{} entries, {} nodes",
        format_count(report.entries_checked),
        report
            .nodes_visited
            .map_or_else(|| "no".to_owned(), format_count)
    );
    let mut verdict = if report.has_definite_faults() {
        format!("{path}: INCONSISTENT against {against} ({scale})\n")
    } else {
        format!("{path}: consistent against {against} ({scale})\n")
    };
    // Reported whatever the verdict, and never part of it: see
    // `PropertyIndexReport::has_definite_faults`.
    if !report.missing_entries.is_empty() {
        let _ = writeln!(
            verdict,
            "  {} covered nodes are named by no entry — this may be entries the index lost, \
             or nodes Oak never indexed; froe cannot tell which",
            format_count(report.missing_entries.len() as u64)
        );
    }
    if !report.has_definite_faults() {
        for missing in report.missing_entries.iter().take(FAULT_REPORT_LIMIT) {
            let _ = writeln!(
                verdict,
                "    unindexed   {} -> {}",
                sanitize_terminal_text(&missing.key),
                sanitize_terminal_text(&missing.path)
            );
        }
        return verdict;
    }
    for (label, faults) in [
        ("stale", report.stale_entries.len()),
        ("mismatched", report.mismatched_entries.len()),
        ("duplicate", report.duplicate_entries.len()),
    ] {
        if faults > 0 {
            let _ = writeln!(verdict, "  {label} {}", format_count(faults as u64));
        }
    }
    for fault in report.stale_entries.iter().take(FAULT_REPORT_LIMIT) {
        let _ = writeln!(
            verdict,
            "  stale       {} -> {}",
            sanitize_terminal_text(&fault.key),
            sanitize_terminal_text(&fault.path)
        );
    }
    for fault in report.mismatched_entries.iter().take(FAULT_REPORT_LIMIT) {
        let _ = writeln!(
            verdict,
            "  mismatched  {} -> {}",
            sanitize_terminal_text(&fault.key),
            sanitize_terminal_text(&fault.path)
        );
    }
    for missing in report.missing_entries.iter().take(FAULT_REPORT_LIMIT) {
        let _ = writeln!(
            verdict,
            "  unindexed   {} -> {}",
            sanitize_terminal_text(&missing.key),
            sanitize_terminal_text(&missing.path)
        );
    }
    for duplicate in report.duplicate_entries.iter().take(FAULT_REPORT_LIMIT) {
        let _ = writeln!(
            verdict,
            "  duplicate   {} -> {}",
            sanitize_terminal_text(&duplicate.key),
            duplicate
                .paths
                .iter()
                .map(|path| sanitize_terminal_text(path))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    verdict
}

fn render_lucene_verdict(path: &str, report: &lucene::LuceneBlobReport) -> String {
    if report.type_mismatch {
        return format!(
            "{path}: INCONSISTENT — its type does not read as \"lucene\", so the Lucene \
             checker refuses it\n"
        );
    }
    if report.is_consistent() {
        return format!(
            "{path}: consistent at level 1 ({} blobs, {})\n",
            format_count(report.blobs_checked),
            froe::format_byte_size(report.bytes_read)
        );
    }
    let mut verdict = format!("{path}: INCONSISTENT at level 1\n");
    for (label, faults) in [
        ("missing blobs", &report.missing_blobs),
        ("invalid blobs", &report.invalid_blobs),
    ] {
        if faults.is_empty() {
            continue;
        }
        let _ = writeln!(verdict, "  {label} {}", format_count(faults.len() as u64));
        for fault in faults.iter().take(FAULT_REPORT_LIMIT) {
            let _ = writeln!(
                verdict,
                "    {}/{}[{}] declared {} streamed {}{}",
                sanitize_terminal_text(&fault.node_path),
                sanitize_terminal_text(&fault.property_name),
                fault.value_index,
                fault.declared_length,
                fault
                    .streamed_length
                    .map_or_else(|| "nothing".to_owned(), |length| length.to_string()),
                fault.reason.as_deref().map_or_else(String::new, |reason| {
                    format!(": {}", sanitize_terminal_text(reason))
                })
            );
        }
    }
    verdict
}
