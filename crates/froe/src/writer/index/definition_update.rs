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
