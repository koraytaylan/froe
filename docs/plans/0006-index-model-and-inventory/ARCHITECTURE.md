# Architecture — Plan 0006

## 0006 — Index Model and Inventory

### Ground truth and citations

The three specification documents this plan writes are where the Java lives: each statement in them names the file and method it was read from, at Apache Jackrabbit Oak commit `4984c4cf26a7ca58ae9ce12c63190b7f492bda78` — the commit `docs/analysis/README.md` already pins — from a blob-filtered sparse checkout of `oak-core`, `oak-core-spi`, `oak-store-spi`, `oak-search`, `oak-lucene`, `oak-commons`, `oak-security-spi` (for the user and group constants), `oak-run` and `oak-run-commons`. This plan and its tasks state the behaviour instead, and point at those documents where a reader needs the source. Where the consumer this repository verifies against — `oak-segment-tar` 1.90.0 inside the digest-pinned Sling image — differs from trunk, the specification says so and the 1.90.0 behaviour wins; task 0601 records that precedence rule in `docs/analysis/README.md`, which today pins only the commit. The modules divide as: `oak-core` holds the indexing editors, the importer, the node-type predicate and the initial content that creates Sling's own definitions; `oak-search` and `oak-lucene` the fulltext index and the repository directory it stores itself in; `oak-store-spi` the JSON serializer and its type codes, the path filter, the visible-editor wrap and the value conversions; `oak-commons` the path helpers and the JSOP builder; and `oak-run-commons` the out-of-band indexer and the options around it, read for the protocol only, because the pinned image ships neither `oak-run` nor `oak-run-commons` — see the judge below.

### The three structures

**Definitions.** A definition is a child of a node named `oak:index` whose `jcr:primaryType` is `oak:QueryIndexDefinition` and which carries a `type`; that triple is what Oak's indexer selects on. froe models: `type`, `async` (a `String` or `String[]` whose lane name is the one value that is neither `sync` nor `nrt`, zero candidates and several both being refusals), `reindex`, `reindexCount`, `reindex-async`, `corrupt`, `supersedes`, `tags`, `queryPaths`, `entryCount`, `keyCount`, the property family's `propertyNames`, `declaringNodeTypes`, `unique`, `valuePattern`, `valueIncludedPrefixes`, `valueExcludedPrefixes`, the path filter (`includedPaths`, `excludedPaths`), the counter's `resolution` and `seed`, and Lucene's `compatVersion`, `:version`, `blobSize` (`max(1024, value)`, default 1,047,552), `saveDirectoryListing`, `evaluatePathRestrictions`, `includePropertyTypes`, `fulltextEnabled` and the presence of `indexRules`, `aggregates`, `facets`, `suggestion`, `analyzers` and `tika`. The stored copy `:index-definition` and the status node `:status` (`uid`, `lastUpdated`, `indexedNodes`, `reindexCompletionTimestamp`) — the latter written by the fulltext editor family alone, when it closes its writer and when it stamps the unique identifier — are modelled beside the definition. Definition drift is the comparison Oak's Lucene index-information provider performs — the current definition's visible clone against the stored `:index-definition`, ignoring `reindex`, `reindexCount` and hidden property names, with a visible child added or removed counting as a difference, a child present on both sides compared by the same filtered rule recursively, and the ignored property names applying at every depth.

**Lanes.** `/:async` holds, per lane `L`: `L` (the checkpoint the lane last indexed to), `L-temp` (checkpoints pending release), `L-lease` (lease expiry), `L-LastIndexedTo` (a `Date`). A property is a lane name when its name is `async-reindex` or ends in `async` — that is Oak's whole test. froe already reads `/:async` for checkpoint retention; this plan gives it a typed model and reports dangling lane checkpoints through the same fact `froe digest` already computes.

**Storage.** Property indexes store under `:index`: the mirror strategy puts `match=true` on `:index/<key>/<path elements…>`, the unique strategy puts `entry=[<absolute path>]` on `:index/<key>`; the reference index uses the mirror strategy under `:references` and `:weakreferences` with the referenced identifier as the key and the *property* path, relative, as the mirrored path; the counter stores `:cnt` per mirrored content node under `:index`. The `:index` node and, for the mirror strategy only, each key node may also carry `:count_<uuid>` approximate-counter properties — the mirror strategy adjusts both on every insert and every remove, the unique strategy the `:index` node alone, and a path-element node never carries one — and those properties are randomized in name, presence and value. Lucene stores each index file as a child of `:data` with `uniqueKey`, `blobSize`, `jcr:lastModified` and `jcr:data`, where `jcr:data` is either a single `Binary` — the streaming form, what Oak 1.90.0 writes by default because `oak.lucene.enableSingleBlobIndexFiles` defaults to true — or a `Binary[]` of `blobSize` chunks, the buffered form, which is what Oak's own Lucene importer writes; in both, the sixteen `uniqueKey` bytes are appended to every stored blob and subtracted from the reported length. `dirListing` on `:data` is consulted only when the definition's `saveDirectoryListing` is not `false` (default true) and is authoritative then; otherwise the child names are the listing.

### Modules

