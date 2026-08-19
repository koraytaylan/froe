//! What the map records promise, proven against hand-built segment
//! bytes: lookup, enumeration, work budgets, diff overlays, and the
//! hash arithmetic at the levels where masking must be exact.

use super::{
    map_entries, map_entries_with_limits, map_entry, map_size, map_size_with_maximum_work,
};
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
fn bounded_map_enumeration_guards_entries_names_and_record_work() {
    let segment = data_segment_identifier(4);
    let mut provider = MemorySegmentProvider::default();
    provider.insert(
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
    let map = RecordIdentifier::new(segment, 10);

    assert!(matches!(
        map_entries_with_limits(&provider, map, 0, u64::MAX, u64::MAX),
        Err(crate::Error::MapEntryBudgetExceeded {
            maximum_entries: 0,
            attempted_entries: 1,
        })
    ));
    assert!(matches!(
        map_entries_with_limits(&provider, map, 1, 4, u64::MAX),
        Err(crate::Error::StringMaterializationBudgetExceeded {
            maximum_stored_bytes: 4,
            attempted_stored_bytes: 5,
            value_identifier,
        }) if value_identifier == RecordIdentifier::new(segment, 1)
    ));
    assert!(matches!(
        map_entries_with_limits(&provider, map, 1, 5, 1),
        Err(crate::Error::MapTraversalWorkBudgetExceeded {
            maximum_work_units: 1,
            attempted_work_units: 6,
        })
    ));
    assert!(matches!(
        map_entries_with_limits(&provider, map, 1, 5, 5),
        Err(crate::Error::MapTraversalWorkBudgetExceeded {
            maximum_work_units: 5,
            attempted_work_units: 6,
        })
    ));
    let (entries, name_bytes, map_records) = map_entries_with_limits(&provider, map, 1, 5, 6)
        .expect("one record visit and five stored name bytes fit exactly");
    assert_eq!(entries.len(), 1);
    assert_eq!((name_bytes, map_records), (5, 1));
}

#[test]
fn a_map_record_already_handled_in_the_same_enumeration_is_not_walked_again() {
    let segment = data_segment_identifier(8);
    let name = "child";
    let hash = map_entry_hash(name);
    let bucket_index = (hash >> 27) & 0x1F;
    let mut branch = 0x21u32.to_be_bytes().to_vec();
    branch.extend_from_slice(&(1u32 << bucket_index).to_be_bytes());
    branch.extend_from_slice(&identifier_bytes(10));

    let mut leaf_provider = MemorySegmentProvider::default();
    leaf_provider.insert(
        segment,
        synthetic_data_segment(
            &[],
            &[
                (1, 4, small_string_record(name)),
                (4, 4, small_string_record("value")),
                (10, 0, leaf_record(0, &[(name, 1, 4)])),
            ],
        ),
    );
    let leaf = RecordIdentifier::new(segment, 10);
    let (_, _, leaf_visits) = map_entries_with_limits(&leaf_provider, leaf, 1, u64::MAX, u64::MAX)
        .expect("leaf enumeration");
    assert_eq!(
        leaf_visits, 1,
        "a leaf is one map record; reading it to leave the diff chain \
         already yields its entries"
    );

    let mut branch_provider = MemorySegmentProvider::default();
    branch_provider.insert(
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
    let branch_map = RecordIdentifier::new(segment, 20);
    let (entries, _, branch_visits) =
        map_entries_with_limits(&branch_provider, branch_map, 1, u64::MAX, u64::MAX)
            .expect("branch enumeration");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        branch_visits, 2,
        "a branch and its one leaf are two records; the branch already \
         read to leave the diff chain must not be walked again"
    );

    let mut diff = 0xFFFF_FFFFu32.to_be_bytes().to_vec();
    diff.extend_from_slice(&map_entry_hash("alpha").to_be_bytes());
    diff.extend_from_slice(&identifier_bytes(1));
    diff.extend_from_slice(&identifier_bytes(6));
    diff.extend_from_slice(&identifier_bytes(10));
    let mut diff_provider = MemorySegmentProvider::default();
    diff_provider.insert(
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
    let (_, _, diff_visits) = map_entries_with_limits(
        &diff_provider,
        RecordIdentifier::new(segment, 20),
        2,
        u64::MAX,
        u64::MAX,
    )
    .expect("diff enumeration");
    assert_eq!(
        diff_visits, 2,
        "a diff and its base leaf are two records; the base already \
         identified as the trie root must not be walked again"
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
