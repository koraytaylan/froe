---
id: model-indexing-rules-and-name-patterns
title: Model Indexing Rules, Property Definitions And Name Patterns
workstream: "0010"
kind: task
depends_on: [specify-oaks-lucene-documents, implement-oaks-analyzers]
gated: false
touches:
  - crates/froe/src/index/lucene/documents/mod.rs
  - crates/froe/src/index/lucene/documents/rules.rs
  - crates/froe/src/index/lucene/documents/name_pattern.rs
  - crates/froe/src/index/lucene/documents/aggregate.rs
  - crates/froe/tests/lucene_rules_tests.rs
  - crates/froe-cli/tests/interop/judge/RegularExpressionVectors.java
  - crates/froe/tests/fixtures/java-regular-expression-vectors.tsv
  - crates/froe/src/index/lucene/documents/document_maker.rs
  - crates/froe/src/index/lucene/documents/facets.rs
  - crates/froe/src/index/lucene/documents/binaries.rs
status: planned
merged_as: ""
---
# Model Indexing Rules, Property Definitions And Name Patterns

Implement the definition-side model the document maker consults: `IndexingRule` with its `PropertyDefinition`s, inheritance and node-type resolution (through plan 0006's `TypePredicate` machinery), the codec-selection verdict (the `oak.lucene.compressing-codec` system property first, which froe cannot observe, a recorded departure, then an explicit `codec` property, then `is_fulltext_enabled`, as Oak's own codec selection resolves it, `Lucene46` being the verdict for a non-fulltext definition without an explicit `codec`; a definition whose verdict is not `oakCodec` is refused by name before anything else is read), rule and pattern precedence exactly as the specification states it (with a vector for two overlapping patterns; hidden names such as `:nodeName` are matched by the patterns too, as Oak's own name lookup matches them), the `valueRegex`, `similarityTags`, `useInSimilarity`, `dynamicBoost`, `function` and `compatVersion 1` refusals (each writes fields froe does not produce: similarity binaries and strings, the dynamic-boost field, a function-named ordered and typed field, and — for `compatVersion 1` — a bare rather than `full:`-prefixed analyzed field name), the refusal of `maxFieldLength = 0` (Lucene's token-count-limiting filter refuses a limit below 1 and Oak drops every document), a refusal for an `nt:base`-based rule carrying any `nullCheckEnabled` property definition, which Oak's own rule validation throws on so Oak cannot load the definition at all, a refusal when two rules assign different doc-value types to one `:dv` field name (Oak keeps whichever type arrived first and drops the later documents, the writer refusing a doc-values type change and the fulltext editor catching that refusal per document, traversal-order dependent; froe refuses at load, a departure recorded at the site), the `suggestion` child's `suggestAnalyzed` flag with its definition-root fallback (false leaves `:suggest` on the suggest helper's newline tokenizer; true analyzes and caps it with the definition analyzer, which task 1006 selects from this flag), the `analyzers` node (its `indexOriginalTerm` property makes the word delimiter filter preserve the undelimited original term; any child of `analyzers` is refused by name), the `tika` child accepted and reported (nothing is read from it), and the hybrid refusal (any property definition with `sync` or `unique`, or a `nodeTypeIndex` rule with `sync`, the test `docs/analysis/index-definitions.md` records, since Oak keeps a synchronous `:property-index` for those that this plan does not build), aggregate include patterns with the matcher state machine, and `name_pattern.rs`: a bounded regular-expression subset for `isRegexp` property names — character classes with negation and escapes, `^`, `$`, `.`, `*`, `+`, `?`, alternation and grouping — enough for Oak's own catch-all name pattern and the patterns AEM's shipped definitions use (`jcr:content/.*`, `.*Tags`), with a typed refusal for anything beyond it. The subset serves property-name patterns only; `valuePattern` regular expressions stay refused as plans 0006 and 0007 state. The subset is proved against Java's own regular-expression engine through the judge on a committed table of patterns and names.

**Steps:**

1. `documents/mod.rs` (stubbed by task 1003; filled here) with documented stubs for `document_maker`, `facets` and `binaries` (task 1006 owns them; `binaries` is `pub mod`, so its policy types are reachable as `froe::index::lucene::documents::binaries`); `rules.rs`, `aggregate.rs`: parsing per the specification, with the case-insensitive name lookup the specification established and the fulltext-enabled verdict.
2. `name_pattern.rs`: parser and matcher, splitting the pattern text at its last `/` into a parent path and a name expression exactly as Oak does when it builds a name pattern (a leading `/` yields the parent `/`; the catch-all pattern special-cased to an empty parent), and matching identical to the parent-path equality check followed by a whole-string match of the name expression against the name; the committed vector table includes a leading-slash pattern and a trailing-slash one.
3. Judge class `judge/RegularExpressionVectors.java` with `regular-expression-vectors <table>` printing the filled table on standard output, the fixture header recording the redirection evaluating the committed pattern-and-name table `crates/froe/tests/fixtures/java-regular-expression-vectors.tsv` with Java's own regular-expression engine, whose verdict column the judge fills in; tests replay it and assert every unsupported construct is refused rather than misread.
4. Tests over the fixture's default definition and over synthetic definitions with inheritance, relative names, aggregates, a definition that is not fulltext-enabled, a definition carrying `valueRegex`, one with `analyzers/@indexOriginalTerm`, one with an `analyzers` child, one with a `tika` child, one with a `unique` property definition, one with each of `useInSimilarity`, `dynamicBoost`, `function` and `compatVersion 1` refused by name, an `nt:base` rule with a `nullCheckEnabled` property refused, one with `suggestAnalyzed` true and one false, and every refusal.

- **Done when:** every regular-expression vector matches Java's verdict, every unsupported construct is a named refusal, the non-fulltext definition is refused with `Lucene46` named, and the stable host gate passes.
