//! `:status` and `:index-definition`: the two hidden children the fulltext
//! editor family maintains beside a definition, and the drift comparison
//! Oak's Lucene index-information provider performs between them.
//!
//! Only the fulltext editors write these. The property, reference and counter
//! editors write neither, so their absence beside one of those definitions is
//! not a defect. `docs/analysis/index-definitions.md` §6 records what each
//! property means, which state the stored clone is taken from, and why
//! `creationTimestamp` is absent right after a reindex or an import.

use std::collections::BTreeSet;

use crate::content::node::{NodeState, PropertyState};
use crate::content::property::PropertyValue;
use crate::index::{IndexResult, strict_string, values_of};

/// The name of the status child.
pub const STATUS_NODE_NAME: &str = ":status";

/// The name of the stored-definition child.
pub const STORED_DEFINITION_NODE_NAME: &str = ":index-definition";

/// The property names the drift comparison ignores on both sides, at every
/// depth, beside every hidden property name.
pub const DRIFT_IGNORED_PROPERTY_NAMES: [&str; 2] = ["reindex", "reindexCount"];

/// The `:status` node beside a fulltext definition.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StatusNode {
    /// `uid`, time-increasing decimal epoch milliseconds in a `STRING`, read
    /// back strictly as Oak reads it.
    pub unique_identifier: Option<String>,
    /// `lastUpdated`, a `DATE` in its stored form: the cycle's
    /// `indexingCheckpointTime` commit attribute when it had one, wall-clock
    /// otherwise.
    pub last_updated: Option<String>,
    /// `indexedNodes`, a **per-cycle** counter the editor context resets on
    /// every cycle. It is not a document count, and comparing it with a
    /// Lucene document count is a category error.
    pub indexed_nodes: Option<i64>,
    /// `reindexCompletionTimestamp`, a `DATE`, written on the reindex path
    /// and removed again by the next `ReindexOperations.apply`.
    pub reindex_completion_timestamp: Option<String>,
}

impl StatusNode {
    /// Reads `:status` from a definition node, or `None` when it is absent —
    /// which is the state of every property, reference and counter
    /// definition, and of a fulltext one whose last cycle changed nothing.
    pub fn read(definition: &NodeState<'_>) -> IndexResult<Option<Self>> {
        let Some(node) = definition.child_node(STATUS_NODE_NAME)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            unique_identifier: strict_string(node.property("uid")?.as_ref()).map(str::to_owned),
            last_updated: first_text(node.property("lastUpdated")?.as_ref()),
            indexed_nodes: crate::index::converting_long(node.property("indexedNodes")?.as_ref()),
            reindex_completion_timestamp: first_text(
                node.property("reindexCompletionTimestamp")?.as_ref(),
            ),
        }))
    }
}

/// The `:index-definition` node: a visible clone of the definition, taken
/// from the builder's *base* state on a reindex and from the updated state on
/// an import.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StoredDefinition {
    /// `creationTimestamp`, written only where `refresh` is consumed or the
    /// clone is first created. It is therefore **absent** right after a
    /// reindex or an import, which is not a defect.
    pub creation_timestamp: Option<String>,
    /// `seed`, which the editor keeps in step with the definition's.
    pub seed: Option<i64>,
}

impl StoredDefinition {
    /// Reads `:index-definition` from a definition node, or `None` when it is
    /// absent.
    pub fn read(definition: &NodeState<'_>) -> IndexResult<Option<Self>> {
        let Some(node) = definition.child_node(STORED_DEFINITION_NODE_NAME)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            creation_timestamp: first_text(node.property("creationTimestamp")?.as_ref()),
            seed: crate::index::converting_long(node.property("seed")?.as_ref()),
        }))
    }
}

/// One difference the drift comparison found, as a path relative to the
/// definition node.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct DefinitionDifference {
    /// The path of the property or child that differs, relative to the
    /// definition node and beginning with `/`.
    pub path: String,
    /// What differs there.
    pub kind: DifferenceKind,
}

/// What kind of difference the comparison found.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DifferenceKind {
    /// The current definition has it and the stored clone does not.
    Added,
    /// The stored clone has it and the current definition does not.
    Removed,
    /// Both have it, with different values.
    Changed,
}

