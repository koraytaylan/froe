# Oak interop test suite

End-to-end tests verifying that froe reads stores written by Apache
Jackrabbit Oak, writes stores that Oak reads, and performs maintenance
operations that leave the store in a state Oak boots against cleanly.

The suite uses an Apache Sling image (Apache-2.0) as a real Oak instance —
no Adobe/AEM license is involved. Sling boots Oak with TarMK by default,
so the store is byte-for-byte what a production Oak repository produces.

The image is **pinned by manifest digest** in
`crates/froe-cli/tests/interop/environment.rs`, because the claim in the README names an
Oak build and a mutable tag could be re-pushed with a different one. The
suite also asserts the `oak-segment-tar` version inside the image, so a
substitution fails loudly rather than silently redefining what was verified.

Setting `FROE_INTEROP_CANARY=1` runs against the floating `:14` tag instead.
The two modes answer different questions: the pinned run asks whether froe
still interoperates with the build the claim names, and the floating run asks
whether the ecosystem has moved underneath it. On a canary run, the Oak
version assertion failing is the useful result.

## Prerequisites

- **podman** installed and runnable by the current user.
- **Network access** to pull the pinned Sling image (once).
- **froe** built: `cargo build --release`.

## Environment

| Variable | Effect |
| --- | --- |
| `FROE_INTEROP_WORK_ROOT` | Where fixtures are built. Defaults to the system temporary directory, which on many hosts is a small tmpfs — point it at real disk before generating anything large. |
| `FROE_INTEROP_SLING_IMAGE` | Overrides the image outright. |
| `FROE_INTEROP_CANARY=1` | Runs against the floating `:14` tag (see above). |
| `FROE_INTEROP_COMMAND_TIMEOUT_SECONDS` | Ceiling on a single froe command. Defaults to 900. |

The command timeout is a hang detector, not a performance budget. It defaults
high because it has to clear the slowest legitimate command on the largest
fixture anyone points the suite at: `froe compact` over a 10 GB, 41-archive
Sling store measures 120–135 s here. A CI run over the small generated store
can tighten it; an unparseable or zero value falls back to the default rather
than disabling the check. The command's exit status is asserted before its
duration, so a command that fails *and* is slow reports its own output rather
than being relabelled a timeout.

Phases share their fixture through an in-process `OnceLock`, and — when that
is empty, which is the case in any process that did not itself run
`generate` — through the path `generate` records in `work_root()/fixture-path`.
The pointer is what makes a single phase re-runnable on its own, which
matters more than it sounds: a failure that cannot be reproduced in isolation
cannot be attributed to the operation that caused it, and attribution is the
whole point of the digest comparisons below.

`generate` deletes the pointer before it starts. Within one run, a phase
scheduled ahead of `generate` therefore still fails loudly instead of quietly
picking up the previous run's store and reporting a pass about bytes nobody
produced today.

The whole suite must still never be run by selecting every ignored test.
`interop_full` runs the chain itself and claims the same `OnceLock` `generate`
does, and the harness orders tests by name rather than by dependency, so an
unfiltered run has `interop_full` collide with `generate` — always name
`interop_full`, or one phase.

## Running

```console
# All phases in dependency order:
$ scripts/interop-fixture.sh

# A single phase (generate runs first, in its own cargo process):
$ scripts/interop-fixture.sh compact

# Re-run one phase against a fixture an earlier run already built, without
# regenerating it — this is the debugging loop after a failure:
$ cargo test -p froe-cli --features interop -- --ignored --nocapture phase_maintenance::compact

# Direct cargo invocation — `interop_full` is the whole chain, and naming it
# is required: an unfiltered `--ignored` run collides with itself as above.
$ cargo test -p froe-cli --features interop -- --ignored --test-threads=1 phase_recovery::interop_full

# A single phase via cargo:
$ cargo test -p froe-cli --features interop -- --ignored phase_baseline::read
```

Tests are `#[ignore]`d by default so they don't run in the normal
`cargo test` gate. The `interop` feature flag gates compilation; without
it the test file is empty.

## Dependency chain

The phases run in a strict dependency chain. Each phase depends on the
previous one and aborts the chain on failure. There is no point testing
a later phase if an earlier one is broken — the later phases use the
earlier phases' output as input.

