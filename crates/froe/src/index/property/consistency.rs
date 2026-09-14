//! The structural consistency check for the property family: froe's
//! contribution beyond oak-run, which has none for these index types.
//!
//! Two halves, with two budgets, because two callers need different things:
//!
//! * [`check_entries`] runs the **entry half** alone — for every indexed
//!   entry, the content node exists and carries the key under one of the
//!   definition's property names — and is charged per entry. It is the pass
//!   a reindex's publication tail runs, where the index was just built and
//!   the question is whether what was written resolves.
//! * [`check`] runs that half and then the **covered-node half** — for every
//!   node the definition covers, the entry exists — and is charged per
//!   visited node. It is what `froe index check` runs, where the question is
//!   whether the store's index agrees with the store's content.
//!
//! Neither half is oak-run's check. oak-run's `--index-consistency-check`
//! covers Lucene only; a property index has no check there at all, so a
//! missing entry is invisible until a query returns short.
//!
//! # What "the state it indexes" means
//!
//! The check compares an index against a *state*, not against the head: a
//! synchronous definition indexes the head, and an asynchronous one indexes
//! the state at its lane's checkpoint. Choosing between them is the caller's
//! job — `index/lanes.rs` answers it — because a dangling lane checkpoint
//! makes the definition uncheckable rather than inconsistent, and only the
//! caller can say which verdict its exit code needs.

use std::collections::{BTreeSet, HashSet};

use crate::content::node::NodeState;
use crate::content::property::PropertyValue;
use crate::index::definition::IndexDefinition;
use crate::index::path_filter::PathVerdict;
use crate::index::property::key_encoding::keys_for_property;
use crate::index::property::mirror::{MirrorEntry, MirrorIndex};
use crate::index::property::type_predicate::TypePredicate;
use crate::index::property::unique::UniqueIndex;
use crate::index::{IndexError, IndexResult, values_of};
use crate::segment::record::RecordIdentifier;

/// How many index entries a check may examine before it refuses.
///
/// The accounting unit is **one entry**: every `match = true` node of a
/// mirror index, and every path of every `entry` property of a unique index,
/// charges exactly one. Nothing else is charged, so the limit means what an
/// operator reading a refusal would assume it means.
///
/// The field is private rather than the struct `#[non_exhaustive]`, because
/// a private field is what actually blocks a struct literal from a
/// downstream crate while leaving the named constructor usable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntryCheckBudget {
    maximum_entries: u64,
}

impl EntryCheckBudget {
    /// A budget of `maximum_entries` index entries.
    #[must_use]
    pub const fn of_entries(maximum_entries: u64) -> Self {
        Self { maximum_entries }
    }

    /// The limit, for a caller that renders it.
    #[must_use]
    pub const fn maximum_entries(self) -> u64 {
        self.maximum_entries
    }
}

/// How many content nodes a check may visit before it refuses.
///
/// The accounting unit is **one visited node**: every node the covered-node
/// walk enters charges exactly one, whether or not the definition covers it,
/// because the cost being bounded is the walk rather than the match.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeCheckBudget {
    maximum_nodes: Option<u64>,
}

impl NodeCheckBudget {
    /// A budget of `maximum_nodes` visited content nodes.
    #[must_use]
    pub const fn of_nodes(maximum_nodes: u64) -> Self {
        Self {
            maximum_nodes: Some(maximum_nodes),
        }
    }

    /// No bound at all.
    ///
    /// This exists for the one case where a guessed number would be worse
    /// than none: the counter index holds no data, so no node estimate is
    /// available, and a made-up limit would refuse a healthy store at a
    /// number nothing justifies.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            maximum_nodes: None,
        }
    }

    /// The limit, or `None` when there is none.
    #[must_use]
    pub const fn maximum_nodes(self) -> Option<u64> {
        self.maximum_nodes
    }
}

/// What a check found.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PropertyIndexReport {
    /// Index entries that name a content node which does not exist.
    pub stale_entries: Vec<IndexEntryFault>,
    /// Index entries whose content node exists but does not carry the key
    /// under any of the definition's property names.
    pub mismatched_entries: Vec<IndexEntryFault>,
    /// Nodes the definition covers that no entry names.
    pub missing_entries: Vec<MissingEntry>,
    /// Unique keys holding more than one path, which Oak refuses commits on.
    pub duplicate_entries: Vec<DuplicateEntry>,
    /// How many entries the entry half examined.
    pub entries_checked: u64,
    /// How many content nodes the covered-node half visited, or `None` when
    /// that half did not run.
    pub nodes_visited: Option<u64>,
    /// How many `:count_*` approximate counters the storage carries. They
    /// are randomized by design and are never a fault; the count is what
    /// lets a report say how much a comparison excused.
    pub approximate_counters: usize,
}

