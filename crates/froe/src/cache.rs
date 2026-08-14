//! A byte-budgeted cache shared by the read and write paths.
//!
//! Every cache in froe holds a memo: something derived from bytes that are
//! already on disk, kept only so the next lookup does not re-derive it. None
//! of them is a correctness property, so all of them may evict. What matters
//! is that the ceiling is expressed in the unit an operator can reason about.
//!
//! Counting entries does not do that. A parsed segment's resident size is a
//! function of how many records the segment happens to hold, which varies by
//! two orders of magnitude across a real store, so an entry cap that behaves
//! on one repository is a multi-gigabyte cap on another. The budget here is
//! in bytes, and each cached type reports its own approximate weight.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Arc;

/// Approximate resident cost of one cached value, in bytes.
///
/// Approximate is sufficient and deliberate: the number decides when to
/// evict, not what is correct. Implementations account for the heap a value
/// owns, since that is what the budget exists to bound; the handful of bytes
/// of inline struct is noise beside a record table.
pub(crate) trait CacheWeight {
    /// This value's contribution to its cache's byte budget.
    fn cache_weight(&self) -> usize;
}

/// Per-entry overhead charged on top of the value's own weight: the key, the
/// map slot, and the eviction-order entry. Without it a cache of very small
/// values would be budgeted at a fraction of what it actually occupies.
const ENTRY_OVERHEAD_BYTES: usize = 64;

/// A cache with a byte ceiling, evicting in insertion order.
///
/// Insertion order rather than access order: eviction is O(1) with no
/// bookkeeping on the hot read, and froe's access pattern is a traversal —
/// broadly forward, rarely returning to what it finished with. A recency
/// cache would cost more on every hit than it saves on the rare revisit.
pub(crate) struct BoundedCache<Key, Value> {
    entries: HashMap<Key, (Value, usize)>,
    insertion_order: VecDeque<Key>,
    used_bytes: usize,
    budget_bytes: usize,
}

impl<Key: Eq + Hash + Clone, Value: Clone + CacheWeight> BoundedCache<Key, Value> {
    /// A cache holding at most `budget_bytes` of values.
    ///
    /// A zero budget is a valid choice and disables caching entirely: every
    /// insert is immediately evicted, every lookup misses, and the derived
    /// value is recomputed from the mapping. That is the configuration a
    /// memory-constrained host wants, and it must not be a special case.
    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            used_bytes: 0,
            budget_bytes,
        }
    }

    pub(crate) fn get(&self, key: &Key) -> Option<Value> {
        self.entries.get(key).map(|(value, _)| value.clone())
    }

    pub(crate) fn insert(&mut self, key: Key, value: Value) {
        let weight = value.cache_weight().saturating_add(ENTRY_OVERHEAD_BYTES);
        // Re-inserting a key already present replaces it rather than
        // double-counting its weight; the eviction queue already names it.
        if let Some((existing, existing_weight)) = self.entries.get_mut(&key) {
            self.used_bytes = self.used_bytes.saturating_sub(*existing_weight);
            *existing = value;
            *existing_weight = weight;
            self.used_bytes = self.used_bytes.saturating_add(weight);
        } else {
            self.entries.insert(key.clone(), (value, weight));
            self.insertion_order.push_back(key);
            self.used_bytes = self.used_bytes.saturating_add(weight);
        }
        self.evict_to_budget();
    }

    /// Evicts until the budget holds, which for a value bigger than the
    /// whole budget means evicting the value itself: the ceiling is honoured
    /// exactly rather than being exceeded by one outsized entry. Nothing is
    /// lost by that — the caller already holds the value it inserted, and a
    /// later lookup simply re-derives it. The empty check is a termination
    /// guard, not a policy.
    fn evict_to_budget(&mut self) {
        while self.used_bytes > self.budget_bytes {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            if let Some((_, weight)) = self.entries.remove(&oldest) {
                self.used_bytes = self.used_bytes.saturating_sub(weight);
            }
        }
    }

    /// Drops every entry, releasing the budget immediately.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.insertion_order.clear();
        self.used_bytes = 0;
    }

    /// Bytes currently charged against the budget.
    #[cfg(test)]
    pub(crate) fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// Entries currently resident. Cache residency is an implementation
    /// detail everywhere except in the tests that pin eviction behaviour.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is currently cached.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl CacheWeight for Arc<crate::segment::parsed_segment::ParsedSegment> {
    fn cache_weight(&self) -> usize {
        self.as_ref().cache_weight()
    }
}

impl CacheWeight for crate::segment::parsed_segment::ParsedSegment {
    fn cache_weight(&self) -> usize {
        // The two owned tables dominate; everything else is inline scalars.
        std::mem::size_of::<Self>()
            .saturating_add(
                self.record_table()
                    .len()
                    .saturating_mul(std::mem::size_of::<
                        crate::segment::parsed_segment::RecordTableEntry,
                    >()),
            )
            .saturating_add(
                self.referenced_segments
                    .len()
                    .saturating_mul(std::mem::size_of::<
                        crate::segment::identifier::SegmentIdentifier,
                    >()),
            )
    }
}

