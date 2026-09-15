# Architecture — Plan 0010

## 0010 — Lucene Offline Reindex

Requires plans 0006 through 0009 merged.

### From a node to a document

Oak's fulltext editor resolves the applicable indexing rule for a node from its primary type and then its mixins, following the node-type hierarchy; the definition's path filter decides `Include`, `Traverse` or `Exclude`; hidden nodes are invisible, as the visible-editor filter makes them everywhere. For an included node with a rule, Oak's document maker walks the node's visible properties plus the synthetic `:nodeName`, looks each up in the rule (with the case-folding the specification must pin, and regular-expression definitions), and per property definition adds the ordered doc values (`:dv<name>`) first, then typed fields (a long, a double or an unanalyzed string term, a DATE as its epoch-millisecond long), analyzed fields (`full:<name>` in Oak's own norm-omitting analyzed form, tokenized unless the name is one Oak never tokenizes), node-scope fulltext (`:fulltext`, Lucene's own analyzed form, with norms), suggest and spellcheck fields, facets, then aggregates (include patterns, `relativeNode` and the reaggregation limit), then the null, function and not-null markers, then the dynamic boost and the consumer-registered field augmentation, node name, `:fulltext` of the node name when fulltext is enabled, and, when `evaluatePathRestrictions`, `:ancestors` over the parent path plus `:depth` over the node's own. The document is then finalized: the facet configuration's build pass re-emits it, and the merged suggest fields are added last. Every field's name, kind, index options, stored flag, norms and doc-value type is fixed by Oak's field construction over Lucene's own field defaults; the writer of plan 0009 receives tokens, so this plan's analysis module produces exactly the token stream Lucene's own inverter would have consumed. Within a property the order is the ordered doc values first, then per value `full:<name>`, `:suggest`, `:spellcheck` and `:fulltext`; the specification pins the whole addition order because positions, offsets and the stored-field sequence depend on it. `docs/analysis/lucene-oak-documents.md` is that specification, and the place a reader goes for the Java behind any statement here.

### Modules

```
crates/froe/src/index/lucene/analysis/
├── mod.rs
├── standard_tokenizer.rs  UAX#29 tokenizer over the Lucene 4.7 grammar's generated scanner
├── lower_case.rs          the consumer JVM's own lower-casing, per code point
├── word_delimiter.rs      word delimiter filter with Oak's flags
├── shingle.rs             shingle filter (2..3, unigrams, "_" filler)
├── path_hierarchy.rs      path-hierarchy tokenizer for :ancestors (one term per ancestor path)
├── suggest_tokenizer.rs   Oak's suggest tokenizer: split on newlines and at 255 UTF-16 code units
├── token_limit.rs         token-count cap: at most maxFieldLength tokens per field (no cap when
│                          negative), except :spellcheck and un-analyzed :suggest, whose per-field
│                          analyzers bypass it
├── unicode/
│   ├── mod.rs
│   ├── word_break.rs      generated Word_Break ranges (pinned WordBreakProperty.txt, packed rows)
│   ├── script.rs          generated Script ranges (Scripts.txt) for the classes the grammar uses
│   ├── line_break.rs      generated Line_Break ranges (LineBreak.txt): Complex_Context
│   ├── block.rs           generated Block ranges (Blocks.txt): Halfwidth and Fullwidth Forms
│   ├── general_category.rs  generated General_Category ranges (UnicodeData.txt): Nd
│   └── lower_case.rs      generated lower-case map enumerated from the image's JVM
└── numeric.rs             numeric token streams, prefix-coded terms, precision steps
crates/froe/src/index/lucene/documents/
├── mod.rs
├── rules.rs               IndexingRule, PropertyDefinition, inheritance, name matching
├── name_pattern.rs        bounded regular-expression subset for property names
├── aggregate.rs           aggregate include patterns and collection
├── document_maker.rs      the port of Oak's document maker
├── facets.rs              the facet configuration's build semantics
└── binaries.rs            pre-extracted text provider, extraction-error marker
crates/froe/src/writer/index/lucene_reindex.rs   the Lucene arm of plan 0007's rebuild dispatch (1007 also edits plan 0007's
                                                 selection.rs, plan.rs, prepared.rs, apply.rs and mod.rs to admit it)
crates/froe/src/writer/fault_injection/lucene_reindex.rs   cutpoints
crates/froe/examples/generate_unicode_tables.rs  the table generator (a Rust example, so the gate compiles it)
crates/froe/src/java/iso8601.rs                 millisecond ISO-8601 parser (1004's `refactor:` commit; `version_storage.rs` delegates)
crates/froe-cli/tests/interop/judge/Analyze.java                    the judge classes this plan adds (1003)
crates/froe-cli/tests/interop/judge/NumericVectors.java             (1004)
crates/froe-cli/tests/interop/judge/RegularExpressionVectors.java   (1005)
```

The generated tables live in their own files under `analysis/unicode/`, one per Unicode property the 4.7 grammar's character classes reference — Word_Break, Script, Line_Break, Block and General_Category (the token types `<IDEOGRAPHIC>`, `<HIRAGANA>`, `<KATAKANA>`, `<HANGUL>` and `<SOUTHEAST_ASIAN>` cannot be derived from Word_Break alone, whose value for Han and Hiragana is `Other`; which property each class uses, for instance whether Katakana is `\p{WB:Katakana}` or a Script class, task 1002 quotes from the pinned grammar and the tables follow the quotation, not this summary) — with `#[rustfmt::skip]` packed rows so each stays under the thousand-line gate; every Unicode Character Database file is pinned by URL and SHA-256 for the Unicode version task 1002 reads from the pinned grammar's `%unicode` directive, and the lower-case table is enumerated by the judge from the image's JVM, the consumer's own truth. The generator, a Rust example, takes the downloaded files' paths and verifies each against its pinned checksum by running `sha256sum` (or `shasum -a 256` where coreutils is absent) through `std::process::Command`, so no digest implementation and no network client enter the crate; the requirement is recorded in each table's header.

