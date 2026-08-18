//! The exact source-record-keyed memo that preserves the content graph's
//! sharing: each distinct node is copied once, and a checkpoint sharing
//! records with the live root still shares them afterwards.

/// Slots in a fresh table. Grown geometrically, so this only sets the floor.
/// Probing masks with `slots - 1`, so a non-power-of-two would silently make
/// part of the table unreachable; nothing else holds that property.
pub(crate) const INITIAL_MEMO_SLOTS: usize = 1024;

const _: () = assert!(INITIAL_MEMO_SLOTS.is_power_of_two());

/// Source node to its rewritten copy, exactly and without eviction.
///
/// Copying each distinct node once is an invariant, not an optimization: a
/// miss does not cost one extra copy, it re-walks the whole subtree, and
/// misses nest. So this is an open-addressed table over two `Vec<u64>` —
/// no per-entry overhead, no eviction queue, and sixteen bytes a slot
/// against the ~110 a `HashMap<RecordIdentifier, RecordIdentifier>` measures.
/// A packed key of zero marks an empty slot, which [`SegmentInterner`]
/// guarantees no real record can collide with.
pub(crate) struct RewrittenNodes {
    pub(crate) keys: Vec<u64>,
    pub(crate) values: Vec<u64>,
    pub(crate) len: usize,
}

impl RewrittenNodes {
    pub(crate) fn new() -> Self {
        Self {
            keys: vec![0; INITIAL_MEMO_SLOTS],
            values: vec![0; INITIAL_MEMO_SLOTS],
            len: 0,
        }
    }

