//! The unique-entry strategy's storage: `:index/<key>` carrying an `entry`
//! `String[]` of absolute paths.
//!
//! `docs/analysis/index-property-storage.md` §3 specifies it. Two facts
//! separate it from the mirror strategy on disk, and a reader that tested for
//! the wrong one would report a whole index as missing:
//!
//! * the path is stored **as a value**, absolute, rather than as a subtree;
//! * the approximate counter is adjusted on the `:index` node **alone** —
//!   the key node carries none.
//!
//! More than one `entry` value is a transient duplicate state Oak sets only
//! while trying to add a duplicate, and then refuses the commit on. One
//! surviving in a committed store is therefore a real defect, which is why
//! it is reported rather than tolerated.

use crate::content::node::NodeState;
use crate::content::property::PropertyValue;
use crate::index::{IndexResult, values_of};

/// The property a unique index stores its paths in.
pub const ENTRY_PROPERTY_NAME: &str = "entry";

/// One key of a unique index.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct UniqueEntry {
    /// The key node's name: the URL-encoded indexed value.
    pub key: String,
    /// The absolute paths stored under it. More than one is a duplicate
    /// state Oak would have refused the commit on.
    pub paths: Vec<String>,
}

impl UniqueEntry {
    /// Whether this key is in the duplicate state Oak refuses commits on.
    #[must_use]
    pub fn is_duplicate(&self) -> bool {
        self.paths.len() > 1
    }
}

/// A unique-entry storage subtree, opened at its hidden child.
pub struct UniqueIndex<'provider> {
    entries_node: NodeState<'provider>,
}

impl<'provider> UniqueIndex<'provider> {
    /// Opens `:index` under a definition node, or `None` when it is absent.
    ///
    /// For a `property` definition absence *is* reportable, unlike the
    /// reference index's lazily created children: the editor's
    /// `checkUniquenessConstraints` creates `:index` unconditionally on every
    /// cycle, so a definition Oak has indexed always has one (§2.3).
    pub fn open(definition: &NodeState<'provider>, child_name: &str) -> IndexResult<Option<Self>> {
        Ok(definition
            .child_node(child_name)?
            .map(|entries_node| Self { entries_node }))
    }

    /// Every key and its paths, in key order.
    ///
    /// There is no walk here and so no cycle bound: the storage is one level
    /// deep by construction, and a key node's children — if a corrupt store
    /// has any — are not part of this strategy's shape and are ignored.
    pub fn entries(&self) -> IndexResult<Vec<UniqueEntry>> {
        let mut entries = Vec::new();
        for (key, key_node) in self.entries_node.child_node_entries()? {
            let Some(property) = key_node.property(ENTRY_PROPERTY_NAME)? else {
                continue;
            };
            entries.push(UniqueEntry {
                key,
                paths: values_of(&property)
                    .iter()
                    .filter_map(PropertyValue::as_text)
                    .collect(),
            });
        }
        entries.sort();
        Ok(entries)
    }

    /// The paths one key holds, or an empty vector when the key is absent.
    pub fn paths_for_key(&self, key: &str) -> IndexResult<Vec<String>> {
        let Some(key_node) = self.entries_node.child_node(key)? else {
            return Ok(Vec::new());
        };
        let Some(property) = key_node.property(ENTRY_PROPERTY_NAME)? else {
            return Ok(Vec::new());
        };
        Ok(values_of(&property)
            .iter()
            .filter_map(PropertyValue::as_text)
            .collect())
    }

    /// How many `:count_*` approximate counters the subtree carries.
    ///
    /// The unique strategy writes them on the `:index` node alone, so this
    /// deliberately does not look at the key nodes: finding one there would
    /// be a store the mirror strategy had written, not this one.
    pub fn approximate_counter_count(&self) -> IndexResult<usize> {
        Ok(super::mirror::approximate_counters(&self.entries_node)?.len())
    }
}
