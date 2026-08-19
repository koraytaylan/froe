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
use crate::content::value::{read_string, read_string_stored_length};
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
///
/// The returned counters are, respectively, the stored key bytes
/// materialized and map records visited. Work is their sum.
pub fn map_entries_with_limits(
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

    // Walk down any diff chain, remembering the overlaid entries. The first
    // non-diff record is the trie root: expand it from the view already in
    // hand so the same record is not charged and read again to start collect.
    let mut walk_length = 0u32;
    let mut entries = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut pending = Vec::new();
    loop {
        if walk_length >= MAXIMUM_WALK_LENGTH {
            return Err(walk_too_long(map_identifier));
        }
        walk_length += 1;
        charge_map_record(&mut budget)?;
        let view = provider.segment(current.segment)?;
        let head = view.read_u32(current.record_number, 0)?;
        if head == DIFF_HEAD {
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
            continue;
        }
        visited.insert(current);
        expand_map_record(
            provider,
            current,
            &view,
            head,
            &mut entries,
            &mut pending,
            &mut budget,
        )?;
        break;
    }
    collect_map_entries(provider, pending, &mut entries, &mut visited, &mut budget)?;
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
    let stored_length = read_string_stored_length(provider, identifier)?;
    let attempted_stored_bytes = budget.stored_name_bytes.saturating_add(stored_length);
    if attempted_stored_bytes > budget.maximum_stored_name_bytes {
        return Err(Error::StringMaterializationBudgetExceeded {
            maximum_stored_bytes: budget.maximum_stored_name_bytes,
            attempted_stored_bytes,
            value_identifier: identifier,
        });
    }
    let attempted_work_units = budget
        .visited_map_records
        .saturating_add(attempted_stored_bytes);
    if attempted_work_units > budget.maximum_work_units {
        return Err(Error::MapTraversalWorkBudgetExceeded {
            maximum_work_units: budget.maximum_work_units,
            attempted_work_units,
        });
    }
    budget.stored_name_bytes = attempted_stored_bytes;
    read_string(provider, identifier)
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
    mut pending: Vec<RecordIdentifier>,
    entries: &mut Vec<CollectedEntry>,
    visited: &mut std::collections::HashSet<RecordIdentifier>,
    budget: &mut Option<MapEnumerationBudget>,
) -> Result<()> {
    // The walk carries its own stack, so it imposes no depth limit: how deep
    // a trie is belongs to the records, not to this code. In a valid trie
    // every record is reachable exactly once — a key's hash fixes its unique
    // bucket path, so two buckets can never share a subtree — so `visited`
    // decides corruption exactly, and it is what stops a walk that a depth
    // bound alone could not (Java enumerates a DAG-shaped map forever;
    // returning an error is the documented safer deviation).
    while let Some(map_identifier) = pending.pop() {
        if !visited.insert(map_identifier) {
            return Err(walk_too_long(map_identifier));
        }
        charge_map_record(budget)?;
        let view = provider.segment(map_identifier.segment)?;
        let head = view.read_u32(map_identifier.record_number, 0)?;
        expand_map_record(
            provider,
            map_identifier,
            &view,
            head,
            entries,
            &mut pending,
            budget,
        )?;
    }
    Ok(())
}

/// Expands one already-read map record: schedule its children, or collect
/// its leaf entries. The caller charged the visit and holds the view.
fn expand_map_record(
    provider: &dyn SegmentProvider,
    map_identifier: RecordIdentifier,
    view: &crate::segment::view::SegmentView<'_>,
    head: u32,
    entries: &mut Vec<CollectedEntry>,
    pending: &mut Vec<RecordIdentifier>,
    budget: &mut Option<MapEnumerationBudget>,
) -> Result<()> {
    if head == DIFF_HEAD {
        // A nested diff below a branch never occurs in well-formed data,
        // but the Java reader recurses, so we do too.
        let base = view.read_record_identifier(map_identifier.record_number, 8, 2)?;
        pending.push(base);
        return Ok(());
    }
    let size = head & SIZE_MASK;
    let level = head >> LEVEL_SHIFT;
    if size == 0 {
        return Ok(());
    }
    if size > BUCKETS_PER_LEVEL && level < MAXIMUM_LEVEL {
        let bitmap = view.read_u32(map_identifier.record_number, 4)?;
        let bucket_count = bitmap.count_ones() as usize;
        let mut buckets = Vec::with_capacity(bucket_count);
        for bucket_position in 0..bucket_count {
            buckets.push(view.read_record_identifier(
                map_identifier.record_number,
                8,
                bucket_position,
            )?);
        }
        // Reversed so `pop` yields bucket order, keeping enumeration order
        // identical to the recursive walk's.
        pending.extend(buckets.into_iter().rev());
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
mod tests;
