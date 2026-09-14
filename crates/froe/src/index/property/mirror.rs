//! The content-mirror strategy's storage: `:index/<key>/<path elements…>`
//! with `match = true` on the node the indexed path addresses.
//!
//! `docs/analysis/index-property-storage.md` §2 and §7 specify the shape and
//! the two rules a reader gets wrong:
//!
//! * **`match` lands on interior nodes.** The insert descends every path
//!   element and sets the property unconditionally, so a node carrying
//!   `match` may also have children — whenever a shorter indexed path shares
//!   the key with a longer one. The real Sling fixture has this case.
//! * **The key level is not the content level.** Every direct child of
//!   `:index` is a key whatever its name, because the empty value's key is
//!   the hidden name `:`. Hidden-name filtering belongs strictly *below* the
//!   key level, where names are content path elements and Oak's visible
//!   editor never indexed a hidden one — except the `:count_*` approximate
//!   counters, which are counted and reported rather than treated as
//!   content.
//!
//! The reference index uses this same strategy under `:references` and
//! `:weakreferences`, with the referenced identifier as the key — unencoded —
//! and the *property* path, made relative by stripping the leading `/`, as
//! the mirrored path (§8).

use std::collections::HashSet;

use crate::content::node::NodeState;
use crate::index::{IndexError, IndexResult, strict_boolean};
use crate::segment::record::RecordIdentifier;

/// The prefix Oak's approximate counter writes its randomized properties
/// under.
pub const APPROXIMATE_COUNT_PREFIX: &str = ":count_";

/// One entry of a mirror index.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MirrorEntry {
    /// The key node's name: a URL-encoded value for a property index, an
    /// unencoded identifier for a reference index.
    pub key: String,
    /// The path the entry indexes, relative to the key node and with a
    /// leading `/`. For a property index this is the indexed node's absolute
    /// path; for a reference index it is the referencing property's path
    /// made relative, so it has no leading `/` in Oak's storage and gains
    /// one here for a uniform rendering — [`MirrorEntry::content_path`]
    /// gives the caller the form it needs.
    pub path: String,
}

impl MirrorEntry {
    /// The path as a content path: `/` for an entry on the key node itself,
    /// and the stored path otherwise.
    #[must_use]
    pub fn content_path(&self) -> &str {
        if self.path.is_empty() {
            "/"
        } else {
            &self.path
        }
    }
}

/// A mirror-strategy storage subtree, opened at its hidden child.
pub struct MirrorIndex<'provider> {
    entries_node: NodeState<'provider>,
    definition_path: String,
    child_name: String,
}

impl<'provider> MirrorIndex<'provider> {
    /// Opens the mirror subtree `child_name` under a definition node, or
    /// `None` when the child is absent.
    ///
    /// Absence is a legal state rather than a defect for a reference index:
    /// Oak creates `:references` and `:weakreferences` lazily, on the first
    /// insert, so a store with no weak references has no `:weakreferences`
    /// (§8.4). A `property` definition's `:index` is the exception — the
    /// editor creates it unconditionally — and the caller that knows which
    /// it is holds that distinction.
    pub fn open(
        definition: &NodeState<'provider>,
        definition_path: &str,
        child_name: &str,
    ) -> IndexResult<Option<Self>> {
        Ok(definition.child_node(child_name)?.map(|entries_node| Self {
            entries_node,
            definition_path: definition_path.to_owned(),
            child_name: child_name.to_owned(),
        }))
    }

    /// The hidden child this index was opened at.
    #[must_use]
    pub fn child_name(&self) -> &str {
        &self.child_name
    }

    /// Visits every `match = true` entry, streaming, in key order and then
    /// in path-element byte order within a key.
    ///
    /// The walk is depth-bounded by an exact set of the node records on the
    /// current root-to-node path, so a corrupt child pointer back into an
    /// ancestor is refused at the record that closes the cycle rather than
    /// followed forever. A legitimately shared subtree elsewhere is not a
    /// cycle and is still visited.
    pub fn for_each_entry(
        &self,
        mut visit: impl FnMut(&MirrorEntry) -> IndexResult<()>,
    ) -> IndexResult<()> {
        for (key, key_node) in sorted_children(&self.entries_node)? {
            let mut records_on_path = HashSet::new();
            self.walk_key(&key, &key_node, "", &mut records_on_path, &mut visit)?;
        }
        Ok(())
    }

