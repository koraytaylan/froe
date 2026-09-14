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

The native reindex publishes through task 0707's `apply.rs`, writing `:data` through plan 0008's `OakDirectoryWriter` and rewriting the definition inside the one head move that `apply.rs`'s shared publication tail performs, so this plan reopens neither case. It keeps its own section here, written by task 1011 before any code lands, filled by task 1007 (fault and guards tables) and frozen by task 1010, that cites plan 0007's `### Safety case` for the publication boundaries and plan 0008's for the `:data` write and adds only what the native reindex introduces, which task 1011 step 1 enumerates: the mutation rows above, the `lucene-reindex.` cutpoints, and the facts, refusals and policies that task's list names, plus the interoperability record of task 1009. Plan 0007's section is never edited after 0714 freezes it, plan 0008's never after 0811, and plan 0009's frozen verification report, known gaps and review never after 0913. Until 1011 lands, this paragraph is the placeholder.