impl PropertyIndexReport {
    /// Whether the index is consistent with the state it was checked
    /// against.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.stale_entries.is_empty()
            && self.mismatched_entries.is_empty()
            && self.missing_entries.is_empty()
            && self.duplicate_entries.is_empty()
    }
}

/// An entry that does not agree with the content it names.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct IndexEntryFault {
    /// The key the entry is stored under.
    pub key: String,
    /// The content path the entry names.
    pub path: String,
}

/// A node the definition covers that no entry names.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MissingEntry {
    /// The key the node's value should have been indexed under.
    pub key: String,
    /// The node's path.
    pub path: String,
}

/// A unique key holding more than one path.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct DuplicateEntry {
    /// The key.
    pub key: String,
    /// Every path stored under it.
    pub paths: Vec<String>,
}

/// Runs the entry half alone, charged per entry.
pub fn check_entries(
    definition_node: &NodeState<'_>,
    definition: &IndexDefinition,
    state_root: &NodeState<'_>,
    budget: EntryCheckBudget,
) -> IndexResult<PropertyIndexReport> {
    let mut report = PropertyIndexReport::default();
    check_entry_half(definition_node, definition, state_root, budget, &mut report)?;
    Ok(report)
}

/// Runs the entry half and then the covered-node half.
pub fn check(
    definition_node: &NodeState<'_>,
    definition: &IndexDefinition,
    state_root: &NodeState<'_>,
    entry_budget: EntryCheckBudget,
    node_budget: NodeCheckBudget,
) -> IndexResult<PropertyIndexReport> {
    let mut report = check_entries(definition_node, definition, state_root, entry_budget)?;
    check_covered_node_half(
        definition_node,
        definition,
        state_root,
        node_budget,
        &mut report,
    )?;
    Ok(report)
}

/// Which storage shape a definition uses, which follows from the one strict
/// `unique` read and from the index type — never from what is on disk.
enum Storage {
    Mirror { child_names: Vec<String> },
    Unique { child_name: String },
}

fn storage_for(definition: &IndexDefinition) -> Option<Storage> {
    match definition.index_type.as_ref()? {
        crate::index::IndexType::Reference => Some(Storage::Mirror {
            child_names: vec![":references".to_owned(), ":weakreferences".to_owned()],
        }),
        crate::index::IndexType::Property if definition.property.unique => Some(Storage::Unique {
            child_name: crate::index::INDEX_CONTENT_NODE_NAME.to_owned(),
        }),
        crate::index::IndexType::Property => Some(Storage::Mirror {
            child_names: vec![crate::index::INDEX_CONTENT_NODE_NAME.to_owned()],
        }),
        _ => None,
    }
}

fn check_entry_half(
    definition_node: &NodeState<'_>,
    definition: &IndexDefinition,
    state_root: &NodeState<'_>,
    budget: EntryCheckBudget,
    report: &mut PropertyIndexReport,
) -> IndexResult<()> {
    match storage_for(definition) {
        Some(Storage::Mirror { child_names }) => {
            let is_reference =
                definition.index_type.as_ref() == Some(&crate::index::IndexType::Reference);
            for child_name in child_names {
                let Some(index) =
                    MirrorIndex::open(definition_node, &definition.path, &child_name)?
                else {
                    continue;
                };
                report.approximate_counters += index.approximate_counter_count()?;
                index.for_each_entry(|entry| {
                    charge_entry(budget, definition, report)?;
                    if is_reference {
                        check_reference_entry(state_root, entry, report)
                    } else {
                        check_mirror_entry(state_root, definition, entry, report)
                    }
                })?;
            }
        }
        Some(Storage::Unique { child_name }) => {
            let Some(index) = UniqueIndex::open(definition_node, &child_name)? else {
                return Ok(());
            };
            report.approximate_counters += index.approximate_counter_count()?;
            for entry in index.entries()? {
                if entry.is_duplicate() {
                    report.duplicate_entries.push(DuplicateEntry {
                        key: entry.key.clone(),
                        paths: entry.paths.clone(),
                    });
                }
                for path in &entry.paths {
                    charge_entry(budget, definition, report)?;
                    check_unique_entry(state_root, definition, &entry.key, path, report)?;
                }
            }
        }
        None => {}
    }
    Ok(())
}