    /// Every entry, collected and sorted by `(key, path elements)`.
    ///
    /// The element-wise order is what plan 0007's builder requires and
    /// differs from a plain sort of whole paths whenever one indexed path is
    /// a strict prefix of another: `/a` sorts before `/a/b`, and `/a-b`
    /// sorts after both, because the comparison is per element rather than
    /// over the joined string.
    pub fn entries(&self) -> IndexResult<Vec<MirrorEntry>> {
        let mut entries = Vec::new();
        self.for_each_entry(|entry| {
            entries.push(entry.clone());
            Ok(())
        })?;
        Ok(entries)
    }

    /// How many entries one key holds, without materializing them.
    pub fn count_for_key(&self, key: &str) -> IndexResult<u64> {
        let Some(key_node) = self.entries_node.child_node(key)? else {
            return Ok(0);
        };
        let mut count = 0u64;
        let mut records_on_path = HashSet::new();
        self.walk_key(key, &key_node, "", &mut records_on_path, &mut |_| {
            count += 1;
            Ok(())
        })?;
        Ok(count)
    }

    /// The key names, in stored order — every direct child of the hidden
    /// node, whatever its name.
    pub fn keys(&self) -> IndexResult<Vec<String>> {
        Ok(self
            .entries_node
            .child_node_entries()?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    /// How many `:count_*` approximate counters the subtree carries, on the
    /// hidden node itself and on its key nodes — the two places the mirror
    /// strategy writes them.
    ///
    /// They are randomized in name, presence and value, so no rebuild can
    /// reproduce them; counting them is what lets a report say how much a
    /// comparison excused.
    pub fn approximate_counter_count(&self) -> IndexResult<usize> {
        let mut count = approximate_counters(&self.entries_node)?.len();
        for (_, key_node) in self.entries_node.child_node_entries()? {
            count += approximate_counters(&key_node)?.len();
        }
        Ok(count)
    }

    fn walk_key(
        &self,
        key: &str,
        key_node: &NodeState<'provider>,
        path: &str,
        records_on_path: &mut HashSet<RecordIdentifier>,
        visit: &mut impl FnMut(&MirrorEntry) -> IndexResult<()>,
    ) -> IndexResult<()> {
        let record = key_node.record_identifier();
        if !records_on_path.insert(record) {
            return Err(IndexError::Record(crate::Error::InvalidFormat {
                details: format!(
                    "the index storage of {} at {}/{key} contains node record {record} in its \
                     own subtree; the node records form a cycle",
                    self.definition_path, self.child_name
                ),
            }));
        }
        if strict_boolean(key_node.property("match")?.as_ref()) {
            visit(&MirrorEntry {
                key: key.to_owned(),
                path: path.to_owned(),
            })?;
        }
        for (name, child) in sorted_children(key_node)? {
            // Below the key level the names are content path elements, and
            // Oak's visible editor never indexed a hidden one.
            if name.starts_with(':') {
                continue;
            }
            self.walk_key(
                key,
                &child,
                &format!("{path}/{name}"),
                records_on_path,
                visit,
            )?;
        }
        records_on_path.remove(&record);
        Ok(())
    }
}

/// A node's children sorted by name as byte strings, which is what makes the
/// entry order element-wise rather than a sort of joined paths.
fn sorted_children<'provider>(
    node: &NodeState<'provider>,
) -> IndexResult<Vec<(String, NodeState<'provider>)>> {
    let mut children = node.child_node_entries()?;
    children.sort_by(|first, second| first.0.as_bytes().cmp(second.0.as_bytes()));
    Ok(children)
}

/// The `:count_*` property names on a node, sorted.
pub fn approximate_counters(node: &NodeState<'_>) -> IndexResult<Vec<String>> {
    let mut names: Vec<String> = node
        .properties()?
        .into_iter()
        .map(|property| property.name)
        .filter(|name| name.starts_with(APPROXIMATE_COUNT_PREFIX))
        .collect();
    names.sort();
    Ok(names)
}
