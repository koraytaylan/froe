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
  which is not a count. For a property index it is Oak's own estimate. For
  Lucene it is the **document count**: the sum over the index's segments of
  documents minus deletions, read out of the commit file's table of
  contents, which is how Oak's own count over a directory computes it.
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
proves the index data is readable and complete.

Beside it, froe now reads the index's **table of contents** — `segments_N`,
each segment's `.si`, and the compound file's entries — and reports whether
the directory is a coherent set of Lucene files: every file the segments name
is present and every present file is named, every codec header is valid, and
each segment's deletion count is within its document count. An unregistered
codec name is reported rather than refused, because the files are coherent
and it is Oak that would fail at open.

That is still short of Oak's **level 2**, which is Lucene's own `CheckIndex`
reading the postings, and which needs a JVM. The interop suite gets that
verdict from `CheckIndex` itself.

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
> store, and the adversarial review that freezes that evidence has run —
> it is recorded in
> [plan 0007](plans/0007-property-index-reindex/ARCHITECTURE.md) and
> [plan 0010](plans/0010-lucene-offline-reindex/ARCHITECTURE.md), with the
> known gaps it could not close. `v0.12.0` is the first release to ship
> this command, and that review is its own, so the label stays for one
> release. Take a backup first, and compare with `froe index check`
> afterwards.

