# Oak interop test suite

End-to-end tests verifying that froe reads stores written by Apache
Jackrabbit Oak, writes stores that Oak reads, and performs maintenance
operations that leave the store in a state Oak boots against cleanly.

The suite uses an Apache Sling image (Apache-2.0) as a real Oak instance —
no Adobe/AEM license is involved. Sling boots Oak with TarMK by default,
so the store is byte-for-byte what a production Oak repository produces.

The image is **pinned by manifest digest** in
`crates/froe-cli/tests/interop.rs`, because the claim in the README names an
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
$ cargo test -p froe-cli --features interop -- --ignored --nocapture interop::compact

# Direct cargo invocation — `interop_full` is the whole chain, and naming it
# is required: an unfiltered `--ignored` run collides with itself as above.
$ cargo test -p froe-cli --features interop -- --ignored --test-threads=1 interop_full

# A single phase via cargo:
$ cargo test -p froe-cli --features interop -- --ignored interop::read
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
journal_retention
   │  a plain froe compact retires every revision but the head it
   │  wrote and sweeps the segments behind them; Oak boots the result
   │  and serves the baseline tree from the one revision kept
   │  If this fails: froe's by-policy destruction of reachable history
   │  leaves a store Oak cannot open.
   ▼
repair
   │  Oak's own JVM is killed with SIGKILL while it holds an archive
   │  open; froe compact --repair-archive-indexes rebuilds the index and
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

`--retain-journal-revisions` is the only froe operation that makes repository
bytes unreachable *by policy* rather than by Oak's generation predicate. It
removes journal lines whose revisions still resolve, and the segments behind
them are swept in the same run.

That is precisely the case a froe-to-froe round trip cannot answer: froe
agreeing with its own reachability rules says nothing about whether Oak can
open what is left. So this phase bounds the journal to one revision on a copy
of the Oak fixture, asserts the plan names the revisions it retires and that
exactly one line survives on disk beside a numbered backup, and then boots
Sling against the result — which must serve the exact baseline tree from the
single revision froe kept.

Depends on `generate` only; it uses the Oak-written journal directly, because
Oak's own history is the thing being retired.

### repair

Loads the fixture store into a volume, boots Oak, writes content so Oak holds
an archive open, then kills the JVM with `SIGKILL` and asserts the container
exited 137. The extracted store has exactly one archive without an index —
Oak writes the `.gph`, `.brf` and index trailers only on close, so this is the
authentic artifact of a crash rather than a simulated one.

Then, read-only until the repair runs, because every froe *write* command
rebuilds a missing index on open and would heal the fixture: `froe archives`
confirms the damage, and `froe compact --dry-run` without the task confirms
the refusal names `--repair-archive-indexes`. The repair itself runs through
`froe compact --yes` with that task selected, and the original is asserted
present under its `.bak` name.

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

- **Push, path-filtered** on the write path, the suite, and the workflow —
  the froe-side axis, where a regression is possible. Pinned digest.
- **Monthly schedule** against the floating tag — the environment axis, which
  can break with no froe commit at all: a new Oak build in the image, a new
  runner image, a new stable compiler. This is what a timer is actually for;
  a weekly cadence added nothing the push filter did not already cover.
- **Manual dispatch**, for re-verifying deliberately.

A failing run opens or comments on an `interop`-labelled issue, because a
scheduled failure produces no pull request and would otherwise sit unnoticed.

`.github/workflows/release.yml` runs the suite as a release gate: the release
notes assert that maintenance is verified against a named Oak build, so the
publishing job depends on the suite passing at the tagged commit rather than
on a run from some earlier day.

## Implementation

The tests live in `crates/froe-cli/tests/interop.rs`, behind the
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