```
generate
   │  Sling writes the Oak store fixture
   ▼
read
   │  froe reads the Oak store
   │  If this fails: froe cannot read Oak's format. No write-path
   │  verification is meaningful without a working reader.
   ▼
judge_smoke
   │  the Oak-side judge is compiled inside the pinned image and each of
   │  its verdicts shown reachable: Oak's dumper produces a Lucene
   │  directory froe did not write, Lucene's own CheckIndex calls it
   │  clean, and one flipped byte makes it refuse
   │  If this fails: every later comparison against Oak would be
   │  meaningless while passing, which is the failure a smoke phase exists
   │  to catch.
   ▼
index_inventory
   │  froe's index readers against Oak's own printers over the same bytes
   │  If this fails: froe reads Oak's index structures differently from
   │  Oak. Runs before commit, because froe's direct commits run none of
   │  Oak's index editors and a fixture froe has written to is
   │  legitimately short an index entry.
   ▼
property_reindex
   │  Oak rebuilds every property-family definition in the fixture; froe
   │  rebuilds the same extracted bytes and the two must render identically
   │  If this fails: froe's offline rebuild is not what Oak's own reindex
   │  produces, which is the whole claim of `froe index reindex`. Runs
   │  before commit for the same reason index_inventory does.
   ▼
lucene_dump
   │  Oak's own dumper reads :data out of the fixture; froe's `index dump`
   │  reads the same :data; every file must be byte-identical, and Lucene's
   │  own CheckIndex and Oak's own consistency checker at its full level
   │  must both pass over froe's output
   │  If this fails: froe reads Lucene index data out of the segment store
   │  differently from Oak, so nothing it dumps can be imported anywhere.
   │  Read-only, so its position is free; it runs here because the
   │  fixture's Lucene index reflects the state Sling left.
   ▼
lucene_import
   │  Both directions: froe dumps the index, loses it, imports it back and
   │  the definition must render as it did; and Oak's own editors build one
   │  out of band at the lane's checkpoint, froe imports that, and a booted
   │  Oak answers a fulltext query through it
   │  If this fails: `froe index import` installs something Oak cannot use,
   │  which is the one thing this command must never do.
   ▼
lucene_writer_conformance
   │  A committed corpus is written twice — once by froe's own Lucene
   │  writer, once by Lucene's under the same oakCodec composition — and the
   │  two indexes are enumerated and compared line for line, after Lucene's
   │  own CheckIndex has called froe's clean
   │  If this fails: froe writes a Lucene index whose contents are not what
   │  Lucene writes for the same documents, which is what plan 0010's
   │  rebuild would install. Reads the fixture not at all and writes only
   │  into its work directory, so its position is free.
   ▼
lucene_reindex
   │  Oak's own async lane rebuilds every lucene definition in the fixture;
   │  froe rebuilds the same extracted bytes offline, and the two indexes
   │  enumerate identically outside the one declared binary difference.
   │  Every query pair and every plan agree through Oak's own engine, and a
   │  reset on a lane whose checkpoint is gone ends in Oak's own
   │  from-scratch rebuild.
   │  If this fails: froe's offline Lucene rebuild is not what Oak's own
   │  reindex produces, which is the whole claim of `froe index reindex`
   │  for a lucene definition. Runs before commit for the reason
   │  property_reindex does.
   ▼
commit
   │  froe adds nodes with typed properties to the content tree via
   │  the library's commit API, then Sling reads them back
   │  If this fails: froe cannot write content that Oak reads — the
   │  core interop claim. No point testing checkpoint, compact,
   │  backup, or recover if the writer can't produce content Oak reads.
   ▼
checkpoint
   │  froe writes a checkpoint (metadata-only write-path test)
   │  If this fails: the writer's checkpoint machinery is broken,
   │  which affects compact's expired-checkpoint handling and its
   │  checkpoint preservation.
   ▼
compact
   │  froe compacts a copy — the one maintenance command, so this is
   │  also the reclamation test: orphan segments, a partially dead
   │  archive rewritten to its next generation letter, a stale archive,
   │  expired checkpoints and corrupt journal lines all go in the same
   │  run. Sling boots against the result.
   │  If this fails: the write path's plan-and-apply machinery is broken.
   ▼
compact_tail
   │  the same, with --tail: the shared full generation is retained, so
   │  the run reclaims less and must still leave a store Oak boots
   ▼
checkpoint_removal
   │  remove by name, remove-unreferenced and remove-all; the checkpoint
   │  Oak's async indexer references survives remove-unreferenced
   ▼
cleanup
   │  a multi-generational store with an expired checkpoint, a stale
   │  archive, a truncated journal and corrupt journal lines
   ▼
journal_retention
   │  a plain froe compact retires every revision but the head it
   │  wrote and sweeps the segments behind them; Oak boots the result
   │  and serves the baseline tree from the one revision kept
   │  If this fails: froe's by-policy destruction of reachable history
   │  leaves a store Oak cannot open.
   ▼
compact_convergence
   │  the run after a full compaction proves the store fully compacted,
   │  mutates nothing, and says so; Oak boots the twice-run store
   │  If this fails: the convergence gate either churns or over-gates.
   ▼
version_history_purge
   │  Oak itself versions two nodes and deletes one; froe detects the
   │  orphaned history, purges it under a digest with the purge as its
   │  only exclusion, and Oak boots the result, serves the survivors,
   │  and checks the surviving versionable in again
   │  If this fails: the one content mutation maintenance performs is
   │  not something Oak accepts, or it took more than it named.
   ▼
repair
   │  Oak's own JVM is killed with SIGKILL while it holds an archive
   │  open; an authorized froe compact rebuilds the index and
   │  Oak boots against the result
   │  If this fails: froe cannot repair the state a crashed Oak leaves,
   │  or Oak will not read what froe rebuilt.
   ▼
backup
   │  froe backup + restore, Sling boots against the result
   │  Independent of compact but later because lower-risk.
   ▼
recover
   │  froe recover-journal after deleting journal.log
   │  Last because it is the most destructive (deletes the journal).
```

## The content digest, and why attribution matters more than detection

Booting Sling is a liveness gate, not an integrity claim. A store can be
subtly wrong — a property decoded at the wrong arity, a value re-rendered, a
node dropped from a subtree nobody inspects — and Oak will still start, still
serve, and still log nothing. The damage surfaces later, and by then several
maintenance runs have happened and there is no way to tell which one caused
it.

So every mutating phase renders its store with `froe digest` before and after
its operation and asserts the difference is exactly what the phase
**declared** — `ExpectedDigestDelta::None` for operations that must preserve
everything, `CheckpointsOnly` where retiring checkpoints is the point. The
operation named in a failing difference *is* the operation that changed
something. That is the whole mechanism: no ledger, no replay, no bisection.

It is affordable because it is offline. On the interop fixture — 51,352
nodes, 107,592 properties, 5,698 binaries totalling ~124 MB — a digest takes
about 0.3 s and is byte-identical across runs.

What each line covers, and why:

- **Scope is the super-root**: `root`'s subtree, the super-root's own
  properties, and every checkpoint, including each checkpoint node's own
  properties. A checkpoint's expiry timestamp drives froe's own retirement
  logic, so a corrupted one is self-fulfilling corruption.
- **Sorted by name**, never by storage order. Two encodings of the same
  content — a map that split into a branch where the other stayed a leaf — are
  both legal, and ordering by storage would report a difference where there is
  none.
- **No identity**: record, segment and stable identifiers are all absent,
  because compaction legitimately changes every one of them. What survives
  compaction is exactly what this renders.
- **Type and arity are explicit.** `tags=String[]:a` and `tags=String:a` are
  different lines. Arity is invisible to a check that only resolves records.
- **Binaries are content**: `<declared>/<read>@<crc32>` over the streamed
  bytes, so a changed, truncated or reordered binary is a changed line.
- **Lookup probes.** Oak reaches a child or property two ways — enumeration,
  and lookup by name through `MapRecord.getEntry`'s unsigned-hash descent and
  `Template.getPropertyTemplate`'s signed-hash binary search. Those read
  different bytes. A mis-sorted map leaf leaves every entry *present under
  enumeration*, so a digest, an export and a consistency check all pass, while
  `getChildNode("page3")` returns nothing in production. Sorting by name — which
  the digest does for comparability — actively erases that evidence, so every
  enumerated child and property is looked up by name as well.
- **`/:async` closure**: checkpoints an index lane still resumes from must
  still exist. Properties ending `-temp` are excluded: that list holds
  checkpoints the indexer intends to *release*, and it routinely names ones
  already gone, so checking it would fail on every pristine Oak store.

`froe digest --baseline <file>` does the same comparison outside the suite and
exits non-zero on a difference, which is how an operator answers "did this
maintenance change my content?".

### What the digest does not prove

It is froe reading froe. A misconception shared by froe's writer *and* its
reader — a map-leaf ordering rule wrong in both — is invisible to it, and only
an independent Oak-side rendering would catch that. It says nothing about the
`.gph` and `.brf` trailers, which Oak checks against their own CRC and then
trusts; those are consumed by Oak's own compaction and blob GC long after any
assertion here has passed. And two values whose stored bytes differ but decode
identically (`"TRUE"` and `"true"`) render the same line.

## What each phase proves

### generate

Boots Sling with TarMK, populates content under `/content/interop`
(folders, ordered folders, multi-value properties, an inline binary),
churns content (create + delete 20 subtrees × 5 children × 3 rounds) to
produce orphaned segments, and stops cleanly. The resulting store is the
shared fixture for all later phases.

Beside the content it adds the index shapes the reindex phases rebuild,
each through Sling so the bytes are authentically Oak's: a
`mix:referenceable` target with a `REFERENCE` and a `WEAKREFERENCE` to it, a
group with two members, a property index over `jcr:title` posted *after* the
content exists so Oak rebuilds it in one synchronous cycle, and — for plan
0010 — a second Lucene definition beside the repository-wide `lucene` one
Sling ships.

