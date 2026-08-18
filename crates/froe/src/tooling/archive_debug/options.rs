//! What a diagnostic run is allowed to cost: the ceilings on work,
//! results, and rendered text that keep one archive's attribution
//! bounded whatever the store holds.

use super::{ArchiveDebugError, ArchiveDebugResult, ArchivePathReference};

/// Default maximum number of path-attribution rows retained in one report.
///
/// Two hundred and fifty thousand rows leave room for a large production
/// tree while keeping the enum/vector portion of one diagnostic result in
/// the tens of MiB instead of allowing a hostile billion-node tree to grow
/// until the process aborts.
pub const DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES: usize = 250_000;

/// Default maximum UTF-8 bytes cloned into path-attribution rows.
///
/// This counts paths, property names, and rendered values. Fixed-size record
/// identifiers and enum discriminants are separately bounded by
/// [`DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES`].
pub const DEFAULT_MAXIMUM_ARCHIVE_REFERENCE_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum number of logical record-graph operations in one archive
/// diagnostic.
///
/// A unit is charged for each traversal step, node/template/property lookup,
/// list element, template-name/list lookup, long-value block identifier,
/// stored name byte, graph-data byte, and graph row/edge totalization
/// operation. Allocation-heavy
/// operations preflight their complete charge before materializing. The limit
/// therefore remains deterministic across repository cache state and bounds
/// compact hostile records that repeatedly point at the same list.
pub const DEFAULT_MAXIMUM_ARCHIVE_WORK_UNITS: u64 = 100_000_000;

/// Default maximum child entries one traversal step may materialize.
pub const DEFAULT_MAXIMUM_ARCHIVE_SCHEDULED_CHILDREN_PER_NODE: u64 = 250_000;

/// Default cumulative stored bytes of names materialized while expanding and
/// interpreting one node.
pub const DEFAULT_MAXIMUM_ARCHIVE_NAME_BYTES_PER_NODE: u64 = 16 * 1024 * 1024;

/// Default maximum number of child visits retained on the traversal stack.
pub const DEFAULT_MAXIMUM_ARCHIVE_PENDING_NODES: u64 = 250_000;

/// Default maximum number of rows retained in an archive graph.
pub const DEFAULT_MAXIMUM_ARCHIVE_GRAPH_ROWS: usize = 250_000;

/// Default maximum number of edges parsed and retained in an archive graph.
pub const DEFAULT_MAXIMUM_ARCHIVE_GRAPH_EDGES: usize = 1_000_000;

/// Resource limits for [`debug_archive_with_options`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct ArchiveDebugOptions {
    /// Maximum node, template, and property attribution rows retained.
    pub maximum_path_references: usize,
    /// Maximum UTF-8 bytes cloned into retained paths, names, and values.
    pub maximum_reference_text_bytes: usize,
    /// Maximum logical record-graph operations performed by the diagnostic.
    pub maximum_work_units: u64,
    /// Maximum child entries materialized while expanding one node.
    pub maximum_scheduled_children_per_node: u64,
    /// Maximum cumulative stored bytes of child and template names
    /// materialized while processing one node.
    pub maximum_name_bytes_per_node: u64,
    /// Maximum child visits retained on the traversal stack at one time.
    pub maximum_pending_nodes: u64,
    /// Maximum rows parsed or retained in the archive graph.
    pub maximum_graph_rows: usize,
    /// Maximum edges parsed or retained in the archive graph.
    pub maximum_graph_edges: usize,
}

impl Default for ArchiveDebugOptions {
    fn default() -> Self {
        Self {
            maximum_path_references: DEFAULT_MAXIMUM_ARCHIVE_PATH_REFERENCES,
            maximum_reference_text_bytes: DEFAULT_MAXIMUM_ARCHIVE_REFERENCE_TEXT_BYTES,
            maximum_work_units: DEFAULT_MAXIMUM_ARCHIVE_WORK_UNITS,
            maximum_scheduled_children_per_node:
                DEFAULT_MAXIMUM_ARCHIVE_SCHEDULED_CHILDREN_PER_NODE,
            maximum_name_bytes_per_node: DEFAULT_MAXIMUM_ARCHIVE_NAME_BYTES_PER_NODE,
            maximum_pending_nodes: DEFAULT_MAXIMUM_ARCHIVE_PENDING_NODES,
            maximum_graph_rows: DEFAULT_MAXIMUM_ARCHIVE_GRAPH_ROWS,
            maximum_graph_edges: DEFAULT_MAXIMUM_ARCHIVE_GRAPH_EDGES,
        }
    }
}

pub(crate) struct WorkBudget {
    pub(crate) maximum: u64,
    pub(crate) consumed: u64,
}

impl WorkBudget {
    pub(crate) const fn new(maximum: u64) -> Self {
        Self {
            maximum,
            consumed: 0,
        }
    }

