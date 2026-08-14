# Design: exact sharing and exact termination in compaction

Written against `develop` at `01132c8`.

`Compactor` (`crates/froe/src/writer/compaction.rs`) is the one tree walk in
this repository that carries its invariants in numbers rather than in data
structures. Three other walks already carry them correctly. This is a plan to
make compaction match them, not to invent anything.

## The rule the other walks already follow

A walk over records read from a repository needs three guarantees, and each
needs its own instrument. Fusing any two into one number, or into one cache,
is how a walk loses an invariant.

| guarantee | instrument | who does it right |
|---|---|---|
| terminate on a self-referential graph | exact unbounded set of records on the current path | `check.rs:707`, `backup.rs:502`, `map.rs:283` |
| bound work on a corrupt but acyclic graph | a budget charged against work actually done | `traversal.rs:42` |
| do not exhaust the stack | a depth cap, on recursive walks only | — |

A memo may be evicted under a byte budget only when a miss changes running
time and nothing else. That requires all three of: insert on **completion**,
never on entry; re-evaluate at the hit any guard the cached value
participates in; and bound the cost of a miss — a miss that re-walks a
subtree containing further misses compounds, and a memo whose miss compounds
is carrying an invariant, not an optimization.

`check.rs` satisfies every clause: `ancestors` is tested at `:728` *before*
the memo probe at `:733`, the hit is gated on the cached subtree height still
fitting under the cap, and `verified.insert` happens at `:781` after
`ancestors.remove` at `:780` and after every child returned `Ok`. Its
rationale is already written at `:682-683`.

## Where compaction departs from it

`Compactor::compact_node` (`compaction.rs:168-232`) has no ancestor set, no
work budget, and one constant doing three jobs.

`rewritten_nodes` (`compaction.rs:159`, built `:110`) fuses two
responsibilities into one `BoundedCache`:

| job | nature | effect of a miss |
|---|---|---|
| preserve DAG sharing | optimization | larger output |
| copy each distinct node once | **invariant** | multiplicative re-traversal |

`BoundedCache` is right for the first and silently discards the second. A
chain of diamond-shaped shares re-copies its tail once per path when lookups
miss; the field observation of 21M copied nodes on an 18.8M-node head was the
mild version. Note the pre-existing code held the invariant *by accident*,
through an unbounded map.

`MAXIMUM_COMPACTION_DEPTH` (`compaction.rs:129`) is then left standing in for
cycle detection — the use-site comment at `:176-178` assigns it that job
openly — which it cannot perform. It fires on depth alone, so it cannot tell
a cycle from a deep tree, cannot name the record that closes the cycle, and
burns 4000 levels before saying anything. Its own doc claim to sit "well
below the point where recursion would overflow the stack" does not hold in a
debug build or on a 2 MiB spawned-thread stack.

## What is *not* wrong with the depth cap

Compaction's cap cannot be raised or deleted on its own, and this constrains
every fix below.

`compaction.rs:356-363` runs `writer.finish()`, `set_head` and `flush`, and
only then does `:371` reach `store_writer.rs:815` `validate_finalized_session`
→ `:1568` `verify_node_tree` → `check.rs`'s own `MAXIMUM_CHECK_DEPTH`. By that
point `store_writer.rs:804-805` records that "the compacted head is already
journal-visible". So on a tree deeper than 4000, today's cap buys a free
refusal before any work; removing it buys a full duplicate write, a durable
head move, and then the same refusal from `check.rs`.

`traversal.rs` is the repo's own precedent against "make it iterative and the
cap goes away": that walk has an explicit heap stack and still carries both
`MAXIMUM_TRAVERSAL_DEPTH` (`:36`) and `MAXIMUM_TRAVERSAL_NODES` (`:42`), whose
comment states outright that a depth bound alone cannot stop a wide DAG.

## The design

### 1. An exact set of records on the path

Add `nodes_on_path: HashSet<RecordIdentifier>` to `Compactor`. Insert after
the memo probe at `:173`, remove before `rewritten_nodes.insert` at `:225`, and
on a repeat return the error naming the record — the same shape as
`check.rs:728` and the same message form it already uses.

Cost is one entry per live level, so this is never the thing worth budgeting.
The set cannot leak on an error path: `Compactor` is a local of
`deep_copy_tree_with_memo_budget` (`compaction.rs:100`) and dies with any `?`.

Once this lands, the depth cap keeps only its stack job and its message must
stop mentioning cycles. Whether to make the walk iterative is a separable
change, and by the section above it does not remove the cap either way.

### 2. Intern segment identifiers

```rust
struct SegmentInterner {
    indices: HashMap<SegmentIdentifier, u32>,
    identifiers: Vec<SegmentIdentifier>,   // index -> identifier, reverse map
}
```

Cost is per *segment*, not per node — on a 25 GB store roughly 250k of them,
a few MB — and it replaces a 16-byte UUID with a `u32` in every memo entry.
Reserve index `0` as never-issued so a packed key of `0` is unambiguously
empty.

### 3. An exact map, not a cache

```rust
/// (segment index, record number) -> (segment index, record number)
struct RewrittenNodes {
    keys:   Vec<u64>,   // packed (u32 segment << 32) | record number; 0 = empty
    values: Vec<u64>,
    len:    usize,
}
```

