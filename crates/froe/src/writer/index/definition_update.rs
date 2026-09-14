//! The bookkeeping Oak performs on a definition node around a reindex.
//!
//! `docs/analysis/index-definitions.md` records the protocol. When a cycle
//! decides a definition needs a reindex it sets `reindex = false`,
//! increments `reindexCount`, removes every hidden child not flagged
//! `retainNodeInReindex`, clears `corrupt`, and sets the hidden
//! `:disableIndexesOnNextCycle` when the definition's `supersedes` names an
//! index that is still active — the *next* cycle is what disables those
//! indexes, and froe never disables them itself.
//!
//! Getting this wrong does not corrupt a store. It makes Oak redo, skip or
//! double work, which is a fault that appears later and somewhere else —
//! which is why every rule here cites the read it is made with.
//!
//! # Strict and converting reads are not interchangeable
//!
//! `retainNodeInReindex` is read **strictly** as a `BOOLEAN`, so a `STRING`
//! `"true"` does *not* retain a hidden child. `reindex` is read
//! **converting**, so a `STRING` `"true"` *does* flag the definition.
//! `reindexCount` is read converting to `LONG`, as Oak's own increment reads
//! it, and a multi-valued value is a typed refusal because Oak's commit
//! fails on it. These are Oak's own disagreements, observable in a store,
//! and reproducing them is the point.

use crate::PropertyType;
use crate::content::node::NodeState;
use crate::content::{PropertyValues, SegmentProvider};
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;
use crate::writer::commit::{ChildEdits, NodeEdits, rewrite_node_with_edits};
use crate::writer::record_writer::{
    PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};

/// The hidden flag Oak's *next* cycle reads to disable a superseded index.
pub const DISABLE_ON_NEXT_CYCLE_PROPERTY: &str = ":disableIndexesOnNextCycle";

/// The flag that keeps a hidden child across a reindex.
const RETAIN_PROPERTY: &str = "retainNodeInReindex";

/// What a rewrite does to `reindexCount`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReindexCount {
    /// Read the current value and add one, or create it at 1.
    Increment,
    /// Write exactly this value. Plan 0008's importer sets the file's value
    /// plus one.
    Set(u64),
    /// Leave it as it is — a reset touches no visible property.
    Keep,
}

/// Whether the disabler flag is written.
///
/// The verdict is computed by selection against the store's **head**, over
/// the other definitions' raw properties, and passed in: the rewrite itself
/// never sees a root, so it cannot accidentally evaluate the predicate
/// against the state being indexed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisablerVerdict {
    /// `supersedes` names an index that is still active.
    Flag,
    /// It does not, or there is no `supersedes`.
    Leave,
}

/// Everything one definition rewrite changes.
///
/// Grouped rather than passed as arguments so plan 0008's importer and the
/// `seed` creation of the counter and of plan 0010's Lucene reindex compose
/// their own bookkeeping into the same single rewrite.
pub struct DefinitionEdits {
    /// What happens to `reindexCount`.
    pub reindex_count: ReindexCount,
    /// Whether `:disableIndexesOnNextCycle` is written.
    pub disabler_verdict: DisablerVerdict,
    /// Properties to write over whatever the definition carries.
    pub property_replacements: Vec<PropertyToWrite>,
    /// Properties to remove beyond `corrupt`.
    pub property_removals: Vec<String>,
    /// Visible children to change. Plan 0007 writes none; plan 0010's
    /// `facets` subtree is its first user, and task 0706's digest comparison
    /// is the regression that says so.
    pub visible_children: ChildEdits,
    /// The hidden children this rebuild produced, by name. Every *other*
    /// hidden child is dropped unless its node carries a strict `BOOLEAN`
    /// `retainNodeInReindex = true`.
    ///
    /// Empty is a real value: a counter with no hits and a reference index
    /// with no entries both produce none.
    pub hidden_children: Vec<(String, RecordIdentifier)>,
}

impl DefinitionEdits {
    /// The edits an ordinary reindex makes, with no extra properties.
    #[must_use]
    pub fn reindexed(
        disabler_verdict: DisablerVerdict,
        hidden_children: Vec<(String, RecordIdentifier)>,
    ) -> Self {
        Self {
            reindex_count: ReindexCount::Increment,
            disabler_verdict,
            property_replacements: Vec::new(),
            property_removals: Vec::new(),
            visible_children: ChildEdits::new(),
            hidden_children,
        }
    }

