# Scope — Plan 0006

> Teach froe what an Oak index *is* before it rebuilds one: parse every index definition, async lane, status node and index-data structure a real Oak store holds, list and check them read-only, and give the interop suite an Oak-side judge that can prove those readings against Oak's own tooling.

## Why this plan

Every oak-run `index` operation — `--index-info`, `--index-definitions`, `--index-consistency-check`, `--index-dump`, `--reindex`, `--index-import` — starts from the same three structures: the `oak:QueryIndexDefinition` nodes under `/oak:index`, the asynchronous indexer state under `/:async`, and the hidden storage children each index type writes — `:index` for the property family, in its mirror, unique-entry and counter shapes, and `:data` and `:suggest-data` for Lucene. froe cannot rebuild what it cannot read, and the repository's prime directive says the reading must be specified from the Java first.

This plan is read-only end to end. It carries no high-risk change, so it can land quickly and become the foundation the three mutating plans build on. It also makes the first structural investment in the interop suite: a judge program compiled and run inside the pinned Sling image with the image's own `oak-lucene-1.90.0.jar` (which inlines Lucene 4.7.2, its own index checker included), so that from here on every index claim froe makes can be checked against the Oak build it is published for rather than against froe's own opinion of itself.

The inspected fixture that motivates the model is the store `generate` produces today: 23 `oak:QueryIndexDefinition` nodes under `/oak:index` (a `lucene` fulltext index, a `counter`, a `reference`, and twenty `property` indexes, four of them `unique` and four restricted by `declaringNodeTypes`), one `async` lane with `async`, `async-temp` and `async-LastIndexedTo` properties, property-index `:index` nodes carrying randomly named `:count_<uuid>` approximate counters, and a Lucene `:data` directory whose nine files are single-blob binaries with a `uniqueKey`, a `blobSize` of 1,047,552 and a `dirListing`.

## In scope

- **Specifications.** Three new analysis documents extracted from the pinned Oak sources: index definitions, lanes and the reindex protocol; property-family storage; Lucene storage in the repository.
- **Java semantics.** The URL encoding a property-index key passes through, and the SipHash variant the counter index chains, each ported exactly and pinned by vectors generated with the image's own JDK.
- **The model.** A `froe::index` module: definitions, path filters, value patterns (prefix form), async lanes, status nodes, stored-definition drift, property-index and counter-index readers, the Lucene index-file reader for both binary encodings, and an inventory that aggregates them.
- **Definitions in oak-run's format.** `froe index definitions` writes the same JSON oak-run's definition printer writes, so a file froe produces is a file `oak-run index --index-definitions-file` and Oak's own definition updater accept.
- **Read-only commands.** `froe index list`, `froe index definitions`, `froe index check`, each safe against a live repository like every other read-only command.
- **Digest support.** `froe digest --exclude-property-prefix`, so the randomized `:count_*` counters can be excused from a comparison without excusing anything else, stamped into the digest header like subtree exclusions are.
- **Interop.** The Oak-side judge, a richer `generate` fixture (references, group membership, a property index Oak rebuilt in one synchronous cycle), the `index_inventory` phase, the phase list and run record, and a repair of the interop workflow's stale path filter.

## Out of scope

- Any mutation of a repository. Reindexing and importing are plans 0007, 0008 and 0010; definition updates stay with oak-run and AEM.
- Reading Lucene segment files (`segments_N`, `.si`, `.cfs`) for document counts or a full consistency check; that is plan 0008. This plan's Lucene check is oak-run's level 1, the blobs-only level: every blob readable and every length consistent.
- `valuePattern` regular expressions. froe carries no general regular-expression engine and will not approximate Java's; definitions that use one are reported here and refused by the later reindex plan, never silently reinterpreted (plan 0010's bounded subset serves Lucene property-name patterns only and does not extend to `valuePattern`). `valueIncludedPrefixes` and `valueExcludedPrefixes` are supported in full.
- Elasticsearch definitions, `disabled` definitions and the deprecated `ordered` type: inventoried by type, otherwise ignored.
- Composite-store mounts (`:oak:mount-*`, `:<mount>-index-data`): detected and reported as present; not modelled.
