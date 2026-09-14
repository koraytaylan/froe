# Oak indexes in froe

Everything here is **read-only**. `froe index` opens a store exactly as
`froe summary` does: no repository lock is taken, no manifest is written, and
no file is created — except the one `definitions --output` names, which is
created outside the store or refused. All three subcommands are therefore
safe against a running Oak instance, under the never-modify-in-place file
protocol the rest of froe's read path relies on.

## 1. The three things an index is

**A definition.** An `oak:QueryIndexDefinition` node under `/oak:index`, or
under any content node — Oak allows nested definitions, and froe lists them.
The definition's `type` — `property`, `reference`, `counter`, `lucene`,
`elasticsearch`, `disabled`, `ordered` — decides everything else about it.

**A lane.** A definition with an `async` property is maintained by a
background indexer rather than inside the commit that changes content. The
lane's state lives on `/:async`: `<lane>` is the checkpoint the next run
resumes from, `<lane>-LastIndexedTo` a date, `<lane>-lease` a lease held only
while a run is in progress, `<lane>-temp` a list of checkpoints the indexer
intends to release. A definition without `async` is synchronous and always
corresponds to the head.

**Storage.** Hidden children of the definition node. The property family
writes `:index` (and the reference index also `:references` and
`:weakreferences`); the counter writes `:index`; Lucene writes `:data` and,
with a suggester, `:suggest-data`. Oak also keeps `:status` and
`:index-definition` beside them, which are bookkeeping rather than data.

`docs/analysis/index-definitions.md`, `index-property-storage.md` and
`index-lucene-storage.md` specify all three against the Java, and are the
ground truth froe is written from.

## 2. `froe index list`

```console
$ froe index list /path/to/segmentstore
/oak:index/uuid
  type              property
  reindex           false (count 1)
  estimated entries 1,500
  counters          4
/oak:index/lucene
  type              lucene
  lane              async
  lane checkpoint   95521dd3-c005-4b45-a901-0754c7315904
  indexed up to     2026-09-14T06:24:27.689Z
  reindex           false (count 1)
  size              1.8 MiB (1916418)
  lucene files      9
```

`--index PATH` narrows the listing and is repeatable. A path naming no
definition in the store is refused by name, for all three subcommands: a run
that silently listed nothing would read as an index that exists and is empty.

Fields worth knowing:

* **estimated entries** is what the type's own information provider computes,
  which is not a count. For a property index it is Oak's own estimate; for
  Lucene it is not filled in yet (plan 0008 adds the document count).
* **estimated nodes**, shown for a counter definition, is the approximate
  descendant count at the content root. It is a *sampling* estimate, and it
  counts only what Oak's visible-editor wrap passes — no hidden child, so no
  `:index` subtree. It is not comparable with a node count of the store, and
  froe never presents it as one.
* **counters** is how many randomized `:count_<uuid>` approximate counters the
  storage carries. They are not a fault; they are how Oak estimates. They are
  also why comparing two indexes built from identical content needs
  `froe digest --exclude-property-prefix :count_`.
* **lane checkpoint** marked `(dangling)` means `/:async` resumes from a
  checkpoint `/checkpoints` no longer holds. Oak does not fail on this: it
  logs a warning and reindexes from the missing state, which for a Lucene
  index means its writer appends every document again to the retained
  `:data`, doubling the index with no error anywhere.

Warnings go to standard error, never to standard output. A definition froe
cannot model at all is *listed* with the reason rather than ending the run:
one malformed definition must never kill a listing.

`list` works on a store whose `/oak:index/nodetype` index is disabled or
missing, reporting a warning that the non-root definitions could not be
enumerated: Oak's index path service refuses such a store outright, but the
root definitions are still readable from `/oak:index` directly, and an
operator whose nodetype index is broken is exactly the one who needs to see
what the store holds.

`definitions` and `check` refuse it **unnarrowed**, because they reproduce an
Oak printer and an Oak checker, both of which walk that path service: falling
back silently would make froe's output differ from Oak's on precisely the
store where they must agree. A `--index` run of either succeeds, because a
caller who names the paths never consults the path service and its nodetype
precondition is never evaluated — which is how oak-run behaves when it is
given `--index-paths`. The refusal says so.

## 3. `froe index definitions`