    /// Fibonacci hashing over the packed key, which is dense in the low bits
    /// (record numbers count up) and in the high bits (segment indices count
    /// up), so the multiply is what spreads both across the probe sequence.
    pub(crate) fn slot_of(&self, key: u64) -> usize {
        let mixed = key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> 32) as usize & (self.keys.len() - 1)
    }

    /// Counts the slots actually holding an entry, independently of `len`.
    pub(crate) fn occupied_slots(&self) -> usize {
        self.keys.iter().filter(|key| **key != 0).count()
    }

    pub(crate) fn get(&self, key: u64) -> Option<u64> {
        let mut slot = self.slot_of(key);
        loop {
            match self.keys[slot] {
                0 => return None,
                found if found == key => return Some(self.values[slot]),
                _ => slot = (slot + 1) & (self.keys.len() - 1),
            }
        }
    }

    pub(crate) fn insert(&mut self, key: u64, value: u64) {
        // Grow at ~70% load, before probe sequences get long.
        if (self.len + 1) * 10 >= self.keys.len() * 7 {
            self.grow();
        }
        self.insert_without_growing(key, value);
        self.len += 1;
    }

    /// Places a key known to be absent. Open addressing has no natural
    /// duplicate check — a second insert of the same key would occupy a
    /// second slot, leaving `len` counting one node twice while `get` still
    /// answered correctly, so the drift would be silent. The walk cannot
    /// produce one (it probes before inserting, and only inserts on the
    /// single path out of a miss), which is exactly why a violation here
    /// means a logic error rather than bad input, and must be loud.
    pub(crate) fn insert_without_growing(&mut self, key: u64, value: u64) {
        let mut slot = self.slot_of(key);
        while self.keys[slot] != 0 {
            assert_ne!(
                self.keys[slot], key,
                "a source record was memoized twice; the memo probe or the \
                 path set is broken"
            );
            slot = (slot + 1) & (self.keys.len() - 1);
        }
        self.keys[slot] = key;
        self.values[slot] = value;
    }

    pub(crate) fn grow(&mut self) {
        let occupied: Vec<(u64, u64)> = self
            .keys
            .iter()
            .zip(&self.values)
            .filter(|(key, _)| **key != 0)
            .map(|(key, value)| (*key, *value))
            .collect();
        self.keys = vec![0; self.keys.len() * 2];
        self.values = vec![0; self.values.len() * 2];
        for (key, value) in occupied {
            self.insert_without_growing(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::writer::compaction::deep_copy_tree_with_progress;
    use crate::writer::compaction::test_support::*;
    use crate::writer::store_writer::WritableRepository;

    #[test]
    fn the_exact_memo_costs_a_bounded_number_of_bytes_a_node() {
        use crate::segment::identifier::SegmentIdentifier;
        use crate::segment::record::RecordIdentifier;
        use crate::writer::compaction::{RewrittenNodes, SegmentInterner};

        // Exactness is only affordable because an entry is two packed u64s
        // rather than two `RecordIdentifier`s: the same map keyed on the
        // 24-byte identifier measures ~110 bytes a node, which is the figure
        // that made an exact memo look impossible. One segment holds many
        // records, so the interner stays small while the memo grows.
        for count in [1_000_000usize, 4_000_000] {
            let mut interner = SegmentInterner::new();
            let mut memo = RewrittenNodes::new();
            for index in 0..count {
                let record = RecordIdentifier {
                    segment: SegmentIdentifier {
                        most_significant_bits: (index / 8192) as u64,
                        least_significant_bits: 0x5eed,
                    },
                    record_number: index as u32,
                };
                let packed = interner.pack(record);
                memo.insert(packed, packed);
                assert_eq!(interner.unpack(packed), record, "packing round-trips");
            }
            // Resident bytes vary with allocator reuse, so the pinned figure
            // is the table's own occupancy: two `u64` vectors over its slots.
            // Resident size tracked it within a few bytes a node when measured
            // in isolation (44 at a million entries, 35 at four million).
            // `len` is a counter, so it would still be right if a growth
            // dropped entries. Retrieval is what actually pins the invariant:
            // the table crosses many growths at these sizes, and losing one
            // entry means re-copying that node's whole subtree.
            for index in 0..count {
                let record = RecordIdentifier {
                    segment: SegmentIdentifier {
                        most_significant_bits: (index / 8192) as u64,
                        least_significant_bits: 0x5eed,
                    },
                    record_number: index as u32,
                };
                let packed = interner.pack(record);
                assert_eq!(
                    memo.get(packed),
                    Some(packed),
                    "entry {index} of {count} survives every growth"
                );
            }
            let bytes_per_node = memo.keys.len() * 2 * std::mem::size_of::<u64>() / count;
            assert_eq!(memo.len, count);
            assert!(
                bytes_per_node <= 48,
                "{count} entries cost {bytes_per_node} bytes a node; the packed \
                 table must stay far below the ~110 an identifier-keyed map costs"
            );
            assert!(
                memo.len * 10 <= memo.keys.len() * 7,
                "the table stays under its load factor"
            );
        }
    }

    #[test]
    fn the_exact_memo_holds_only_what_the_tree_reaches() {
        for fanout in [100usize, 320] {
            let directory = TestDirectory::new(&format!("footprint-{fanout}"));
            build_wide_store(&directory, fanout);
            let store = WritableRepository::open(&directory.path).expect("open");
            let head = store.head();
            let distinct = distinct_reachable_nodes(&store, head);
            let generation = store.writing_generation().expect("generation");
            let mut writer = store.record_writer(generation);
            let (_root, copied) = deep_copy_tree_with_progress(
                &store,
                &mut writer,
                head,
                &mut crate::progress::DiscardedProgress,
            )
            .expect("deep copy");
            writer.finish().expect("finish");
            store.close().expect("close");
            assert_eq!(copied as usize, distinct, "the copy is exact at {fanout}");
        }
    }

    #[test]
    fn the_memo_and_the_interner_hold_their_own_invariants() {
        use crate::segment::identifier::SegmentIdentifier;
        use crate::segment::record::RecordIdentifier;
        use crate::writer::compaction::{RewrittenNodes, SegmentInterner};

        let mut rng = Rng(0x5EED_1234_9ABC_DEF1);
        let mut interner = SegmentInterner::new();
        let mut memo = RewrittenNodes::new();
        let mut expected = std::collections::HashMap::new();
        let mut packed_seen = std::collections::HashMap::new();

        for _ in 0..60_000 {
            let record = RecordIdentifier {
                segment: SegmentIdentifier {
                    most_significant_bits: rng.next() % 400,
                    least_significant_bits: rng.next() % 7,
                },
                record_number: (rng.next() % 5000) as u32,
            };
            let packed = interner.pack(record);

            // The sentinel is never a real key, so an occupied slot is never
            // mistaken for an empty one.
            assert_ne!(packed, 0, "no real record packs to the empty-slot key");
            // Packing is injective: two distinct records never share a key,
            // and a key always unpacks to the record it came from.
            assert_eq!(interner.unpack(packed), record, "packing round-trips");
            if let Some(previous) = packed_seen.insert(packed, record) {
                assert_eq!(previous, record, "two distinct records packed alike");
            }

            if let std::collections::hash_map::Entry::Vacant(slot) = expected.entry(packed) {
                let value = interner.pack(RecordIdentifier {
                    segment: record.segment,
                    record_number: record.record_number ^ 0x00FF_00FF,
                });
                slot.insert(value);
                memo.insert(packed, value);
            }

            // Everything ever inserted is still retrievable, across every
            // growth the table has performed by now.
            assert_eq!(memo.len, expected.len());
            for (key, value) in &expected {
                assert_eq!(memo.get(*key), Some(*value));
            }
            if expected.len() > 40 {
                expected.clear();
                memo = RewrittenNodes::new();
            }
        }
    }
}