    /// The edits a reset makes: hidden children removed, nothing else.
    ///
    /// A reset is not a reindex — it does not increment `reindexCount` and
    /// does not clear `reindex`, because Oak's next cycle is what rebuilds
    /// and what does that bookkeeping.
    #[must_use]
    pub fn reset() -> Self {
        Self {
            reindex_count: ReindexCount::Keep,
            disabler_verdict: DisablerVerdict::Leave,
            property_replacements: Vec::new(),
            property_removals: Vec::new(),
            visible_children: ChildEdits::new(),
            hidden_children: Vec::new(),
        }
    }
}

/// Rewrites a definition node as a reindex leaves it.
///
/// Every property and every visible child the edits do not name is preserved
/// **by record identity**, through the commit path's own slot-preserving
/// rewrite rather than an imitation of it.
pub fn rewrite_definition<Sink: SegmentSink>(
    provider: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    definition: &NodeState<'_>,
    edits: &DefinitionEdits,
) -> Result<RecordIdentifier> {
    let node_edits = definition_node_edits(writer, definition, edits)?;
    rewrite_node_with_edits(
        provider,
        writer,
        Some(definition.record_identifier()),
        &node_edits,
    )
}

/// The commit-path edits one [`DefinitionEdits`] resolves to.
///
/// Separated from the rewrite because the stored-definition clone needs the
/// *same* edits with the hidden children taken back out, and deriving them
/// twice is how the clone and the definition come to disagree.
fn definition_node_edits<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    definition: &NodeState<'_>,
    edits: &DefinitionEdits,
) -> Result<NodeEdits> {
    let mut node_edits = NodeEdits {
        property_replacements: edits.property_replacements.clone(),
        property_removals: edits.property_removals.clone(),
        child_edits: edits.visible_children.clone(),
    };

    if edits.reindex_count != ReindexCount::Keep {
        // `reindex` is cleared on the same commit that registers the editor,
        // so a definition froe rebuilt is one Oak will not rebuild again.
        let truth = writer.write_string("false")?;
        node_edits.property_replacements.push(PropertyToWrite {
            name: "reindex".to_owned(),
            property_type: PropertyType::Boolean,
            values: PropertyValuesToWrite::Single(truth),
        });

        let next = match edits.reindex_count {
            ReindexCount::Increment => current_reindex_count(definition)?.saturating_add(1),
            ReindexCount::Set(value) => value,
            ReindexCount::Keep => unreachable!("excluded by the branch"),
        };
        let value = writer.write_string(&next.to_string())?;
        node_edits.property_replacements.push(PropertyToWrite {
            name: "reindexCount".to_owned(),
            property_type: PropertyType::Long,
            values: PropertyValuesToWrite::Single(value),
        });

        // A definition Oak marked corrupt is no longer corrupt once it has
        // been rebuilt.
        node_edits.property_removals.push("corrupt".to_owned());
    }

    if edits.disabler_verdict == DisablerVerdict::Flag {
        let truth = writer.write_string("true")?;
        node_edits.property_replacements.push(PropertyToWrite {
            name: DISABLE_ON_NEXT_CYCLE_PROPERTY.to_owned(),
            property_type: PropertyType::Boolean,
            values: PropertyValuesToWrite::Single(truth),
        });
    }

    // Every hidden child is replaced or dropped, except one whose node
    // carries a strict `BOOLEAN` `retainNodeInReindex = true`.
    let produced: Vec<&str> = edits
        .hidden_children
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    for (name, child) in definition.child_node_entries()? {
        if !name.starts_with(':') || produced.contains(&name.as_str()) {
            continue;
        }
        if retains_across_reindex(&child)? {
            continue;
        }
        node_edits.child_edits.insert(name, None);
    }
    for (name, record) in &edits.hidden_children {
        node_edits.child_edits.insert(name.clone(), Some(*record));
    }

    Ok(node_edits)
}