```console
$ froe index definitions /path/to/segmentstore --index /oak:index/uuid
{
  "/oak:index/uuid": {
    "jcr:primaryType": "nam:oak:QueryIndexDefinition",
    "propertyNames": ["nam:jcr:uuid"],
    "unique": true,
    "info": "Oak index for UUID lookup (direct lookup of nodes with the mixin 'mix:referenceable').",
    "type": "property",
    "reindex": false,
    "reindexCount": 1
  }
}
```

This is `oak-run index --index-definitions-file` and Oak's own
`IndexDefinitionUpdater` format, to the byte: the same key order, the same
type codes, the same two-space pretty-printing, hidden *properties* such as
`:version` included and hidden *children* excluded. Applying such a file
**replaces the whole definition node**, so the fidelity is the point rather
than a nicety.

`--output FILE` **refuses an existing file** rather than truncating it, and
refuses any path inside the store directory. This is deliberately unlike
`froe digest --output`, which truncates: a digest is a throwaway rendering
taken before and after an operation, while a definitions file is an artifact
an operator keeps and re-imports. The file carries the printer's bytes
exactly and ends at the closing brace, with no trailing newline, because that
is where Oak's printer ends.

## 4. `froe index check`

```console
$ froe index check /path/to/segmentstore
/oak:index/uuid: consistent against the head (2,857 entries, 8,256 nodes)
/oak:index/counter: no applicable check for type counter
/oak:index/lucene: consistent at level 1 (9 blobs, 1.8 MiB)
```

### 4.1 The exit codes

| Code | Meaning |
| --- | --- |
| `0` | Every index with an applicable check ran and was consistent. |
| `3` | At least one index is inconsistent. |
| `4` | None is inconsistent, but at least one index with an applicable check could not be run. |

3 and 4 rather than 1 or 2, because the binary already returns 1 for a
runtime failure and leaves 2 to the argument parser: an inconsistent index
must not look like a store that would not open. oak-run's
`--index-consistency-check` has no such contract; a runbook needs one.

A definition whose *type* has no applicable check — the counter, the
disabled, the Elasticsearch and the unknown — is reported per definition and
**does not reach exit 4**. Every store carries a counter, so if it did, no
unnarrowed run could ever exit 0 and a runbook gating on the command would
never pass.

Exit 4 is for a check that exists and could not be run: a dangling or absent
lane checkpoint, a budget refusal, or a definition the inventory could not
model. It is deliberately not exit 3, because reporting an index as
inconsistent when its lane cannot be resolved would send an operator to
reindex something that may be perfectly correct.

### 4.2 What it checks, per type

**The property family** — `property`, including unique and node-type indexes,
and `reference` — is checked against the state it indexes: the head for a
synchronous definition, the lane's checkpoint for an asynchronous one.
Checking an asynchronous index against the head would report every commit
since its checkpoint as a missing entry, so the checkpoint is not an
optimization but the only state the comparison means anything against.

Two halves:

* The **entry half** walks the stored index and asks whether each entry
  agrees with the content it names. A *stale* entry names a node that does
  not exist; a *mismatched* entry names a node that does not carry the key
  under any of the definition's property names; a *duplicate* is a unique key
  holding more than one path, which Oak refuses commits on. All three are
  unambiguous damage, and any of them means exit 3.
* The **covered-node half** walks the content the definition covers and asks
  whether an entry names each node. Its findings are reported and are
  **never** part of the verdict. A covered node no entry names may be an
  entry the index lost — or a node Oak never indexed, and froe cannot tell
  which. This is not hypothetical: a pristine Oak 1.90.0 store written by
  Sling has eighteen of them under `/oak:index/nodetype` — the `lucene`
  definition's `indexRules` subtree and the `rep:permissionStore` nodes.

  Oak itself says why. Its editor *does* cover those nodes: the interop
  suite creates a node under each through Oak's own index update and the
  entry appears every time. What those eighteen record is the commit that
  wrote them — one that ran before Oak's index hook was in the chain — and
  nothing will add their entries later, because nothing will change
  `jcr:primaryType` on a node that already has it. A healthy store keeps
  them forever, which is why they are an observation.
  `docs/analysis/index-property-storage.md` §13 invariant 7 has the
  evidence.

