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
  Sling has eighteen of them under `/oak:index/nodetype`.
  `docs/analysis/index-property-storage.md` §13 invariant 7 records the
  evidence and the open question.

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

## 5. What is not here yet

| Subcommand | Plan |
| --- | --- |
| `froe index dump` — Lucene index data to a filesystem directory | 0008 |
| `froe index reindex` — offline rebuild of the property family | 0007 |
| `froe index reindex` for fulltext-enabled Lucene definitions | 0009 |
| `froe index import` — oak-run's filesystem transport back into a store | 0010 |

The read-only set grows in place; the mutating commands get their own
sections, with the confirmation and locking rules the rest of froe's
maintenance commands follow.