fn charge_entry(
    budget: EntryCheckBudget,
    definition: &IndexDefinition,
    report: &mut PropertyIndexReport,
) -> IndexResult<()> {
    report.entries_checked += 1;
    if report.entries_checked > budget.maximum_entries() {
        return Err(IndexError::Record(crate::Error::InvalidFormat {
            details: format!(
                "checking the index entries of {} would examine more than {} entries",
                definition.path,
                budget.maximum_entries()
            ),
        }));
    }
    Ok(())
}

/// For a property index: the content node exists and carries a value keying
/// to this entry's key under one of the definition's property names.
fn check_mirror_entry(
    state_root: &NodeState<'_>,
    definition: &IndexDefinition,
    entry: &MirrorEntry,
    report: &mut PropertyIndexReport,
) -> IndexResult<()> {
    let path = entry.content_path().to_owned();
    let Some(node) = resolve(state_root, &path)? else {
        report.stale_entries.push(IndexEntryFault {
            key: entry.key.clone(),
            path,
        });
        return Ok(());
    };
    if !node_has_key(&node, definition, &entry.key)? {
        report.mismatched_entries.push(IndexEntryFault {
            key: entry.key.clone(),
            path,
        });
    }
    Ok(())
}

/// For a reference index: the key is an unencoded identifier and the path is
/// a *property* path made relative, so the node is the path's parent and the
/// last element is the property name.
fn check_reference_entry(
    state_root: &NodeState<'_>,
    entry: &MirrorEntry,
    report: &mut PropertyIndexReport,
) -> IndexResult<()> {
    let relative = entry.path.trim_start_matches('/');
    let fault = |report: &mut PropertyIndexReport,
                 list: fn(&mut PropertyIndexReport) -> &mut Vec<IndexEntryFault>| {
        list(report).push(IndexEntryFault {
            key: entry.key.clone(),
            path: relative.to_owned(),
        });
    };
    let Some((node_path, property_name)) = relative.rsplit_once('/') else {
        fault(report, |report| &mut report.stale_entries);
        return Ok(());
    };
    let Some(node) = resolve(state_root, &format!("/{node_path}"))? else {
        fault(report, |report| &mut report.stale_entries);
        return Ok(());
    };
    let Some(property) = node.property(property_name)? else {
        fault(report, |report| &mut report.mismatched_entries);
        return Ok(());
    };
    let holds_identifier = values_of(&property)
        .iter()
        .filter_map(PropertyValue::as_text)
        .any(|text| text == entry.key);
    if !holds_identifier {
        fault(report, |report| &mut report.mismatched_entries);
    }
    Ok(())
}