That second definition, `/oak:index/interopLucene`, is the fixture's
coverage of everything the shipped one has no branch for. It is
fulltext-enabled (an `analyzed`, `nodeScopeIndex` property, so Oak selects
`oakCodec`), on the fixture's one `async` lane, with `includedPaths` and
`queryPaths` both `/content/interop/variant` — the subtree its content lives
in, which is what keeps plan 0008's transport phases unchanged: its import
phase names `/oak:index/lucene` explicitly, its dump phase compares per
definition, and both query under `/content/interop/pages`. It carries
`evaluatePathRestrictions`, an `ordered = true` property definition in the
new-format place under `indexRules` (the old `orderedProps` list is read
only by the old-format rule construction, for definitions without
`indexRules`), a `nullCheckEnabled` property under a rule whose node type is
`nt:unstructured` rather than `nt:base` — Oak's own rule validation refuses
that combination — a `facets` property and an `aggregates` rule including
`jcr:content`.

The content under `/content/interop/variant` is what those branches need: six
items carrying a long, a double, a date, a boolean and two strings, one of
which is present on four of them and absent on two, and three page-like trees
with `jcr:content` children for the aggregate. Its words are nonsense on
purpose, so a query's row set is attributable to this content rather than to
whatever else the image ships.

`generate` waits for Oak's `async` lane to finish that definition before it
churns, and asserts afterwards — from the extracted store, through `froe
node` — that Oak wrote a `facets` configuration into the visible definition.
That configuration is the evidence Oak read the rules: Oak's own editor
writes it only when a facet property was actually indexed, and a definition
whose rules Oak could not load would still leave a `:data`, an empty one, and
a reindex oracle comparing two empty indexes would pass.

### The log gate, and its positive control

Every phase that boots Oak against a froe-written store asserts Oak logged
none of its own repair messages — `Unable to access revision`, `Could not
find a valid tar index`, `Recovering segments from tar file`, `Could not
read tar file`, `Regenerating tar file`. A content assertion made after a
repair proves nothing about froe's output, and this is the signal a
froe-to-froe round trip cannot produce at all.

A scan for *absent* markers passes trivially on an empty string, so the gate
first requires the captured log to contain `Apache Sling Application
Launcher` — the launcher's own banner, present in any container that came up
at all. Without that control, a mistyped container name or a `podman logs`
that failed for any reason would report "Oak consumed the store as froe
wrote it" while having read nothing.

The store also carries three shapes the index phases and the later plans
need, added through Sling and never through froe: a `mix:referenceable`
target with a sibling holding a `REFERENCE` and a `WEAKREFERENCE` to it, so
`/oak:index/reference` has entries under both hidden children; a group with
two members, so `repMembers` indexes a multi-valued `rep:members` under a
declared `rep:MemberReferences` type; and a property index over `jcr:title`
posted *after* the content exists, which Oak rebuilds in the same commit.

`generate` asserts all three are in the extracted store before it records the
digest baseline, through `froe node` and `froe tree` rather than through the
index readers under test — a check written against the reader it is checking
cannot fail when that reader is wrong.

The rebuilt index cannot be left flagged, and that is Oak's doing rather than
a choice: under the default `oak.indexUpdate.ignoreReindexFlags=false`,
collecting the editors clears `reindex`, increments `reindexCount` and
registers the editor, and the cycle then runs that editor from the missing
state to the head before the commit completes. So `generate` asserts the
opposite — `reindex = false`, `reindexCount = 1`, a non-empty `:index` — and
a still-flagged definition, which the reindex plans need, comes from a
synthetic store instead.

### read

froe reads the Oak-written store: `summary`, `tree`, `check`,
`search-nodes`, and `export` (json-lines). All must succeed. This is the
foundation — every later phase uses froe's reader to verify results.

`check` runs as `--path / --binaries` and the phase asserts the reported
revision **is the head froe wrote**, read from `journal.log`. Exit status
alone would prove far less than it appears to:
`ConsistencyReport::has_good_revision` is an `any` over the head paths
chained with every checkpoint's paths, so a store whose head is broken still
exits zero as long as one checkpoint resolves somewhere. `--binaries` matters
for the same reason — without it binary records are resolved but never read.

The phase also re-derives the content digest `generate` recorded and requires
it to match, which proves the rendering is reproducible across processes.
Without that, every later comparison would report differences that mean
nothing.

### judge_smoke

The suite can make Oak *consume* froe's output through every other phase.
This is the one that lets it *ask Oak questions*.

The judge is a handful of Java classes compiled and run **inside the pinned
Sling image**, against the Oak bundles that image ships. It is not a second
implementation and not a second image: `oak-core`, `oak-store-spi`,
`oak-segment-tar` and `oak-lucene-1.90.0.jar` are all there, the last
inlining Lucene 4.7.2 whole — `CheckIndex` and the `META-INF/services`
registration of `oakCodec` included — and the image carries a full Temurin 21
JDK. What it does **not** ship is `oak-run` or `oak-run-commons`, so where a
phase relies on one of their helpers the judge re-implements it from the
shipped classes. `javac` succeeding is itself the assertion that every class
the judge needs is in the image.

The classes are compiled rather than committed. A committed `.class` file
would have been built against whatever JDK and Oak were on the machine that
produced it and would keep working against an image it no longer matches;
compiling in the image makes a mismatch a compile error rather than a silent
`NoSuchMethodError` three phases later. The class directory is cached under a
hash of the sources **and** the resolved image reference, so neither a
changed judge nor a different image — the canary's floating tag included —
can run against stale classes.

What the judge can stand in for: Oak's own verdicts about a store or a
directory — its definition printer, its index printer, its Lucene dumper,
Lucene's `CheckIndex`, a document count, and a commit through Oak's own index
update. Since plan 0009 it also stands in for Lucene's own *writer*:
`Corpus` builds the committed writer corpus through Lucene's `IndexWriter`
under the `oakCodec` composition and enumerates any index into a canonical
dump, `CodecVectors` prints the bytes Lucene's own `DataOutput` and `PackedInts`
*writers* produce — a fixture generator run by hand inside the image, not a
phase: froe replays its committed output in `lucene_codec_primitive_tests`,
outside the suite — and `FstCheck` enumerates froe's transducers with
Lucene's own reader, printing the pairs it found for the phase to compare
against the corpus's own expected column. Plan 0010 added three more, none of which needs a store at
all: `Analyze` puts a corpus through each of Oak's own analyzer chains and
prints the tokens with their increments and offsets, `NumericVectors` prints
Lucene's own prefix-coded numeric terms and Jackrabbit's own `ISO8601`
parse of a date corpus — refusing to run unless that class came from
`jackrabbit-jcr-commons` — and `RegularExpressionVectors` puts a pattern and
a name through Java's own engine under Oak's `NamePattern` logic. What the
judge cannot stand in for: anything `oak-run` alone does, and anything that
needs a booted Sling, which is what the container phases are for.

