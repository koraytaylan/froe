# Design: an exact compaction sharing memo

Written against `develop` at `7dbb102`. Successor to the `--memo-budget-mb`
mitigation added in that commit, which is a knob where an invariant belongs.

## The invariant to restore

```
compacted_nodes == |{ distinct node records reachable from the source head }|
```

Exactly, not approximately. Today not even `<=` holds: with an evicting memo
the count is unbounded above.

## Why it broke

`Compactor::rewritten_nodes` (`crates/froe/src/writer/compaction.rs:159`)
carries two responsibilities that were never separated:

| Job | Nature | Effect of a miss |
|---|---|---|
| preserve DAG sharing | optimization | larger output |
| ensure each node is copied once | **invariant** | unbounded re-traversal |

`BoundedCache` was correct for job 1 and silently discarded job 2. The
termination guard that remains — `MAXIMUM_COMPACTION_DEPTH`
(`compaction.rs:129`) — bounds *depth*, not *repetition*, so it does not
restore it.

Worst case is exponential, not linear: a chain of n diamond-shaped shares
re-copies the tail 2^n times if each lookup misses. The field observation —
21M copied nodes on an 18.8M-node head — was the mild version.

Note the pre-existing code held this invariant *by accident*, via an
unbounded map. The memory work removed the accident and replaced it with a
tuning parameter.

## Root cause of the cost

| Component | Bytes |
|---|---|
| `RecordIdentifier` = 16-byte segment UUID + 4-byte record number | 24 padded |
| key + value | 48 |
| `BoundedCache` per-entry overhead (`ENTRY_OVERHEAD_BYTES`, `cache.rs`) | 64 |
| **total** | **~112 / node** |

18.8M nodes -> 2.1 GB. That is what made an exact memo look unaffordable and
motivated eviction.

The 16-byte UUID is the waste. A compaction only ever names segments that
already exist in the source, plus those it writes — on a 25 GB store roughly
250k of them, which is a `u32`.

## The design

### 1. Intern segment identifiers

```rust
struct SegmentInterner {
    indices: HashMap<SegmentIdentifier, u32>,
    identifiers: Vec<SegmentIdentifier>,   // index -> identifier, reverse map
}
```

Cost is per *segment*, not per node: ~250k x ~28 B ~= 7 MB. Negligible, and it
replaces 16 bytes with 4 in every entry.

### 2. A purpose-built exact map

```rust
/// (segment index, record number) -> (segment index, record number)
struct RewrittenNodes {
    keys:   Vec<u64>,   // packed (u32 segment << 32) | record number; 0 = empty
    values: Vec<u64>,
    len:    usize,
}
```

Open addressing, linear probing, power-of-two capacity, grow at ~70% load. No
per-entry generic overhead, no `VecDeque` eviction queue, no boxed keys.

| | bytes/node |
|---|---|
| key | 8 |
| value | 8 |
| load-factor headroom (~1.43x) | **~23 total** |

### 3. Resulting footprint

| tree | exact memo |
|---|---|
| 18.8M nodes (the field store) | **~430 MB** |
| 100M nodes | ~2.3 GB |
| 300M nodes (memory-audit target) | ~6.9 GB |

At the field scale an exact memo costs less than twice today's *evicting*
256 MB one, and buys back an invariant.

Reserve up front from the head's node count where known, or grow
geometrically — never evict.

## Sentinel and correctness details

- Pack as `(u32 segment_index << 32) | u32 record_number`. Reserve interner
  index `0` as "never issued" so a packed key of `0` is unambiguously empty;
  real segments start at index 1.
- Record numbers are already `u32` (`RecordTableEntry::record_number`), so no
  truncation.
- The map holds only *node* records — the same population as today.
- Insertion still happens after a node is fully written, preserving the
  existing cycle behaviour where the depth bound is the backstop for a
  corrupt store.

## Behaviour when a tree genuinely will not fit

Do **not** evict. Two honest options, in order:

1. **Fail loudly** — refuse before the copy with the measured requirement:
   "this head needs ~6.9 GB for an exact sharing memo; raise
   `--memo-budget-mb` or pass `--allow-duplicate-sharing`." A maintenance tool
   that refuses beats one that silently produces a bloated store.
2. **Spill to disk** — a sorted external map, insert-once/lookup-many. Only
   worth building if (1) proves limiting in practice.

`--memo-budget-mb` survives as a **ceiling that triggers refusal**, not a
target that triggers degradation. That semantic inversion is the point: the
knob stops being load-bearing for correctness of resource use. Anyone who
genuinely wants the old trade gets an explicit opt-in flag whose name says
what it does.

## Tests that pin it

1. **The invariant, directly.** Build a store with heavy sharing — one subtree
   referenced from the live root and three checkpoints — compact, assert
   `outcome.compacted_nodes` *equals* the distinct reachable node count, not
   merely `<=`. Fails today at any budget below the tree.
2. **The pathological DAG.** A chain of diamonds where each level shares its
   child with the level above. Assert the count stays linear. Under the
   current evicting memo with a small budget this blows up exponentially —
   the sharpest available demonstration that eviction was never safe.
3. **Refusal, not degradation.** With a budget below the measured requirement,
   assert compaction refuses, names the figure, and leaves the store
   unmodified.
4. **Interner round-trip.** Every issued index maps back to the identifier it
   came from; index 0 is never issued.
5. **Interop unchanged.** `compact` and `compact --tail` still serve the exact
   baseline tree — the map change must be invisible to Oak.

## Migration from what is committed

- `compaction.rs:159` — `BoundedCache<RecordIdentifier, RecordIdentifier>` ->
  `RewrittenNodes`; construction at `compaction.rs:110`.
- The lookup (`compaction.rs:173`) and the insert (`compaction.rs:225`) keep
  the same call shape, so the recursion is untouched.
- `deep_copy_tree_with_memo_budget` keeps its signature; the budget becomes
  the refusal ceiling.
- `COMPACTION_MEMO_BYTES_PER_NODE` (`compaction.rs:142`) drops from 112 to ~23
  and stays exported — the CLI help quotes it (`crates/froe-cli/src/main.rs`,
  `--memo-budget-mb`).
- Raise the `--memo-budget-mb` default under the new cost: 512 MiB covers
  ~22M nodes, which would have covered the field store outright.
- Safety-case gap 1 gains the neutralization it currently lacks: test 2 above
  *is* the disabling experiment, because it fails loudly under eviction.
  (`docs/plans/bounded-memory-and-journal-retention-safety-case.md`)

## What this does not fix

`backup` and `restore` share the same deep copy
(`crates/froe/src/writer/backup.rs:108` calls `deep_copy_tree_with_progress`)
and inherit both the problem and the fix — assert the invariant there too
rather than assuming it.

## Caveat

The ~23 bytes/node figure is arithmetic, not a measurement. Verify it with the
same before/after harness used for the writer dedup caches (a scratch
integration test that builds a store, compacts, and reports
before/peak/after) before believing the footprint table.
