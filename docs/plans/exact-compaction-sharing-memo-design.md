# Exact sharing and exact termination in compaction

Landed. The rule below is repository-wide, and every walk over records now
follows it: the compaction memo is exact, and no walk imposes a depth limit.
What that traded away is listed at the end as open work.

## The rule

A walk over records read from a repository needs three guarantees, and each
needs its own instrument. Fusing any two into one number, or into one cache,
is how a walk loses an invariant.

| guarantee | instrument | who does it |
|---|---|---|
| terminate on a self-referential graph | exact unbounded set of records on the current path | every walk |
| bound work on a corrupt but acyclic graph | a budget charged against the resource actually consumed | `traversal.rs`, `diff.rs` |
| do not exhaust the stack | an explicit stack on the heap — never a depth cap | every walk |

A depth cap is not an instrument for any of the three. It cannot decide the
first, it charges the wrong resource for the second, and for the third it
substitutes a guess for a bound the walk can simply not need: depth belongs to
the repository, so capping it refuses valid stores while doing none of the
jobs it appears to do.

A memo may be evicted under a byte budget only when a miss changes running
time and nothing else. That requires all three of: insert on **completion**,
never on entry; re-evaluate at the hit any guard the cached value
participates in; and bound the cost of a miss — a miss that re-walks a subtree
containing further misses compounds, and a memo whose miss compounds is
carrying an invariant, not an optimization.

`check.rs` did **not** satisfy the third clause, although this document once
asserted it did. Its memo inserted on completion and its guard was the separate
`ancestors` set, but a miss re-walked the whole subtree below the missing
record, and every miss inside that subtree compounded — so the memo was
carrying an invariant while being evicted under a byte budget. The reported
node count made the defect visible: the walk counted one node per memo miss, so
a 58 GB AEM repository whose head held 18,796,598 nodes reported 56,389,743.
For a super-root walk, which reaches the content root and every checkpoint
snapshot root as sibling subtrees inside one traversal, *any* budget below the
tree size gives roughly one full walk per root — not the near-total starvation
this document describes for compaction. `NodeTreeVerifier` now uses the same
exact, unbudgeted `PackedRecordSet` and counts at the certificate-issue site,
so the reported number equals the certificate count by construction.

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

The depth bounds had the same coupling, which is why they were removed
together rather than one at a time: `compaction.rs:356-363` runs
`writer.finish()`, `set_head` and `flush`, and only then does `:371` reach
`store_writer.rs:815` → `:1568` `verify_node_tree`, by which point
`store_writer.rs:804-805` records that the compacted head is already
journal-visible. Lifting compaction's bound alone would have converted a free
pre-flight refusal into a full duplicate write, a durable head move, and then
the same refusal from the verifier.

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
- `every_random_shape_copies_each_distinct_node_exactly_once` — 200 generated
  DAGs, every twentieth large enough to cross a memo growth, each cross-checked
  against a `HashSet<RecordIdentifier>` walk.
- `the_memo_and_the_interner_hold_their_own_invariants` — 60k operations
  asserting the sentinel is never issued, packing round-trips and is injective,
  and every entry survives every growth.

Two guards run outside the tests, on every real copy: a duplicate-key
assertion in `insert_without_growing`, and a postcondition comparing
`compacted_nodes` against the table's *recounted* occupancy. The postcondition
originally compared against `len`, which is incremented alongside
`compacted_nodes` and so could only compare a counter with itself; recounting
is what lets it catch a growth that loses entries. Both were verified by
mutation — dropping one entry on rehash and making the memo miss one key in
three each fail the suite, the duplicate-key assertion firing first.

## The invariant, adversarially tested

Five agents attacked `compacted_nodes == distinct reachable node records`
along separate angles — packing collisions, table mechanics, tree shapes,
corrupt input, and callers plus scale — with three claimed breaks, all three
independently reproduced and all three reclassified as not exactness breaks.
Every run that returned at all was exact, up to 4,020,101 distinct nodes and
13 memo doublings, including a graph with 2^40 distinct root-to-leaf paths
over 480,043 nodes, and backup → restore → compact → compact on one graph.

Three findings from that exercise are worth keeping:

- **`pack` is injective and the sentinel is unreachable.** Forcing a freshly
  written sink record to pack to the same `u64` as an unvisited source record
  produced real collisions, and they are harmless: keys and values live in
  separate arrays, only source-packed values are ever keys, and `get` reads
  `values[slot]` only after a key match.
