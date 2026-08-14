# Exact sharing and exact termination in compaction

Landed for `crates/froe/src/writer/compaction.rs`. The rule below is
repository-wide and three walks still do not follow it; those are listed at the
end as open work.

## The rule

A walk over records read from a repository needs three guarantees, and each
needs its own instrument. Fusing any two into one number, or into one cache,
is how a walk loses an invariant.

| guarantee | instrument | who does it |
|---|---|---|
| terminate on a self-referential graph | exact unbounded set of records on the current path | `compaction.rs`, `check.rs:707`, `backup.rs:502`, `map.rs:283` |
| bound work on a corrupt but acyclic graph | a budget charged against work actually done | `traversal.rs:42` |
| do not exhaust the stack | a depth cap, on recursive walks only | — |

A memo may be evicted under a byte budget only when a miss changes running
time and nothing else. That requires all three of: insert on **completion**,
never on entry; re-evaluate at the hit any guard the cached value
participates in; and bound the cost of a miss — a miss that re-walks a subtree
containing further misses compounds, and a memo whose miss compounds is
carrying an invariant, not an optimization.

`check.rs` satisfies every clause and its rationale is written at `:682-683`.

## What compaction had, and why it was wrong

`rewritten_nodes` was a `BoundedCache` carrying two responsibilities:

| job | nature | effect of a miss |
|---|---|---|
| preserve DAG sharing | optimization | larger output |
| copy each distinct node once | **invariant** | multiplicative re-traversal |

`BoundedCache` was right for the first and silently discarded the second, and
`MAXIMUM_COMPACTION_DEPTH` was left standing in for cycle detection it could
not perform — it fired on depth alone, so it could not tell a cycle from a deep
tree nor name the record that closed one.

The blowup is real but needs near-total starvation, which is worth recording
because the earlier draft of this document overstated it. Measured on a
14-level chain where each level references the same next-level node twice
(464 distinct nodes): at three memo entries or fewer, 557,024 copies in 78.79 s;
at four or more, exactly 464. The cliff does not move when filler nodes are
placed between the two references, because the memo inserts on *completion*, so
a parent's second reference finds the newest entry. At zero the shape is
brutal: a 20-level chain of 22 distinct nodes copied 2,097,152.

## What landed

**An exact set of records on the path.** `nodes_on_path`, tested before the
memo probe, cleared before the memo insert. A repeat is refused as
`node record {…} is contained in its own subtree`, at the closing record.
`MAXIMUM_COMPACTION_DEPTH` keeps only its stack job and its message says so.

**Interning.** `SegmentInterner` maps each segment identifier met during one
compaction to a `u32`. The cost is per *segment*, not per node. Index 0 is
never issued, so a packed key of zero is unambiguously an empty slot.

**An exact map.** `RewrittenNodes`: open addressing over two `Vec<u64>`, keys
packed as `(segment_index << 32) | record_number`, power-of-two capacity,
growth at 70% load, no eviction.

`compacted_nodes` now equals the number of distinct node records reachable
from the head, at any tree shape.

## Measured cost

| memo | bytes/node | 18.8M-node head |
|---|---|---|
| `HashMap<RecordIdentifier, RecordIdentifier>` | 104–115 | ~2.1 GB |
| interned + packed | 44 at 1M entries, 35 at 4M | 512 MiB (2^25 slots × 16 B) |

Sixteen bytes a slot is the invariant; the per-node figure moves between
roughly 23 and 46 depending where the table sits between growths. The earlier
~23 B/node estimate in this document was the best case, reached just before a
growth, not the typical one.

That first row is why interning and packing are load-bearing rather than an
optimization: a plain `HashMap` buys the count invariant and re-buys the
memory problem that motivated eviction in the first place. It was measured and
rejected, not reasoned about.

Residency is now proportional to the tree. That is a deliberate trade against
the bounded-memory claim in
[`bounded-memory-and-journal-retention-safety-case.md`](bounded-memory-and-journal-retention-safety-case.md),
recorded there as the one store-proportional cache.

## No knob replaces the budget

