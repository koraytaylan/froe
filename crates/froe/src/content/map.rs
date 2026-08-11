//! Map records: the hash array mapped trie storing child nodes by name.
//!
//! A map stores `name → record identifier` entries. Three physical shapes
//! exist, discriminated by the 32-bit head word:
//!
//! * a *diff* (`head == 0xFFFFFFFF`) overlays one changed entry on a base
//!   map — written when a single entry of a large map was updated;
//! * a *branch* (more than 32 entries, level below 7) holds a bitmap of 32
//!   buckets selected by five bits of the entry name's hash, each bucket
//!   pointing at a sub-map one level deeper;
//! * a *leaf* holds the entries directly: a sorted array of hashes
//!   followed by interleaved key and value record identifiers.
//!
//! The hash is `(utf16_string_hash(name) ^ M) * M + A` with Java's wrapping
//! 32-bit arithmetic, and hashes compare as *unsigned* values in leaf
//! ordering. Branch bucket selection at level 6 relies on Java masking
//! shift distances to five bits — the shift becomes `29`, re-reading the
//! hash's top bits. Both quirks are load-bearing and reproduced here.

use crate::content::provider::SegmentProvider;
use crate::content::value::read_string_with_stored_byte_budget;
use crate::error::{Error, Result};
use crate::hashing::map_entry_hash;
use crate::segment::record::RecordIdentifier;

/// The head word marking a diff record.
const DIFF_HEAD: u32 = 0xFFFF_FFFF;

/// Mask extracting the entry count from a head word (29 bits).
const SIZE_MASK: u32 = 0x1FFF_FFFF;

/// Shift extracting the trie level from a head word (3 bits).
const LEVEL_SHIFT: u32 = 29;

/// Entries per branch level.
const BUCKETS_PER_LEVEL: u32 = 32;

/// Levels below this may be branches; level 7 records are always leaves.
const MAXIMUM_LEVEL: u32 = 7;

/// Upper bound on the number of map records visited while descending one
/// lookup or diff chain. Valid data never comes close (at most seven trie
/// levels plus a single diff); corrupt data could otherwise form a cycle
/// of records and hang the reader.
const MAXIMUM_WALK_LENGTH: u32 = 1024;

/// The error returned when a map walk exceeds [`MAXIMUM_WALK_LENGTH`].
fn walk_too_long(map_identifier: RecordIdentifier) -> Error {
    Error::InvalidFormat {
        details: format!(
            "map walk starting at {map_identifier} exceeds {MAXIMUM_WALK_LENGTH} records; \
             the map records probably form a cycle"
        ),
    }
}

/// One entry of a map: the child name and the record it points to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MapEntry {
    /// The entry's key — for child node maps, the child's name.
    pub name: String,
    /// The record the entry points to — for child node maps, the child's
    /// node record.
    pub value: RecordIdentifier,
}

/// The number of entries in the map rooted at `map_identifier`.
/// A diff record reports the size of its base map.
pub fn map_size(provider: &dyn SegmentProvider, map_identifier: RecordIdentifier) -> Result<u64> {
    let mut current = map_identifier;
    for _ in 0..MAXIMUM_WALK_LENGTH {
        let view = provider.segment(current.segment)?;
        let head = view.read_u32(current.record_number, 0)?;
        if head == DIFF_HEAD {
            current = view.read_record_identifier(current.record_number, 8, 2)?;
            continue;
        }
        return Ok(u64::from(head & SIZE_MASK));
    }
    Err(walk_too_long(map_identifier))
}

/// Reads a map's declared size while bounding record visits through a corrupt
/// diff chain. Returns the size and records inspected.
pub(crate) fn map_size_with_maximum_work(
    provider: &dyn SegmentProvider,
    map_identifier: RecordIdentifier,
    maximum_work_units: u64,
) -> Result<(u64, u64)> {
    let mut current = map_identifier;
    for visited_map_records in 1..=u64::from(MAXIMUM_WALK_LENGTH) {
        if visited_map_records > maximum_work_units {
            return Err(Error::MapTraversalWorkBudgetExceeded {
                maximum_work_units,
                attempted_work_units: visited_map_records,
            });
        }
        let view = provider.segment(current.segment)?;
        let head = view.read_u32(current.record_number, 0)?;
        if head == DIFF_HEAD {
            current = view.read_record_identifier(current.record_number, 8, 2)?;
            continue;
        }
        return Ok((u64::from(head & SIZE_MASK), visited_map_records));
    }
    Err(walk_too_long(map_identifier))
}

