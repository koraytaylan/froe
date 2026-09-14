//! The counter index: the `:cnt` map Oak's approximate descendant-node
//! counter maintains, and the estimated node count read back from it.
//!
//! `docs/analysis/index-property-storage.md` §9 specifies the editor and §10
//! the estimate, with the constants each actually uses. [`sip_hash`] holds
//! the hash that makes the map deterministic given a `seed`.
//!
//! Two things a reader gets wrong by assuming they are simpler than they are:
//!
//! * **`:cnt` counts descendants, not the node.** The hit test runs on an
//!   added child's own hash and the increment lands on every *strict*
//!   ancestor, so `:index/<p>/:cnt` is a count of what is below `p`.
//! * **A mirror node with no `:cnt` is legal.** `leaveNew` removes the
//!   property when a count reaches zero but never the node — the branch that
//!   would remove the node is unreachable, because `getChildNodeCount` never
//!   returns a negative number — so an incrementally maintained index
//!   accumulates such nodes after deletions. They are reported with an
//!   *absent* count rather than a zero, so a rebuild's oracle can find them.
//!
//! The definition's `resolution` plays no part in the estimate at any step;
//! the only resolution the estimate uses is the approximate counter's own
//! constant of 100.

pub mod sip_hash;

use std::collections::HashSet;

use crate::content::node::NodeState;
use crate::index::definition::{DEFAULT_COUNTER_RESOLUTION, IndexDefinition};
use crate::index::property::mirror::APPROXIMATE_COUNT_PREFIX;
use crate::index::{IndexError, IndexResult, converting_long, values_of};
use crate::segment::record::RecordIdentifier;

pub use sip_hash::{SipHash, hash_for_path, narrowed_seed};

/// The property the hashed counter writes.
pub const COUNT_HASH_PROPERTY_NAME: &str = ":cnt";

/// The property the *old* counter wrote. A store an older Oak maintained can
/// hold both, and the estimate adds them, which is why the reader does too.
pub const COUNT_PROPERTY_NAME: &str = ":count";

/// The approximate counter's own resolution, which is the only resolution the
/// estimate uses — never the definition's.
pub const APPROXIMATE_COUNT_RESOLUTION: i64 = 100;

/// Which bound the caller wants, which is Oak's `max` flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CountBound {
    /// The expected count.
    Expected,
    /// The maximum expected count: the stored value plus the approximate
    /// counter's resolution.
    Maximum,
}

impl CountBound {
    /// What the bound adds to a stored count.
    const fn addend(self) -> i64 {
        match self {
            CountBound::Expected => 0,
            CountBound::Maximum => APPROXIMATE_COUNT_RESOLUTION,
        }
    }
}

/// What the estimate answered.
///
/// Oak returns `-1` for "unknown" and a placeholder number — `0` or `2000` —
/// for "the sampling counter never recorded this path". Neither placeholder
/// counts anything, so froe returns the verdict instead of the number and
/// lets the caller decide; `froe index check` needs to tell `Fallback` from a
/// real count of zero, because only one of them makes a derived budget
/// meaningless.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeCountEstimate {
    /// No counter index, or one with no data node: Oak's `-1`.
    Unknown,
    /// The counter has data, but nothing recorded for this path. Oak answers
    /// `2000` under the maximum bound and `0` otherwise; neither is a count.
    Fallback,
    /// A real estimate.
    Count(u64),
}

/// One node of the counter's mirror map.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct CounterEntry {
    /// The content path this node counts the descendants of.
    pub path: String,
    /// The combined `:cnt` and `:count`, or `None` when the node carries
    /// neither — which is a legal state after deletions, not a zero.
    pub count: Option<i64>,
}

/// A counter definition's storage, opened at its data child.
pub struct CounterIndex<'provider> {
    definition_node: NodeState<'provider>,
    definition_path: String,
    resolution: i64,
    seed: i64,
}

impl<'provider> CounterIndex<'provider> {
    /// Opens the counter storage of `definition`.
    ///
    /// `resolution` and `seed` are read from the model, which read them
    /// **converting** to `LONG` as `NodeCounterEditorProvider` does, so a
    /// `STRING` `"500"` counts. The seed is narrowed here rather than by the
    /// caller, because every use of it after the creating run is narrowed and
    /// a caller holding the raw value would be holding a trap.
    #[must_use]
    pub fn open(definition_node: NodeState<'provider>, definition: &IndexDefinition) -> Self {
        Self {
            definition_node,
            definition_path: definition.path.clone(),
            resolution: definition.resolution.unwrap_or(DEFAULT_COUNTER_RESOLUTION),
            seed: narrowed_seed(definition.seed.unwrap_or(0)),
        }
    }