impl CacheWeight for Arc<str> {
    fn cache_weight(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.len())
    }
}

impl CacheWeight for Arc<crate::content::template::Template> {
    fn cache_weight(&self) -> usize {
        let template = self.as_ref();
        let mut bytes = std::mem::size_of::<crate::content::template::Template>();
        bytes = bytes.saturating_add(template.primary_type.as_ref().map_or(0, String::len));
        for mixin in &template.mixin_types {
            bytes = bytes.saturating_add(mixin.len()).saturating_add(24);
        }
        for property in &template.properties {
            bytes = bytes
                .saturating_add(property.name.len())
                .saturating_add(std::mem::size_of::<
                    crate::content::template::PropertyTemplate,
                >());
        }
        bytes
    }
}

/// A membership memo carries no value; the key and the slot are the cost.
impl CacheWeight for () {
    fn cache_weight(&self) -> usize {
        0
    }
}

/// A record-to-record memo: both sides are fixed-size identifiers.
impl CacheWeight for crate::segment::record::RecordIdentifier {
    fn cache_weight(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// A subtree height memo: the key dominates, the value is one word.
impl CacheWeight for usize {
    fn cache_weight(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// Bytes of segment payload held by a session writer awaiting read-back.
impl CacheWeight for Arc<Vec<u8>> {
    fn cache_weight(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.len())
    }
}

/// A pair is weighed as the sum of its parts, which is what the session
/// read-back cache holds: a parsed structure beside the payload it parsed.
impl<Left: CacheWeight, Right: CacheWeight> CacheWeight for (Left, Right) {
    fn cache_weight(&self) -> usize {
        self.0.cache_weight().saturating_add(self.1.cache_weight())
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundedCache, CacheWeight};

    #[derive(Clone)]
    struct Weighed(usize);

    impl CacheWeight for Weighed {
        fn cache_weight(&self) -> usize {
            self.0
        }
    }

    #[test]
    fn a_cache_evicts_oldest_first_until_it_is_within_its_byte_budget() {
        let mut cache = BoundedCache::new(3 * (100 + 64));
        for key in 0..3u32 {
            cache.insert(key, Weighed(100));
        }
        assert_eq!(cache.len(), 3);

        cache.insert(3, Weighed(100));

        assert_eq!(cache.len(), 3, "the fourth entry displaces the first");
        assert!(cache.get(&0).is_none(), "the oldest entry was evicted");
        for key in 1..4u32 {
            assert!(cache.get(&key).is_some(), "entry {key} must survive");
        }
        assert!(cache.used_bytes() <= 3 * (100 + 64));
    }

    #[test]
    fn a_large_value_is_charged_by_its_weight_not_its_count() {
        let mut cache = BoundedCache::new(1000);
        cache.insert(0, Weighed(10));
        cache.insert(1, Weighed(900));

        // The big entry alone nearly fills the budget, so the small one it
        // displaced is gone even though the cache holds only two entries.
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&1).is_some());
    }

    #[test]
    fn a_value_larger_than_the_whole_budget_is_not_retained_at_all() {
        let mut cache = BoundedCache::new(64);
        cache.insert(0, Weighed(4096));

        // The budget is a ceiling, not a suggestion: rather than exceed it
        // by one outsized entry, the cache declines to hold that entry. It
        // must also not spin trying.
        assert_eq!(cache.len(), 0);
        assert!(cache.get(&0).is_none());
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    fn a_zero_budget_disables_caching_without_a_special_case() {
        let mut cache = BoundedCache::new(0);
        cache.insert(0, Weighed(1));

        assert_eq!(cache.len(), 0);
        assert!(cache.get(&0).is_none());
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    fn reinserting_a_key_replaces_its_value_and_weight_rather_than_accumulating() {
        let mut cache = BoundedCache::new(10_000);
        cache.insert(0, Weighed(100));
        let after_first = cache.used_bytes();
        cache.insert(0, Weighed(100));

        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.used_bytes(),
            after_first,
            "a replacing insert must not double-charge the budget"
        );

        cache.insert(0, Weighed(7));
        assert_eq!(
            cache.get(&0).expect("entry present").0,
            7,
            "reinsertion updates the value"
        );
        assert!(cache.used_bytes() < after_first, "and its weight");
    }

    #[test]
    fn clearing_releases_the_whole_budget() {
        let mut cache = BoundedCache::new(10_000);
        for key in 0..10u32 {
            cache.insert(key, Weighed(100));
        }
        cache.clear();

        assert_eq!(cache.len(), 0);
        assert_eq!(cache.used_bytes(), 0);
        assert!(cache.get(&0).is_none());
    }
}