**Lucene** gets Oak's **level 1** (`IndexConsistencyChecker.BLOBS_ONLY`): every
binary property under the definition subtree, hidden children included, is
streamed to its end and its length compared with the length it declares. It
proves the index data is readable and complete. It proves nothing about the
Lucene 4.7.2 bytes inside those blobs — that is level 2, which needs the file
format, and plan 0008 adds it. The interop suite gets the level-2 verdict
from Lucene's own `CheckIndex` in the meantime.

The Lucene check is deliberately independent of the lane: it reads the
definition subtree at the head and needs no lane state, so a dangling lane
never makes a Lucene definition uncheckable.

**The counter** has no applicable check. Its storage is a sampled hash map
whose contents are a function of a random seed and the order Oak happened to
visit nodes in; there is no state to compare it against that would mean
anything.

### 4.3 What it does not prove

Query-time semantics. A consistent index is one whose stored entries agree
with the content it indexes; whether Oak's query planner would *choose* it,
and what a query through it would return, is a different question this
command does not ask.

### 4.4 The work budget

The check derives a node budget per definition rather than taking one from a
flag, from the counter index's own estimate at each of that definition's
included paths — `/` when the definition has no path filter, and always `/`
for a `reference` definition, whose editor Oak builds with no path filter at
all, so its `includedPaths` bound a budget while the walk covers the store.

The arithmetic is the counter's summed maximum-bound estimate multiplied by
**8**, with a floor of **100,000** nodes. Both numbers are deliberately
generous rather than tight, and the reason is worth stating plainly: nothing
in this repository measures how far the counter's sampling estimate runs from
a true count on a real store, and a factor presented as though it had been
measured would be worse than none. The budget's job is to stop a walk that
has clearly gone wrong — a path filter that matched the whole store when it
was meant to match a branch — not to be a performance budget.

Two cases take the **unbudgeted** form instead of a limit, because the
estimate is not a count at all: the store has no counter index, or has one
with no data node (Oak answers `-1`); and an included path the sampling
counter never recorded (Oak answers a placeholder, `0` or `2000`). A limit
invented for either would refuse a healthy store at a number nothing
justifies.

A budget refusal names the definition, and the remedy is to narrow `--index`
past it.

## 5. `froe index reindex`

The one `index` subcommand that writes. It rebuilds the indexes Oak has
flagged, offline, from the state Oak's own editors would have indexed —
without an Oak runtime, and without the hours of blocked startup Oak's own
synchronous reindex costs on a large store.

> **Beta.** The rebuild is proven against Oak's own reindex of the same
> store, but the review that freezes that evidence has not run yet. Take a
> backup first, and compare with `froe index check` afterwards.

```
froe index reindex REPOSITORY [--index PATH]… [--dry-run] [--yes]
                   [--work-directory DIRECTORY] [--from-head]
                   [--sort-budget-mebibytes N]
```

The repository must be offline: no Oak instance may be running against it.
The run takes the repository lock from planning through publication and
moves the head exactly once, appending one journal line.

### 5.1 What it rebuilds, and from what

| `type` | What is written | Indexed from |
| --- | --- | --- |
| `property` | `:index/<key>/<path…>` with `match = true` — Oak's `ContentMirrorStoreStrategy` | the head, or the definition's lane checkpoint |
| `property` with a strict `BOOLEAN` `unique = true` | `:index/<key>` with `entry` holding the absolute path — `UniqueEntryStoreStrategy` | the same |
| `reference` | `:references` and `:weakreferences`, keyed by the referenced identifier unencoded | the same |
| `counter` | `:index`, one node per counted path carrying `:cnt` | the same |

A definition with no `async` property is rebuilt from the head. One that
names a lane is rebuilt from **that lane's checkpoint**, not the head:
an asynchronous index is exactly as current as its lane, and indexing the
head would move it forward silently, past entries Oak's own lane has not
reached.

Everything else is refused by name rather than approximated — `lucene`
(plan 0010), `elasticsearch`, `disabled`, `ordered`, an unknown type, a
`valuePattern` regular expression froe does not evaluate, a definition
carrying a composite mount's index data, a path filter Oak cannot construct,
a definition nested under a content node, a node that is not an
`oak:QueryIndexDefinition`, a lane mid-run at `/:async/async-reindex`, and a
multi-valued `reindexCount`. A definition you name with `--index` is always
answered: rebuilt, or refused with the reason. Without `--index`, every
definition flagged `reindex = true` is considered, exactly as Oak's own
cycle considers them.