- **A duplicate insert is unreachable from any input**, because `insert` grows
  before placing so an empty slot always exists, and `grow` rebuilds the probe
  invariant from an empty table, so `get` can never miss a present key. The
  duplicate-key assertion stays anyway: it is the one guard that fires at the
  point of corruption rather than downstream.
- **The oracles are less independent than they look.** Every "distinct
  reachable nodes" count — in these attacks and in the committed tests — is
  built on `NodeState::child_node_entries()`, the same call the walk itself
  uses. A defect there would make the walk, the oracle, and any re-read agree
  on the same wrong number. Nothing here is independent of that function.

## No depth limits anywhere

Every walk over records now carries its own stack on the heap and imposes no
depth limit. Depth is a property of the repository, not something this code
may choose, and a bound on it refuses valid stores by fiat.

| walk | was | now |
|---|---|---|
| `compaction.rs` | `MAXIMUM_COMPACTION_DEPTH = 4000` | iterative; exact `nodes_on_path` |
| `check.rs` | `MAXIMUM_CHECK_DEPTH = 4000` | iterative; exact `ancestors` |
| `backup.rs` | `MAXIMUM_RECOVERY_DEPTH = 4000` | iterative; exact `ancestors` |
| `traversal.rs` | `MAXIMUM_TRAVERSAL_DEPTH = 16_384` | was already iterative; gained an exact path set |
| `diff.rs` | `MAXIMUM_DIFF_DEPTH = 4000` | iterative; exact set over the record *pair* |
| `map.rs` | `depth >= 64` | iterative; `visited` already decided it |

Three of the six were not bounding the stack at all — they were standing in
for cycle detection that did not exist, `diff.rs` having none in 718 lines.
Each now refuses a self-referential graph exactly, at the record that closes
it. And the caps could not do the stack job they claimed either: at ~740 bytes
a frame in release, 4000 levels needed ~2.8 MiB against the 2 MiB a spawned
thread gets, so a legitimate 3000-deep tree aborted the process with SIGABRT
rather than being refused — through public API, where SIGABRT cannot unwind
and the caller gets no report. `a_tree_deeper_than_any_call_stack_copies_whole`
now copies *and* verifies a 100,000-level tree on a 2 MiB stack.

Two fixes fell out of opening the walks. `backup.rs` inserted into its visited
memo on entry, before the node was checked, which cached failed and cyclic
subtrees and made a shared subtree root the oldest entry of its own subtree —
so any subtree over budget guaranteed a sibling miss. It inserts on completion
now, which is what makes its "time rather than a different answer" true. And
`check.rs`'s subtree height existed only to gate a memo hit against the cap;
with no cap there is no guard to re-evaluate, both callers already discarded
it, and the memo is now `BoundedCache<RecordIdentifier, ()>`.

Two budgets deliberately stay: `MAXIMUM_DIFF_VISITS` and
`MAXIMUM_TRAVERSAL_NODES`. They charge against work actually done, which is
what a wide corrupt DAG exhausts while staying shallow, and unlike a depth cap
they cannot refuse a valid store. `descent_limit` and the CLI's `--depth` stay
too: those are asked for by the caller, not invented.

## Still open

- **The depth-proportional term has no ceiling.** Removing the caps replaced a
  constant-bounded amount of walk state with one proportional to depth.
  Measured on `verify_node_tree`: ~175 bytes a level — the ancestor set, the
  explicit frame stack, and the path buffer — so 17 MiB at 100,000 levels and
  65 MiB at 400,000. `MAXIMUM_CHECK_DEPTH` used to cap that at ~700 KB. It is
  now bounded only by how many distinct records the store holds, since a
  repeat is refused as a cycle.

  This is not the node-count-proportional map that motivated `BoundedCache`:
  that charged per distinct node on every ordinary run, where this needs a
  store shaped as a long chain (a real content tree runs a few hundred levels,
  about 70 KB). But `check.rs` and `backup.rs` have no work budget at all, so
  nothing bounds it there, and `traversal.rs`'s and `diff.rs`'s budgets charge
  against node counts rather than residency — at 1e9 they would let the term
  reach tens of gigabytes before firing.

  The rule already names the right instrument: a budget charged against the
  resource actually consumed. For this the resource is the walk's own state,
  not a node count, and a ceiling on it refuses when the host cannot supply
  what the walk needs — a figure that can be read from the host rather than
  guessed, which is what separates it from the caps just removed.
  `measure_deep_chain_walk_footprint` is the harness.

- **The oracles share a primitive.** Every "distinct reachable nodes" count —
  in the committed tests and in the adversarial attacks — is built on
  `NodeState::child_node_entries()`, the same call the walks use. A defect
  there would make walk, oracle and re-read agree on the same wrong number.

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