    /// The definition's `resolution`, defaulting to 1000.
    #[must_use]
    pub const fn resolution(&self) -> i64 {
        self.resolution
    }

    /// The seed as every run after the creating one uses it.
    #[must_use]
    pub const fn seed(&self) -> i64 {
        self.seed
    }

    /// The bit mask the hit test applies: the highest set bit of
    /// `resolution` doubled, then reduced by one. For the default 1000 that
    /// is `(512 * 2) - 1 = 1023`, and the increment is `1024`.
    #[must_use]
    pub const fn bit_mask(&self) -> i32 {
        let resolution = self.resolution as i32;
        (highest_one_bit(resolution).wrapping_mul(2)).wrapping_sub(1)
    }

    /// Whether the node at `path` is one the counter samples, which is the
    /// test `childNodeAdded` performs on the added child's own hash.
    #[must_use]
    pub fn is_sampled(&self, path: &str) -> bool {
        hash_for_path(self.seed, path).hash_code() & self.bit_mask() == 0
    }

    /// Every node of the mirror map, with its combined count.
    ///
    /// The walk carries an exact set of the node records on the current
    /// root-to-node path, so a corrupt child pointer back into an ancestor is
    /// refused at the record that closes the cycle.
    pub fn entries(&self) -> IndexResult<Vec<CounterEntry>> {
        let mut entries = Vec::new();
        for (name, data_node) in self.definition_node.child_node_entries()? {
            if !is_data_node_name(&name) {
                continue;
            }
            let mut records_on_path = HashSet::new();
            self.walk(&data_node, "/", &mut records_on_path, &mut entries)?;
        }
        entries.sort();
        Ok(entries)
    }

    fn walk(
        &self,
        node: &NodeState<'provider>,
        path: &str,
        records_on_path: &mut HashSet<RecordIdentifier>,
        entries: &mut Vec<CounterEntry>,
    ) -> IndexResult<()> {
        let record = node.record_identifier();
        if !records_on_path.insert(record) {
            return Err(IndexError::Record(crate::Error::InvalidFormat {
                details: format!(
                    "the counter storage of {} contains node record {record} in its own \
                     subtree; the node records form a cycle",
                    self.definition_path
                ),
            }));
        }
        entries.push(CounterEntry {
            path: path.to_owned(),
            count: combined_count(node)?,
        });
        for (name, child) in node.child_node_entries()? {
            let child_path = if path == "/" {
                format!("/{name}")
            } else {
                format!("{path}/{name}")
            };
            self.walk(&child, &child_path, records_on_path, entries)?;
        }
        records_on_path.remove(&record);
        Ok(())
    }

    /// Whether the definition has any data node at all, which is Oak's
    /// `dataNodeExists`.
    pub fn has_data_node(&self) -> IndexResult<bool> {
        Ok(self
            .definition_node
            .child_node_entries()?
            .iter()
            .any(|(name, _)| is_data_node_name(name)))
    }
}

/// The estimated number of nodes below `path`, following
/// `NodeCounter.doGetEstimatedNodeCount` — the default path, reached when
/// both of the switches §1 records sit at their defaults.
///
/// The rules, in Oak's order:
///
/// 1. `Count(0)` when the target node does not exist;
/// 2. under [`CountBound::Expected`] only, the target node's own approximate
///    count when it has one — the branch that answers for a property index's
///    `:index` node;
/// 3. under **both** bounds, the target node's combined `:cnt` and `:count`
///    when either is present, plus the bound's addend;
/// 4. [`NodeCountEstimate::Unknown`] when the definition literally named
///    `counter` is absent or has no data node — Oak consults that one name,
///    not a definition of type `counter`;
/// 5. otherwise the sum, across every `:index` and `:<mount>-index` child, of
///    the combined count of the node reached by descending `path`'s elements
///    under it, plus the bound's addend — so a non-root `path` answers for
///    that subtree rather than for the store;
/// 6. and [`NodeCountEstimate::Fallback`] when that sum is zero.
pub fn estimated_node_count(
    content_root: &NodeState<'_>,
    path: &str,
    bound: CountBound,
) -> IndexResult<NodeCountEstimate> {
    let Some(target) = descend(content_root, path)? else {
        return Ok(NodeCountEstimate::Count(0));
    };
    if bound == CountBound::Expected
        && let Some(approximate) = approximate_count(&target)?
    {
        return Ok(non_negative(approximate));
    }
    if let Some(combined) = combined_count(&target)? {
        return Ok(non_negative(combined + bound.addend()));
    }
    let counter = content_root
        .child_node(crate::index::INDEX_DEFINITIONS_NAME)?
        .and_then(|index| index.child_node("counter").transpose())
        .transpose()?;
    let Some(counter) = counter else {
        return Ok(NodeCountEstimate::Unknown);
    };
    let data_children: Vec<NodeState<'_>> = counter
        .child_node_entries()?
        .into_iter()
        .filter(|(name, _)| is_data_node_name(name))
        .map(|(_, node)| node)
        .collect();
    if data_children.is_empty() {
        return Ok(NodeCountEstimate::Unknown);
    }
    let mut sum = 0i64;
    for data_node in data_children {
        if let Some(node) = descend(&data_node, path)?
            && let Some(combined) = combined_count(&node)?
        {
            sum += combined;
        }
    }
    if sum == 0 {
        return Ok(NodeCountEstimate::Fallback);
    }
    Ok(non_negative(sum + bound.addend()))
}

