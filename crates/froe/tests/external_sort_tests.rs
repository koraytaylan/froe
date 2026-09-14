//! The external sort's public surface, from outside the crate.
//!
//! The module itself is `pub(crate)`, and its unit tests cover the sorting.
//! What they cannot cover is the thing this file exists for: whether the
//! re-exports are reachable at all. An integration test is a separate crate,
//! so it sees exactly what a downstream consumer sees — and plan 0009's
//! doc-value and norms tests, which will also live in separate crates, must
//! be able to construct a `SortedPasses` to call the consumers they cover.
//!
//! A `pub` function may not name a type reachable only through a
//! `pub(crate)` module, nor a public generic carry a crate-private bound.
//! Both are rustc's warn-by-default `private_interfaces` and
//! `private_bounds`, which the `-D warnings` gate turns into errors — so a
//! mistake here fails the build rather than this test. What this test proves
//! is the other half: that the surface a consumer needs is actually
//! *exported*, not merely not-private.

use froe::writer::index::{IndexEntry, SortedPasses, SpillRecord as _};

#[test]
fn a_sorted_sequence_is_constructible_and_walkable_from_another_crate() {
    let entries = vec![
        IndexEntry::new("alpha", "/content/one"),
        IndexEntry::new("alpha", "/content/two"),
        IndexEntry::new("beta", "/content/one"),
    ];
    let mut passes = SortedPasses::from_sorted_records(entries.clone());
    for attempt in 0..2 {
        let walked: Vec<IndexEntry> = passes
            .pass()
            .expect("open a pass")
            .collect::<froe::Result<Vec<_>>>()
            .expect("read every record");
        assert_eq!(walked, entries, "pass {attempt} differs");
    }
}

#[test]
fn an_entry_orders_by_key_then_by_path_element() {
    // Comparing the path as one string would not do: `/a/b` and `/a-b/c`
    // order differently under the two comparisons, and the trie writer would
    // see a subtree it had already left.
    let nested = IndexEntry::new("k", "/a/b");
    let hyphenated = IndexEntry::new("k", "/a-b/c");
    assert!(
        nested < hyphenated,
        "element-wise, `a` precedes `a-b`; as one string, `/a-b/c` precedes `/a/b`"
    );
    assert!("/a-b/c" < "/a/b", "which is exactly the order not to use");

    assert!(
        IndexEntry::new("a", "/z") < IndexEntry::new("b", "/a"),
        "the key is compared first"
    );
}

#[test]
fn an_entry_survives_the_spill_encoding() {
    // The encoding is what crosses the file boundary, so a key holding the
    // separator the path uses must not be able to move the boundary.
    for entry in [
        IndexEntry::new("plain", "/content/page"),
        IndexEntry::new("", "/content/page"),
        IndexEntry::new("a%2Fb", "/content/page"),
        IndexEntry::new("key", "/"),
    ] {
        let mut buffer = Vec::new();
        entry.encode(&mut buffer);
        let decoded = IndexEntry::decode(&buffer).expect("decode what encode wrote");
        assert_eq!(decoded, entry);
    }
}
