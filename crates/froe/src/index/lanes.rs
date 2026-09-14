//! `/:async`: the asynchronous indexer's per-lane state.
//!
//! Per lane `L`, Oak keeps four properties on this one node — `L` (the
//! checkpoint the lane last indexed to), `L-temp` (checkpoints pending
//! release), `L-lease` (a lease expiry, present only while a run holds it)
//! and `L-LastIndexedTo` (a date). `docs/analysis/index-definitions.md` §3
//! records the naming helpers and the lifetimes.
//!
//! A **dangling** lane checkpoint — one `L` names that `/checkpoints` no
//! longer holds — is the fact this module exists to surface. Oak does not
//! fail on one: it logs a warning and re-runs from the missing state, which
//! for a Lucene index means its writer *appends* every document again to the
//! retained `:data`, doubling the index with no error anywhere. See §3.1.
//!
//! # The one rule, and its two callers
//!
//! [`AsyncLanes::dangling_checkpoints`] is where that verdict is decided,
//! and `crate::tooling::digest` is its other caller — the digest reports a
//! dangling reference among the invariants it judges on its own. The rule
//! is deliberately **conservative**: *every* string value of *every*
//! non-`-temp` property on `/:async` is treated as a checkpoint reference,
//! whatever the property is called, because Oak stores each lane's resume
//! point as an ordinary string property whose name varies by lane. Only the
//! UUID shape narrows it, and only so an unrelated string property cannot
//! be reported as gone.
//!
//! The `-temp` exclusion is not an optimization. `AsyncIndexUpdate` keeps
//! `<lane>-temp` as the list of checkpoints the indexer *intends to
//! release*; entries in it are routinely already gone, because releasing
//! them is what it is for. Treating that list like a resume point reports a
//! dangling reference on a pristine, untouched Oak store — verified against
//! the interop fixture, where Oak's own `async-temp` names one live
//! checkpoint and one already released.
//!
//! `commit.rs`'s `remove_unreferenced_checkpoints` keeps a twin of the rule
//! deliberately: it decides what maintenance may *delete*, so it must stay
//! at least as conservative as this one and is free to be more so.

use std::collections::{BTreeMap, BTreeSet};

use crate::content::node::NodeState;
use crate::content::property::PropertyValue;
use crate::index::{ASYNC_NODE_NAME, IndexResult};

/// The suffix Oak appends for a lane's pending-release list.
const PENDING_RELEASE_SUFFIX: &str = "-temp";

/// The suffix Oak appends for a lane's lease.
const LEASE_SUFFIX: &str = "-lease";

/// The suffix Oak appends for a lane's last-indexed-to date.
const LAST_INDEXED_TO_SUFFIX: &str = "-LastIndexedTo";

/// One asynchronous indexing lane's state.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct AsyncLane {
    /// The lane name, which is the property name the checkpoint is stored
    /// under.
    pub name: String,
    /// The checkpoint the lane last indexed to, read strictly as a `STRING`
    /// the way `AsyncIndexUpdate` reads it.
    pub checkpoint: Option<String>,
    /// The checkpoints the lane intends to release.
    pub pending_release: Vec<String>,
    /// The lease expiry in epoch milliseconds, present only while a run holds
    /// the lane.
    pub lease_expiry: Option<i64>,
    /// The last-indexed-to date, in its stored ISO-8601 form.
    pub last_indexed_to: Option<String>,
}

/// Every lane `/:async` records.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct AsyncLanes {
    lanes: BTreeMap<String, AsyncLane>,
    node_present: bool,
}

impl AsyncLanes {
    /// Reads `/:async` from the content root.
    ///
    /// A store with no `/:async` node reads as an empty set of lanes rather
    /// than as an error: an Oak store that has never run an asynchronous
    /// cycle has none.
    pub fn read(content_root: &NodeState<'_>) -> IndexResult<Self> {
        let Some(node) = content_root.child_node(ASYNC_NODE_NAME)? else {
            return Ok(Self::default());
        };
        let mut lanes: BTreeMap<String, AsyncLane> = BTreeMap::new();
        for property in node.properties()? {
            let (lane_name, field) = split_lane_property(&property.name);
            let lane = lanes
                .entry(lane_name.to_owned())
                .or_insert_with(|| AsyncLane {
                    name: lane_name.to_owned(),
                    ..AsyncLane::default()
                });
            let values = crate::index::values_of(&property);
            match field {
                LaneField::Checkpoint => {
                    lane.checkpoint =
                        crate::index::strict_string(Some(&property)).map(str::to_owned);
                }
                LaneField::PendingRelease => {
                    lane.pending_release =
                        values.iter().filter_map(PropertyValue::as_text).collect();
                }
                LaneField::Lease => {
                    lane.lease_expiry = crate::index::converting_long(Some(&property));
                }
                LaneField::LastIndexedTo => {
                    lane.last_indexed_to = values.first().and_then(PropertyValue::as_text);
                }
            }
        }
        Ok(Self {
            lanes,
            node_present: true,
        })
    }

    /// Whether the store has an `/:async` node at all.
    #[must_use]
    pub fn node_present(&self) -> bool {
        self.node_present
    }

    /// One lane by name.
    #[must_use]
    pub fn lane(&self, name: &str) -> Option<&AsyncLane> {
        self.lanes.get(name)
    }

