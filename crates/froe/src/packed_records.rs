//! Packed record identifiers, for memos that must be exact.
//!
//! A [`RecordIdentifier`] is a sixteen-byte segment identifier plus a record
//! number, and a `HashMap` entry costs about a hundred bytes more. Interning
//! the segment — a 25 GB store names on the order of 250k of them — packs a
//! record into eight bytes, and an open-addressed table over one `Vec<u64>`
//! carries no per-entry overhead at all.
//!
//! That difference is what makes an *exact* memo affordable where an evicting
//! one was not, and exactness is the point: a memo whose miss merely costs a
//! repeated lookup may be evicted under a byte budget, but a memo whose miss
//! re-walks a whole subtree is carrying an invariant. Both the compaction
//! copier and the node-tree verifier are in the second category — their
//! misses nest, so an evicted entry multiplies the work and the reported
//! counts rather than adding to them.

use crate::segment::{RecordIdentifier, SegmentIdentifier};

/// Maps each segment identifier met during one walk to a small index, so a
/// packed record holds four bytes where a [`SegmentIdentifier`] holds sixteen.
///
/// The cost is per *segment*, not per node, which is why this is affordable
/// where storing the identifier in every entry is not. Index 0 is never
/// issued, so a packed key of zero is unambiguously an empty slot.
pub(crate) struct SegmentInterner {
    indices: std::collections::HashMap<SegmentIdentifier, u32>,
    identifiers: Vec<SegmentIdentifier>,
}

impl SegmentInterner {
    pub(crate) fn new() -> Self {
        Self {
            indices: std::collections::HashMap::new(),
            // Index 0 is the never-issued sentinel; this placeholder keeps
            // `identifiers[index]` addressable without an offset everywhere.
            identifiers: vec![SegmentIdentifier {
                most_significant_bits: 0,
                least_significant_bits: 0,
            }],
        }
    }

    fn index_of(&mut self, segment: SegmentIdentifier) -> u32 {
        if let Some(index) = self.indices.get(&segment) {
            return *index;
        }
        let index = u32::try_from(self.identifiers.len()).expect("segments per walk fit u32");
        self.identifiers.push(segment);
        self.indices.insert(segment, index);
        index
    }

    /// The index already issued for `segment`, without issuing a new one.
    /// A lookup for a segment never seen can answer "absent" without growing
    /// the interner, which keeps membership tests on a shared `&self`.
    fn existing_index_of(&self, segment: SegmentIdentifier) -> Option<u32> {
        self.indices.get(&segment).copied()
    }

    fn identifier(&self, index: u32) -> SegmentIdentifier {
        self.identifiers[index as usize]
    }

    /// Packs an interned record into the eight bytes a table stores.
    pub(crate) fn pack(&mut self, record: RecordIdentifier) -> u64 {
        u64::from(self.index_of(record.segment)) << 32 | u64::from(record.record_number)
    }

    /// Packs a record whose segment is already interned. `None` means no
    /// record in that segment has ever been packed, so no table built on this
    /// interner can hold it.
    fn pack_existing(&self, record: RecordIdentifier) -> Option<u64> {
        self.existing_index_of(record.segment)
            .map(|index| u64::from(index) << 32 | u64::from(record.record_number))
    }

    pub(crate) fn unpack(&self, packed: u64) -> RecordIdentifier {
        RecordIdentifier {
            segment: self.identifier((packed >> 32) as u32),
            record_number: packed as u32,
        }
    }
}

/// Slots in a fresh table. Grown geometrically, so this only sets the floor.
/// Probing masks with `slots - 1`, so a non-power-of-two would silently make
/// part of the table unreachable; nothing else holds that property.
const INITIAL_TABLE_SLOTS: usize = 1024;
const _: () = assert!(INITIAL_TABLE_SLOTS.is_power_of_two());

/// Fibonacci hashing over a packed key, which is dense in the low bits
/// (record numbers count up) and in the high bits (segment indices count up),
/// so the multiply is what spreads both across the probe sequence.
fn slot_of(key: u64, slots: usize) -> usize {
    let mixed = key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (mixed >> 32) as usize & (slots - 1)
}

/// An exact, non-evicting set of record identifiers.
///
/// Eight bytes a slot at a ~70% load factor, so about 11.4 bytes a member,
/// against the ~110 a `HashMap<RecordIdentifier, ()>` measures and the ~68 a
/// `BoundedCache` entry costs. There is no capacity: the set holds what the
/// walk reaches, which is the only size at which the count it supports is
/// correct.
pub(crate) struct PackedRecordSet {
    segments: SegmentInterner,
    keys: Vec<u64>,
    len: usize,
}