/// The definition as `NodeStateCloner.cloneVisibleState` clones it from the
/// **updated** state — which is the state an import stores under
/// `:index-definition` (§6.2).
///
/// It cannot be produced by cloning the rewritten record, because that
/// record is still in the writer's buffer and no provider can read it. So
/// the same edits are applied to the original node with every hidden child
/// taken back out, which is what the clone of the updated state *is*: the
/// updated properties, the visible children, nothing hidden.
///
/// Visible children keep their own subtrees, cloned so that a hidden child
/// nested under one is dropped too. A visible child the edits *replace*
/// is pointed at as the edits give it — no import writes one today, and
/// plan 0010's `facets` child is the first that would.
pub fn clone_updated_definition_state<Sink: SegmentSink>(
    provider: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    definition: &NodeState<'_>,
    edits: &DefinitionEdits,
) -> Result<RecordIdentifier> {
    let mut node_edits = definition_node_edits(writer, definition, edits)?;
    // Every hidden child goes, whether the rewrite produced it or the
    // definition already carried it. The produced ones are the ones that
    // matter: `:status` and `:data` are written by this very run, so they
    // appear only in the edits and never in the node below.
    for name in node_edits
        .child_edits
        .keys()
        .filter(|name| name.starts_with(':'))
        .cloned()
        .collect::<Vec<String>>()
    {
        node_edits.child_edits.insert(name, None);
    }
    for (name, child) in definition.child_node_entries()? {
        if name.starts_with(':') {
            node_edits.child_edits.insert(name, None);
            continue;
        }
        if node_edits.child_edits.contains_key(&name) {
            continue;
        }
        let cloned = clone_visible_state(provider, writer, &child)?;
        if cloned != child.record_identifier() {
            node_edits.child_edits.insert(name, Some(cloned));
        }
    }
    rewrite_node_with_edits(
        provider,
        writer,
        Some(definition.record_identifier()),
        &node_edits,
    )
}

/// `reindexCount`, read converting to `LONG` as Oak's own increment reads
/// it.
///
/// A multi-valued value is a typed refusal, because Oak's own commit fails
/// on it: `getLong` on a multi-valued property throws, and a rebuild that
/// guessed which value to take would write a count Oak never would.
fn current_reindex_count(definition: &NodeState<'_>) -> Result<u64> {
    let Some(property) = definition.property("reindexCount")? else {
        return Ok(0);
    };
    match &property.values {
        // A value that does not parse, or a negative one, reads as zero —
        // the same answer Oak's `getLong` default gives, so the increment
        // lands on 1 either way.
        PropertyValues::Single(value) => Ok(value
            .as_text()
            .and_then(|text| text.parse::<i64>().ok())
            .and_then(|count| u64::try_from(count).ok())
            .unwrap_or(0)),
        PropertyValues::Multiple(_) => Err(Error::InvalidFormat {
            details: "reindexCount is multi-valued, which Oak's own increment refuses; \
                      a rebuild cannot guess which value to carry forward"
                .to_owned(),
        }),
    }
}

/// Whether a hidden child survives a reindex.
///
/// Read **strictly** as a `BOOLEAN`: a `STRING` `"true"` does not retain.
fn retains_across_reindex(child: &NodeState<'_>) -> Result<bool> {
    let property = child.property(RETAIN_PROPERTY)?;
    Ok(crate::index::strict_boolean(property.as_ref()))
}

/// How deep a `supersedes` path may be resolved.
///
/// The corrupt-input rule: a path from a store is file-supplied, so the walk
/// that resolves it is bounded rather than trusted.
const MAXIMUM_SUPERSEDES_DEPTH: usize = 64;

/// Oak's `IndexDisabler.isAnyIndexToBeDisabled`, as a verdict.
///
/// `docs/analysis/index-definitions.md` §5.5 states the rule and quotes the
/// Java (`oak-core`, `plugins/index/upgrade/IndexDisabler.java`). The flag is
/// raised when, among the `supersedes` values — read **converting** to
/// `STRINGS` — there is either
///
/// * a plain index path whose node exists and whose `type` does **not** read
///   **strictly** as the `STRING` `disabled`, so a `NAME`-typed or
///   array-typed `type` counts as active and raises the flag; or
/// * a `/path/@type` entry whose named node type is still among that index's
///   `declaringNodeTypes`, read **converting** to `NAMES`.
///
/// The asymmetry between the two reads is load-bearing and is reproduced
/// rather than tidied: it is why a superseded index whose `type` was stored
/// as a `NAME` is disabled over and over.
///
/// froe never *acts* on the flag — `disableOldIndexes` changes which index
/// answers a query, which is a running Oak's decision and an operator's, not
/// an offline tool's (§10, invariant 7). It only raises it where Oak raises
/// it, so the cycle after froe's behaves as it would have.
pub fn disabler_verdict(
    content_root: &NodeState<'_>,
    definition: &NodeState<'_>,
) -> Result<DisablerVerdict> {
    let superseded = crate::index::converting_strings(definition.property("supersedes")?.as_ref());
    for entry in superseded {
        let (path, node_type) = match entry.rsplit_once('/') {
            Some((parent, last)) if last.starts_with('@') => (parent, Some(&last[1..])),
            _ => (entry.as_str(), None),
        };
        let Some(node) = resolve_under(content_root, path)? else {
            continue;
        };
        if let Some(node_type) = node_type {
            let declared =
                crate::index::converting_strings(node.property("declaringNodeTypes")?.as_ref());
            if declared.iter().any(|declared| declared == node_type) {
                return Ok(DisablerVerdict::Flag);
            }
        } else if crate::index::strict_string(node.property("type")?.as_ref()) != Some("disabled") {
            return Ok(DisablerVerdict::Flag);
        }
    }
    Ok(DisablerVerdict::Leave)
}