/// Whether the definition has drifted from its stored clone, and where.
///
/// This is `LuceneIndexInfoProvider.computeIndexDefinitionChange` composed
/// with `FilteringEqualsDiff`, stated in
/// `docs/analysis/index-definitions.md` §6.4:
///
/// * `reindex`, `reindexCount` and **every hidden property name** are
///   ignored, at every depth — the diff recurses with the same filtering
///   instance, so the ignore set is not a property of the definition node.
/// * A visible child added or removed is a difference; one present on both
///   sides is compared by the same rule, recursively.
/// * Oak clones only the current definition (hidden properties kept, hidden
///   child nodes dropped) and compares it with the stored node as it stands.
///   froe clones **both** sides, which is equivalent because every Oak-written
///   `:index-definition` is itself such a clone, and states so here rather
///   than reproducing an asymmetry that has no observable effect.
///
/// `extra_ignored_property_names` is how a caller excuses the properties an
/// oak-run out-of-band build rewrites; it is applied at every depth too.
///
/// Oak renders its diff as JSOP text. froe renders a sorted list of changed
/// paths instead: the JSOP is a debugging aid rather than a contract, and a
/// path list is what a listing row can usefully show.
pub fn definition_drift(
    definition: &NodeState<'_>,
    stored: &NodeState<'_>,
    extra_ignored_property_names: &[String],
) -> IndexResult<Vec<DefinitionDifference>> {
    let mut ignored: BTreeSet<&str> = DRIFT_IGNORED_PROPERTY_NAMES.into_iter().collect();
    ignored.extend(extra_ignored_property_names.iter().map(String::as_str));
    let mut differences = Vec::new();
    compare_nodes(stored, definition, "", &ignored, &mut differences)?;
    differences.sort();
    Ok(differences)
}

fn compare_nodes(
    before: &NodeState<'_>,
    after: &NodeState<'_>,
    path: &str,
    ignored: &BTreeSet<&str>,
    differences: &mut Vec<DefinitionDifference>,
) -> IndexResult<()> {
    compare_properties(before, after, path, ignored, differences)?;
    compare_children(before, after, path, ignored, differences)
}

fn compare_properties(
    before: &NodeState<'_>,
    after: &NodeState<'_>,
    path: &str,
    ignored: &BTreeSet<&str>,
    differences: &mut Vec<DefinitionDifference>,
) -> IndexResult<()> {
    let before_properties = visible_properties(before, ignored)?;
    let after_properties = visible_properties(after, ignored)?;
    for property in &after_properties {
        match before_properties
            .iter()
            .find(|candidate| candidate.name == property.name)
        {
            None => differences.push(DefinitionDifference {
                path: format!("{path}/{}", property.name),
                kind: DifferenceKind::Added,
            }),
            Some(previous) if previous != property => differences.push(DefinitionDifference {
                path: format!("{path}/{}", property.name),
                kind: DifferenceKind::Changed,
            }),
            Some(_) => {}
        }
    }
    for property in &before_properties {
        if !after_properties
            .iter()
            .any(|candidate| candidate.name == property.name)
        {
            differences.push(DefinitionDifference {
                path: format!("{path}/{}", property.name),
                kind: DifferenceKind::Removed,
            });
        }
    }
    Ok(())
}

fn compare_children(
    before: &NodeState<'_>,
    after: &NodeState<'_>,
    path: &str,
    ignored: &BTreeSet<&str>,
    differences: &mut Vec<DefinitionDifference>,
) -> IndexResult<()> {
    let before_children = visible_children(before)?;
    let after_children = visible_children(after)?;
    for (name, child) in &after_children {
        match before_children
            .iter()
            .find(|(candidate, _)| candidate == name)
        {
            None => differences.push(DefinitionDifference {
                path: format!("{path}/{name}"),
                kind: DifferenceKind::Added,
            }),
            Some((_, previous)) => {
                compare_nodes(
                    previous,
                    child,
                    &format!("{path}/{name}"),
                    ignored,
                    differences,
                )?;
            }
        }
    }
    for (name, _) in &before_children {
        if !after_children
            .iter()
            .any(|(candidate, _)| candidate == name)
        {
            differences.push(DefinitionDifference {
                path: format!("{path}/{name}"),
                kind: DifferenceKind::Removed,
            });
        }
    }
    Ok(())
}

/// The properties the comparison considers: everything but the ignored names
/// and every hidden name.
fn visible_properties(
    node: &NodeState<'_>,
    ignored: &BTreeSet<&str>,
) -> IndexResult<Vec<PropertyState>> {
    Ok(node
        .properties()?
        .into_iter()
        .filter(|property| {
            !property.name.starts_with(':') && !ignored.contains(property.name.as_str())
        })
        .collect())
}

/// The children the comparison considers, which are the visible ones — the
/// clone drops the hidden ones, so comparing them would report a difference
/// against every stored definition.
fn visible_children<'provider>(
    node: &NodeState<'provider>,
) -> IndexResult<Vec<(String, NodeState<'provider>)>> {
    Ok(node
        .child_node_entries()?
        .into_iter()
        .filter(|(name, _)| !name.starts_with(':'))
        .collect())
}

fn first_text(property: Option<&PropertyState>) -> Option<String> {
    property.and_then(|property| values_of(property).first().and_then(PropertyValue::as_text))
}