impl PackedRecordSet {
    pub(crate) fn new() -> Self {
        Self {
            segments: SegmentInterner::new(),
            keys: vec![0; INITIAL_TABLE_SLOTS],
            len: 0,
        }
    }

    /// Whether `record` was inserted. Takes `&self`: a record whose segment
    /// was never interned is absent by construction, so a membership test
    /// never has to grow the interner.
    pub(crate) fn contains(&self, record: RecordIdentifier) -> bool {
        let Some(key) = self.segments.pack_existing(record) else {
            return false;
        };
        let mut slot = slot_of(key, self.keys.len());
        loop {
            match self.keys[slot] {
                0 => return false,
                found if found == key => return true,
                _ => slot = (slot + 1) & (self.keys.len() - 1),
            }
        }
    }

    /// Inserts `record`, which the caller has already proved absent.
    ///
    /// Open addressing has no natural duplicate check — a second insert of
    /// the same record would occupy a second slot, leaving `len` counting one
    /// record twice while `contains` still answered correctly, so the drift
    /// would be silent. A walk that probes before inserting cannot produce
    /// one, which is exactly why a violation here is a logic error rather
    /// than bad input, and must be loud.
    pub(crate) fn insert(&mut self, record: RecordIdentifier) {
        // Grow at ~70% load, before probe sequences get long.
        if (self.len + 1) * 10 >= self.keys.len() * 7 {
            self.grow();
        }
        let key = self.segments.pack(record);
        self.place(key);
        self.len += 1;
    }

    fn place(&mut self, key: u64) {
        let mut slot = slot_of(key, self.keys.len());
        while self.keys[slot] != 0 {
            assert_ne!(
                self.keys[slot], key,
                "a record was certified twice; the probe or the caller's path \
                 set is broken"
            );
            slot = (slot + 1) & (self.keys.len() - 1);
        }
        self.keys[slot] = key;
    }

    fn grow(&mut self) {
        let occupied: Vec<u64> = self.keys.iter().copied().filter(|key| *key != 0).collect();
        self.keys = vec![0; self.keys.len() * 2];
        for key in occupied {
            self.place(key);
        }
    }

    /// How many distinct records the set holds.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Counts the slots actually holding an entry, independently of `len`.
    /// Comparing the two compares a counter against the storage rather than
    /// against itself, so a growth that lost entries cannot hide.
    #[cfg(test)]
    pub(crate) fn occupied_slots(&self) -> usize {
        self.keys.iter().filter(|key| **key != 0).count()
    }
}

#[cfg(test)]
mod tests {
    use super::PackedRecordSet;
    use crate::segment::{RecordIdentifier, SegmentIdentifier};

    fn record(segment_seed: u64, record_number: u32) -> RecordIdentifier {
        RecordIdentifier {
            segment: SegmentIdentifier {
                most_significant_bits: segment_seed.wrapping_mul(0x0123_4567_89AB_CDEF),
                least_significant_bits: segment_seed ^ 0xDEAD_BEEF_0BAD_F00D,
            },
            record_number,
        }
    }

    /// The oracle is `std::collections::HashSet`, which shares no code with
    /// the table under test: every insertion and every membership question is
    /// asked of both and the answers must agree.
    #[test]
    fn a_packed_set_agrees_with_an_independent_hash_set() {
        let mut packed = PackedRecordSet::new();
        let mut oracle: std::collections::HashSet<RecordIdentifier> =
            std::collections::HashSet::new();

        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..200_000 {
            // Heavy duplication: a small segment and record-number domain, so
            // the same record recurs constantly and every probe path is used.
            let candidate = record(next() % 64, (next() % 4096) as u32);
            assert_eq!(
                packed.contains(candidate),
                oracle.contains(&candidate),
                "membership disagreed for {candidate}"
            );
            if oracle.insert(candidate) {
                packed.insert(candidate);
            }
            assert_eq!(packed.len(), oracle.len());
        }

        assert_eq!(packed.occupied_slots(), oracle.len());
        for known in &oracle {
            assert!(
                packed.contains(*known),
                "{known} was inserted but is absent"
            );
        }
    }

    /// A record whose segment the set has never seen must answer absent
    /// without interning it, or a membership test would grow the interner
    /// with every miss on a large store.
    #[test]
    fn an_unseen_segment_is_absent_without_being_interned() {
        let mut packed = PackedRecordSet::new();
        packed.insert(record(1, 1));
        assert!(!packed.contains(record(2, 1)));
        assert!(!packed.contains(record(1, 2)));
        assert_eq!(packed.len(), 1);
        assert_eq!(packed.occupied_slots(), 1);
    }
}