/// Looks up the entry named `name`, returning the record identifier it
/// maps to, or `None` when the map has no such entry.
pub fn map_entry(
    provider: &dyn SegmentProvider,
    map_identifier: RecordIdentifier,
    name: &str,
) -> Result<Option<RecordIdentifier>> {
    let hash = map_entry_hash(name);
    let mut current = map_identifier;
    for _ in 0..MAXIMUM_WALK_LENGTH {
        let view = provider.segment(current.segment)?;
        let head = view.read_u32(current.record_number, 0)?;

        if head == DIFF_HEAD {
            // Diff: one overlaid entry (hash, key, value), then the base.
            let stored_hash = view.read_u32(current.record_number, 4)?;
            if stored_hash == hash {
                let key_identifier = view.read_record_identifier(current.record_number, 8, 0)?;
                if &*provider.string(key_identifier)? == name {
                    return Ok(Some(view.read_record_identifier(
                        current.record_number,
                        8,
                        1,
                    )?));
                }
            }
            current = view.read_record_identifier(current.record_number, 8, 2)?;
            continue;
        }

        let size = head & SIZE_MASK;
        let level = head >> LEVEL_SHIFT;
        if size == 0 {
            return Ok(None);
        }
        if size > BUCKETS_PER_LEVEL && level < MAXIMUM_LEVEL {
            // Branch: five hash bits select one of 32 buckets.
            let bitmap = view.read_u32(current.record_number, 4)?;
            // Java computes `hash >> (32 - (level + 1) * 5)` with the shift
            // distance masked to five bits; at level 6 the distance is -3,
            // which masks to 29. The shift is arithmetic on the signed hash.
            let shift = (32i32 - (level as i32 + 1) * 5) & 31;
            let bucket_index = (((hash as i32) >> shift) & 0x1F) as u32;
            let bit = 1u32 << bucket_index;
            if bitmap & bit == 0 {
                return Ok(None);
            }
            let bucket_position = (bitmap & (bit - 1)).count_ones() as usize;
            current = view.read_record_identifier(current.record_number, 8, bucket_position)?;
            continue;
        }

        // Leaf: hashes sorted as unsigned values, then interleaved
        // (key, value) identifier pairs.
        return leaf_entry(provider, &view, current, size, hash, name);
    }
    Err(walk_too_long(map_identifier))
}

