# Scope — Plan 0008

> Move Lucene index data between a stopped repository and the filesystem exactly the way oak-run does — `froe index dump` out, `froe index import` in — and check it, so that an index built out of band by Oak's own editors can be installed without a JVM anywhere near the store.

## Why this plan

Offline Lucene reindexing on AEM today is oak-run's two-step: build the index out of band (`--reindex`, against a checkpoint, on a machine that can afford it) and import the result (`--index-import`) into the repository. The import is the step operators dread — it runs against the production store, it rewrites `:data`, it juggles async lanes — and it is pure segment-store work: copying files into blobs, rewriting a few definition properties, moving the head. froe can perform it under the lock with the same discipline as compaction, and can perform the inverse, the dump, read-only. Both are prerequisites for plan 0010, whose native writer needs the importer to install what it builds and the dumper's byte identity to prove what it read.

This plan also adds the Lucene file-level readers froe needs to *look* at an index without Lucene: `segments.gen`, `segments_N`, the `.si` segment descriptors and the compound-file table of contents. Those give the document count oak-run's index printer reports and a structural check between oak-run's level 1 and level 2, and they are the reading half of the codec plan 0009 writes.

## In scope

- **Segment-level readers.** `segments.gen`, `segments_N`, `.si`, `.cfs`/`.cfe`, the codec headers that open every one of those files; document and deletion counts; file inventory validation against the directory listing.
- **Dump.** `froe index dump --output DIRECTORY`: oak-run's `index-dumps` layout, byte-identical files, `index-details.txt`; read-only against the store.
- **Import.** `froe index import --input DIRECTORY`: oak-run's `indexing-result/indexes` layout with `indexer-info.properties`, per-index `index-details.txt` and `index-definitions.json` (mandatory for oak-run's importer, whose definition updater checks at construction that the file exists; froe's dump writes it, and froe's import reads it back with a bounded reader and refuses definition drift under the comparison Oak itself uses — the one its Lucene index-information provider performs, which plan 0006 models — extended by the properties an oak-run build rewrites on its copy: `reindexCount`, `refresh`, a created `seed`, a cleared `corrupt` or `indexImportState`, and a `facets` subtree the file adds, because oak-run prints the lane-switched, already-reindexed copy of the definition, never the store's bytes; drift is evaluated only for the definitions that have an index directory in the input, and the import clears `corrupt` and `indexImportState` on the store's definition as Oak's reindex and importer do); the state rule that replaces oak-run's bring-up-to-date step; `:data` written in the single-blob encoding Oak 1.90.0 writes itself; the definition bookkeeping oak-run's importer, its Lucene half and its definition-refresh step perform; one head move; verification.
- **The `:data` writer** as a reusable component for plan 0010, streaming from any `Read`.
- **Check.** `froe index check` gains the segment-level structural check for Lucene indexes and reports document counts.
- **Evidence.** Safety case, cutpoints, guard evidence, the `lucene_dump` and `lucene_import` interop phases with Oak's own dumper, Lucene's own index checker, Oak's own index consistency check and an out-of-band build by Oak's editors as oracles, a frozen review.

## Out of scope

- Writing Lucene segment files (plan 0009) and building documents from content (plan 0010).
- Bringing an imported index up to date with commits that happened after the checkpoint it was built at. oak-run does that with Lucene's writer; froe instead requires the imported index to reflect the lane checkpoint it will be attached to and refuses otherwise; plan 0010 keeps the same rule.
- Applying definition changes from `index-definitions.json`: the file must agree with the store's definitions under the drift comparison; adding or altering a definition through it is oak-run's job.
- Bringing several lanes up to date from one checkpoint: one `indexer-info.properties` names one checkpoint, so a directory is dumped and imported per lane, and a directory whose checkpoint does not fit every selected definition is refused.
- Importing a hybrid Lucene definition — one with a property definition that is `sync` or `unique`, or a `nodeTypeIndex` rule with `sync`, the test `docs/analysis/index-definitions.md` records: Oak keeps its synchronous property index in the hidden child `:property-index`, which its synchronous editor creates and flags `retainNodeInReindex`; froe builds no property index for a Lucene definition, so such a definition is refused by name.
- Importing a synchronous Lucene definition (one without an `async` property): Oak documents `async` as required, oak-run's importer never completes that case (it leaves the definition on `temp-sync`), and no Oak oracle exists; the import refuses it by name, and a dump of such a definition is a backup only.
- Suggestion directories (`:suggest-data`) on import: absent by a recorded departure from oak-run's Lucene importer, which copies them, and regenerated by Oak's next suggester cycle, whose writer rebuilds the suggestions whenever their `lastUpdated` is missing.
- Elasticsearch definitions and composite-store mounts.
