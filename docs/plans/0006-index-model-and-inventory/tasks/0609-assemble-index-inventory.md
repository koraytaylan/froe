---
id: assemble-index-inventory
title: Assemble The Index Inventory
workstream: "0006"
kind: task
depends_on: [read-property-index-storage, read-counter-index, read-oak-directory]
gated: false
touches:
  - crates/froe/src/index/inventory.rs
  - crates/froe/tests/index_inventory_tests.rs
status: done
merged_as: "a69fa787d0360d5cf6f63f4fdcfe1abd4c1377d2"
---
# Assemble The Index Inventory

Implement `IndexInventory::collect(provider: &dyn SegmentProvider, super_root)` and its observed twin `collect_with_progress(provider, super_root, progress)`, the plain spelling delegating to the twin as `plan_compaction` does: one `IndexInfo` per definition, carrying what oak-run's index printer prints — type, lane, indexed-up-to time from the lane, `lastUpdated`, creation and reindex-completion timestamps, size in bytes, suggest size, estimated entry count, hidden mount and property-index nodes, definition drift with its diff (froe's own rendering of the changed paths, not Oak's JSOP text) — plus what froe can add read-only: the `reindex` flag and count, the corrupt flag, the dangling-lane verdict, the count of `:count_*` counters, and for Lucene the file list with lengths. Sizes and entry counts are computed from the readers of the previous three tasks; Lucene document counts stay `None` until plan 0008 can read `segments_N`.

**Steps:**

1. Define `IndexInfo` with typed optional fields and a `Vec<IndexWarning>` for facts that are not errors. One malformed definition must never kill a listing: anything attributable to a single definition — a typed error from its model construction, a stale stored definition, a dangling lane checkpoint, a `valuePattern` regular expression froe cannot evaluate, an index type froe does not know — is carried on that definition's own `IndexInfo` as a typed field or a warning naming the path and the kind, reported by `list` and treated as uncheckable by `check`. `collect` returns an error only for a failure that belongs to no single definition: an input or record failure reading `/oak:index`, `/:async` or `/checkpoints`. A checkpoint that cannot be read is never reported as dangling: dangling means resolved and absent.
2. Implement collection over the children of `/oak:index`, plus the non-root definitions when they are enumerable, mirroring the three cases Oak's own path service distinguishes but warning where it throws, so a listing always succeeds: the nodetype index absent or its `type` not reading as the `STRING` `property`; present but not declaring `oak:QueryIndexDefinition` among its `declaringNodeTypes`, which is the *default* Oak store, since the shipped nodetype index is created with none — the fixture's own state, and Oak's own verdict there is that non-root indexes will not be listed; and, only in the third case, present and declaring it, where the non-root definitions are read from that index's mirror entries for the type, bounded by its entries rather than by a walk of the content tree. The first two cases yield the root-level definitions with an `IndexWarning` naming which condition held and that non-root definitions were not enumerated. This is why `froe index list` runs where `froe index definitions` refuses. Plan 0007's refusal of a definition whose parent is not `/oak:index` is therefore reachable through `plan_reindex` on a store of the third kind, which its synthetic test store configures deliberately. Then one read of `/:async`; report the walk through the progress observer as `inventorying indexes` counting `WorkUnit::Nodes` (the definition nodes it reads), per the `cli-output.md` rules; `froe index check`'s `checking indexes` counts `Nodes` too, so no new `WorkUnit` variant is needed for the read-only commands.
3. Call task 0607's `estimated_node_count("/", CountBound::Expected)` at the content root for a counter definition, the one bound the inventory asks for, `CountBound::Maximum` being what task 0611's `froe index check` derives its per-definition budget from; `estimated_entry_count` for property indexes as Oak's property-index information provider computes it (the approximate counters, else zero when the data node is empty, else `None`; the same computation serves unique indexes, whose only counter sits on `:index`), and size accounting for Lucene as Oak's Lucene information provider computes it, minus the document count.
4. Tests over a synthetic store holding one definition of every type, including an index with a stale stored definition and a lane whose checkpoint is gone, and the twin-equality test `an_observed_inventory_equals_an_unobserved_one` (`collect` and `collect_with_progress` return identical inventories).

- **Done when:** the inventory over the synthetic store equals a hand-written expectation field by field, the observed and unobserved spellings return identical inventories under `an_observed_inventory_equals_an_unobserved_one`, and the stable host gate passes.