Open addressing, linear probing, power-of-two capacity, grow at ~70% load.
Record numbers are already `u32` (`RecordTableEntry::record_number`), so
packing truncates nothing. The map holds only *node* records — the same
population as today. Reserve from the head's node count where known, or grow
geometrically. Never evict.

Insertion still happens after a node is fully written, which is the
completion-insert clause of the rule above; with §1 in place the ancestor set,
not the depth bound, is what makes that safe.

## Cost, and why no number here is load-bearing

Today's entry charges `size_of::<RecordIdentifier>() + ENTRY_OVERHEAD_BYTES`
= 88 bytes (`cache.rs:68`), while `COMPACTION_MEMO_BYTES_PER_NODE`
(`compaction.rs:142`) declares 112 — the constant never charges the key it
claims to cover. Packing to two `u64`s and interning the segment removes the
UUID, which is where the bulk sits.

The resulting per-node figure is **unmeasured**, and no decision in this plan
may rest on it. Arithmetic puts it near 23 bytes/node; applying the
power-of-two-at-70%-load rule honestly puts steady state nearer 28.6, and a
geometric rehash holds both arrays at once. Until someone runs the before/after
RSS harness used for the writer dedup caches against a real store and records
**peak**, not steady state, the honest statement is that exactness is
affordable at field scale and unquantified beyond it.

What follows from that: this plan does not propose a byte ceiling, a refusal
threshold, or a tuning knob. A ceiling cannot be enforced where it would be
needed anyway — the required figure is a distinct-set cardinality unavailable
before the walk, and `record_writer_with_identifier` is at `compaction.rs:348`,
so any mid-copy refusal has already streamed segments to the sink and every
retry appends another partial generation. That is precisely what the preflight
at `:342-346` exists to prevent. If a tree is ever found that genuinely will
not fit, the answer is a measurement first, then a decision — not a knob.

## Tests that pin it

1. **The invariant, directly.** A store with heavy sharing — one subtree
   referenced from the live root and three checkpoints — compacted, asserting
   `outcome.compacted_nodes` *equals* the distinct reachable node count, not
   merely `<=`. The existing
   `a_deep_copy_with_no_sharing_memo_still_produces_the_whole_tree`
   (`compaction.rs:648`) asserts only `copied > 0` and passes whether the walk
   copies six nodes or six million.
2. **The pathological DAG.** A chain of diamonds where each level shares its
   child with the level above; assert the count stays linear.
3. **A cycle is refused exactly**, naming the closing record, at a depth far
   below the cap. The construction technique is already in this repo —
   `check.rs:1356`'s `write_record_identifier_bytes` splice, and
   `traversal.rs:510`.
4. **A legitimately deep chain** copies whole, up to whatever bound §1 leaves
   standing, and fails with the stack message rather than a cycle accusation
   beyond it.
5. **Interner round-trip.** Every issued index maps back to its identifier;
   index 0 is never issued.
6. **Interop unchanged.** `compact` and `compact --tail` still serve the exact
   baseline tree.

## The same family, elsewhere

- `backup.rs:108` reaches this walk through `deep_copy_tree_with_progress` and
  inherits every defect above with no lever at all. Assert the invariant there
  rather than assuming it.
- `backup.rs`'s *own* memo breaks two clauses of the rule independently:
  `visited.insert` at `:521` happens on entry, before `check_node_shallow`, and
  the hit at `:518` returns without re-checking `depth`. So a cached subtree can
  be accepted where a fresh walk would have been refused, and eviction changes
  the verdict — which makes `:344-346`'s "time rather than a different answer"
  false today. The failure direction is recovery falling back to an older head
  than it needed to, never blessing unverified data.
- `backup.rs:338-339` and `check.rs:24-25` both document their depth cap as a
  cycle detector when each has an exact ancestor set doing that job. The
  `check.rs` one is the more dangerous: it is the single sentence that could
  get a sound walk's `ancestors` set deleted.
- `diff.rs` has no cycle-detection state at all, and its error at `:235` tells
  the operator the records "probably form a cycle" on evidence that cannot
  support it.

## Divergence from Oak

Whether Oak's compactor bounds recursion depth is **unsettled**. There is no
Java source in this repository, and `docs/analysis/README.md` is explicit that
the derived docs are not ground truth over Oak. What the analysis does
enumerate — every `ClassicCompactor` constant and oak-run `compact`'s full flag
surface — contains no depth bound, which is consistent with froe's cap being a
divergence but does not establish it.

What *is* settled: froe has never documented it as one.
`docs/oak-segment-tar-feature-map.md:96` lists offline compaction as plain
parity with no caveat, and the cap is absent from the quirk register — though
`map.rs:387-393` shows the repo knows how to mark exactly this kind of
deliberate deviation. Until someone reads `ClassicCompactor`, the note should
say no depth bound was found in the analysis and that it is unverified against
Java source.

Separately, `docs/analysis/write-record-writers.md:512` settles the *memo*
question and cuts against the framing above: Oak's node dedup cache is a
fixed-capacity, silently-evicting `PriorityCache`, operator-overridable via
`oak.tar.nodeCacheSize`, and Oak's own classification is "required in practice
for compaction, for size not correctness", with checkpoint-heavy repositories
able to "blow up multiplicatively". So making sharing exact is froe choosing to
be stricter than Oak, not repairing a port defect. That is a defensible choice
and it needs to be recorded as a choice.
