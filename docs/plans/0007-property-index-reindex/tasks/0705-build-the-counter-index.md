---
id: build-the-counter-index
title: Build The Counter Index From Oak's Hash Chain
workstream: "0007"
kind: task
depends_on: [implement-bounded-external-sort]
gated: false
touches:
  - crates/froe/src/writer/index/counter_builder.rs
  - crates/froe/tests/counter_index_builder_tests.rs
  - crates/froe/tests/fixtures/oak-counter-index-vectors.tsv
  - crates/froe-cli/tests/interop/judge/CounterVectors.java
status: planned
merged_as: ""
---
# Build The Counter Index From Oak's Hash Chain

Implement `CounterBuilder`: walk the state root with the counter editor's semantics — for each child, one SipHash chain step from the parent's hash, folding in the Java string hash code of the child name; when that step's folded 32-bit hash code — the exclusive-or of the four state words, then `x ^ (x >>> 16)`, as `docs/analysis/index-property-storage.md` records — masked by `bit_mask` is zero, add `bit_mask + 1` to the *parent* and every ancestor; hidden children are never visited; `bit_mask` is the highest set bit of `resolution` doubled and then reduced by one — and write `:index` with one node per ancestor of a hit carrying `:cnt` (`LONG`), nothing for nodes that never accumulated. When no node hits at all, write no `:index` child: Oak's editor returns before creating it, so Oak's own reindex of a small store leaves the counter definition without one, and that is the shape the 0712 oracle compares against. An existing `seed` is read converting to `LONG` and then narrowed to its low 32 bits sign-extended, exactly as the counter's editor provider reads it, which `docs/analysis/index-property-storage.md` records; the fixture's counter carries a 64-bit seed, so a builder that skipped the narrowing would fail the 0712 oracle. When the definition has no `seed`, create one and store it as a `LONG` on the definition. Oak's provider draws the most significant 64 bits of a random UUID and uses them untruncated on the run that creates the seed while narrowing them to 32 bits sign-extended on every later run, so the first cycle and every later one disagree; froe avoids that quirk instead of reproducing it by drawing a seed that already fits `i32` sign-extended (a random 32-bit value from froe's entropy source, widened to 64 bits), so both readings agree — a deliberate, strictly safer deviation from Oak's 64-bit draw, recorded at the site against the specification.

**Steps:**

1. `counter_builder.rs`: `CounterBuilder::new(definition, state_root)`, `count_hits() -> HitCount` for the plan (a walk that writes nothing) and `build(writer) -> BuiltCounter { index_record: Option<RecordIdentifier>, created_seed: Option<i64>, credited_by_path }` — the per-node map of the amount credited to each node, which is the number of its own children whose hash hit times `bit_mask + 1` — a node's own hit credits its parent, never itself, so the difference between a node's `:cnt` and its children's sum is exactly that product (the accumulation task 0602 specifies), so task 0707's tail compares the written `:cnt` against the children's sum plus this amount directly; no walk of the written index can recover it, and the map outlives `build`, staying resident until task 0707's tail has verified against it; the accumulation map is keyed by path and bounded by hits times depth, which the doc comment states with the arithmetic.
2. `judge/CounterVectors.java`, subcommand `counter-vectors` (no argument: it prints, as `Judge::run` returns standard output and `Analyze` and `FstCheck` print; the fixture header records the redirection that captured it): runs Oak's own counter editor inside the image over a small synthetic tree — an in-memory node store the judge populates, resolution fixed and the seed fixed to a value outside the `i32` range so the vector pins the narrowing, driven through Oak's own index-update editor over a diff from the missing state to the populated root, the synthetic definition carrying no `async` property so the synchronous cycle selects it — and prints the resulting `:index` subtree as `path\tcnt`; commit the output as `crates/froe/tests/fixtures/oak-counter-index-vectors.tsv` with the generating command in the header and pin the builder to it.
3. Tests: the vector file; the seed creation path; `resolution` other than 1000; the hidden-child exclusion; and a store where no node hits, which must produce no `:index` node.

- **Done when:** the builder reproduces Oak's `:cnt` map for the vector tree exactly under a seed that does not fit `i32`, a created seed reads back identically through the 32-bit narrowing, the no-hit store yields no `:index`, and the stable host gate passes.