```
froe index reindex REPOSITORY [--index PATH]… [--dry-run] [--yes]
                   [--work-directory DIRECTORY] [--from-head]
                   [--binary-text marker|skip]
                   [--pre-extracted-text-directory DIRECTORY]
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
| `lucene` | `:data`, one Lucene 4.7.2 compound segment and the commit over it | **the definition's lane checkpoint**, always — §5.9 |

A definition with no `async` property is rebuilt from the head. One that
names a lane is rebuilt from **that lane's checkpoint**, not the head:
an asynchronous index is exactly as current as its lane, and indexing the
head would move it forward silently, past entries Oak's own lane has not
reached.

Everything else is refused by name rather than approximated —
`elasticsearch`, `disabled`, `ordered`, an unknown type, a
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

For a **counter** and for a **Lucene** definition it authorizes a
**reset**: the hidden children are removed, nothing is built, every visible
property but `reindex` is left alone, and Oak's own next cycle rebuilds
from scratch. See §5.9.

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

**The figure the plan prints is a proxy, and for a Lucene definition it is
a proxy resting on two other proxies.** A property index spills entries
froe can count and size exactly, so its figure is the entry bytes plus the
fan-in times the budget — a reduction pass writes merged runs before
unlinking the inputs it merged, so that much more is on disk transiently.

A Lucene definition has no such count. What the counting walk can produce
without analyzing anything is two byte totals: the bytes of values the
rules mark **stored**, and the bytes of values they mark **indexed**. The
figure is their sum times three, plus the same fan-in term:

* once for the spilled postings, doc values and norms, which coexist with
  the segment until `finish` drains them;
* once for the assembled segment;
* once for the compound copy, which holds the segment's files a second
  time while it is written.

Nothing in this repository measures bytes per token or bytes per posting,
and both are workload statistics rather than format facts — a stated
figure would be worse than a named proxy. The multiplier is the structural
count above and not a measurement, and the plan line says so.

### 5.4 What the plan prints, and what the summary reports

`--dry-run` plans read-only, takes no lock and writes nothing. Otherwise the
plan is printed under the lock, before the confirmation, so what you approve
is what will run:

```
reindex plan for /var/aem/segmentstore
  rebuild /oak:index/uuid from the head: 51,204 entries, 2.1 MiB to sort
  rebuild /oak:index/lucene-fulltext from lane async's checkpoint c-91f3: 4 indexing rules, 812,406 documents, 96.4 MiB stored and 1.1 GiB indexed, binary text the extraction-error marker under /var/aem/pre-extracted
  nothing to do for /oak:index/counter: it has no hidden child to remove
  warning: /oak:index/old-lucene cannot be rebuilt natively: its codec resolves to Lucene46, and froe writes the oakCodec composition alone
  work directory /var/tmp (3.6 GiB as a proxy for one definition's spill)
  the index records this run replaces stay live through every checkpoint that references them, and are reclaimed only by a later `froe compact`
```

The work-directory figure is **a proxy and says so**. For a property index
it is the entry bytes plus the fan-in times the budget; for a Lucene
definition it rests on the two byte totals above — §5.3 records the basis
for both. It is not an upper bound, and it is not measured.

A scripted run without `--yes` plans and cancels, naming the flag. The
summary afterwards is built from what happened, not from the plan:

```
  /oak:index/uuid: 51,204 entries, 51,204 distinct keys, 68,391 index nodes
  /oak:index/lucene-fulltext: 812,406 documents, 913,022 nodes visited, 1.4 GiB in 5 index files
reindexed 2 indexes; head 8f3c….0000002a -> 2b91….0000010c
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

### 5.6 Lucene definitions

A `lucene` definition is rebuilt natively — froe makes the documents from
the definition's own rules, analyzes them with Oak's own chain, writes one
compound segment and copies it into `:data`. No oak-run, no JVM.

"The definition's own rules" includes the two shapes AEM's own definitions
are written in, and both reach a node's document from **outside** the node:
a **relative** property definition such as `jcr:content/jcr:title`, whose
value lives on a child and whose fields carry the relative path as their
name; and an **aggregate**, whose matched nodes contribute their text to
the aggregating node's `:fulltext` — and to `fullnode:<path>` beside it for
a `relativeNode` include. An aggregated node whose own **node type**
declares an aggregate is followed in turn, as deep as the definition's
`reaggregateLimit` allows — the type is looked up in the definition's own
`aggregates` list, so an aggregate declared for a type no indexing rule
covers is still entered. What `excludeFromAggregation` leaves out is
decided by the **rule covering that node**, which is a different question
and a different lookup.

**`--binary-text` is required for every Lucene definition**, whether it
indexes a binary or not:

```console
$ froe index reindex /var/aem/segmentstore --binary-text skip
$ froe index reindex /var/aem/segmentstore --binary-text marker \
    --pre-extracted-text-directory /var/aem/pre-extracted
```

froe extracts no text from a binary, so what a binary contributes is a
decision only you can make, and `skip` is how you state that a definition
indexes none. What Oak does, and what each choice reproduces:

* Oak runs Tika. For a type Tika does not support it indexes **nothing**,
  which is what `skip` reproduces exactly. Where Tika threw it indexes the
  marker `TextExtractionError`, which is what `marker` reproduces. Neither
  reproduces a *successful* extraction.
* `--pre-extracted-text-directory` is consulted first, and it does
  reproduce one: text in Oak's own pre-extracted store is text Oak
  extracted. `--binary-text` is then the fallback for a blob the store does
  not cover. An **inline** segment blob is never in it — the store is keyed
  by a blob's content identity, which an inlined value has none of.
* A binary on a node with **no `jcr:mimeType`** is never indexed whatever
  you choose, because Oak's own extraction stops there first.

**What is refused, by name, before anything is written.** A definition
whose codec verdict is not `oakCodec`: an explicit `codec` property is
taken as Oak takes it, and without one a definition is `oakCodec` only when
it is fulltext-enabled — a rule with a node aggregate, or with a property
definition that is indexed and either `analyzed` or `nodeScopeIndex`.
Anything else is `Lucene46`, which froe does not write. Then:

* a definition-level `valueRegex` — it gates the fulltext loop with a
  regular expression froe does not evaluate;
* `similarityTags`, `useInSimilarity`, `dynamicBoost`, `function` — each
  writes a field this version does not produce;
* `compatVersion 1`, which names its analyzed fields without the `full:`
  prefix, and a definition with no `indexRules` at all, which Oak reads the
  same way;
* `maxFieldLength = 0`, which in Lucene 4.7.2 leaves every analyzed field
  silently empty;
* two rules assigning **different doc-value types to one `:dv` field
  name** — Oak keeps whichever type arrived first and drops the later
  documents, by traversal order; froe refuses at load instead, which is a
  recorded departure;
* an `nt:base` rule carrying a `nullCheckEnabled` property definition,
  which Oak's own rule validation throws on;
* a definition with no `async`, and a **hybrid** one whose `async` lists
  `sync`, or any `sync` or `unique` property definition: Oak keeps a
  synchronous `:property-index` for those that froe does not build;
* a definition **parked on the `async-reindex` lane** — the lane an
  out-of-band reindex moves a definition onto. No ordinary indexing cycle
  maintains it there, and froe does not move it back, so a rebuild would
  leave an index nothing updates; the lane's own next cycle diffs from a
  missing state and *appends* to what is already there. Restore the lane
  the definition belongs on first. A property-family definition parked
  there is still rebuilt, from the head: its replay is idempotent where a
  fulltext one is not;
* any child of an `analyzers` node — froe reproduces no
  consumer-registered analyzer.

A `tika` child is **accepted** and nothing in it is read, because froe runs
no Tika. Nothing in the plan or the summary says so either: an operator
whose definition configures which mime types Oak extracts is told only
what `--binary-text` does, which is the same thing it does for a
definition with no `tika` child at all.

One refusal lands at document time rather than at load: **a `DATE`
property value that does not parse**. One such value anywhere in the
indexed subtree refuses the run, naming the path, the property and the
value — which is what Oak does too, its own commit failing there.

**The suggester.** `:suggest-data` is removed and never rebuilt; Oak's own
suggester schedule rebuilds it on the lane's next cycle.

**A killed run leaves more behind than a property reindex does.** Its
froe-named subdirectory under the work directory can hold a *complete
assembled segment*, not only spill files. The next run refuses that residue
in a directory you named and warns about it under the default; the remedy
is the same — remove the subdirectory and rerun. A dead run's segment is
never mistaken for a live one's: each attempt assembles into a fresh
directory.

### 5.7 What a reindex costs

The records the run replaces become unreachable from the head — but they are
not garbage. Every checkpoint that references them keeps them live, and each
lane's checkpoint does by construction, since a checkpoint pins the content
root. They are reclaimed only by a `froe compact` run after those
checkpoints are released.

So **a reindex grows the store until the next compaction**. That is the
honest cost, the plan says so, and the summary names the checkpoints that
pin them.

### 5.8 The approximate counters, and why froe writes them

A property index carries hidden `:count_*` properties on its `:index` node
and on each key node. They are Oak's **approximate counter**, and they are
what Oak's query planner prices the index with: `getCountSync` answers `-1`
when a node carries none, and an index Oak cannot price loses to a full
traversal.

froe's rebuild writes them, by running Oak's own algorithm
(`ApproximateCounter.adjustCountSync`) with froe's own entropy. The *bytes*
cannot match Oak's — the name is a fresh UUID, the presence is two random
gates and the value depends on the draws, so two Oak reindexes of one tree
disagree on them too — but the behaviour does, which is what the planner
reads.

> **This changed.** froe's first reindex wrote none, on the reasoning that
> their absence is a state Oak reads without complaint. It is — and then Oak
> stops choosing the index. The interop suite's query probe caught it: over
> Oak's own rebuild of the fixture Oak planned
> `property uuid … estimatedCost: 3102.0`, and over froe's rebuild of the
> same entries it planned `traverse allNodes (warning: slow)`. An index that
> is correct and no longer used is the worst outcome a maintenance command
> can have, because nothing reports it.

A consequence worth knowing when you compare two stores: **any** comparison
of two rebuilds must exclude `:count_*`, froe's against froe's as much as
froe's against Oak's. That is what `froe digest --exclude-property-prefix`
is for.

### 5.9 Why a counter and a Lucene definition are reset rather than rebuilt

A definition whose lane cannot be resolved is refused without
`--from-head`, naming the lane and the checkpoint. With it, a counter and a
Lucene definition are **reset**: froe removes the definition's hidden
children, builds nothing, leaves every visible property alone except
`reindex`, which it raises, and Oak's own next cycle rebuilds from scratch.

froe does not rebuild either of them itself, and for the same kind of
reason. Oak's replay after a lost checkpoint doubles every counter on the
lane whether or not froe ran, so a rebuilt counter would be wrong. And its
fulltext editor re-enters reindex mode on a missing before state at the
root, where its index writer's reindex branch **appends** every document to
whatever `:data` still holds — doubling the index. A definition with **no
hidden child** is the case Oak rebuilds from scratch, so removing them is
what makes the next cycle produce a correct index.

The flag is not optional either. `IndexUpdate.shouldReindex` has two
triggers: the `reindex` flag, and a definition *absent from the before
state's* `/oak:index` with no hidden child. A definition your store already
holds is never absent, so without the flag Oak rebuilds nothing — and the
reset would be an index removed with nothing to restore it.

> **This changed twice.** The first version removed the hidden children and
> left the flag alone; the interop suite's counter-reset scenario found
> that Oak then rebuilt nothing. The second refused the counter outright,
> on a measurement — 129 reindex attempts with no commit, for the counter
> or for any other index on the lane — that turned out to be a symptom of
> froe's own defect: the definition froe rewrote carried its properties in
> an order Oak's own `getProperties` cannot read (`storage-format.md` §4),
> so Oak's conflict merge threw on it and the lane's commit failed every
> cycle. With the writer corrected, Oak rebuilds on the first cycle. Both
> scenarios now run in the interop suite: `property_reindex` for the
> counter and `lucene_reindex` for the Lucene definition, each ending in
> Oak's own from-scratch rebuild, the counter's canonical and the Lucene
> definition's enumerated against a froe rebuild of the same state.

## 6. `froe index dump`

Lucene index data out of the repository and onto the filesystem, in the
layout oak-run's importer reads.

> **Beta.** Byte identity with Oak's own dumper is proven by the interop
> suite, and the review that freezes that evidence is recorded in
> [plan 0008](plans/0008-lucene-index-transport/ARCHITECTURE.md).
> `v0.12.0` is the first release to ship this command, so the label stays
> for one release.

```
froe index dump REPOSITORY --output DIRECTORY [--index PATH]…
```

**Read-only, and testably so.** The dump opens the store exactly as
`froe summary` does — no lock, no manifest write — and writes nothing inside
it. The regression that holds it to that compares a file snapshot of the
store before and after, `repo.lock` included, and a second test runs a dump
while another process holds the lock.

### 6.1 What it writes

```
<output>/index-dumps/
├── index-definitions.json      every dumped definition, in the out-of-band variant
├── indexer-info.properties     the checkpoint the data was taken at
└── <base name>/                one per definition; `/oak:index/lucene` gives `lucene`
    ├── index-details.txt       the JCR path and the directory-name mapping
    ├── data/                   `:data`
    └── suggest-data/           `:suggest-data`, when there is one
```

`<output>/index-dumps` is the directory to hand to
`froe index import --input` or to oak-run's `--index-import-dir`: both scan
its direct children for `index-details.txt`.

A mount-decorated `:data` — a composite store's other mount — is **reported
and skipped**, never copied into a directory claiming to be this mount's.

### 6.2 The one-checkpoint rule

`indexer-info.properties` names **one** checkpoint for the whole directory,
while whether an index can be imported is a question about each definition
separately. So froe writes the file only when every dumped definition is
asynchronous, on the same lane, and that lane's checkpoint resolves in the
store.

Otherwise the files are still written — they are a backup, and worth having
— but the properties file is not, and the dump says which of four reasons
applied: the selection spans several lanes, a definition is synchronous, the
lane's checkpoint is dangling, or the lane has no state on `/:async`. Such a
directory cannot be imported by oak-run either; oak-run warns the same way
when the checkpoint is `head`.

**The remedy is to dump one lane at a time with `--index`.** A definition
you name that is not a `lucene` definition, or names nothing at all, is
refused by name rather than quietly producing an empty dump.

### 6.3 An interrupted dump

An existing dump is **never** written over, and a partially populated
`index-dumps` is refused just as a complete one is: completing an
interrupted dump would leave a directory that is part one dump and part
another, which looks importable and is not.

Each file is streamed to a froe-named temporary and renamed into place, so
a file under its real name was written whole. A returned error removes its
temporary; abrupt death leaves it. Either way the remedy is the same —
**delete the output directory and rerun**.

## 7. `froe index import`

Lucene index data built out of band, installed back into a **stopped**
store.

> **Beta.** The round trip and the guards are covered by froe's own tests,
> the comparison against Oak's own importer is proven by the interop
> suite, and the review that freezes that evidence is recorded in
> [plan 0008](plans/0008-lucene-index-transport/ARCHITECTURE.md) with its
> known gaps. `v0.12.0` is the first release to ship this command, so the
> label stays for one release.

```
froe index import REPOSITORY --input DIRECTORY [--index PATH]… [--dry-run] [--yes]
```

`--input` is a `<…>/index-dumps` directory: the one `froe index dump`
wrote, or the one an oak-run out-of-band build produced.

The flow is `froe compact`'s. `--dry-run` plans read-only without the lock
and prints what a run would do. Otherwise the run prepares under the lock,
prints the plan, asks while still holding the lock, applies, and prints a
summary built from what happened rather than from what was intended.

### 7.1 The state rule, and why it replaces bring-up-to-date

oak-run's importer brings an imported index up to date by replaying every
commit made since the index was built, against a **live** repository. froe
is an offline tool and has no editors to replay with, so it requires
instead that **there is nothing to catch up on**: the checkpoint named in
`indexer-info.properties` must resolve to the same state the definition's
lane will resume from.

Concretely, per definition: the root of the directory's checkpoint must be
the *same record* as the root of the checkpoint `/:async/<lane>` names. A
directory built at any other state is refused, naming both checkpoints and
both roots, and telling you to rebuild at the lane's own checkpoint.

That is a precondition, not a check. It is what lets froe install bytes it
did not compute and cannot verify semantically: the index is current
because the state it was built at is the state the lane is at.

**`indexer-info.properties` names one checkpoint for the whole directory**,
so import one lane at a time — one dump, one import, per lane — exactly as
you dump one lane at a time.

### 7.2 Building the index out of band, at the right checkpoint

1. Stop AEM. It stays stopped from here through the import: a **running
   lane releases its previous checkpoint after every cycle**, so a
   checkpoint name read from a live store can be gone by the time the
   build finishes.
2. Read the lane's checkpoint from the stopped store:

   ```
   froe index list REPOSITORY
   ```

   which prints each definition's lane and the checkpoint that lane will
   resume from, or read `/:async/<lane>` directly with
   `froe node REPOSITORY /:async`.
3. Build with oak-run, passing that checkpoint name to `--checkpoint`.
4. `froe index dump REPOSITORY --output BACKUP` — the old index data, so
   the import is reversible.
5. `froe index import REPOSITORY --input BUILD/index-dumps`
6. `froe index check REPOSITORY` — the blobs resolve and the table of
   contents is coherent.
7. Start AEM and watch the lane: it must resume without logging a reindex.

### 7.3 The definitions file must agree with the store

`index-definitions.json` is mandatory, and froe compares it against the
store's own definitions before it copies anything. **froe imports index
*data*, never a definition change**: make definition changes through
oak-run or AEM first.

The comparison is Oak's own — the one its index-information provider
performs over visible clones, which keep hidden properties and drop hidden
child nodes — extended by the properties an out-of-band build legitimately
rewrites. These are accepted, each in the one direction it happens:

| Difference | Accepted when |
| --- | --- |
| `reindexCount` | always; the file's value is one above the store's for a froe dump and two for an oak-run build |
| `refresh` | only on the file's side, where the lane revert sets it |
| `seed` | only when the store lacks one; two *different* seeds are drift, because the counters they drive would disagree |
| `corrupt`, `indexImportState` | only when the store has them and the file does not, which is the usual reason for an out-of-band build |
| a `facets` subtree | only when the file has it and the store does not, since the build's document maker persists it |

Anything else — a changed visible property, a removed visible child — is
drift, and the refusal names the first difference.

### 7.4 What it refuses

* **A synchronous Lucene definition.** oak-run's own importer never
  completes that case: its catch-up step skips the `sync` lane and leaves
  the definition on `async = temp-sync`. There is no Oak behaviour to
  match and no oracle to prove against.
* **A hybrid definition** — one whose property definitions or `nodeTypeIndex`
  rule carry `sync` or `unique`. Oak keeps the synchronous half in the
  hidden child `:property-index`, maintained by its incremental editor, and
  froe builds none.
* **A directory that is not a coherent Lucene index** — no commit file, a
  file a segment names that is absent, a file no segment names, or a header
  that does not read. Refused before a byte is copied.
* **A named `--index` path with no directory in the input**, by name.

### 7.5 What it changes, and what it does not

Per imported definition: `:data` (and any other mapped index directory)
replaced, `:status` replaced with a node carrying a fresh `uid`,
`:index-definition` replaced with a visible clone of the updated
definition, `:version` written, `reindex` cleared, `reindexCount` set to
the file's value plus one, `corrupt` and `indexImportState` removed when
present, and `:disableIndexesOnNextCycle` written when the definition's
`supersedes` names an index that is still active — which is where Oak's
own importer writes it. froe raises that flag and never acts on it:
disabling a superseded index changes which index answers a query, and that
is a decision for a running Oak and for you.

Every pre-existing hidden child is dropped, as Oak's own definition updater
drops them when it installs the file's node wholesale. **A stored
`:suggest-data` goes and is never re-imported** — Oak's own Lucene writer
rebuilds the suggestions whenever their `lastUpdated` is missing, so the
suggester is absent only until the next cycle.

**No checkpoint is released.** oak-run's importer releases the one
`indexer-info.properties` names as its fourth step; froe does not, because
the only checkpoint it accepts is a lane's, and the lane owns it.

**The store grows by the index size.** The replaced records stay live
through every checkpoint that references them and are reclaimed only by a
`froe compact` run after those are released.

## 8. What is not here yet

| Subcommand | Plan |
| --- | --- |
| A Lucene reindex proved against a live Oak, end to end | 0010, task 1009 |

The read-only set grows in place; the mutating commands get their own
sections, with the confirmation and locking rules the rest of froe's
maintenance commands follow.
