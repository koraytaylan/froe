---
id: write-mirror-and-unique-records
title: Write Mirror And Unique Index Records From Sorted Entries
workstream: "0007"
kind: task
depends_on: [implement-bounded-external-sort]
gated: false
touches:
  - crates/froe/src/writer/index/property_builder.rs
  - crates/froe/tests/property_index_builder_tests.rs
  - crates/froe/tests/support/property_index_layout.rs
status: done
merged_as: "3c7cc2f1a422fa052a2f4f875d5afa1f2401e076"
---
# Write Mirror And Unique Index Records From Sorted Entries

Implement `MirrorBuilder` and `UniqueBuilder`, selected by task 0605's strategy verdict (a `BOOLEAN` `unique = true` and nothing else): given the sorted `(key, path)` sequence (the public `SortedEntries` iterator task 0704's collector returns), write the `:index` subtree through `RecordWriter` exactly as the mirror and unique strategies' inserts would leave it after a reindex — key nodes, one node per path element, `match = true` (`BOOLEAN`) on the node each indexed path addresses — an interior trie node when a shorter indexed path shares the key, since the insert sets it unconditionally after descending every element — the root path mapping to the key node itself; or one node per key with `entry` as a `String[]` holding the absolute path. No `:count_*` properties are written — a deviation recorded at the site: every Oak insert adjusts the approximate counter, so an Oak rebuild carries some, but a fresh Oak index has none until the random generator happens to add one, and their absence is a legal state Oak reads, the approximate counter answering `-1` for a count that is not there. The mirror builder is streaming and bottom-up: it holds, per ancestor on the current path, the completed children, and writes a node the moment the sequence leaves its subtree.

**Steps:**

1. `property_builder.rs`: `MirrorBuilder::new(writer)`, `push(key, path)`, `finish() -> RecordIdentifier` of the `:index` node, with a write counter for the safety case's cost statement and a resident-state accounting counter (peak completed children held) the tests assert against each fixture's widest fan-out; nodes carry no primary type (Oak's builders set none), children go through `ChildNodesToWrite::Zero`, `One` or `Many` per the count — `Zero` is not `Many(vec![])`, which writes child arity 2 plus an empty map record, a shape Oak's own segment writer never produces; the `:index` node itself is written last. An empty sequence still yields an `:index` node for the property family, because Oak's uniqueness check creates that node unconditionally; the reference collector of task 0704 decides separately whether to call the builder at all, because Oak creates `:references` and `:weakreferences` only on the first insert.
2. `UniqueBuilder` with the same interface; a key seen twice is a typed `DuplicateUniqueKey { key, paths }` error, because Oak would refuse the commit.
3. Extend the independent mirror-layout helper task 0606 added in `crates/froe/tests/support/property_index_layout.rs` (a naive in-memory tree builder that emits nodes through the independent encoder) with whatever the builder tests need, so the builder's output is compared against a second implementation, not against the reader that shares its assumptions.
4. Tests: an empty index (an `:index` node with no children, asserted on the written template's child arity, a distinction the digest lines do not show), an empty value whose key is the hidden name `:` (written as a key like any other), a single root-path entry, two keys sharing a path prefix, two entries under one key where one path is a strict ancestor of the other (both nodes carry `match`; the case Oak's `nodetype` index hits for nested `rep:AuthorizableFolder` nodes), deep paths, thousands of siblings under one key, and the duplicate refusal (two distinct nodes sharing a unique key; a single node whose two indexed properties share the value is one entry, not a duplicate), and a repeated `(key, path)` from a multi-valued reference property treated as one entry. Subtrees are compared through the lines of `tooling::digest::digest_repository_excluding` under each path after prefix normalization.

- **Done when:** for every fixture the builder's subtree renders identically to the independent helper's subtree (both written under distinct paths of one synthetic store and compared through the lines of `tooling::digest::digest_repository_excluding` under each path after prefix normalization), the empty index's written template carries child arity 0, the builder's resident state never exceeds the widest fan-out on one path (pinned by an accounting assertion, not by the process's resident set size), the write counter equals the number of nodes the helper emitted, and the stable host gate passes.