/// Resolves an absolute or relative path under `root`, bounded.
fn resolve_under<'store>(
    root: &NodeState<'store>,
    path: &str,
) -> Result<Option<NodeState<'store>>> {
    let mut node = *root;
    for (depth, element) in path.split('/').filter(|part| !part.is_empty()).enumerate() {
        if depth >= MAXIMUM_SUPERSEDES_DEPTH {
            return Ok(None);
        }
        let Some(child) = node.child_node(element)? else {
            return Ok(None);
        };
        node = child;
    }
    Ok(Some(node))
}

/// The hidden property `IndexDefinition.INDEX_VERSION`.
pub const INDEX_VERSION_PROPERTY: &str = ":version";

/// The hidden child `IndexDefinition.INDEX_DEFINITION_NODE`.
pub const STORED_DEFINITION_CHILD: &str = ":index-definition";

/// `IndexDefinition.determineVersionForFreshIndex`, collapsed.
///
/// `docs/analysis/index-definitions.md` §6.2 quotes the Java and works the
/// collapse: `IndexFormatVersion` has exactly `V1(1)` and `V2(2)`, the
/// default is `V2`, and every branch but the `compatMode` one returns
/// `max(V2, …)`. So a definition carrying `compatMode` gets that value, read
/// **converting** to `LONG`, and every other definition gets 2 — neither
/// `:version` nor `fullTextEnabled` can change the answer.
///
/// A `compatMode` outside `{1, 2}` is what Oak's `getVersion(int)` throws
/// on; froe refuses it rather than writing a version Oak will not read.
pub fn fresh_index_format_version(definition: &NodeState<'_>) -> Result<i64> {
    let Some(property) = definition.property("compatMode")? else {
        return Ok(2);
    };
    match crate::index::converting_long(Some(&property)) {
        Some(version @ (1 | 2)) => Ok(version),
        other => Err(Error::InvalidFormat {
            details: format!(
                "compatMode reads as {}, and Oak's IndexFormatVersion knows only 1 and 2",
                other.map_or_else(|| "no number".to_owned(), |value| value.to_string())
            ),
        }),
    }
}

/// How deep [`clone_visible_state`] walks.
const MAXIMUM_CLONE_DEPTH: usize = 64;

/// `NodeStateCloner.cloneVisibleState`.
///
/// `oak-search`, `plugins/index/search/util/NodeStateCloner.java`: its
/// `ApplyVisibleDiff` overrides `childNodeAdded` alone, so the clone **drops
/// hidden child nodes and keeps hidden properties**, at every depth.
///
/// Unchanged visible children are shared rather than copied: the clone is a
/// node in the same store, and a record that already holds the right subtree
/// is the right record to point at.
pub fn clone_visible_state<Sink: SegmentSink>(
    provider: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    node: &NodeState<'_>,
) -> Result<RecordIdentifier> {
    clone_visible_state_bounded(provider, writer, node, 0)
}

fn clone_visible_state_bounded<Sink: SegmentSink>(
    provider: &dyn SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    node: &NodeState<'_>,
    depth: usize,
) -> Result<RecordIdentifier> {
    if depth >= MAXIMUM_CLONE_DEPTH {
        return Err(Error::InvalidFormat {
            details: format!(
                "a definition nests more than {MAXIMUM_CLONE_DEPTH} levels deep, which no \
                 index definition does and a corrupt record can claim"
            ),
        });
    }

    let mut edits = ChildEdits::new();
    for (name, child) in node.child_node_entries()? {
        if name.starts_with(':') {
            edits.insert(name, None);
            continue;
        }
        let cloned = clone_visible_state_bounded(provider, writer, &child, depth + 1)?;
        if cloned != child.record_identifier() {
            edits.insert(name, Some(cloned));
        }
    }
    crate::writer::commit::rewrite_node_with_child_edits(
        provider,
        writer,
        Some(node.record_identifier()),
        &edits,
    )
}