### 5.2 `--from-head`

Consulted only when a definition's lane checkpoint is dangling, or its lane
is absent from `/:async`. A definition whose lane resolves ignores it.

For a mirror or unique index it is the explicit choice to index the head
instead. That is safe: the lane's own replay re-inserts the same entries,
leaving `match` and `entry` unchanged, and only the randomized `:count_*`
estimates drift.

For a **counter** it is the choice to *reset*. The hidden children are
removed, nothing is built, and Oak's own replay rebuilds the counter from
scratch. That is not a lesser outcome: Oak's replay after a lost checkpoint
doubles every counter on the lane whether or not froe ran, so a rebuilt
counter would be wrong and a removed one is right. The visible properties
are untouched and `reindexCount` is left alone. A rerun reports
`nothing to do`.

### 5.3 The work directory

The sort spills to disk so a rebuild's memory does not grow with the number
of indexed nodes. `--work-directory` says where; the default is the system
temporary directory, **which on many Linux systems is a tmpfs held in
memory** — name a directory on disk for a large store.

The run creates one subdirectory there, named from the store's path and
locked for the run's duration, so runs against different stores never
collide and a live run is never mistaken for residue. It is removed on
return, whatever the outcome.

A froe-named subdirectory left behind by a run that was killed is
**refused** in a directory you named — it is your directory, and froe does
not delete what it did not create in it — and only **warned about** under
the default. The remedy either way: remove the subdirectory and rerun.

`--sort-budget-mebibytes` raises how much stays resident before spilling.
Higher is faster and uses more memory.

### 5.4 What the plan prints, and what the summary reports

`--dry-run` plans read-only, takes no lock and writes nothing. Otherwise the
plan is printed under the lock, before the confirmation, so what you approve
is what will run:

```
reindex plan for /var/aem/segmentstore
  rebuild /oak:index/uuid from the head: 51,204 entries, 2.1 MiB to sort
  nothing to do for /oak:index/counter: it has no hidden child to remove
  warning: /oak:index/lucene-fulltext is a lucene definition, which this froe version does not rebuild
  work directory /var/tmp (up to 130 MiB for one definition's spill)
  the index records this run replaces stay live through every checkpoint that references them, and are reclaimed only by a later `froe compact`
```

A scripted run without `--yes` plans and cancels, naming the flag. The
summary afterwards is built from what happened, not from the plan:

```
  /oak:index/uuid: 51,204 entries, 51,204 distinct keys, 68,391 index nodes
reindexed 1 index; head 8f3c….0000002a -> 2b91….0000010c
the replaced index records stay live through 2 checkpoints: …, …. They are
reclaimed only by a `froe compact` run after those are released.
```

### 5.5 What a run refuses, after it has started

Two refusals land during the apply rather than the plan, because neither can
be known until the entries are read:

A **duplicate unique key** — two distinct nodes carrying the same value for
a definition with `unique = true`. Oak's own commit refuses to create the
second, so a store holding both is one Oak could not have produced, and froe
will not write an index asserting otherwise. The refusal names the key and
the paths, and lands before anything is published.

An **entry budget** exhaustion from the publication tail, which verifies the
subtree it just built against the content it indexes under a budget of the
collected entry count plus one. Reaching it means the written index holds
more entries than the walk produced, which is a bug rather than a data
condition; the head does not move, and the run should be reported.

### 5.6 What a reindex costs

The records the run replaces become unreachable from the head — but they are
not garbage. Every checkpoint that references them keeps them live, and each
lane's checkpoint does by construction, since a checkpoint pins the content
root. They are reclaimed only by a `froe compact` run after those
checkpoints are released.

So **a reindex grows the store until the next compaction**. That is the
honest cost, the plan says so, and the summary names the checkpoints that
pin them.

## 6. What is not here yet

| Subcommand | Plan |
| --- | --- |
| `froe index dump` — Lucene index data to a filesystem directory | 0008 |
| `froe index reindex` for fulltext-enabled Lucene definitions | 0009 |
| `froe index import` — oak-run's filesystem transport back into a store | 0010 |

The read-only set grows in place; the mutating commands get their own
sections, with the confirmation and locking rules the rest of froe's
maintenance commands follow.