fn check_unique_entry(
    state_root: &NodeState<'_>,
    definition: &IndexDefinition,
    key: &str,
    path: &str,
    report: &mut PropertyIndexReport,
) -> IndexResult<()> {
    let Some(node) = resolve(state_root, path)? else {
        report.stale_entries.push(IndexEntryFault {
            key: key.to_owned(),
            path: path.to_owned(),
        });
        return Ok(());
    };
    if !node_has_key(&node, definition, key)? {
        report.mismatched_entries.push(IndexEntryFault {
            key: key.to_owned(),
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// Whether `node` carries a value keying to `key` under any of the
/// definition's property names.
fn node_has_key(
    node: &NodeState<'_>,
    definition: &IndexDefinition,
    key: &str,
) -> IndexResult<bool> {
    for property_name in &definition.property.property_names {
        let Some(property) = node.property(property_name)? else {
            continue;
        };
        let keys = keys_for_property(
            &property,
            &definition.property.value_pattern,
            &definition.path,
        )?;
        if keys.contains(key) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_covered_node_half(
    definition_node: &NodeState<'_>,
    definition: &IndexDefinition,
    state_root: &NodeState<'_>,
    budget: NodeCheckBudget,
    report: &mut PropertyIndexReport,
) -> IndexResult<()> {
    // The reference index's covered-node half would have to walk every
    // property of every node looking for references, which is a different
    // computation from this one; plan 0007's oracle covers it instead.
    if definition.index_type.as_ref() == Some(&crate::index::IndexType::Reference) {
        return Ok(());
    }
    let Some(storage) = storage_for(definition) else {
        return Ok(());
    };
    if definition.property.property_names.is_empty() {
        return Ok(());
    }

    let predicate = if definition.property.declaring_node_types.is_empty() {
        None
    } else {
        Some(TypePredicate::new(
            state_root,
            &definition.property.declaring_node_types,
        )?)
    };
    let indexed = indexed_keys_by_path(definition_node, definition, &storage)?;

    let mut visited = 0u64;
    let mut records_on_path = HashSet::new();
    walk_covered(
        state_root,
        "/",
        definition,
        predicate.as_ref(),
        &indexed,
        budget,
        &mut visited,
        &mut records_on_path,
        report,
    )?;
    report.nodes_visited = Some(visited);
    Ok(())
}

/// Every `(path, key)` pair the storage holds, so the covered-node half can
/// ask "is this node indexed under this key" without re-walking the index.
fn indexed_keys_by_path(
    definition_node: &NodeState<'_>,
    definition: &IndexDefinition,
    storage: &Storage,
) -> IndexResult<BTreeSet<(String, String)>> {
    let mut indexed = BTreeSet::new();
    match storage {
        Storage::Mirror { child_names } => {
            for child_name in child_names {
                let Some(index) = MirrorIndex::open(definition_node, &definition.path, child_name)?
                else {
                    continue;
                };
                index.for_each_entry(|entry| {
                    indexed.insert((entry.content_path().to_owned(), entry.key.clone()));
                    Ok(())
                })?;
            }
        }
        Storage::Unique { child_name } => {
            let Some(index) = UniqueIndex::open(definition_node, child_name)? else {
                return Ok(indexed);
            };
            for entry in index.entries()? {
                for path in entry.paths {
                    indexed.insert((path, entry.key.clone()));
                }
            }
        }
    }
    Ok(indexed)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the walk's state is a stack frame rather than a configuration: every \
              argument is either the position in the tree or a per-walk accumulator, \
              and grouping them into a struct would hide that the recursion owns them"
)]
fn walk_covered(
    node: &NodeState<'_>,
    path: &str,
    definition: &IndexDefinition,
    predicate: Option<&TypePredicate>,
    indexed: &BTreeSet<(String, String)>,
    budget: NodeCheckBudget,
    visited: &mut u64,
    records_on_path: &mut HashSet<RecordIdentifier>,
    report: &mut PropertyIndexReport,
) -> IndexResult<()> {
    let record = node.record_identifier();
    if !records_on_path.insert(record) {
        return Err(IndexError::Record(crate::Error::InvalidFormat {
            details: format!(
                "the content at {path} contains node record {record} in its own subtree; \
                 the node records form a cycle"
            ),
        }));
    }
    *visited += 1;
    if let Some(maximum) = budget.maximum_nodes()
        && *visited > maximum
    {
        return Err(IndexError::Record(crate::Error::InvalidFormat {
            details: format!(
                "checking {} would visit more than {maximum} content nodes",
                definition.path
            ),
        }));
    }

    let verdict = definition.path_filter.filter(path);
    if verdict == PathVerdict::Include {
        check_covered_node(node, path, definition, predicate, indexed, report)?;
    }
    if verdict != PathVerdict::Exclude {
        for (name, child) in node.child_node_entries()? {
            // Hidden content is invisible to indexing, because every index
            // update Oak drives is wrapped in the visible-editor filter.
            if name.starts_with(':') {
                continue;
            }
            let child_path = if path == "/" {
                format!("/{name}")
            } else {
                format!("{path}/{name}")
            };
            walk_covered(
                &child,
                &child_path,
                definition,
                predicate,
                indexed,
                budget,
                visited,
                records_on_path,
                report,
            )?;
        }
    }
    records_on_path.remove(&record);
    Ok(())
}

fn check_covered_node(
    node: &NodeState<'_>,
    path: &str,
    definition: &IndexDefinition,
    predicate: Option<&TypePredicate>,
    indexed: &BTreeSet<(String, String)>,
    report: &mut PropertyIndexReport,
) -> IndexResult<()> {
    if let Some(predicate) = predicate
        && !predicate.test(node)?
    {
        return Ok(());
    }
    for property_name in &definition.property.property_names {
        let Some(property) = node.property(property_name)? else {
            continue;
        };
        for key in keys_for_property(
            &property,
            &definition.property.value_pattern,
            &definition.path,
        )? {
            if !indexed.contains(&(path.to_owned(), key.clone())) {
                report.missing_entries.push(MissingEntry {
                    key,
                    path: path.to_owned(),
                });
            }
        }
    }
    Ok(())
}

/// Resolves a content path against a state root, with `/` resolving to the
/// root itself.
fn resolve<'provider>(
    state_root: &NodeState<'provider>,
    path: &str,
) -> IndexResult<Option<NodeState<'provider>>> {
    let mut current = *state_root;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}
