---
id: read-counter-index
title: Read The Counter Index And Its SipHash
workstream: "0006"
kind: task
depends_on: [model-index-definitions, specify-property-index-storage]
gated: false
touches:
  - crates/froe/src/index/counter/mod.rs
  - crates/froe/src/index/counter/sip_hash.rs
  - crates/froe/tests/counter_index_tests.rs
  - crates/froe/tests/fixtures/oak-sip-hash-vectors.tsv
status: planned
merged_as: ""
---
# Read The Counter Index And Its SipHash

Implement Oak's SipHash variant and the counter index reader. The hash is the whole reason the counter index is deterministic given its `seed`: seeding takes the four state words from `seed` and from `seed` rotated left by 32 bits; each step down the tree runs two SipRounds and folds a message word into the first state word, that word being Java's string hash of the child name sign-extended to 64 bits; and the final fold exclusive-ors the four state words into one value `x` and returns the low 32 bits of `x ^ (x >>> 16)`. Plan 0007 rebuilds `:cnt` from it, so this task pins it with vectors from the JDK. The hash and the chunk arithmetic are 64-bit work, so this task runs the i686 width sentinel from `docs/high-risk-changes.md` beside the host gate.

**Steps:**

1. `sip_hash.rs`: the two constructors and `hash_code`, over `i64`/`u64` with wrapping arithmetic and Java's rotate semantics, with the module comment pointing at `docs/analysis/index-property-storage.md` for the hash itself, for the chain the counter editor drives over it, and for the narrowing of the stored `seed` to 32 bits.
2. Generate `crates/froe/tests/fixtures/oak-sip-hash-vectors.tsv` with the image's JDK against Oak's own hash class on the image's classpath, in `oak-core-1.90.0.jar`, which `docs/analysis/index-property-storage.md` names: for a fixed seed list and a fixed name list, the chained hash codes for `/`, `/a`, `/a/b`, plus names with non-ASCII characters; record the generating command in the header.
3. `counter/mod.rs`: `CounterIndex` reading `resolution` and `seed` converting to `LONG` as the counter's editor provider does, `bit_mask`, `entries()` streaming `(path, Option<u64>)` — the combined `:cnt`/`:count` value, absent for a node carrying neither, never a `0` standing in for absence — and `estimated_node_count(path, bound: CountBound) -> NodeCountEstimate` following Oak's default estimate, which is the path taken when both switches task 0602 records sit at their defaults, and where reading `:cnt` and `:count` together keeps the estimate right over a store an old-counter Oak maintained. `CountBound::{Expected, Maximum}` is Oak's maximum flag, and `NodeCountEstimate::{Unknown, Fallback, Count(u64)}` replaces Oak's `-1` with an `Option`-shaped absence rather than a sentinel. The rules and constants are the ones Oak actually uses: `Count(0)` when the target node does not exist; then two stages at the target node itself, before the index is consulted at all — under `Expected` only, its own approximate count whenever that count is present; then, under *both* bounds, its combined `:cnt` and `:count` whenever those are present, plus the approximate counter's own resolution of 100 for `Maximum` and nothing for `Expected`; `Unknown` when the definition literally named `counter` has no data node, since Oak consults only `/oak:index/counter` and sums every `:index` and `:<mount>-index` child; otherwise the combined `:cnt` and `:count` of the node reached by descending `path`'s elements under each `:index` and `:<mount>-index` child, summed across those children — so a non-root `path` answers for that subtree, not the store, plus 100 for `Maximum`, and, when that sum is zero, `Fallback` — Oak answers 2000 for `Maximum` and 0 for `Expected` there, but neither counts anything, so froe returns the verdict rather than the number and a caller decides what to do with it. The definition's `resolution` plays no part in the estimate.
4. Tests: every vector; a `STRING`-typed `resolution` read as its number; each estimate rule pinned by a separate test, including a non-root `path` beside the root one and a path the sampling counter never recorded answering `Fallback`; a reader test over a synthetic store whose `:index` was written by an independent helper inside `counter_index_tests.rs` itself from the same rules; a mirror node without `:cnt` (legal: the counter editor removes the property when a count reaches zero but never the node, so an incrementally maintained index carries such nodes after deletions; `entries()` reports them with the absent count so plan 0007's oracle can find them); and a counter definition with no `:index` child, which reads as an empty index with an `Unknown` estimate.

- **Done when:** every JDK vector matches, each estimate rule has a passing named test, the reader enumerates the independent helper's `:cnt` map exactly, the four i686 sentinel commands `docs/high-risk-changes.md` gives (`check` and `clippy` on stable and on the MSRV, `--target i686-unknown-linux-gnu`, `-D warnings`) pass, and the stable host gate passes.