### Verification design

Three oracles, all on the pinned build: the judge's `analyze` subcommand prints every token with position increment and offsets for a corpus through Oak's analyzer, the shingle wrapper, the path-hierarchy tokenizer and the suggest tokenizer, which the analysis tests replay; the judge's `enumerate` from plan 0009 compares the froe-built `:data` (dumped) with Oak's own reindex of the same definition on a copy of the same store; and a live Sling answers queries through both indexes. Oak's reindex is deterministic in content but not in bytes, so enumeration equality — fields, terms, postings, positions, offsets, stored values, doc values, norms, over live documents with term statistics recomputed — is the standard; segment layout, deleted documents, `uniqueKey`s and timestamps are not compared.

### Task graph

```
1002 analysis specification (independent)
1011 safety case (independent; 1007, the first mutating task, depends on it)
1002 ─► 1001 document specification (owns both README rows)
1002 ─► 1003 analyzers (stubs numeric.rs and documents/mod.rs)
1002, 1003 ─► 1004 numeric encodings
1001, 1003 ─► 1005 rules and patterns
1003, 1004, 1005 ─► 1006 document maker
1006, 1011 ─► 1007 reindex operation (library)
1007 ─► 1008 command flags and documentation
1008 ─► 1009 interop phase and suite wiring
1009 ─► 1010 review (gated)
```

Task 1003 registers `analysis` in the Lucene module root and leaves documented stubs for `analysis/numeric.rs` and `documents/mod.rs`, so 1004 and 1005 run in parallel without touching the root; each judge extension is its own class (`Analyze`, `NumericVectors`, `RegularExpressionVectors`), never a shared file. The command exposes the Lucene branch only in 1008, where its flags and documentation land together; 1007's library operation refuses a Lucene definition until a binary-text policy is supplied, which the command cannot do before 1008, and 1007 lands the library row for the capability it makes public. The analyzers, numerics, rules and document maker are components of that capability. The safety-case task 1011 is numbered after the review because it was added later; it depends on nothing and 1007 depends on it, so no code that publishes bytes lands before the case is written.

### Mutation and publication order

The rows below are the boundaries the native reindex adds. The shared publication and verification rows (head published, applied-state verified — the spine rewrite sits inside the publication row) are cited from plan 0007's table, since the Lucene branch publishes through task 0707's `apply.rs` with 0708's cutpoints and 0709's guard rows, the `OakDirectory` write mechanism and its read-back precondition are cited from plan 0008's case, the definition-rewrite and `:data`-copy rows are restated here because their edits, cutpoints and preconditions differ from the import's, and the two work-directory rows are boundaries this plan adds, the import having none.

