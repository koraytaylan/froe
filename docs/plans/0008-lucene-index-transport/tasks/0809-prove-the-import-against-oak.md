---
id: prove-the-import-against-oak
title: Prove The Import Against Oak
workstream: "0008"
kind: task
depends_on: [add-the-import-command, prove-the-dump-against-oaks-dumper]
gated: false
touches:
  - crates/froe-cli/tests/interop/phase_lucene_transport.rs
  - crates/froe-cli/tests/interop/judge/OutOfBandBuild.java
  - crates/froe-cli/tests/interop/oak.rs
  - crates/froe-cli/tests/interop/definition_edits.rs
  - crates/froe-cli/tests/interop/phase_lucene_import.rs
  - crates/froe-cli/tests/interop/phase_recovery.rs
  - crates/froe-cli/tests/interop/main.rs
  - docs/interop.md
status: done
merged_as: "2207282, 777b99f, 44d6a61"
---
# Prove The Import Against Oak

The `lucene_import` phase covers both directions the import exists for. The judge gains `judge/OutOfBandBuild.java` with `build <store> <indexPath> <checkpoint> <outputDirectory>`: a re-implementation of `oak-run-commons`' out-of-band reindexer from the classes the image ships, since `oak-run-commons` is not in the image — an in-memory node store copy of the named checkpoint's state, the lane switch and `reindex = true` flag oak-run sets before an out-of-band build, reproduced on it with the lane switcher the image ships, then Oak's own cycle with the image's own Lucene editor provider subclassed to hand it a local filesystem directory factory in place of the repository one (the provider has no setter; oak-run subclasses it the same way) on the `offline-reindex-async` lane, driven the way oak-run drives it — a diff under the visible-editor filter from the base state to the lane-switched root, the cycle performing the traversal from the missing state internally — then the post-index and metadata steps: the lanes switched back, `indexer-info.properties`, `index-details.txt` and `index-definitions.json` through Oak's own definition printer in JSON under the reindex filter, and the copy to `indexes/` — the exact artefact `oak-run index --reindex` would leave, built by Oak's own editors on the pinned build, definitions file included with its `reindexCount` one above the store's and `refresh = true`.

**Steps:**

1. **Round trip.** Copy the fixture; `froe index dump --index /oak:index/lucene` (the selection is explicit because plan 0010 later adds a second Lucene definition to the fixture, and this phase's delta assertion names one); on a second copy remove the `lucene` definition's hidden children through the helpers of `definition_edits.rs` (task 0712's file, extended here with `flag_corrupt` and `remove_async`) built on the public API — re-emitting the definition node with `RecordWriter::write_node` and splicing it up the spine with `rewrite_node_with_child_edits`, as `phase_writing.rs` splices content today (the state a lost index leaves); `froe index import --yes --index /oak:index/lucene --input <dump>/index-dumps`; `froe index check` and the judge's `consistency … 2 <workDirectory>` (level 2, Lucene's own index checker inside Oak) must be clean; the `:data` subtree must render identically to the original under the digest minus `uniqueKey`, `jcr:lastModified` and `:status/uid`, with `dirListing` compared as a set, and `reindexCount` one above the original.
2. **Out-of-band build.** The judge builds the fixture's `lucene` index at the lane's *existing* checkpoint, read from `/:async/async` — the only checkpoint that can satisfy the state rule for an asynchronous definition, since every lane cycle commits after taking its checkpoint and the head's root therefore never equals a lane checkpoint's — on the store copy; froe imports the result, accepting the oak-run-shaped definitions file under the drift comparison (the phase first flags the definition `corrupt` on the copy's head through the writer helper, as a `DATE` property exactly as Oak's async lane writes it and Oak's own cycle reads it converting to `DATE`; the judge builds from the lane checkpoint's state, which predates the flag, so its file lacks `corrupt` regardless, and the acceptance covers the standard remediation case and the import's clearing of the flag); the same checks, with `reindexCount` two above the original, as oak-run's own import would leave it, no `corrupt` property, and the lane's checkpoint still present.
3. **Oak consumption.** Boot Sling on each imported store; `assert_oak_consumed_store_as_written`; no reindex marker; through the query probe, a fulltext query (`CONTAINS(*, 'Page')` restricted to `/content/interop/pages`, whose five `jcr:title` values the default definition indexes without Tika, and which plan 0010's variant definition does not cover; the phase asserts the pristine store returns the five rows, so the comparison is never vacuous) returns the same paths as on the pristine store and its `EXPLAIN` plan names `lucene:lucene`; Oak's log carries none of the three index-failure markers this phase adds to the marker set in `oak.rs` — the lines Oak prints when an index is missing, when a file the index names is absent, and when its bytes fail Lucene's own checks, matched on the text Oak logs: `IndexNotFoundException`, `FileNotFoundException`, `CorruptIndexException`.
4. **Whole-store delta.** For each imported store, the digest before and after the import differs only under `/oak:index/lucene` (`ExpectedDigestDelta::Subtrees`), and `froe check` passes at the new head.
5. **Refusals.** An import whose `indexer-info.properties` names a checkpoint that is not the attachment state is refused with both roots named; an import whose definitions file carries a changed visible property is refused naming it; an import selecting a definition made synchronous on the copy (its `async` removed through the writer helper) is refused by name; the store snapshot is unchanged in every case.

- **Done when:** the phase passes against the pinned image for both directions with Oak answering the fulltext query through the imported index and the whole-store delta confined to the definition, and an import with the state rule neutralized makes the phase fail on the refusal assertion, and the stable host gate passes (`--all-features`, so the interop suite is compiled and linted).