    pub(crate) fn charge_one(&mut self) -> ArchiveDebugResult<()> {
        self.charge_amount(1)
    }

    pub(crate) const fn remaining(&self) -> u64 {
        self.maximum.saturating_sub(self.consumed)
    }

    pub(crate) fn exceeded_by(&self, units: u64) -> ArchiveDebugError {
        ArchiveDebugError::WorkBudgetExceeded {
            maximum_work_units: self.maximum,
            attempted_work_units: self.consumed.saturating_add(units),
        }
    }

    pub(crate) fn charge_amount(&mut self, units: u64) -> ArchiveDebugResult<()> {
        let attempted = self.consumed.saturating_add(units);
        if attempted > self.maximum {
            return Err(self.exceeded_by(units));
        }
        self.consumed = attempted;
        Ok(())
    }

    pub(crate) fn charge_many(&mut self, units: usize) -> ArchiveDebugResult<()> {
        self.charge_amount(u64::try_from(units).unwrap_or(u64::MAX))
    }
}

pub(crate) struct ResultBudget {
    pub(crate) options: ArchiveDebugOptions,
    pub(crate) retained_path_references: usize,
    pub(crate) retained_reference_text_bytes: usize,
}

impl ResultBudget {
    pub(crate) fn new(options: ArchiveDebugOptions) -> Self {
        Self {
            options,
            retained_path_references: 0,
            retained_reference_text_bytes: 0,
        }
    }

    pub(crate) fn retain(&mut self, reference: &ArchivePathReference) -> ArchiveDebugResult<()> {
        let attempted_path_references = self.retained_path_references.saturating_add(1);
        let attempted_reference_text_bytes = self
            .retained_reference_text_bytes
            .saturating_add(reference.retained_text_bytes());
        if attempted_path_references > self.options.maximum_path_references
            || attempted_reference_text_bytes > self.options.maximum_reference_text_bytes
        {
            return Err(ArchiveDebugError::ResultBudgetExceeded {
                maximum_path_references: self.options.maximum_path_references,
                maximum_reference_text_bytes: self.options.maximum_reference_text_bytes,
                attempted_path_references,
                attempted_reference_text_bytes,
            });
        }
        self.retained_path_references = attempted_path_references;
        self.retained_reference_text_bytes = attempted_reference_text_bytes;
        Ok(())
    }

    pub(crate) fn candidate_display_budget(
        &self,
        base_text_bytes: usize,
    ) -> ArchiveDebugResult<DisplayBudget> {
        let budget = DisplayBudget {
            // Uniqueness is known only after the complete Oak line is
            // rendered. Candidate construction is bounded by the configured
            // text cap; aggregate row/text reservation happens after dedup.
            maximum_path_references: self.options.maximum_path_references,
            maximum_reference_text_bytes: self.options.maximum_reference_text_bytes,
            attempted_path_references: 1,
            base_reference_text_bytes: base_text_bytes,
        };
        budget.check_display_bytes(0)?;
        Ok(budget)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DisplayBudget {
    pub(crate) maximum_path_references: usize,
    pub(crate) maximum_reference_text_bytes: usize,
    pub(crate) attempted_path_references: usize,
    pub(crate) base_reference_text_bytes: usize,
}

impl DisplayBudget {
    pub(crate) fn check_display_bytes(self, display_bytes: usize) -> ArchiveDebugResult<()> {
        let attempted_reference_text_bytes =
            self.base_reference_text_bytes.saturating_add(display_bytes);
        if self.attempted_path_references > self.maximum_path_references
            || attempted_reference_text_bytes > self.maximum_reference_text_bytes
        {
            return Err(ArchiveDebugError::ResultBudgetExceeded {
                maximum_path_references: self.maximum_path_references,
                maximum_reference_text_bytes: self.maximum_reference_text_bytes,
                attempted_path_references: self.attempted_path_references,
                attempted_reference_text_bytes,
            });
        }
        Ok(())
    }

    pub(crate) fn builder(self) -> BoundedDisplay {
        BoundedDisplay {
            text: String::new(),
            budget: self,
        }
    }
}

pub(crate) struct BoundedDisplay {
    pub(crate) text: String,
    pub(crate) budget: DisplayBudget,
}

impl BoundedDisplay {
    pub(crate) fn push_str(&mut self, text: &str) -> ArchiveDebugResult<()> {
        let attempted = self.text.len().saturating_add(text.len());
        self.budget.check_display_bytes(attempted)?;
        self.text.push_str(text);
        Ok(())
    }

    pub(crate) fn push_char(&mut self, character: char) -> ArchiveDebugResult<()> {
        let attempted = self.text.len().saturating_add(character.len_utf8());
        self.budget.check_display_bytes(attempted)?;
        self.text.push(character);
        Ok(())
    }

    pub(crate) fn into_string(self) -> String {
        self.text
    }
}