One convention runs through it. A class that opens a segment store writes its
data to a file the caller names, never to standard output, because opening a
store initializes Oak's logging and that logging goes to standard output — a
printer's bytes on that stream would arrive interleaved with `TarMK ReadOnly
opened`. A class that renders only a verdict writes nothing to standard
output and exits non-zero with the offending item on standard error.

`judge_smoke` proves each verdict is reachable before any phase trusts one:
Oak's dumper produces a Lucene directory froe did not write, `CheckIndex`
calls it clean — which is also the proof that the class path resolves
`oakCodec`, since without the registration the reader cannot open a segment
at all — a sample `oakCodec` index is written and checked, and one flipped
byte of `segments_1` makes the checker refuse. Without that last one the
clean verdicts would prove nothing, because a checker that always passes also
passes.

### index_inventory

froe's reading of index structures against Oak's own, over a store Oak wrote.

* **`froe index definitions` is byte-identical to Oak's
  `IndexDefinitionPrinter`**, normalized only for trailing whitespace. Same
  key order, same type codes, same pretty-printing, hidden properties such as
  the `lucene` definition's `:version` included. This file is what Oak's own
  definition updater consumes, and applying it replaces the whole definition
  node, so the fidelity is the requirement rather than a nicety. A difference
  names the first differing line with both sides.
* **`froe index list` agrees with Oak's `IndexPrinter` field by field**, over
  the intersection of the paths both list — and the phase asserts the two
  sets are equal, so a fixture holding a `disabled`, untyped or throwing
  definition fails rather than quietly shrinking the comparison. `Is active`
  is excluded because it is constantly true over a store without non-default
  mounts. Two renderings are reconciled rather than papered over: Oak formats
  timestamps to whole seconds where froe renders the stored millisecond form,
  and Oak computes a suggester size of zero where froe omits a size it has no
  child for.
* **`froe index check` passes the pristine store and names a forged defect.**
  Removing the reference target through froe's own writer — the node several
  indexes name — makes the check exit 3 and name the path that no longer
  resolves.
* **Every Lucene directory Oak dumps is a valid Lucene index**, by Lucene's
  own `CheckIndex`. The document count is recorded in the run record rather
  than compared with `:status/indexedNodes`, which is a per-cycle counter Oak
  resets on every indexing cycle and not a document count.
* **Oak is asked whether its editor covers the subtrees whose nodes the
  fixture's node-type index does not name.** A pristine store has eighteen of
  them, and the phase creates a node under each of `/content/interop` (the
  control), `/oak:index/lucene/indexRules` and
  `/jcr:system/rep:permissionStore` on a copy, commits through Oak's own
  index update, and asserts all three entries appear. They do — so the
  absences are about which commit wrote those nodes, not about coverage, and
  `froe index check` is right to report a missing entry as an observation
  rather than a verdict. `docs/analysis/index-property-storage.md` §13
  invariant 7 carries the reasoning; this phase is what keeps it honest.

The phase asserts it runs before `commit`, and that the store's file snapshot
is byte-identical afterwards — the first read-only phase to assert the latter.

### property_reindex

froe's offline rebuild held against the strongest oracle available: Oak
rebuilding the same definitions over the same bytes.

**Both rebuilds must index the very same store.** A booted Sling writes
content of its own before any request arrives — discovery, job and
distribution nodes under `/var`, each keyed by that instance's fresh
identifier — so a rebuild on an un-booted copy could never match a rebuild
on a booted one. The phase therefore boots Sling on a copy of the fixture,
flags every rebuildable definition through the POST servlet, waits for each
`reindex` flag to clear *and* its `reindexCount` to advance, stops Sling,
extracts **that** store, and gives froe a copy of it.

* **The definitions are discovered, not listed.** Every direct child of
  `/oak:index` whose modelled type is `property`, `reference` or `counter`.
  A hardcoded set would quietly stop covering a definition the fixture
  gained; the fixture currently yields 23.

* **The bookkeeping is put back before froe runs.** `definition_edits.rs`
  re-flags each definition and sets `reindexCount` to Oak's value *minus
  one*, so froe's own single increment lands back on exactly Oak's value —
  whatever attempt produced it. Everything else, hidden children included,
  is re-attached by record identity.

* **The counter is checked canonical first.** A lane cycle running between
  Oak's rebuild and the stop maintains the counter incrementally, and Oak's
  editor removes a `:cnt` that reached zero without removing the node it sat
  on. That leaves a mirror node no rebuild produces. Plan 0006's counter
  reader reports exactly those nodes, so the phase repeats the
  boot-flag-wait-stop cycle up to three times and then **fails naming the
  condition** — it never compares against a store it knows is not canonical,
  and never skips the comparison.

* **The comparison.** For every definition, `froe digest` restricted to
  `/oak:index/<name>` must be identical between the extracted store and
  froe's copy, under `--exclude-property-prefix :count_`. The approximate
  counters Oak's mirror strategy keeps are seeded from a random number, so
  two rebuilds of the same content disagree on them by construction; that is
  a recorded deviation, not a difference the phase can assert away.
  `reindexCount` *is* compared, and so is the presence or absence of
  `:index`, `:references` and `:weakreferences`, since Oak creates the
  latter two and the counter's data node lazily.

* **Nothing outside the definitions moved**, proven by the full digest
  before and after froe's run with `/oak:index` as the only declared scope,
  and `froe check` passes at the new head.

* **The query probe.** The Sling GET servlet in this image ships no query
  servlet, so the phase installs a JSP that runs a JCR-SQL2 statement and
  prints one path per row, or the `plan` column for an `EXPLAIN`. It is
  installed **before anything is flagged**, so Oak's rebuild and froe's
  rebuild index it symmetrically and the comparison step only reads. Oak's
  own answers are collected from the session that did the rebuild rather
  than from a second boot of the extracted copy.

* **Oak must accept froe's index rather than rebuild it.** Booting Sling on
  froe's store asserts `Reindexing will be performed` absent from the log.
  Without that, every row compared afterwards would be Oak's own rebuild
  answering itself.

**What this phase does not prove.** Query semantics beyond the sampled
statements: the samples cover one shape per storage strategy, not the query
language. Cost estimation is explicitly excluded — a property index's plan
text prints an `estimatedCost:` the mirror strategy derives from the
randomized `:count_*` counters, and index selection between competing mirror
indexes can differ for the same reason, so the plans are compared with
every counter-derived number replaced by a placeholder.

The one sampled plan is compared **symmetrically**, and only against the
draw: `ApproximateCounter` records a count on two random gates, so a
small index is left unpriced often enough to see, froe's rebuild and
Oak's own alike, and the plan is then a traversal over an index that is
perfectly good. A difference is accepted when either side is the
traversal and refused when neither is.

What catches the defect the comparison was added for — froe's first
reindex wrote **no** counters at all, and Oak planned `traverse allNodes`
over its store where it planned `property uuid` over its own — is a
separate assertion that no draw can produce: the rebuilt store carries at
least one `:count_*` property. The plan comparison was asked to prove
that for one release and could not; a CI run failed on froe's side of the
coin toss over a store whose counters were there. And it proves nothing about Lucene,
which `froe index reindex` refuses by name until plan 0010.

### lucene_dump

The strongest question a read-only transport can be asked: **are the bytes
froe reads out of `:data` the bytes Oak reads out of `:data`?** Not "the
files froe wrote are a coherent Lucene index", which a self-consistent
mistake satisfies — the same store, the same definition, two independent
readers, byte for byte.

* **The definitions are discovered, not listed.** Every direct child of
  `/oak:index` whose modelled type is `lucene` and which carries a `:data`
  child, so the comparison is per definition directory rather than one
  against one; plan 0010 adds a second.

* **Oak's own `LuceneIndexDumper`** writes the reference directory, out of
  the very same store, into a directory of its own per definition.

* **Every file is compared by name and by bytes.** A difference names the
  file, both lengths and the first differing byte — never the contents,
  which run to megabytes. `index-details.txt` is compared as a *parsed*
  Java properties file rather than line by line, because both sides write
  it through `java.util.Properties`, whose `saveConvert` escapes `:` in
  values as well as keys: comparing the raw lines would compare an encoding
  and would pass silently if one side stopped escaping.

* **Lucene's own `CheckIndex`** runs over froe's output — the level-2
  verdict froe cannot produce, and the only thing that can say a directory
  is a real Lucene index.

* **Oak's own `IndexConsistencyChecker` at its full level** runs over the
  store, through the new `Consistency` judge class. The phase asserts that
  the index-check status was **reached** and clean, not merely that some
  verdict was printed: the checker runs Lucene's own checker only once the
  directory's content came out consistent, so a blob failure would
  otherwise read as a quiet pass at the level that matters.

* **The document count is agreed twice.** Oak's `numdocs` over froe's dump
  must equal the count froe computes from `segments_N` and each `.si`.
  `:status/indexedNodes` is deliberately not used: it is a per-cycle
  counter, not a document count.

* **A negative control.** One byte is flipped in a copy of froe's dump and
  the same comparison must refuse it, naming the file and the offset — a
  comparison that passed over everything would pass over this phase too.

* **The store snapshot is byte-identical afterwards.** The dump is
  read-only and this is what holds it to that.

**What it found on its first real run.** froe's dump was byte-identical
and both Oak oracles were clean, and froe's *own* structural check called
the same index incoherent: it reported `_0_1.del` as a file no segment
names. A deletions file is derived from the segment's deletion generation
and written down nowhere, so a reader collecting only the commit file's
strings misses it. Since the import refuses an incoherent directory before
copying a byte, `froe index import` would have refused every real Oak index
whose segments carry deletions. Fixed in
`docs/analysis/index-lucene-storage.md` §8.8 and `segments.rs`.

### lucene_import

Both directions the import exists for, against the real fixture.

**The round trip.** froe dumps `/oak:index/lucene`, the definition's hidden
children are removed — the state a lost index actually leaves — and froe
imports the dump back. The definition must render as it did, and nine files
must read back out of the store byte-identical to the files on disk.

Three differences are legitimate, and the phase says why rather than
excluding them quietly:

* `dirListing` is compared as a **set**. Oak stores it in a concurrent hash
  set's iteration order and reads it back as a set; froe writes it in name
  order, a deviation recorded at the writer.
* `reindexCount` is normalized in the digest and asserted exactly on its
  own, since advancing it is the point.
* `jcr:data` is compared **by length in the digest and by bytes through the
  reader**. A stored blob is the file followed by sixteen fresh `uniqueKey`
  bytes, so two honest imports of one file hash differently — which the
  task's own list of what is fresh by design does not mention.

`:status` is asserted separately: the fresh `uid` present, and
`indexedNodes`, `lastUpdated` and `reindexCompletionTimestamp` absent. That
is the recorded departure from oak-run's importer, which has no post-import
state to copy them from.

**The out-of-band build.** The judge's `OutOfBandBuild` reproduces
`IndexerSupport`'s sequence from the classes the image ships, since
`oak-run-commons` is not one of them: an in-memory copy of the **lane
checkpoint's** state — the only state an asynchronous definition's lane can
resume from — the lane switch and `reindex` flag, Oak's own cycle driven as
a diff under the visible-editor filter, the lanes switched back, and then
the artefact: Oak's own dumper for the index directory and its
`index-details.txt`, Oak's own `JsonSerializer` under the printer's
out-of-band filter for `index-definitions.json`.

*One recorded departure.* oak-run hands the editor provider a filesystem
directory factory; this lets the provider write into the copy's `:data` as
it ordinarily would and then runs Oak's own dumper over that copy. The file
bytes are the same — `lucene_dump` is what says so — and it makes
`index-details.txt` Oak's own output rather than the judge's guess at its
format.

The definition is flagged `corrupt` on the copy's head before the build,
because that is the standard reason to reach for one and it is what makes
the drift comparison's tolerance load-bearing: the judge builds from the
lane checkpoint's state, which predates the flag, so its definitions file
lacks `corrupt` regardless — an acceptance a symmetric ignore set could not
express. After the import, `reindexCount` is **two** above the original,
exactly as oak-run's own import leaves it, the flag is cleared, and the
lane's checkpoint is still there.

**Oak consumption.** A real Oak boots on each imported store, logs no
reindex and no index failure, and answers a fulltext query over the five
interop pages with the same rows the pristine store answers, `EXPLAIN`
naming `lucene:lucene` on both sides. The pristine store is asserted to
answer five rows through that index first: a comparison against an empty
answer would pass on a store whose index answers nothing.

**Refusals**, each leaving the store byte-identical: a checkpoint the store
does not hold, a checkpoint that resolves but is not the attachment state,
a drifting definitions file, and a definition made synchronous.

**What it found.** The state rule — the precondition the whole import rests
on — had no refusal that reached it. A checkpoint the store does not hold
is refused by *resolution*, before the rule is evaluated. The phase now
takes a checkpoint at the head, which resolves and is not the lane's,
because every lane cycle commits after taking its checkpoint. With the rule
neutralized the phase fails; before this it passed.

### lucene_writer_conformance

The plan's oracle for froe's own Lucene writer, and the one phase that
touches the fixture not at all: it reads a committed corpus, writes it
twice, and compares.

`crates/froe/tests/fixtures/lucene-writer-corpus.jsonl` describes 8,312
documents in the writer's own model — already tokenized, already typed —
chosen to reach the places a writer goes wrong. 8,300 of them carry one
term, which puts it three skip levels deep under the skip multiplier of 8
and the interval of 128, and their 8,300 distinct terms floor the
block-tree dictionary many times over. Beside the bulk are every index
option, norms and omitted norms, each doc-value type, stored strings,
binaries, integers and a long beyond a double; several analyzed fields of
one name and several boosted ones; a trailing increment and offset the next
value starts past; overlapping tokens; a term above the maximum term
length; a norms-bearing field whose value yields no token; two documents
that disagree on a field's index options and one on `omitNorms`; and an
empty document.

froe writes them with `LuceneIndexWriter`, under a budget small enough that
the runs spill — the path a real rebuild takes. The judge writes the same
corpus with Lucene's own `IndexWriter` under the same `oakCodec`
composition, from a canned token stream that reports the corpus's own end
state, so the composition rules for several fields of one name are proved
against Lucene's inverter rather than assumed.

Then:

* **Lucene's own `CheckIndex` over froe's directory**, first, because a
  dump that matches is worth nothing if the index it came from is
  malformed.
* **Both indexes enumerated and compared line for line** — 157,944 lines
  in the run of 2026-09-15, the figure the phase prints and the corpus
  decides:
  every field with its options, every term with statistics recomputed from
  live postings, every posting with its frequency, positions and offsets,
  every stored value, every doc value beside its has-a-value bitset, every
  norm, the document count, and the commit file's `counter`.
* **Every transducer in task 0903's committed corpus enumerated back** by
  `fst-check`, the judge's one verdict-only class.

#### What enumeration equality proves, and what it does not

It proves the two indexes have **the same contents**: the same fields with
the same options, the same terms with the same postings, the same stored
values, doc values and norms, over the same documents.

It does **not** prove the two indexes are the same bytes, and they are not.
froe writes `PACKED` for every bit width where Lucene's own selection uses
`PACKED_SINGLE_BLOCK` for three of them; froe's transducers carry linear
arcs where Lucene emits a fixed array for a wide node; froe orders a
`TABLE_COMPRESSED` value table ascending where Lucene leaves it in hash
order. Each is a choice the reader dispatches on, recorded in §10.3 of the
codec specification, and each is invisible to the enumeration precisely
because the reader honours what the file says.

Nor does it say anything about **merging or deletions**: both indexes are
one segment written in one commit, which is what froe writes and all it
writes. A segment that Oak later merges, or into which Oak later writes a
deletions file, is Oak's to produce.

### lucene_reindex

The strongest oracle available for a Lucene reindex. Not "the index froe
wrote reads back", which a self-consistent mistake satisfies, but **"Oak
rebuilt these same definitions over these same bytes, and the two indexes
hold the same thing"**.

Both rebuilds must therefore run over the *very same store*, for the reason
`property_reindex`'s header gives: a booted Sling writes content of its own
before any request arrives. The phase boots Sling on a copy of the fixture,
installs the query probe **before** anything is flagged so both sides index
it symmetrically, flags every `lucene` definition, waits for the `async`
lane, stops, extracts *that* store, and gives froe a copy of it with each
definition's bookkeeping put back to what Oak started from — `reindex`
flagged, `reindexCount` one below Oak's value, so froe's single increment
lands on exactly Oak's.

Three things are specific to Lucene.

**The rebuild is asynchronous.** Oak clears `reindex` and advances
`reindexCount` on the lane, not in the commit that flags it. The wait is on
both together, through Sling's GET servlet; the hidden `:status` is
invisible to JCR, so the phase asserts on the extracted store instead that
each definition's `reindexCompletionTimestamp` is not the one the fixture
carried. A definition whose flag cleared without its writer ever closing
would pass the wait and fail there.

**The extracted index must carry no deletion.** A lane cycle between Oak's
rebuild and the stop updates a document through Oak's index writer as a
delete and an add, and the judge's `enumerate` reads live documents only. An
index carrying deletions therefore enumerates a subset of what it holds,
which no from-scratch rebuild produces. The phase reads every `segments_N`
with plan 0008's own segment reader, repeats the boot-flag-wait-stop-extract
cycle while any segment carries one — bounded to three attempts — and fails
naming the condition rather than comparing a weakened pair. The attempt it
passed on goes into `canonical-index-lucene.txt` beside the property phase's
own verdict, and into the run record.

**One difference is declared.** froe extracts no text. Under `--binary-text
marker` it indexes Oak's own `TextExtractionError` where Oak indexed a
binary's extracted text, so the comparison removes that at the **posting
level from both sides before any statistic is derived**: for each affected
document and field the postings, the stored value and the norm go, terms
left with no postings go, and each surviving term's document and total
frequencies are recomputed from what remains. Which documents those are is
derived twice and the two must agree exactly — from froe's own index, as the
documents whose `:fulltext` carries the marker term, and from the store, as
every node the definition includes that carries a binary property the rule
indexes fulltext and a `jcr:mimeType`. An exclusion wider than the binaries
would hide a real difference; one narrower would fail on a difference that
is declared.

Then:

* **Lucene's own `CheckIndex` over froe's rebuild**, first, because an
  enumeration that matches is worth nothing if the index it came from is
  malformed.
* **Both indexes enumerated by the judge and compared**, re-keyed by each
  document's own `:path` — see below.
* **Each definition node compared** beside its index, excluding `:data`, the
  `:status` timestamps and `uid`, and the `:index-definition` clone's
  `reindexCount`. This is the oracle for the `facets` configuration the
  document maker's dimensions persist, the `seed`, the removal of `refresh`
  and `indexImportState`, the `:version`, and `:status`'s own indexed-node
  count.
* **The whole-store delta** confined to `/oak:index`, and `froe check` at the
  new head.
* **Oak booted on froe's store**, logging no repair, no reindex and no index
  failure, answering ten statements with the same rows and the same
  `EXPLAIN` plan it answers from its own rebuild — node-scope fulltext,
  property fulltext, an `ORDER BY` over an ordered doc value, `IS NULL`, a
  facet column, a **multi-valued** facet column, an `ISDESCENDANTNODE` that
  reaches `:ancestors`, a path-restricted property term, a `CONTAINS` over
  a **relative** property definition's own field, and one statement against
  the repository-wide definition. Every plan must name the index the
  statement was written for: equal rows from a traversal would be equal rows
  proving nothing. Every statement meant for the variant is restricted at or
  below its `queryPaths`, because Oak's own fulltext planner offers an index
  carrying them only to a query restricted that way.
* **The suggester handed back.** froe removes `:suggest-data` and builds no
  suggester dictionary, which is safe only because Oak builds one again. So
  the definition carries `useInSuggest`, the definition comparison above
  *declares* that node rather than excluding it silently — asserting that
  Oak's rebuild has one and froe's has none — and the booted Oak then gets
  one node written under the definition, waited for through a query until
  the lane has run a cycle over it. The store is extracted from that boot
  and `:suggest-data` must be back.
* **The reset**: froe removes the variant's lane checkpoint, resets the
  definition under `--from-head` — which for a Lucene definition is what
  froe does instead of rebuilding, leaving `reindex` raised, every other
  visible property untouched and no hidden child behind — and Oak's own next
  cycle logs `Failed to retrieve previously indexed checkpoint`, advances
  `reindexCount` by exactly one and rebuilds the definition from scratch.
  froe then rebuilds from *that cycle's* own lane checkpoint and the two
  enumerate identically. The comparison is against the second boot's
  checkpoint rather than the first oracle because the second boot's
  instance-keyed content makes the first non-deterministic.

#### Why the comparison is re-keyed by `:path`

`Corpus.enumerate` keys every line by Lucene's **document number**, which is
an artefact of the order documents were added in. The two sides do not share
that order: Oak's own editor walks a node's children in the order its
`MapRecord` yields them, which is by the hash of each name, and froe's
rebuild walks them sorted by name. Neither order is part of the format —
nothing in the index records it, and no query can observe it — so comparing
the enumerations line for line would compare the walk rather than the index.

What *is* comparable is every document identified by its own `:path`, which
Oak's document maker stores on every document it makes. Both enumerations
are re-keyed by it and rendered back in one canonical order, so the equality
is an equality of contents: the same fields with the same options, the same
terms with the same statistics recomputed from live postings, the same
postings with the same frequencies, positions and offsets, the same stored
values, doc values and norms, over the same documents. The commit file's
`counter` is excluded for the same kind of reason: it counts the flushes and
merges that produced the index, not what is in it, and Oak rebuilds through
its own merge policy where froe writes one segment in one commit.

#### The negative control

A comparison that passed over everything would pass over this phase too. The
phase therefore perturbs a copy of froe's own rendered enumeration exactly as
a wrong position increment in the word delimiter would — one `:fulltext`
posting's first position advanced by one — and requires the same comparison
to refuse it, naming `:fulltext`. The perturbation is of the enumeration
rather than of the analyzer because the analyzer is compiled into the binary
under test: a run that rebuilt froe with a defect would be comparing a
different binary from the one that produced the index above. The defect
itself is neutralized against the analysis module's own hand-computed
vectors, which is where a wrong increment is caught first; this proves the
*comparison* would not let one through.

### commit

froe adds nodes with typed properties (String, Long, Boolean) to the
content tree via the library's commit API
(`rewrite_node_with_child_edits`), writing a new subtree under
`/content/interop/froe-written/node`. Then Sling boots against the
modified store and verifies Oak reads the froe-written nodes back
correctly — the same JSON Sling would serve for any other node.

Two assertions bound what else the commit may have done. Oak must log none
of its own repair messages, so reading the node back cannot be satisfied by a
store Oak reconstructed on the way to serving it. And the content digest
before and after the commit must differ **only by added paths under
`/content/interop/froe-written`**: a node record rewritten on the path from
the root that lost a property, or a value re-rendered in passing, is
invisible to every other assertion in this phase.

This is the core interop claim: froe writes content that Oak reads.
If this fails, there is no point testing checkpoint, compact,
backup, or recover — the writer cannot produce content Oak reads.

Depends on `read` (to verify).

### checkpoint

froe creates a checkpoint with a 1-second lifetime against the Oak
store. A metadata-only write-path operation (logical head update) that
exercises the writer's checkpoint machinery against a store froe didn't
write. If this fails, compact's expired-checkpoint handling and its
checkpoint preservation can't be trusted.

Depends on `commit` (the writer can already produce content Oak reads).

### compact

froe compacts a copy of the store. The journal is truncated to the head
first, so the churned subtrees' segments are true orphans (no journal
history protects them). Compaction deep-copies only reachable records,
dropping the orphans. Sling boots against the compacted store.

Two assertions carry the "content preserved" claim, and both are byte-level:

- The **content digest** taken before the run must equal the one taken after,
  outside the checkpoint subtrees compaction is allowed to retire. That covers
  every node, property name, type, arity, value and binary in the head and in
  every surviving checkpoint.
- The uploaded **binary is fetched back from Oak and compared byte for byte**
  against the file that was uploaded. A substring check could not carry this:
  the fixture's binary is one sentence repeated 16384 times, so matching
  `Lorem ipsum` passes on a stream truncated after the first block, missing
  blocks in the middle, or with blocks reordered — exactly what a block-list
  bug produces.

Depends on `read` (to verify the compacted store) and `commit` (to trust
the writer).

### compact_tail

The same run with `--tail`. Tail compaction retains the shared full
generation, so it reclaims strictly less than a full run — and that is the
point: a store Oak must still boot against, produced by the mode an operator
reaches for when a full run is too expensive. The same digest and binary
assertions as `compact` carry the content claim.

Because the retained generation is exactly what a tail run may not reclaim,
`froe compact --tail` never purges orphaned version histories and its report
says so.

### checkpoint_removal

`remove` by name, `remove-unreferenced` and `remove-all`, in that order, each
followed by a boot. The load-bearing assertion is the middle one: the
checkpoint Oak's own asynchronous indexer resumes from **survives**
`remove-unreferenced`. Removing it would not fail anything immediately — Oak
logs a warning and reindexes from the missing state — which is exactly why it
has to be asserted here rather than left to a later phase to notice.

### cleanup

A multi-generational store built by two compactions, carrying an expired
checkpoint, a stale archive left by an interrupted run, a truncated journal
and corrupt journal lines. One `froe compact` run resolves all of them, and
Oak boots the result and serves the baseline tree.

### compact — reclamation

froe compact against a multi-generational store with:

- **A wholly dead archive**: 2000 nodes written directly at generation zero
  and linked to no head, two full generations behind the compacted head. Every
  entry in the archive is reclaimable, so the sweep unlinks the whole file.
- **A partially dead archive**, which needs no fixture at all. The Oak store
  carries a binary large enough to live in bulk segments, and compaction
  references bulk segments where they lie rather than copying them. So the
  archive holding them survives the first compaction while its data segments
  die — some entries reclaimable, some not, which is exactly the disposition
  that forces a rewrite to the next generation letter with a survivor subset
  and reconstructed `.gph`, `.brf` and `.idx` trailers. Oak then reads that
  archive. This is the shape a production store actually has and the one Oak's
  25% savings heuristic declines forever; the phase asserts the source archive
  is gone, its successor letter holds the survivors, and the reported rewrite
  count is at least one.

  The assertion sits on the *first* compaction deliberately. By the second,
  the surviving archives hold nothing but referenced bulk, which is wholly
  live and has nothing left to reclaim — so a rewrite is unreachable there,
  and asserting it would be asserting something the format cannot produce.
- **1 stale archive**: a copied newer-letter duplicate of the active
  archive — the on-disk condition Oak's own compaction leaves behind.
- **1 expired checkpoint**: created by froe with a 1-second lifetime.
- **2 corrupt journal lines**: a no-space line (ParserSkippedNoSpace)
  and an invalid-record-identifier line (InvalidRecordIdentifier).

froe compact removes every one of these conditions in a single run — there is
no second command — and Sling boots against the result.

Depends on `compact` (to build the gen 0→1→2 fixture).

### journal_retention

Journal retirement is the only thing froe does that makes repository bytes
unreachable *by policy* rather than by Oak's generation predicate: it removes
journal lines whose revisions still resolve, and the segments behind them are
swept in the same run. It is not opt-in — every `froe compact` does it — which
is exactly why it needs Oak evidence rather than froe's own agreement with
itself.

That is precisely the case a froe-to-froe round trip cannot answer: froe
agreeing with its own reachability rules says nothing about whether Oak can
open what is left. So this phase runs a plain `froe compact` on a copy of the
Oak fixture, asserts the plan names the revisions it retires and that exactly
one line survives on disk beside a numbered backup, and then boots Sling
against the result — which must serve the exact baseline tree from the single
revision froe kept.

Depends on `generate` only; it uses the Oak-written journal directly, because
Oak's own history is the thing being retired.

### compact_convergence

The convergence gate's promise is double-sided: a run after a completed full
compaction must prove the store already fully compacted and mutate nothing —
byte-identical files, a stated no-op — and it must never hide real work
behind that verdict. The phase runs `froe compact --yes` twice on a copy of
the fixture, asserts the first run's forward-looking summary line, the
second run's verdict and untouched bytes, and then boots Sling against the
twice-run store, because a gate that quietly broke the store while deciding
to do nothing would otherwise pass.

### version_history_purge

The purge is maintenance's one *content* mutation, so its Oak evidence has
to cover both directions: Oak produced the garbage, and Oak accepts its
removal. Sling creates two `mix:versionable` nodes, checks both in — Oak
writing the version histories — and deletes one, orphaning its history the
way real content deletion does. froe's plan then reports exactly that one
orphan; the purge runs under the digest discipline (before and after
rendered with one declared exclusion — the orphan's intermediate hash chain
from the level where it diverges from the live history's, because the purge
legitimately prunes the emptied intermediates too — compared line for
line); the purged history is gone from the head while the live one
survives; and Sling boots the purged store, serves the baseline content,
and checks the surviving versionable out and in again — a fresh version
appended to the surviving history being the strongest available proof that
what survived is a working history rather than a leftover shape.

### repair

Loads the fixture store into a volume, boots Oak, writes content so Oak holds
an archive open, then kills the JVM with `SIGKILL` and asserts the container
exited 137. The extracted store has exactly one archive without an index —
Oak writes the `.gph`, `.brf` and index trailers only on close, so this is the
authentic artifact of a crash rather than a simulated one.

Then, read-only until the repair runs, because every froe *write* command
rebuilds a missing index on open and would heal the fixture: `froe archives`
confirms the damage, and `froe compact --dry-run
--skip-repairing-archive-indexes` confirms the refusal points at
authorizing the repair. The repair itself runs through `froe compact
--yes` — the rebuild is part of the confirmed default run — and the
original is asserted present under its `.bak` name.

The assertion that makes this phase worth having is the last one: Oak boots
against the rebuilt archive, serves the byte-identical baseline tree, and logs
none of its own repair messages — so it consumed froe's index rather than
reconstructing one. `CONTRIBUTING.md` is explicit that a froe-to-froe round
trip is not a substitute for that.

### backup

froe backup copies the store head into a fresh target directory. froe
restore copies that backup into another store. Sling boots against the
restored store and content is preserved.

Both are held to the strongest statement the digest makes available: the
backup must render **identically to its source**, and the restored store
identically to the backup. Identity — record, segment and stable
identifiers — is excluded from the rendering, so everything else has to
agree exactly.

That assertion exists because of what it caught. Copying a binary shares
bulk-segment blocks by reference rather than copying them, which is correct
for compaction — within one store, a reference from the new generation is
exactly what keeps a bulk segment alive — and wrong for a backup, where the
target is a different directory and the reference resolves to nothing.
`froe backup` used the same copy, so it produced a target holding the whole
content tree and none of the binary content: **9.8 MB from a 67 MB store**.

Nothing caught it for a long time, and the reasons are worth recording,
because they are the general case:

- the backup **booted in Oak** and served its content tree;
- it matched the **Sling-side fingerprint**, which reads two string
  properties over one subtree and no binaries at all;
- it passed **`froe check`**, which resolved the binary records without
  reading them — `--binaries` is what fails, and the phase did not pass it;
- and no unit test reached it, because the shape requires blocks in a *bulk*
  segment, which only appears for binaries over 256 KiB.

The regression is
`a_backup_carries_binary_content_that_lived_in_a_bulk_segment`, which reads
the binary back out of the target alone — opening the source anywhere in the
assertion would let the missing blocks resolve through it and hide the
defect.

Depends on `read` and `commit`.

### recover

Deletes `journal.log`, then runs `froe recover-journal` to rebuild it
from the segments. The recovered journal resolves, `froe check` passes,
and Sling boots against the recovered store.

Depends on `read`.

## What the two reclamation fixtures prove

A fresh Oak store has only generation 0. froe's segment reclamation uses
Oak's FULL-generation predicate, which reclaims segments whose `full_generation`
is 2+ behind the head, so a single-generation store has nothing old enough to
reclaim. The phase therefore compacts twice (gen 0→1→2) before building
anything.

The two fixtures exist because the sweep has two dispositions, and one of them
was never exercised against Oak:

- `write_orphan_nodes` writes 2000 unreferenced generation-zero nodes into a
  new archive. *Every* entry is reclaimable, so `plan_archive_sweep` takes the
  whole-file removal branch. This proves reclamation happens; it cannot prove
  anything about rewriting, because the rewrite machinery is never reached.
- The **partially dead archive** needs no fixture function at all, and there is
  none: the Oak store already carries a binary large enough to live in bulk
  segments, and compaction references bulk segments where they lie rather than
  copying them. So the archive holding them survives the first compaction while
  its data segments die — some entries reclaimable, some not, which is exactly
  the disposition that forces a rewrite to the next generation letter with a
  survivor subset and reconstructed `.gph`, `.brf` and `.idx` trailers.
  `assert_first_compaction_rewrites_a_partial_archive` asserts the source
  archive is gone and its successor letter holds the survivors. Oak then boots
  against the result, serves the baseline tree, and logs none of its own repair
  messages — so it consumed froe's rebuilt archive rather than reconstructing
  one.

  The assertion sits on the *first* compaction deliberately. By the second, the
  surviving archives hold nothing but referenced bulk, which is wholly live and
  has nothing left to reclaim — so a rewrite is unreachable there, and asserting
  it would be asserting something the format cannot produce.

An earlier version of this phase built its orphans by restoring the
pre-compaction gen-0 archive at a spare archive number. That stopped working
once compaction began sharing bulk segments the way Oak does: the compacted
head still references gen-0's binary blocks, so re-introducing that archive is
a genuine duplicate-segment condition and froe rightly refuses it. Both
current fixtures write fresh unreferenced segments instead, which are
unreachable by construction rather than by an assumption about what compaction
leaves behind.

## CI

`.github/workflows/interop.yml` runs the suite on three occasions, because
they answer different questions:

- **Push, path-filtered** on the library, the command-line crate, the script
  and the workflow — the froe-side axis, where a regression is possible.
  Pinned digest. The filter was the write path alone until the index phases
  arrived; every phase now exercises a reader as well as a writer, and the
  index phases compare froe's readers against Oak's own printers, so a change
  anywhere in either crate can regress the claim.
- **Monthly schedule** against the floating tag — the environment axis, which
  can break with no froe commit at all: a new Oak build in the image, a new
  runner image, a new stable compiler. This is what a timer is actually for;
  a weekly cadence added nothing the push filter did not already cover.
- **Manual dispatch**, for re-verifying deliberately.

A failing run shows on the Actions tab and carries the run-record artifact;
the repository has issues disabled, so there is no failure-reporting job
that would file one.

`.github/workflows/release.yml` runs the suite as a release gate: the release
notes assert that maintenance is verified against a named Oak build, so the
publishing job depends on the suite passing at the tagged commit rather than
on a run from some earlier day.

## Implementation

The tests live in `crates/froe-cli/tests/interop/`, behind the
`interop` feature flag. The shell script
`scripts/interop-fixture.sh` is a thin wrapper around `cargo test`.

Podman orchestration uses `std::process::Command` to shell out to
`podman run`, `podman volume`, and `podman stop/rm` — the same commands
the previous shell script used, but from Rust with structured
assertions. The Sling image is `docker.io/apache/sling:14` (Apache-2.0);
it boots Oak with TarMK by default.

The froe binary is resolved via `env!("CARGO_BIN_EXE_froe")`, so the
tests always run against the freshly built binary, not whatever is on
`$PATH`.