| Boundary / cutpoint | Preconditions | Published or durable change | Returned-error state and named regression | Abrupt-exit state and named regression | Reconciliation |
| --- | --- | --- | --- | --- | --- |
| Documents made and postings, doc-value and norm runs spilled in a per-run subdirectory under `--work-directory`; stored fields already streaming into the segment (`lucene-reindex.after-last-document`, fired after the last document is added and before `finish`) | plan replanned under the lock | files outside the store only | removed on return (regression named by 1007) | leftover spill files and partial stored-field files in the run's subdirectory, never in the store (1007) | operator removes them; the next run's plan refuses residue in an operator-named directory (0707) |
| Segment finished in the work directory (`lucene-reindex.after-segment-finished`, fired when `finish` returns and before the first `:data` record) | every run merged into the segment | files outside the store only | removed on return (1007) | a complete segment in the run's subdirectory, never in the store (1007) | operator removes it; the next run's plan refuses residue in an operator-named directory and warns under the default, and each attempt writes into a fresh segment directory so a dead run's segment is never mistaken for this one's (1007) |
| Segment files copied into `:data` (`lucene-reindex.mid-file-copy`) | segment assembled and parsed by plan 0008's readers; the three apply-identity gates passed in `prepare`; `open_prepared` after the fingerprint and path-identity rechecks | new archives only, unreachable from the head | store unchanged plus unreferenced archives; the file and byte offset reached are reported (1007) | same (1007) | the next `froe compact` retires the archives |
| Definition rewritten (in addition to plan 0008's row — `reindex=false`, `corrupt` and `indexImportState` removed, `:disableIndexesOnNextCycle` under its predicate — `ReindexCount::Increment`, `refresh` removed when set, `seed` created when absent, the `facets` node created on the first facet property with per-element children carrying `jcr:primaryType` and, for `STRINGS` dimensions, `multivalued`, `:status` and `:version` set, `:index-definition` replaced by the visible clone of the pre-run state, `:suggest-data` removed; under a reset only the hidden children removed) | the segment read back and parsed (not under a reset, which writes no segment); every gate passed in `prepare` | new records only, unreachable from the head until publication | store unchanged plus unreferenced archives at the head's generation (1007) | same (1007) | as plan 0008's row |
| Head publication and applied-state verification | as plan 0007's rows, with the `:data` read-back precondition from plan 0008's (under a `--from-head` reset: no read-back and no `:status`/`:version` — the only mutation is the definition rewrite with its hidden children removed, then the spine; 1007's reset test) | as plan 0007's rows | as plan 0007's rows; the Lucene branch publishes through task 0707's `apply.rs`, so the cutpoints are the `index-reindex.` ones task 0708 armed, reached by a wiring test (1007) | same | as plan 0007's rows |

### Safety case

This plan is high-risk under [`high-risk-changes.md`](../../high-risk-changes.md),
and its safety case lives here, as a section of this file, in the same place
plans 0001, 0002, 0004, 0007 and 0008 keep theirs. It succeeds
[`0007-property-index-reindex/ARCHITECTURE.md`](../0007-property-index-reindex/ARCHITECTURE.md),
which remains the case for the open protocol, the publication boundaries and
the definition bookkeeping, and
[`0008-lucene-index-transport/ARCHITECTURE.md`](../0008-lucene-index-transport/ARCHITECTURE.md),
which remains the case for the `:data` write and its `OakDirectory`
preconditions. Neither is reopened here: what follows is only what the
*native* reindex adds.

Task 1011 writes this section before any of the plan's mutating code lands,
task 1007 fills the fault and guards tables as it arms each cutpoint, task
1009 supplies the interoperability record, and task 1010 freezes the range
with the verification report, the known gaps and the review.

In scope on four counts.

* **It publishes bytes froe computed, from content froe read, in a format
  froe learned to write two plans ago.** The import of plan 0008 copied an
  index Oak's own editors had built; this one *derives* every posting,
  every position and every norm. A wrong derivation is froe's, and no
  container check can catch it: a `.cfs` whose headers, table of contents
  and read-back are all perfect still answers the wrong query if the
  analyzer put the token in the wrong place. The oracle for that is task
  1009's — Oak's own reindex of the same store, enumerated and compared —
  and it is evidence, not a runtime guard.
* **It runs an analysis chain whose answers must match a *different*
  runtime's.** The tokenizer's tables are Unicode 6.3.0 because Lucene
  4.7.2 baked them in; the case mapping and the word-delimiter classes are
  the *consumer JVM's*, because Lucene asks `Character` at run time. A port
  that unified them would be wrong twice, and the terms it wrote would
  differ from the terms a query looks up.
* **It writes outside the store, at index scale.** The property reindex
  spills sorted runs; this one spills postings, doc values and norms, and
  assembles a whole segment in the work directory before a byte reaches
  `:data`. Everything about that is new: the directory's residue after a
  crash, its worst-case size, and the fact that an operator-named directory
  is where it lands.
* **It reads binaries it will not extract.** froe runs no Tika. A binary
  property's text comes from Oak's own pre-extracted store or is the
  `TextExtractionError` marker, and which one is an operator's decision
  carried in the binary-text policy. The index that results answers fulltext
  queries over binaries *differently* from Oak's unless the pre-extracted
  store is complete — a limit, stated here, not a defect to be found later.

Covers `crates/froe/src/index/lucene/analysis/**`,
`crates/froe/src/index/lucene/documents/**`,
`crates/froe/src/java/iso8601.rs`,
`crates/froe/src/writer/index/lucene_reindex.rs`,
`crates/froe/src/writer/fault_injection/lucene_reindex.rs`, and the
Lucene arms task 1007 adds to `crates/froe/src/writer/index/selection.rs`,
`plan.rs`, `prepared.rs`, `apply.rs` and `mod.rs`.

#### Scope and retention

**What survives a native reindex, unconditionally.** Everything outside the
selected definitions: every other definition — including one the same run
skipped or refused — the whole content tree, `/:async`, and **every
checkpoint**. The run releases none: the lane's checkpoint is the state it
indexes, and it is still the lane's when the run ends.

The content tree's survival is a *testable* invariant rather than a
promise: `tooling::digest::digest_repository_excluding` over everything but
`/oak:index` is identical before and after a run, which is the library
entry behind `froe digest --exclude-subtree /oak:index`.

**What survives within a selected definition.** Every visible property and
every visible child, except:

| Property or child | What the reindex does | Why |
| --- | --- | --- |
| `reindex` | set to `false` | the index is now current as of the lane checkpoint's state |
| `reindexCount` | incremented (`ReindexCount::Increment`) | as Oak's own cycle increments it |
| `corrupt` | removed | Oak's cycle clears it when the index becomes usable |
| `indexImportState` | removed | Oak's reindex step removes it |
| `refresh` | removed when set | Oak's editor consumes it when it builds the definition |
| `seed` | created when absent, kept when present | Oak's fulltext editor creates one for a definition that has none |
| `facets` (visible child) | created on the first facet property indexed, with one child per path element of each dimension carrying `jcr:primaryType = nt:unstructured` and, for a multi-valued `STRINGS` dimension, `multivalued = true` | Oak's facet configuration is node-state-backed and persists into the visible definition as the document maker consults it |
| `:disableIndexesOnNextCycle` (hidden, on `/:async`) | written under the disabler's predicate | froe never disables a superseded index itself; Oak's next cycle does |

**What is replaced.** `:data` with the segment this run assembled, `:status`
with a fresh `uid` and the run's `lastUpdated`, `indexedNodes` and
`reindexCompletionTimestamp`, `:version`, and `:index-definition` with the
visible clone of the definition's **pre-run** state — which is what Oak's
own editor clones when it enters reindex mode, so it carries `reindex =
true`, the old `reindexCount` and no `seed` this run created.
`:suggest-data` is removed and never rebuilt.

**What a `--from-head` reset does instead.** Removes the hidden children,
keeps `reindexCount` (`ReindexCount::Keep`), builds nothing, writes no
`:status` and no `:version`, and leaves every visible property untouched.
That is the whole mutation. It exists because a definition whose lane
checkpoint is gone cannot be rebuilt into a state Oak will agree with:
Oak's fulltext editor re-enters reindex mode on a missing before-state and
its writer's reindex branch *appends* every document to whatever `:data`
still holds, doubling the index whether or not froe had rebuilt it. Removing
the hidden children is what makes Oak's own next cycle rebuild from scratch,
and the plan output states the reset and the reason.

**What is never read and never written.** Any binary's bytes. froe opens no
blob for extraction; the policy supplies text from Oak's own pre-extracted
store or supplies the marker, and neither path reads the binary itself.

**What a refusal costs.** Nothing. Every refusal below — the codec verdict,
`valueRegex`, a hybrid or synchronous definition, a missing binary-text
policy, an unsupported property-definition feature, an unparseable DATE
value — lands before the first `:data` record is appended, so a refused run
leaves a store byte-identical to the one it found, and the run's
subdirectory removed.

#### Authoritative state

**The preview is lockless and advisory.** `plan_reindex` opens the store
read-only, selects the definitions, resolves each lane's checkpoint and
walks the state to count. Its record identities do not survive it, and
nothing it reports is acted on.

**`PreparedReindex::prepare` is where authority is taken**, and it is plan
0007's protocol unchanged: the repository-shape check and the two
apply-identity gates *before* the lock, the lock, the same two gates again,
the replan against locked state, the fingerprint, the certified archive
number and the metadata-source gate. `apply` then rechecks the fingerprint
and the lock's path identity and opens through
`WritableRepository::open_prepared`.

**The facts the replan rechecks**, because each one can change between the
preview an operator read and the run that acts:

* the definition set and each definition's record identity;
* each definition's lane and the checkpoint that lane names — a checkpoint
  released between preview and apply turns a rebuild into a refusal or, with
  `--from-head`, into a reset;
* **the codec verdict**, which turns on the definition's own rules: a
  definition edited between preview and apply into one whose verdict is not
  `oakCodec` is refused under the lock;
* **the binary-text policy**, which is the caller's and is therefore checked
  where the caller's other options are;
* the head.

#### Mutation and publication order

The `### Mutation and publication order` section above is this case's
table, cited by its heading rather than copied. Its shared publication and
verification rows are plan 0007's, its `OakDirectory` write mechanism and
read-back precondition are plan 0008's, its two work-directory rows are
boundaries this plan adds, and its `:data`-copy and definition-rewrite rows
are restated there because the edits, the cutpoints and the preconditions
differ from the import's. Each row names the regression task 1007 adds.

#### Interruption prefixes

**The old-or-new-head rule is plan 0007's**, inherited whole: a reader that
opens the store after any interruption sees either the head the run found or
the head it published, never a mixture, because every record the run writes
is unreachable until the one `compare_and_set_head`.

What this plan adds is *outside* the store, and its prefixes are:

* **Before `finish` (`lucene-reindex.after-last-document`).** Spill runs and
  a partially written stored-fields file in the run's subdirectory under the
  work directory. A returned error removes the subdirectory; a dead process
  leaves it. Nothing is in the store.
* **After `finish`, before the first `:data` record
  (`lucene-reindex.after-segment-finished`).** A complete segment in the run's
  subdirectory. Same rule: removed on return, left behind on death, never in
  the store. **Each attempt assembles into a fresh segment directory**, so a
  dead run's complete segment can never be mistaken for this attempt's.
* **During the copy (`lucene-reindex.mid-file-copy`).** Unreferenced archives
  at the head's generation, which the next `froe compact` retires; the store's
  reachable state is unchanged. A returned error reports the file and the
  byte offset reached.

The reconciliation for the first two is an operator removing the
subdirectory: the next run's plan refuses residue in an operator-named work
directory and warns under the default, which is plan 0007's rule and not a
new one.

#### Observed outcomes

Every count the run reports is built from what it observed, not from the
plan it started with: **documents made** (one per visited node with a rule),
**nodes visited** (the counter the resources section's cost statement cites),
**segment bytes** and **files copied** as the copy performed them, and the
head **before and after**. A run that refused reports no counts at all
rather than the plan's estimates, and a reset reports the hidden children it
removed rather than a document count it never produced. `ReindexPlan::is_empty()`
short-circuits a selection with nothing to do, and the outcome says
`nothing to do` with the head unmoved.

#### Resources

**Memory is the writer's budget**, which plan 0009 fixed: postings, doc
values and norms spill through the workspace's external sort against one
shared budget, and the terms writer's pending blocks are charged to it. What
is *uncharged* is the field-infos table and the per-field counters, which are
proportional to the number of distinct field names rather than to the index,
and which the format itself requires a writer to hold; and the document
maker's own per-node state, which is one document.

**Open files** are task 0702's merge fan-in plus one for the spill inputs —
task 0910 merges one format at a time — plus the named constant of segment
outputs a single-segment write holds open at once.

**Temporary disk is the plan's one proxy, and it is named as one.** The plan
reports two byte totals the counting walk produces without analyzing
anything — the bytes of values the rules mark stored, and the bytes of values
they mark indexed — and the figure task 1007 derives from them, with the
basis for whatever scaling it applies recorded in `docs/index.md`. It is a
proxy because nothing in this repository measures bytes per token or bytes
per posting: both are workload statistics rather than format facts, and a
stated figure would be worse than a named proxy. The derivation must account
for the spill runs, which coexist with the assembled segment until `finish`
drains them, and for the compound copy, which holds the segment twice.

**Time** is two walks of the state per confirmed run, as in plan 0007 — one
to count, one to build — plus the inversion's sort and merge passes over the
spilled runs and one pass per codec file, pinned by task 0910's merge-pass
counter and task 1007's visited-node counter rather than by a benchmark.

**The store grows by the index's size** until a `froe compact` run after
every checkpoint that references the old `:data` has been released — each
lane's, by construction. That is the honest cost and the plan output states
it.

**Exhaustion leaves a safe prefix.** A spill or a segment write that fails on
`ENOSPC` returns a typed error before the first `:data` record, with the
store unchanged and the run's subdirectory removed; a copy that fails leaves
unreferenced archives and reports the file and offset.

#### Guards

Every row below was produced the same way plans 0007 and 0008 produced
theirs: the guard was removed on its own in a working tree, its named
regression was run against a separate target directory, the failure was
recorded verbatim, and the code was restored. The regressions live in
`crates/froe/tests/lucene_reindex_guard_tests.rs` unless the row says
otherwise.

| Guard and production callers | Named regression | Neutralization | Observed failing result |
| --- | --- | --- | --- |
| A Lucene definition needs a binary-text policy, and the gate **refuses rather than skips** (`select` → `refuse_lucene`; both production phases — the lockless plan and the replan under the lock — go through it) | `a_lucene_definition_without_a_binary_text_policy_is_refused_by_name`, and `a_lucene_definition_without_a_binary_text_policy_is_refused` in `index_reindex_guard_tests.rs` for a run of any selection | `if !options.has_binary_text_policy` replaced with `if false && …` | `the definition must be refused, not planned: [RebuildLucene { path: "/oak:index/lucene", state: LaneCheckpoint { lane: "async", checkpoint: "lane-checkpoint" }, rules: 1, documents: 2, stored_bytes: 0, indexed_bytes: 47, binary_text_policy: "none" }]` — a plan to rebuild a fulltext index with no answer for its binaries. |
| The codec verdict must be `oakCodec` (`select` → `refuse_lucene` → `IndexingRules::read`) | `a_definition_whose_codec_is_not_oak_codec_is_refused_naming_lucene46` | `if codec != CodecVerdict::OakCodec` replaced with `if false && …` | `the definition must be refused, not planned: [RebuildLucene { path: "/oak:index/lucene", state: LaneCheckpoint { lane: "async", checkpoint: "lane-checkpoint" }, rules: 1, documents: 2, stored_bytes: 0, indexed_bytes: 17, binary_text_policy: "the extraction-error marker" }]` — froe would have written the `oakCodec` composition into an index Oak opens with `Lucene46`. |
| A definition-level `valueRegex` is refused: it gates the per-property fulltext loop with `Matcher.find`, and froe evaluates no value regular expression (same path) | `a_definition_level_value_regex_is_refused_by_name`, and the twin in `lucene_rules_tests.rs` for the rules reader itself | the `valueRegex` read replaced with `None` | `the definition must be refused, not planned: [RebuildLucene { path: "/oak:index/lucene", state: LaneCheckpoint { lane: "async", checkpoint: "lane-checkpoint" }, rules: 1, documents: 2, stored_bytes: 0, indexed_bytes: 47, binary_text_policy: "the extraction-error marker" }]` — every value indexed, where Oak indexes the ones the expression finds. |
| A construct this plan does not port — `function`, `dynamicBoost`, `useInSimilarity`, `similarityTags`, `compatVersion 1`, `maxFieldLength = 0`, a consumer-registered analyzer, an `nt:base` rule with `nullCheckEnabled`, a `unique` or `sync` property definition, two rules typing one ordered property differently (same path) | `a_definition_using_an_unported_feature_is_refused_by_name`, over the eleven cases of `lucene_rules_tests.rs` | the four-name loop's `is_some()` test replaced with `false && …` | `the definition must be refused, not planned: [RebuildLucene { path: "/oak:index/lucene", state: LaneCheckpoint { lane: "async", checkpoint: "lane-checkpoint" }, rules: 1, documents: 2, stored_bytes: 0, indexed_bytes: 47, binary_text_policy: "the extraction-error marker" }]` — an index missing the fields that construct writes. |
| A hybrid definition is refused: Oak keeps a synchronous `:property-index` froe does not build (same path) | `a_hybrid_definition_is_refused_by_name` | `if definition.indexing_mode.synchronous_synonym` replaced with `if false && …` | `the definition must be refused, not planned: [RebuildLucene { path: "/oak:index/lucene", state: Head, rules: 1, documents: 6, stored_bytes: 0, indexed_bytes: 127, binary_text_policy: "the extraction-error marker" }]` — note the state: a hybrid definition falls through to a **head** rebuild, which is the wrong state as well as the wrong index. |
| A definition with no `async` is refused: Oak documents it as required and no oracle exists for the synchronous case (same path) | `a_definition_without_async_is_refused_by_name` | `if definition.indexing_mode.synchronous` replaced with `if false && …` | `the definition must be refused, not planned: [RebuildLucene { path: "/oak:index/lucene", state: Head, rules: 1, documents: 6, stored_bytes: 0, indexed_bytes: 127, binary_text_policy: "the extraction-error marker" }]` |
| A Lucene definition on a lane whose checkpoint is gone is **reset** under `--from-head` and refused without it (`resolve_state`) | `a_lost_checkpoint_resets_a_lucene_definition_under_from_head` and `a_lost_checkpoint_is_refused_without_from_head` (in `lucene_reindex_tests.rs`) | the `IndexType::Lucene` arm removed, so the definition falls through to the head rebuild | `a lost checkpoint resets rather than rebuilds: RebuiltIndex { documents: 0, nodes_visited: 9, files: ["segments.gen", "segments_1"], segment_bytes: 65 }` — an index rebuilt from a head whose content the lane never reached, which Oak's own next cycle would then append to again. |
| `:data` is copied **by name from the file set `finish` returned**, never from the directory listing (`rebuild_lucene_index` → `copy_segment_into_store`) | `a_stray_file_in_the_segment_directory_is_not_copied`, in-crate in `writer/index/lucene_reindex.rs` over the seam that plants one | the loop replaced with one over `std::fs::read_dir(segment_directory)` | `the rebuild runs: InvalidFormat { details: "the rebuilt /oak:index/lucene does not read back as a coherent index: 0 missing, 1 unreferenced, 0 unreadable" }` — **defence in depth**: the read-back verification catches the stray as an unreferenced file before the head moves, so the run refuses rather than publishing it. The regression's own assertion never runs, which is the honest result and is why the row says so. |
| A node's template property names are written in **Oak's own `Template` sort order** — Java string hash, then the name in UTF-16 order, then the type tag — whatever order the caller passed (`RecordWriter::write_node_with_stable_identifier`; every node froe writes, the index definitions `rewrite_node_with_edits` rewrites among them) | `a_node_written_out_of_order_is_stored_in_template_order`, in-crate in `writer/record_writer/nodes.rs` | `if in_template_order(properties)` replaced with `if true \|\| …` | `assertion left == right failed: the stored name order is the order Oak's own Template sorts into` — `left: ["zz", "Aa", "active"]`, `right: ["active", "Aa", "zz"]`. Oak's `getProperties` pairs the *i*-th **sorted** property template with the *i*-th value slot, so the stored order is the only thing that keeps a value with its own name; froe's own reader pairs stored position with stored position and cannot see the difference. Task 1009's interop oracle is what found it: Oak read a definition froe had rewritten as `:version LONG "async"`, `includedPaths STRINGS count=20054016`. |
| A regular-expression property definition writes an **untokenized** `full:<name>` for a name in `IndexHelper.NOT_TOKENIZED` (`DocumentMaker::index_analyzed` → `skip_tokenization`) | `fields::a_regular_expression_definition_does_not_tokenize_the_names_oak_excludes` (in `tests/lucene_documents/`) | the `skip_tokenization` branch replaced with `if false && …` | `left: ("full:jcr:uuid", DocumentsAndFrequenciesAndPositionsAndOffsets, true, false, ["b4f8474f", "885f", "4725", "a389", "ce6deb1c3532"])`, `right: ("full:jcr:uuid", Documents, false, false, ["b4f8474f-885f-4725-a389-ce6deb1c3532"])` — a `jcr:uuid` broken into five terms with offsets and stored, where Oak writes one untokenized `DOCS_ONLY` term and stores nothing. Every `jcr:uuid` in a Sling repository reaches the fixture's catch-all pattern. |
| The facet configuration persists a child **only** for a multi-valued dimension (`lucene_definition_edits` → `write_facet_configuration`) | `a_single_valued_facet_leaves_the_configuration_node_childless` (in `lucene_reindex_tests.rs`), with `a_multi_valued_facet_writes_one_child_carrying_multivalued` beside it | `if !dimension.multi_valued { continue; }` replaced with `if false && …` | `left: ["\tjcr:primaryType=Name:nt:unstructured", "/jcr:title\tjcr:primaryType=Name:nt:unstructured\tmultivalued=Boolean:true"]`, `right: ["\tjcr:primaryType=Name:nt:unstructured"]` — a child per dimension whatever its arity, where `NodeStateFacetsConfig` writes one only from `setMultiValued(dim, true)`. Oak's own rebuild of the fixture's faceted definition writes an empty `facets` node. |
| The document-time refusal of an unparseable `DATE` (`DocumentMaker::make` → `date_value`) | `fields::an_unparseable_date_is_refused_by_name` (in `tests/lucene_documents/`) | the conversion replaced with `Ok(0)` | `an unparseable date is refused` — every unparseable date indexed as the epoch, where Oak's own commit fails. |
| The segment reads back through plan 0008's own readers **before publication**, and its live document count is the writer's (`verify_before_publication` → `verify_lucene_segment`) | `a_run_rebuilds_a_lucene_index_and_leaves_the_content_tree_untouched` (in `lucene_reindex_tests.rs`), and the row above, whose neutralization it caught | — | **Not neutralizable from outside**: no input makes the bytes disagree, because the only way to produce a mismatch is a writer/reader disagreement, which is what the check exists to catch — the same carve-out plan 0008 records for its own read-back. The row above is the evidence it fires. |
| Each attempt assembles into a **fresh** segment directory (`rebuild_lucene_index`) | `a_death_after_the_segment_is_finished_leaves_a_whole_segment_outside_the_store`, in-crate in `writer/fault_injection/lucene_reindex.rs` | — | **Not neutralized.** The residue refusal of plan 0007 stops a retry against a dead run's directory before the freshness rule could matter, so removing the `remove_dir_all` changes no observable behaviour today — defence in depth, recorded as a finding about the design rather than a gap in the evidence. |
| The empty-plan short circuit (`ReindexPlan::is_empty`) | `a_refused_run_leaves_the_store_byte_identical` | — | Task 0709's row: this plan adds a caller, not a guard. |
| The disabler's flag is written only under Oak's predicate (`rebuild_one` → `disabler_verdict`) | plan 0008's `a_supersedes_naming_an_active_index_raises_the_disabler_flag` and plan 0007's rows | — | Task 0709 recorded the neutralization; this plan adds a caller, not a guard. |
| The three writer refusals plan 0009 installs and this plan publishes through — a boost on an `omit_norms` field, a first token with position increment 0, a doc-values type change (`LuceneIndexWriter::add_document`) | task 0910's named regressions in `lucene_writer_tests.rs` | — | Task 0910 recorded each neutralization in the landed writer. |

#### Fault and subprocess tests

Every test runs through the child harness of
`writer::fault_injection::test_support`: the child arms one cutpoint,
exits with `CRASH_EXIT_CODE` there or with `VERIFIED_EXIT_CODE` after its
own assertions, and the parent reopens the store freshly and asserts the
prefix. The tests live in `writer/fault_injection/lucene_reindex.rs`.

| Cutpoint | Fault model | Named test | Asserted prefix |
| --- | --- | --- | --- |
| `lucene-reindex.after-last-document` | returned error | `an_error_after_the_last_document_leaves_the_store_unchanged` | every store file byte-identical, the definition as the run found it, and the run's subdirectory removed |
| `lucene-reindex.after-last-document` | abrupt exit | `a_death_after_the_last_document_leaves_files_outside_the_store` | the same store, spill files left in the run's subdirectory, and the retry refusing that residue before rebuilding once it is cleared |
| `lucene-reindex.after-segment-finished` | returned error | `an_error_after_the_segment_is_finished_leaves_the_store_unchanged` | the same store, subdirectory removed |
| `lucene-reindex.after-segment-finished` | abrupt exit | `a_death_after_the_segment_is_finished_leaves_a_whole_segment_outside_the_store` | the same store, a **complete** segment in the subdirectory, and the retry refusing it as residue |
| `lucene-reindex.mid-file-copy` | returned error | `an_error_in_the_middle_of_the_copy_changes_no_byte_of_the_store` | the store unchanged; the records appended for the bytes already read are unreachable from the head |
| `lucene-reindex.mid-file-copy` | abrupt exit | `a_death_in_the_middle_of_the_copy_leaves_the_head_where_it_was` | the same prefix, and the retry rebuilding after the residue is cleared |
| `index-reindex.before-head-publish`, `index-reindex.after-head-publish-before-flush`, `index-reindex.before-applied-verification` | both | task 0708's own tests, with `a_lucene_selection_runs_through_the_prepared_wrapper` (in-crate in `writer/index/apply/tests.rs`) proving a Lucene selection reaches them | as plan 0007's rows |

#### Interoperability

The `lucene_reindex` phase, against the pinned image
(`docker.io/apache/sling@sha256:8722cd66…`, oak-segment-tar 1.90.0), with
`docs/interop.md` carrying the operator's account of it.

**The loop.** Sling boots on a copy of the fixture, the query probe is
installed *before* anything is flagged so both sides index it
symmetrically, every `lucene` definition is flagged, the `async` lane
rebuilds them, Sling stops and that store is extracted. froe gets a copy
with each definition's bookkeeping put back to what Oak started from —
`reindex` flagged, `reindexCount` one below Oak's value — and rebuilds it
with `--binary-text marker`. Both `:data` subtrees are dumped and
enumerated by the judge's `Corpus enumerate`, which reads live documents
only through Lucene's own readers.

**The extracted index is checked canonical first.** A lane cycle between
Oak's rebuild and the stop updates a document as a delete and an add, so
the phase reads every `segments_N` with plan 0008's own segment reader and
repeats the cycle — bounded to three attempts — while any segment carries a
deletion. The attempt it passed on goes into `canonical-index-lucene.txt`
and into the run record.

**The comparison is re-keyed by `:path`.** Lucene's document numbers record
the order a writer added documents in: Oak's editor takes it from a node's
`MapRecord` order and froe's walk takes it sorted by name. Nothing in the
index records that order and no query can observe it, so both enumerations
are re-keyed by each document's own stored `:path` and rendered back in one
canonical order. The commit file's `counter` is excluded for the same kind
of reason — it counts flushes and merges, not contents.

**What the variant definition carries.** The branches the one Sling ships
has none of, and which the first version of this phase did not reach
either: `evaluatePathRestrictions`; an `ordered` property in the
new-format place, over a long, a double, a date, a **string** and a
**boolean**, so both doc-value kinds are written; a **multi-valued**
string that is faceted, ordered and node-scope indexed at once; `facets`
on a **long**, which Oak's own type test writes no facet field for;
`nullCheckEnabled` and `notNullCheckEnabled` under a rule whose node type
is not `nt:base`; `useInSuggest` and `useInSpellcheck`; a `boost` other
than the default; a **relative** property definition and a relative
**pattern**, which are the two shapes AEM's own definitions are written
in; `indexNodeName`; `excludedPaths` inside the included subtree;
`valueExcludedPrefixes`; `maxFieldLength`; `indexOriginalTerm` beside a
word the delimiter filter splits; a `codec` naming `oakCodec` outright;
an aggregate of seven includes — a plain one, two `relativeNode` ones, a
two-step `jcr:content/*`, two carrying a `primaryType` that holds and one
that does not, and one over a child whose type **no rule covers**; a
**second indexing rule** over the node type another aggregated child
carries, with `excludeFromAggregation` on one property, a relative
definition of its own, and an aggregate of its own, which is what
re-aggregation is; and an aggregate declared for that ruleless type,
whose grandchild reaches the page all the same. The content carries a **binary with no
`jcr:mimeType`**, so the gate Oak's extraction stops at is compared
rather than excluded.

**Observed, 2026-09-15.** Both definitions identical:
`/oak:index/interopLucene`, 24 documents and 1,988 enumerated lines, no
exclusion; `/oak:index/lucene`, 8,335 documents and 107,180 enumerated
lines, identical outside 2,843 declared binary exclusions. Lucene's own
`CheckIndex` clean over each froe rebuild, and `froe index check` clean
over the store it wrote them into. Each definition node identical
to Oak's beside its index, excluding `:data`, `:suggest-data`, the
`:status` timestamps and
`uid` and the `:index-definition` clone's `reindexCount` — so the `facets`
configuration, the `seed`, the removed `refresh`, the `:version` and
`:status`'s indexed-node count are all Oak's own values. The content digest
changed only inside `/oak:index` and `froe
check` passed at the new head. A booted Oak logged no repair, no reindex
and no index failure, and answered ten statements — node-scope and
property fulltext, `ORDER BY` over an ordered doc value, `IS NULL`, a facet
column, a multi-valued facet column, an `ISDESCENDANTNODE` that reaches
`:ancestors`, a path-restricted
property term, a `CONTAINS` over a relative definition's own field, and one
against the repository-wide definition — with the
same rows and the same `EXPLAIN` plan it answers from its own rebuild, each
plan naming the index the statement was written for.

**The suggester is handed back, and that is checked.** froe removes
`:suggest-data` and builds no dictionary. Oak's rebuild of a definition
carrying `useInSuggest` writes one, so the definition comparison declares
that node on both sides rather than excluding it quietly; the booted Oak
then gets one node written under the definition and is waited for through
a query until its lane has run a cycle, the store is extracted from that
boot, and `:suggest-data` is back.

**The declared difference.** froe extracts no text, so under
`--binary-text marker` it indexes Oak's own `TextExtractionError` where Oak
indexed a binary's extracted text. The exclusion is applied at the posting
level to **both** sides before any statistic is derived — postings, stored
value and norm for that document and field, terms left with no postings
dropped, frequencies recomputed — and its extent is derived twice and
required to agree exactly: from froe's index, as the documents whose
`:fulltext` carries the marker term, and from the store, as every node the
definition includes that carries a binary property the rule indexes
fulltext and a `jcr:mimeType`. 2,843 documents on the repository-wide
definition, none on the variant.

**The reset scenario.** froe removes the variant's lane checkpoint and
resets the definition under `--from-head`, leaving `reindex` raised, every
other visible property untouched and no hidden child behind. Oak's own next
cycle logs `Failed to retrieve previously indexed checkpoint`, logs
`Reindexing will be performed for following indexes:
[/oak:index/interopLucene]`, advances `reindexCount` by **exactly one** and
rebuilds from scratch; froe then rebuilds from *that* cycle's own lane
checkpoint and the two enumerate identically (12 documents, 1,031 lines).

**The negative control.** One `:fulltext` posting's first position is
advanced by one on a copy of froe's rendered enumeration — exactly what a
wrong position increment in the word delimiter produces — and the same
comparison must refuse it naming `:fulltext`. The perturbation is of the
enumeration rather than of the analyzer because the analyzer is compiled
into the binary under test; the defect itself is neutralized against the
analysis module's own hand-computed vectors.

**What it found.** Four defects on its first run, recorded in the plan's
status: the template property order `RecordWriter::write_node` now
enforces, the counter refusal that order defect's symptom had justified,
`skipTokenization` for a regular-expression definition, and the facet
configuration's arity rule.

**And seven more when the definition above was written**, each of them a
shape the fixture had never carried — six of the document model and one of
the writer, every one proved by Oak's own rebuild of the same store and
fixed with a test that fails when the fix is neutralized:

1. a `boost` on an analyzed property set a boost on `full:<name>`, which
   omits norms, so froe's own writer **refused the run** — for the shape
   nearly every AEM definition is written in;
2. **relative property definitions were never indexed**: they reach
   nothing through `getConfig`, and Oak's property includes of the
   aggregate walk are their only path into a document;
3. a relative **pattern** indexed hidden names, where Oak indexes none;
4. `facets` on a non-string property wrote facet fields Oak does not;
5. a `relativeNode` include wrote `fullnode:<path>` **instead of**
   `:fulltext` rather than beside it;
6. `excludeFromAggregation` was read from the document's own rule instead
   of the rule covering the aggregated node, and **re-aggregation was
   missing entirely** — an aggregated node whose rule declares an
   aggregate contributes that aggregate's nodes too;
7. the writer kept the **first** doc value of a field group, so a
   multi-valued facet lost every label but one; Lucene unions a sorted
   set and refuses a second of any other kind.

An eighth was found beside them without the oracle, and proved with it: a
definition naming `codec = oakCodec` outright — which `Codec.forName`
resolves to the composition froe writes — was refused as though it named
something else.

**Three more came from reading the pinned image's own bytecode**, which
is where a branch the fixture cannot reach has to be settled:

9. a sorted doc value over 32,766 bytes was cut mid-character, where
   `getTruncatedBytesRef` walks back off the character that straddles the
   cut — and off its lead byte whether or not it would have fitted;
10. a re-aggregation was bounded by the limit of the aggregate it was
    entering, where `Matcher.nextSet` compares the stack against the
    **root** aggregate's;
11. and it entered the aggregate of the matched node's **covering rule**,
    where Oak looks the node's own primary type — then its mixins — up in
    the definition's `aggregates` map. So an aggregate declared for a type
    no `indexRules` child covers was never entered, and a node covered by
    a rule through type inheritance borrowed an aggregate Oak would not
    have given it. The fixture now carries the first half and Oak's own
    rebuild proves it; the second is a named test.

#### Verification report

*To be filled by task 1010, which freezes the range.*

#### Known gaps

*To be filled by task 1010.* Two are already known and are recorded here so
that the freeze inherits them rather than discovers them:

* **No consumer-registered field augmentor is reproduced.** Oak's document
  maker calls `augmentCustomFields`, and an AEM deployment that registers one
  gets fields froe cannot know about. A froe-built index is equivalent to
  Oak's only for a deployment with none.
* **Binary text is not extracted.** See the scope section: the index answers
  fulltext queries over binaries differently from Oak's unless Oak's own
  pre-extracted text store is complete and is given to the run.
* **`--pre-extracted-text-directory` is proved by unit tests alone.** The
  store is keyed by a blob's external content identity, and the interop
  fixture's Sling runs with no external `DataStore` — every binary in it is
  a segment blob, which has none — so the branch that reads Oak's own
  extracted text cannot be reached with the oracle. What *is* proved
  against Oak is the gate in front of it: a binary on a node with no
  `jcr:mimeType` is indexed by neither side, and the fixture carries one.

#### Review

*To be filled by task 1010.*