`--memo-budget-mb`, `compact_with_memo_budget` and
`COMPACTION_MEMO_BYTES_PER_NODE` were removed — a semver break, and a compile
break rather than a silent behavioural one. Two reasons a ceiling was not
substituted:

1. The figure an operator would need is a distinct-set cardinality unavailable
   before the walk.
2. `record_writer_with_identifier` is at `compaction.rs:348`, so a ceiling can
   only fire mid-copy, after segments have streamed to the sink, and every
   raise-and-retry appends another partial generation — exactly what the
   preflight at `:342-346` exists to prevent.

The depth bound also cannot be raised or deleted on its own:
`compaction.rs:356-363` runs `writer.finish()`, `set_head` and `flush`, and
only then does `:371` reach `store_writer.rs:815` → `:1568` `verify_node_tree`
→ `check.rs`'s own `MAXIMUM_CHECK_DEPTH`, by which point
`store_writer.rs:804-805` records that the compacted head is already
journal-visible. Removing compaction's bound alone converts a free pre-flight
refusal into a full duplicate write, a durable head move, and the same refusal.

## Tests

- `a_deep_copy_copies_each_distinct_node_exactly_once` — the invariant on a
  store with a checkpoint sharing the live root. Replaces a test that asserted
  only `copied > 0` and passed whether the walk copied six nodes or six million.
- `a_shared_subtree_is_copied_once_however_deep_the_sharing_nests` — nested
  diamonds at 4, 14 and 24 levels, with and without filler between references.
- `the_exact_memo_costs_a_bounded_number_of_bytes_a_node` — packing round-trip
  and the footprint bound, pinned as table occupancy rather than RSS, because
  resident size is order-dependent under allocator reuse.
- `a_cyclic_source_is_refused_at_the_record_that_closes_the_cycle` — built by
  splicing a record identifier in place, the technique already used at
  `check.rs:1356` and `traversal.rs:510`.

## Still open

- **`backup.rs`'s own memo** breaks two clauses of the rule independently:
  `visited.insert` at `:521` happens on entry, before `check_node_shallow`, and
  the hit at `:518` returns without re-checking `depth`. A cached subtree can be
  accepted where a fresh walk would have been refused, so eviction changes the
  verdict and `:344-346`'s "time rather than a different answer" is false. The
  direction is recovery falling back to an older head than it needed to, never
  blessing unverified data. Its deep copy already inherits the exact memo
  through `deep_copy_tree_with_progress`.
- **`backup.rs:338-339` and `check.rs:24-25`** document their depth cap as a
  cycle detector when each has an exact ancestor set doing that job. The
  `check.rs` one is the more dangerous: it is the single sentence that could
  get a sound walk's `ancestors` set deleted.
- **`diff.rs`** has no cycle-detection state at all, its error at `:235` claims
  the records "probably form a cycle" on evidence that cannot support it, and
  two mutually recursive frames per level mean it exhausts the stack before the
  cap can fire on anything under roughly 15 MiB in debug.
- **`traversal.rs:34-35`** overclaims in the same way, though that walk loses no
  invariant: it is iterative and pairs its depth bound with a node budget.

## Divergence from Oak

Whether Oak's compactor bounds recursion depth is **unsettled**. There is no
Java source in this repository, and `docs/analysis/README.md` is explicit that
the derived docs are not ground truth over Oak. The analysis enumerates every
`ClassicCompactor` constant and oak-run `compact`'s full flag surface and
contains no depth bound, which is consistent with froe's cap being a divergence
but does not establish it. `docs/oak-segment-tar-feature-map.md:96` lists
offline compaction as plain parity with no caveat, and the cap is absent from
the quirk register, though `map.rs:387-393` shows the repo knows how to mark
this kind of deliberate deviation.

The *memo* question is settled and cuts the other way.
`docs/analysis/write-record-writers.md:512` classifies Oak's node dedup cache —
a fixed-capacity, silently-evicting `PriorityCache`, operator-overridable via
`oak.tar.nodeCacheSize` — as "required in practice for compaction, for size not
correctness", with checkpoint-heavy repositories able to "blow up
multiplicatively". Making sharing exact is froe choosing to be stricter than
Oak, not repairing a port defect. It is recorded here as that choice.