fn non_negative(value: i64) -> NodeCountEstimate {
    NodeCountEstimate::Count(u64::try_from(value).unwrap_or(0))
}

/// A node's combined `:cnt` and `:count`, or `None` when it carries neither.
///
/// This is `NodeCounter.getCombinedCountIfAvailable`: both are read
/// converting to `LONG`, and the two are *added* rather than one preferred,
/// so an index an old-counter Oak maintained and a new one has since touched
/// still estimates correctly.
fn combined_count(node: &NodeState<'_>) -> IndexResult<Option<i64>> {
    let hashed = converting_long(node.property(COUNT_HASH_PROPERTY_NAME)?.as_ref());
    let old = converting_long(node.property(COUNT_PROPERTY_NAME)?.as_ref());
    Ok(match (hashed, old) {
        (None, None) => None,
        (hashed, old) => Some(hashed.unwrap_or(0) + old.unwrap_or(0)),
    })
}

/// `ApproximateCounter.getCountSync`: `None` when the node carries no
/// `:count_*` property at all, and `max(added / 2, added - removed)`
/// otherwise.
fn approximate_count(node: &NodeState<'_>) -> IndexResult<Option<i64>> {
    let mut found = false;
    let mut added = 0i64;
    let mut removed = 0i64;
    for property in node.properties()? {
        if !property.name.starts_with(APPROXIMATE_COUNT_PREFIX) {
            continue;
        }
        found = true;
        let value = values_of(&property)
            .first()
            .and_then(crate::content::property::PropertyValue::as_text)
            .and_then(|text| text.parse::<i64>().ok())
            .unwrap_or(0);
        if value > 0 {
            added += value;
        } else {
            removed -= value;
        }
    }
    Ok(found.then(|| (added / 2).max(added - removed)))
}

/// `NodeCounter.isDataNodeName`: `:index`, or a hidden name ending in
/// `-index`, which is what a composite-store mount decorates it to.
fn is_data_node_name(name: &str) -> bool {
    name == crate::index::INDEX_CONTENT_NODE_NAME
        || (name.starts_with(':') && name.ends_with("-index"))
}

/// `NodeCounter.child`: descends `path`'s elements, with `/` resolving to the
/// node itself.
fn descend<'provider>(
    node: &NodeState<'provider>,
    path: &str,
) -> IndexResult<Option<NodeState<'provider>>> {
    let mut current = *node;
    for element in path.split('/').filter(|element| !element.is_empty()) {
        match current.child_node(element)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// `Integer.highestOneBit`: the highest set bit of `value`, or zero.
const fn highest_one_bit(value: i32) -> i32 {
    if value <= 0 {
        return 0;
    }
    1i32 << (value as u32).ilog2()
}

#[cfg(test)]
mod tests {
    use super::{highest_one_bit, narrowed_seed};

    #[test]
    fn the_bit_mask_of_the_default_resolution_is_one_thousand_and_twenty_three() {
        assert_eq!(highest_one_bit(1000), 512);
        assert_eq!(highest_one_bit(1000) * 2 - 1, 1023);
    }

    #[test]
    fn the_highest_one_bit_of_a_power_of_two_is_itself() {
        assert_eq!(highest_one_bit(1024), 1024);
        assert_eq!(highest_one_bit(1), 1);
    }

    #[test]
    fn a_non_positive_resolution_has_no_highest_bit() {
        assert_eq!(highest_one_bit(0), 0);
        assert_eq!(highest_one_bit(-1), 0);
    }

    #[test]
    fn the_stored_seed_is_narrowed_to_thirty_two_bits_sign_extended() {
        // The value the real Sling fixture stores, and what every run after
        // the one that created it actually hashes with.
        assert_eq!(narrowed_seed(-7_610_761_686_379_641_542), -584_039_110);
        assert_eq!(narrowed_seed(1), 1);
        assert_eq!(narrowed_seed(-1), -1);
        assert_eq!(narrowed_seed(i64::from(i32::MIN) - 1), i64::from(i32::MAX));
    }
}