```
crates/froe/src/index/
├── mod.rs                 module root; re-exports
├── definition.rs          IndexDefinition, IndexType, discovery under /oak:index
├── path_filter.rs         PathFilter with Include / Exclude / Traverse
├── value_pattern.rs       prefix include/exclude; typed refusal for regular-expression patterns
├── lanes.rs               AsyncLanes read from /:async
├── status.rs              StatusNode, StoredDefinition, drift comparison
├── inventory.rs           IndexInventory: one IndexInfo per definition
├── definitions_json.rs    oak-run's index-definitions.json rendering
├── definitions_json_reader.rs  the same file read back (documented stub here; plan 0008's importer fills it)
├── property/
│   ├── mod.rs
│   ├── key_encoding.rs    key derivation: empty token, 100-unit truncation, URL encoding
│   ├── mirror.rs          ContentMirror enumeration, counting, consistency
│   ├── unique.rs          UniqueEntry enumeration and consistency
│   ├── type_predicate.rs  TypePredicate over /jcr:system/jcr:nodeTypes
│   └── consistency.rs     check_entries + EntryCheckBudget (the cross-plan pass), check +
│                          NodeCheckBudget, PropertyIndexReport: missing, stale, mismatched, duplicate entries
├── counter/
│   ├── mod.rs             :cnt reader and the estimated node count
│   └── sip_hash.rs        Oak's SipHash-2-2 variant
└── lucene/
    ├── mod.rs
    ├── directory.rs       OakDirectory listing and file streams (both encodings)
    ├── check.rs           level-1 blob check (0611; plan 0008 adds the structural report)
    └── layout.rs          index-details.txt, indexer-info.properties, dump directory names
crates/froe/src/java/url_encoder.rs   Java's URL encoding of a string, UTF-8
```

The tree lists the library modules this plan adds; the command-crate files (`index_display.rs`, `command_line/index.rs`, `command_line/tests.rs`) and the edits to existing files (`tooling/digest.rs`, `tooling_display.rs`, and `content/value/stream.rs` for the `Seek` half task 0608 adds) are the tasks' `touches`. The read-only model, readers and inventory are components; the capability the feature map inventories is the command surface of task 0611, whose rows land with the commands. The CLI adds `crates/froe-cli/src/index_display.rs` and an `Index` command with read-only subcommands; every one opens the repository with `Repository::open_with_progress` exactly like `froe summary` does and never takes the lock. The digest gains `--exclude-property-prefix`.

The interop suite adds `crates/froe-cli/tests/interop/judge.rs` and a `crates/froe-cli/tests/interop/judge/` directory of single-purpose Java classes — `StoreSupport`, which opens the fixture as a segment node store through Oak's own read-only file-store builder, so the judge can never write to what it is judging; `IndexJudge` (`definitions`, `info`); and `LuceneJudge` (`checkindex`, `numdocs`, `dump`, `sample-index`) — copied into a one-off container from the pinned image, compiled together there with the image's `javac 21` against every jar under `/opt/sling/artifacts`, and run by class name with `java -cp` against a bind-mounted store copy. Later plans add one class per subcommand family rather than growing one file, so no dispatcher is shared between tasks; each class stays under the thousand-line rule by convention, since `scripts/oversized-files.sh` gates `.rs` files only. The image ships the Oak 1.90.0 runtime bundles (`oak-core`, `oak-store-spi`, `oak-search`, `oak-lucene` with Lucene inlined, `oak-segment-tar`) but neither `oak-run` nor `oak-run-commons`. Everything the judge asks Oak for is in a shipped bundle: the definition printer and the index printer, the index-information services for Lucene, for the property family and for the async lanes, the service that enumerates index paths, Oak's Lucene dumper, its consistency checker and its document count over a directory, and Lucene's own index checker. What the plans take from `oak-run-commons` — how it opens a segment store as a fixture, how its out-of-band indexer drives a build, and the support around one — is re-implemented in the judge from shipped classes, never loaded. A successful `javac` is the proof that every class the judge needs is in the image. Judge-Oak is consumer-Oak: no second image, no second build.

### Task graph

```
0602 specification: property-family storage (independent)
0603 specification: Lucene storage in the repository (independent)
0602, 0603 ─► 0601 specification: definitions, lanes, reindex protocol, printer JSON; README rows for all three
0602 ─► 0604 Java URL encoder
0601 ─► 0605 definition model
0602, 0604, 0605 ─► 0606 property readers
0602, 0605 ─► 0607 counter reader
0603, 0605 ─► 0608 OakDirectory reader
0601, 0605 ─► 0610 definitions JSON
0606, 0607, 0608 ─► 0609 inventory
0617 command-line module split (independent; `refactor:`)
0617 ─► 0613 digest property-prefix exclusion
0609, 0610, 0613 ─► 0611 CLI read-only commands and their documentation
0614 judge harness (independent)
0616 fixture enrichment (independent)
0611, 0614, 0616 ─► 0615 index_inventory phase
0615 ─► 0612 interop wiring, workflow filter, run record
```

Footprints are disjoint between parallel tasks: the three tasks that edit `crates/froe-cli/src/command_line.rs` (0617, 0613 and 0611) and the two that edit the feature map and `README.md` (0613 and 0611) are chained; the three specification tasks share `docs/analysis/README.md` through 0601 alone, which lands last and lists all three documents; `crates/froe/src/lib.rs` is edited by 0605 only (0604 keeps the encoder crate-private); and the tasks that edit the interop suite's shared files (0614, 0616, 0615, 0612) are chained through 0615 and 0612. User-facing documentation lands in the task that adds the capability, as `CONTRIBUTING.md` requires, so there is no trailing documentation task.

### Proof layers

Unit tests with hand-authored node fixtures for every parser; independent oracles for the two Java-semantics functions (vectors produced by the image's JDK and committed with the producing command); end-to-end tests over synthetic stores built through the writer for the readers and the inventory; and the `index_inventory` interop phase for the real store, where the judge's rendering of Oak's own printers is the oracle for froe's listing and definitions output, Lucene's own index checker is the oracle for froe's Lucene blob check, and the judge's `numdocs` reading of a dumped directory is recorded as the anchor plan 0008's document count is later compared against. The stable host gate from `CONTRIBUTING.md` runs on every task; `cargo clippy --workspace --all-targets --all-features` is mandatory after any interop edit because the suite only compiles under the `interop` feature.