/// Searches a leaf's sorted hash array for `hash`, then compares key
/// strings among equal hashes.
fn leaf_entry(
    provider: &dyn SegmentProvider,
    view: &crate::segment::view::SegmentView<'_>,
    leaf: RecordIdentifier,
    size: u32,
    hash: u32,
    name: &str,
) -> Result<Option<RecordIdentifier>> {
    let size = size as usize;
    // Binary search for the first entry with the target hash.
    let mut low = 0usize;
    let mut high = size;
    while low < high {
        let middle = usize::midpoint(low, high);
        let middle_hash = view.read_u32(leaf.record_number, 4 + middle * 4)?;
        if middle_hash < hash {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    // Compare key strings for every entry sharing the hash.
    let identifiers_base = 4 + size * 4;
    let mut position = low;
    while position < size {
        if view.read_u32(leaf.record_number, 4 + position * 4)? != hash {
            break;
        }
        let key_identifier =
            view.read_record_identifier(leaf.record_number, identifiers_base, position * 2)?;
        if &*provider.string(key_identifier)? == name {
            return Ok(Some(view.read_record_identifier(
                leaf.record_number,
                identifiers_base,
                position * 2 + 1,
            )?));
        }
        position += 1;
    }
    Ok(None)
}

/// Reads all entries of the map rooted at `map_identifier`, in storage
/// order (leaf order within each bucket, buckets in ascending bit order).
///
/// A diff overlays its entry on the base map by *key record identifier*
/// equality, matching the Java reader.
pub fn map_entries(
    provider: &dyn SegmentProvider,
    map_identifier: RecordIdentifier,
) -> Result<Vec<MapEntry>> {
    map_entries_internal(provider, map_identifier, None).map(|(entries, _, _)| entries)
}

/// Reads all map entries while bounding concrete entries, cumulative stored
/// key bytes, and map-record/key-byte work before allocation or materialization.
pub(crate) fn map_entries_with_limits(
    provider: &dyn SegmentProvider,
    map_identifier: RecordIdentifier,
    maximum_entries: u64,
    maximum_stored_name_bytes: u64,
    maximum_work_units: u64,
) -> Result<(Vec<MapEntry>, u64, u64)> {
    map_entries_internal(
        provider,
        map_identifier,
        Some(MapEnumerationBudget {
            maximum_entries,
            maximum_stored_name_bytes,
            maximum_work_units,
            stored_name_bytes: 0,
            visited_map_records: 0,
        }),
    )
}

fn map_entries_internal(
    provider: &dyn SegmentProvider,
    map_identifier: RecordIdentifier,
    mut budget: Option<MapEnumerationBudget>,
) -> Result<(Vec<MapEntry>, u64, u64)> {
    let mut overlays: Vec<(RecordIdentifier, RecordIdentifier)> = Vec::new();
    let mut current = map_identifier;

    // Walk down any diff chain, remembering the overlaid entries.
    let mut walk_length = 0u32;
    let base = loop {
        if walk_length >= MAXIMUM_WALK_LENGTH {
            return Err(walk_too_long(map_identifier));
        }
        walk_length += 1;
        charge_map_record(&mut budget)?;
        let view = provider.segment(current.segment)?;
        let head = view.read_u32(current.record_number, 0)?;
        if head != DIFF_HEAD {
            break current;
        }
        let key = view.read_record_identifier(current.record_number, 8, 0)?;
        let value = view.read_record_identifier(current.record_number, 8, 1)?;
        // Earlier diffs win over later ones in the chain.
        if !overlays
            .iter()
            .any(|(existing_key, _)| *existing_key == key)
        {
            overlays.push((key, value));
        }
        current = view.read_record_identifier(current.record_number, 8, 2)?;
    };

    let mut entries = Vec::new();
    let mut visited = std::collections::HashSet::new();
    collect_map_entries(provider, base, &mut entries, 0, &mut visited, &mut budget)?;
    if !overlays.is_empty() {
        for entry in &mut entries {
            if let Some((_, value)) = overlays
                .iter()
                .find(|(key, _)| *key == entry.key_identifier)
            {
                entry.value = *value;
            }
        }
    }
    let stored_name_bytes = budget.as_ref().map_or(0, |budget| budget.stored_name_bytes);
    let visited_map_records = budget
        .as_ref()
        .map_or(0, |budget| budget.visited_map_records);
    Ok((
        entries
            .into_iter()
            .map(|entry| MapEntry {
                name: entry.name,
                value: entry.value,
            })
            .collect(),
        stored_name_bytes,
        visited_map_records,
    ))
}

struct MapEnumerationBudget {
    maximum_entries: u64,
    maximum_stored_name_bytes: u64,
    maximum_work_units: u64,
    stored_name_bytes: u64,
    visited_map_records: u64,
}

fn charge_map_record(budget: &mut Option<MapEnumerationBudget>) -> Result<()> {
    let Some(budget) = budget else {
        return Ok(());
    };
    let attempted_map_records = budget.visited_map_records.saturating_add(1);
    let attempted_work_units = attempted_map_records.saturating_add(budget.stored_name_bytes);
    if attempted_work_units > budget.maximum_work_units {
        return Err(Error::MapTraversalWorkBudgetExceeded {
            maximum_work_units: budget.maximum_work_units,
            attempted_work_units,
        });
    }
    budget.visited_map_records = attempted_map_records;
    Ok(())
}

fn read_map_name(
    provider: &dyn SegmentProvider,
    identifier: RecordIdentifier,
    budget: &mut Option<MapEnumerationBudget>,
) -> Result<String> {
    let Some(budget) = budget else {
        return Ok(provider.string(identifier)?.as_ref().to_owned());
    };
    let maximum_from_work = budget
        .maximum_work_units
        .saturating_sub(budget.visited_map_records);
    read_string_with_stored_byte_budget(
        provider,
        identifier,
        budget.maximum_stored_name_bytes.min(maximum_from_work),
        &mut budget.stored_name_bytes,
    )
}

/// A map entry augmented with its key record identifier, needed to apply
/// diff overlays.
struct CollectedEntry {
    name: String,
    key_identifier: RecordIdentifier,
    value: RecordIdentifier,
}

fn collect_map_entries(
    provider: &dyn SegmentProvider,
    map_identifier: RecordIdentifier,
    entries: &mut Vec<CollectedEntry>,
    depth: u32,
    visited: &mut std::collections::HashSet<RecordIdentifier>,
    budget: &mut Option<MapEnumerationBudget>,
) -> Result<()> {
    // Valid tries are at most seven levels deep (plus stray diffs); a much
    // larger depth means the records form a cycle.
    if depth >= 64 {
        return Err(walk_too_long(map_identifier));
    }
    // In a valid trie every record is reachable exactly once — a key's
    // hash fixes its unique bucket path, so two buckets can never share a
    // subtree. A revisit therefore means corrupt records shaped as a DAG,
    // on which a depth bound alone would allow exponential work (Java
    // enumerates such maps forever; returning an error is the documented
    // safer deviation).
    if !visited.insert(map_identifier) {
        return Err(walk_too_long(map_identifier));
    }
    charge_map_record(budget)?;
    let view = provider.segment(map_identifier.segment)?;
    let head = view.read_u32(map_identifier.record_number, 0)?;
    if head == DIFF_HEAD {
        // A nested diff below a branch never occurs in well-formed data,
        // but the Java reader recurses, so we do too.
        let base = view.read_record_identifier(map_identifier.record_number, 8, 2)?;
        return collect_map_entries(provider, base, entries, depth + 1, visited, budget);
    }
    let size = head & SIZE_MASK;
    let level = head >> LEVEL_SHIFT;
    if size == 0 {
        return Ok(());
    }
    if size > BUCKETS_PER_LEVEL && level < MAXIMUM_LEVEL {
        let bitmap = view.read_u32(map_identifier.record_number, 4)?;
        let bucket_count = bitmap.count_ones() as usize;
        for bucket_position in 0..bucket_count {
            let bucket =
                view.read_record_identifier(map_identifier.record_number, 8, bucket_position)?;
            collect_map_entries(provider, bucket, entries, depth + 1, visited, budget)?;
        }
        return Ok(());
    }
    let size = size as usize;
    let identifiers_base = 4 + size * 4;
    for position in 0..size {
        if let Some(budget) = budget {
            let attempted_entries = u64::try_from(entries.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1);
            if attempted_entries > budget.maximum_entries {
                return Err(Error::MapEntryBudgetExceeded {
                    maximum_entries: budget.maximum_entries,
                    attempted_entries,
                });
            }
        }
        let key_identifier = view.read_record_identifier(
            map_identifier.record_number,
            identifiers_base,
            position * 2,
        )?;
        let value = view.read_record_identifier(
            map_identifier.record_number,
            identifiers_base,
            position * 2 + 1,
        )?;
        entries.push(CollectedEntry {
            name: read_map_name(provider, key_identifier, budget)?,
            key_identifier,
            value,
        });
    }
    Ok(())
}

/// Validation helper shared by tests and diagnostics: `true` when the head
/// word describes a branch record.
#[must_use]
pub fn is_branch_head(head: u32) -> bool {
    head != DIFF_HEAD
        && (head & SIZE_MASK) > BUCKETS_PER_LEVEL
        && (head >> LEVEL_SHIFT) < MAXIMUM_LEVEL
}

#[cfg(test)]
mod tests {
    use super::{map_entries, map_entry, map_size, map_size_with_maximum_work};
    use crate::content::provider::tests::{CountingSegmentProvider, MemorySegmentProvider};
    use crate::hashing::map_entry_hash;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;

    fn small_string_record(text: &str) -> Vec<u8> {
        let mut bytes = vec![text.len() as u8];
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    fn identifier_bytes(record_number: u32) -> [u8; 6] {
        let mut bytes = [0u8; 6];
        bytes[2..6].copy_from_slice(&record_number.to_be_bytes());
        bytes
    }

    /// Builds a leaf record for entries of (name, key record, value record),
    /// sorting by unsigned hash then name in UTF-16 order as the writer
    /// does.
    fn leaf_record(level: u32, entries: &[(&str, u32, u32)]) -> Vec<u8> {
        let mut sorted: Vec<&(&str, u32, u32)> = entries.iter().collect();
        sorted.sort_by(|first, second| {
            map_entry_hash(first.0)
                .cmp(&map_entry_hash(second.0))
                .then_with(|| first.0.encode_utf16().cmp(second.0.encode_utf16()))
        });
        let head = (level << 29) | entries.len() as u32;
        let mut bytes = head.to_be_bytes().to_vec();
        for (name, _, _) in &sorted {
            bytes.extend_from_slice(&map_entry_hash(name).to_be_bytes());
        }
        for (_, key_record, value_record) in &sorted {
            bytes.extend_from_slice(&identifier_bytes(*key_record));
            bytes.extend_from_slice(&identifier_bytes(*value_record));
        }
        bytes
    }

    #[test]
    fn empty_map_has_no_entries() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(0, 0, leaf_record(0, &[]))]),
        );
        let map = RecordIdentifier::new(segment, 0);
        assert_eq!(map_size(&provider, map).expect("size"), 0);
        assert_eq!(map_entry(&provider, map, "anything").expect("lookup"), None);
        assert!(map_entries(&provider, map).expect("entries").is_empty());
    }

    #[test]
    fn leaf_lookup_finds_entries_by_name() {
        let segment = data_segment_identifier(1);
        // Records 1-3: name strings; 4-6: value targets (small strings as
        // stand-ins for node records).
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record("alpha")),
                    (2, 4, small_string_record("beta")),
                    (3, 4, small_string_record("gamma")),
                    (4, 4, small_string_record("v1")),
                    (5, 4, small_string_record("v2")),
                    (6, 4, small_string_record("v3")),
                    (
                        10,
                        0,
                        leaf_record(0, &[("alpha", 1, 4), ("beta", 2, 5), ("gamma", 3, 6)]),
                    ),
                ],
            ),
        );
        let map = RecordIdentifier::new(segment, 10);
        assert_eq!(map_size(&provider, map).expect("size"), 3);
        assert_eq!(
            map_entry(&provider, map, "beta").expect("lookup"),
            Some(RecordIdentifier::new(segment, 5))
        );
        assert_eq!(map_entry(&provider, map, "delta").expect("lookup"), None);

        let entries = map_entries(&provider, map).expect("entries");
        assert_eq!(entries.len(), 3);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta") && names.contains(&"gamma"));
    }

    #[test]
    fn ordinary_map_enumeration_uses_the_provider_string_surface() {
        let segment = data_segment_identifier(2);
        let mut inner = MemorySegmentProvider::default();
        inner.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record("child")),
                    (4, 4, small_string_record("value")),
                    (10, 0, leaf_record(0, &[("child", 1, 4)])),
                ],
            ),
        );
        let provider = CountingSegmentProvider::new(&inner);

        assert_eq!(
            map_entries(&provider, RecordIdentifier::new(segment, 10)).expect("entries")[0].name,
            "child"
        );
        assert_eq!(provider.string_reads(), 1);
    }

    #[test]
    fn bounded_map_size_charges_each_diff_record_before_following_it() {
        let segment = data_segment_identifier(3);
        let diff_record = |base_record_number: u32| {
            let mut bytes = u32::MAX.to_be_bytes().to_vec();
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(&identifier_bytes(1));
            bytes.extend_from_slice(&identifier_bytes(4));
            bytes.extend_from_slice(&identifier_bytes(base_record_number));
            bytes
        };
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record("child")),
                    (4, 4, small_string_record("value")),
                    (10, 0, leaf_record(0, &[("child", 1, 4)])),
                    (11, 0, diff_record(10)),
                    (12, 0, diff_record(11)),
                ],
            ),
        );
        let map = RecordIdentifier::new(segment, 12);

        assert!(matches!(
            map_size_with_maximum_work(&provider, map, 2),
            Err(crate::Error::MapTraversalWorkBudgetExceeded {
                maximum_work_units: 2,
                attempted_work_units: 3,
            })
        ));
        assert_eq!(
            map_size_with_maximum_work(&provider, map, 3).expect("exact diff budget"),
            (1, 3)
        );
    }

    #[test]
    fn colliding_hashes_resolve_by_name_within_a_leaf() {
        // "Aa", "BB", "AaAa", and "BBBB" all pairwise collide in Java's
        // string hash (and therefore in the scrambled map hash), driving
        // the binary-search-then-walk over equal hash runs.
        assert_eq!(map_entry_hash("Aa"), map_entry_hash("BB"));
        assert_eq!(map_entry_hash("AaAa"), map_entry_hash("BBBB"));
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record("Aa")),
                    (2, 4, small_string_record("BB")),
                    (3, 4, small_string_record("AaAa")),
                    (4, 4, small_string_record("BBBB")),
                    (5, 4, small_string_record("v1")),
                    (6, 4, small_string_record("v2")),
                    (7, 4, small_string_record("v3")),
                    (8, 4, small_string_record("v4")),
                    (
                        10,
                        0,
                        leaf_record(
                            0,
                            &[("Aa", 1, 5), ("BB", 2, 6), ("AaAa", 3, 7), ("BBBB", 4, 8)],
                        ),
                    ),
                ],
            ),
        );
        let map = RecordIdentifier::new(segment, 10);
        assert_eq!(
            map_entry(&provider, map, "Aa").expect("lookup"),
            Some(RecordIdentifier::new(segment, 5))
        );
        assert_eq!(
            map_entry(&provider, map, "BB").expect("lookup"),
            Some(RecordIdentifier::new(segment, 6))
        );
        assert_eq!(
            map_entry(&provider, map, "AaAa").expect("lookup"),
            Some(RecordIdentifier::new(segment, 7))
        );
        assert_eq!(
            map_entry(&provider, map, "BBBB").expect("lookup"),
            Some(RecordIdentifier::new(segment, 8))
        );
        // "AaBB" shares the four-character collision hash but is absent:
        // the equal-hash walk must confirm names, not stop at the hash.
        assert_eq!(map_entry_hash("AaBB"), map_entry_hash("AaAa"));
        assert_eq!(map_entry(&provider, map, "AaBB").expect("lookup"), None);
    }

    #[test]
    fn level_six_branch_lookup_sign_extends_the_masked_shift() {
        // At trie level 6 the shift distance is (32 - 7*5) & 31 = 29 with
        // an *arithmetic* shift. map_entry_hash("root") = 0xC28924EE has
        // its top bit set, so sign extension selects bucket
        // ((0xC28924EE as i32) >> 29) & 0x1F = 30 — a logical shift would
        // select bucket 6 and the lookup would miss.
        let name = "root";
        assert_eq!(map_entry_hash(name), 0xC289_24EE);
        let bucket_index = 30u32;
        let bitmap = 1u32 << bucket_index;

        // Head: level 6, declared size 33.
        let mut branch = ((6u32 << 29) | 0x21).to_be_bytes().to_vec();
        branch.extend_from_slice(&bitmap.to_be_bytes());
        branch.extend_from_slice(&identifier_bytes(10));

        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record(name)),
                    (4, 4, small_string_record("value")),
                    (10, 0, leaf_record(7, &[(name, 1, 4)])),
                    (20, 1, branch),
                ],
            ),
        );
        let map = RecordIdentifier::new(segment, 20);
        assert_eq!(
            map_entry(&provider, map, name).expect("lookup"),
            Some(RecordIdentifier::new(segment, 4))
        );
    }

    #[test]
    fn branch_lookup_descends_by_hash_bits() {
        let segment = data_segment_identifier(1);
        // Build a branch at level 0 with 33 declared entries (so it counts
        // as a branch) and one real leaf bucket containing "child".
        let name = "child";
        let hash = map_entry_hash(name);
        let bucket_index = (hash >> 27) & 0x1F;
        let bitmap = 1u32 << bucket_index;

        // Head: level 0, declared size 33 (0x21).
        let mut branch = 0x21u32.to_be_bytes().to_vec();
        branch.extend_from_slice(&bitmap.to_be_bytes());
        branch.extend_from_slice(&identifier_bytes(10));

        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record(name)),
                    (4, 4, small_string_record("value")),
                    (10, 0, leaf_record(1, &[(name, 1, 4)])),
                    (20, 1, branch),
                ],
            ),
        );
        let map = RecordIdentifier::new(segment, 20);
        assert_eq!(
            map_entry(&provider, map, name).expect("lookup"),
            Some(RecordIdentifier::new(segment, 4))
        );
        // A name whose bucket bit is clear resolves to nothing.
        let mut absent_name = None;
        for candidate_index in 0..1000 {
            let candidate = format!("absent-{candidate_index}");
            let candidate_bucket = (map_entry_hash(&candidate) >> 27) & 0x1F;
            if candidate_bucket != bucket_index {
                absent_name = Some(candidate);
                break;
            }
        }
        let absent_name = absent_name.expect("some candidate lands in another bucket");
        assert_eq!(
            map_entry(&provider, map, &absent_name).expect("lookup"),
            None
        );

        let entries = map_entries(&provider, map).expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, name);
    }

    #[test]
    fn diff_overlays_one_entry_on_the_base_map() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();

        // Base leaf (record 10): alpha -> 4, beta -> 5.
        // Diff (record 20): alpha -> 6 overlaid on record 10.
        let mut diff = 0xFFFF_FFFFu32.to_be_bytes().to_vec();
        diff.extend_from_slice(&map_entry_hash("alpha").to_be_bytes());
        diff.extend_from_slice(&identifier_bytes(1));
        diff.extend_from_slice(&identifier_bytes(6));
        diff.extend_from_slice(&identifier_bytes(10));

        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, small_string_record("alpha")),
                    (2, 4, small_string_record("beta")),
                    (4, 4, small_string_record("old")),
                    (5, 4, small_string_record("kept")),
                    (6, 4, small_string_record("new")),
                    (10, 0, leaf_record(0, &[("alpha", 1, 4), ("beta", 2, 5)])),
                    (20, 1, diff),
                ],
            ),
        );
        let map = RecordIdentifier::new(segment, 20);
        assert_eq!(
            map_size(&provider, map).expect("size"),
            2,
            "diff reports base size"
        );
        assert_eq!(
            map_entry(&provider, map, "alpha").expect("lookup"),
            Some(RecordIdentifier::new(segment, 6)),
            "diff overlays the changed entry"
        );
        assert_eq!(
            map_entry(&provider, map, "beta").expect("lookup"),
            Some(RecordIdentifier::new(segment, 5)),
            "other entries come from the base"
        );

        let entries = map_entries(&provider, map).expect("entries");
        let alpha = entries
            .iter()
            .find(|entry| entry.name == "alpha")
            .expect("alpha");
        assert_eq!(alpha.value, RecordIdentifier::new(segment, 6));
    }

    #[test]
    fn cyclic_diff_chains_are_rejected_instead_of_hanging() {
        // A corrupt diff record whose base points back at itself.
        let segment = data_segment_identifier(1);
        let mut cyclic_diff = 0xFFFF_FFFFu32.to_be_bytes().to_vec();
        cyclic_diff.extend_from_slice(&map_entry_hash("x").to_be_bytes());
        cyclic_diff.extend_from_slice(&identifier_bytes(1));
        cyclic_diff.extend_from_slice(&identifier_bytes(1));
        cyclic_diff.extend_from_slice(&identifier_bytes(20)); // its own record
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[(1, 4, small_string_record("x")), (20, 1, cyclic_diff)],
            ),
        );
        let map = RecordIdentifier::new(segment, 20);
        assert!(map_size(&provider, map).is_err());
        assert!(map_entry(&provider, map, "y").is_err());
        assert!(map_entries(&provider, map).is_err());
    }

    #[test]
    fn level_six_shift_masks_to_five_bits() {
        // At level 6 the Java shift distance is 32 - 35 = -3, masked to 29.
        // Verify our branch head helper agrees a level-6 record can branch
        // and the lookup math cannot panic for any hash.
        assert!(super::is_branch_head((6u32 << 29) | 0x64));
        assert!(
            !super::is_branch_head((7u32 << 29) | 0x64),
            "level 7 is always a leaf"
        );
        let shift = (32i32 - (6i32 + 1) * 5) & 31;
        assert_eq!(shift, 29);
    }
}