    /// Every lane, by name.
    pub fn lanes(&self) -> impl Iterator<Item = &AsyncLane> {
        self.lanes.values()
    }

    /// The checkpoints `/:async` names that `/checkpoints` no longer holds,
    /// sorted.
    ///
    /// `crate::tooling::digest` calls this for the invariant it reports
    /// beside the digest itself, so a change here changes what `froe digest`
    /// says about a store.
    ///
    /// Dangling means *resolved and absent*: a checkpoint whose node cannot
    /// be read is an error, never a dangling reference, because reporting an
    /// unreadable checkpoint as gone would send an operator to delete
    /// something that is still there.
    pub fn dangling_checkpoints(
        content_root: &NodeState<'_>,
        super_root: &NodeState<'_>,
    ) -> IndexResult<Vec<String>> {
        let Some(node) = content_root.child_node(ASYNC_NODE_NAME)? else {
            return Ok(Vec::new());
        };
        let existing: BTreeSet<String> = match super_root.child_node("checkpoints")? {
            None => BTreeSet::new(),
            Some(checkpoints) => checkpoints
                .child_node_entries()?
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
        };
        let mut dangling = BTreeSet::new();
        for property in node.properties()? {
            if property.name.ends_with(PENDING_RELEASE_SUFFIX) {
                continue;
            }
            for value in crate::index::values_of(&property) {
                if let PropertyValue::String(text) = value
                    && is_checkpoint_reference(text)
                    && !existing.contains(text)
                {
                    dangling.insert(text.clone());
                }
            }
        }
        Ok(dangling.into_iter().collect())
    }
}

/// Which of a lane's four properties a name is.
enum LaneField {
    Checkpoint,
    PendingRelease,
    Lease,
    LastIndexedTo,
}

/// Splits a property name on `/:async` into its lane name and its field.
///
/// Order matters: `async-temp` ends in neither `-lease` nor
/// `-LastIndexedTo`, and a lane name may itself contain a hyphen
/// (`fulltext-async`), so the suffixes are stripped rather than the name
/// split on the first hyphen.
fn split_lane_property(name: &str) -> (&str, LaneField) {
    if let Some(lane) = name.strip_suffix(PENDING_RELEASE_SUFFIX) {
        (lane, LaneField::PendingRelease)
    } else if let Some(lane) = name.strip_suffix(LEASE_SUFFIX) {
        (lane, LaneField::Lease)
    } else if let Some(lane) = name.strip_suffix(LAST_INDEXED_TO_SUFFIX) {
        (lane, LaneField::LastIndexedTo)
    } else {
        (name, LaneField::Checkpoint)
    }
}

/// Whether a string has the shape Oak names checkpoints with, which is a
/// UUID. Requiring the shape keeps an unrelated string property on `/:async`
/// from being reported as a dangling reference.
fn is_checkpoint_reference(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && [8, 4, 4, 4, 12] == groups.iter().map(|group| group.len()).collect::<Vec<_>>()[..]
        && text
            .chars()
            .all(|character| character == '-' || character.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{LaneField, is_checkpoint_reference, split_lane_property};

    fn field_name(name: &str) -> (&str, &'static str) {
        let (lane, field) = split_lane_property(name);
        let field = match field {
            LaneField::Checkpoint => "checkpoint",
            LaneField::PendingRelease => "pending",
            LaneField::Lease => "lease",
            LaneField::LastIndexedTo => "last-indexed-to",
        };
        (lane, field)
    }

    #[test]
    fn the_bare_lane_name_is_the_checkpoint() {
        assert_eq!(field_name("async"), ("async", "checkpoint"));
        assert_eq!(
            field_name("fulltext-async"),
            ("fulltext-async", "checkpoint")
        );
    }

    #[test]
    fn each_suffix_names_its_field_without_splitting_a_hyphenated_lane() {
        assert_eq!(
            field_name("fulltext-async-temp"),
            ("fulltext-async", "pending")
        );
        assert_eq!(
            field_name("fulltext-async-lease"),
            ("fulltext-async", "lease")
        );
        assert_eq!(
            field_name("fulltext-async-LastIndexedTo"),
            ("fulltext-async", "last-indexed-to")
        );
    }

    #[test]
    fn a_checkpoint_reference_is_recognized_only_in_oaks_shape() {
        assert!(is_checkpoint_reference(
            "8b3d5f2a-1c4e-4a7b-9f01-2d3e4f5a6b7c"
        ));
        // An ordinary string property on /:async must not be reported as a
        // dangling checkpoint just because the checkpoint set lacks it.
        assert!(!is_checkpoint_reference("async"));
        assert!(!is_checkpoint_reference("not-a-checkpoint"));
        assert!(!is_checkpoint_reference("2026-08-17T10:00:00.000Z"));
        assert!(!is_checkpoint_reference(""));
        // Right characters, no groups.
        assert!(!is_checkpoint_reference("8b3d5f2a1c4e4a7b9f012d3e4f5a6b7c"));
        // Right group count, wrong widths.
        assert!(!is_checkpoint_reference(
            "8b3d5f2-1c4e-4a7b-9f01-2d3e4f5a6b7c"
        ));
        // Right shape, non-hexadecimal.
        assert!(!is_checkpoint_reference(
            "8b3d5f2a-1c4e-4a7b-9f01-2d3e4f5a6b7z"
        ));
    }
}
