# Compaction: froe's one maintenance command

`froe compact` is offline maintenance for an existing segment-tar repository,
and it is the only maintenance command froe has. One run deep-copies the head
and every retained checkpoint into a fresh garbage-collection generation,
retires `journal.log` to that single revision, and reclaims everything the new
head does not reach — orphan segments, whole archives, safe archive leftovers,
expired checkpoints, and staging files it can prove redundant. It never creates
or bootstraps a repository.

Its safety case — scope, mutation ordering, guards, fault coverage, resource
bounds, and known gaps — is
[`plans/0004-merged-maintenance-command/ARCHITECTURE.md`](plans/0004-merged-maintenance-command/ARCHITECTURE.md).

Compaction and reclamation are one command because they are one decision. A
sweep alone works at *segment* granularity, so a segment holding one live
record is wholly live however much dead content sits beside it; only a rewrite
recovers that. And only a rewrite lets a run retain a single generation, which
is what makes the sweep that follows it complete rather than incidental.
Splitting them left froe able to identify garbage it could not remove — see
[Why froe rewrites archives Oak would leave
alone](#why-froe-rewrites-archives-oak-would-leave-alone).

Exactly one generation is retained. That is Oak's own offline setting:
`SegmentGCOptions.setOffline()` sets `retainedGenerations = 1`. At that value
head safety no longer follows from the generation predicate, so froe proves it
per run — the reclaim reference invariant re-evaluates the run's exact reclaim
rule over the head's transitive segment closure and refuses before any
mutation.

Journal history is retired, not preserved: after a successful run
`journal.log` holds exactly one line, naming the compacted head. The removed
history is not recoverable from the store afterwards; `journal.log` is copied
to a numbered `.bak` first.

> **Run compaction only while Oak/AEM is stopped, and keep a recoverable copy of
> important repositories.** Compaction is covered by format-level and failure-path
> tests and by the interoperability suite ([`interop.md`](interop.md)), which
> applies it to a store written by Apache Jackrabbit Oak `oak-segment-tar`
> 1.90.0 — reclaiming orphan segments, a stale archive, an expired checkpoint
> and corrupt journal lines — and then boots Oak against the result, which
> serves the same content tree it served before and logs none of its own repair
> messages. The run is also held to a canonical content digest taken before and
> after it: every node, property name, type, arity, value and binary checksum
> must be unchanged outside the checkpoints the run is meant to retire. Not yet verified against a live instance: `store.version=1`
> stores, external blob stores, and Adobe AEM itself, which ships its own Oak
> build.

Apply is Unix-only and must run as the operating-system user that owns
`journal.log`—normally the Oak/AEM service account, not through `sudo`. This is
checked before `repo.lock` or any replacement file can be created. Rewritten
files preserve the source uid, gid, and permission bits, and the run refuses
before mutation if the repository filesystem cannot strictly fsync a directory.
Read-only `--dry-run` remains available without those apply preconditions.
Platform ACLs and extended attributes are not cloned; before publication,
replacement TARs, the journal, its recovery backup, and an upgraded manifest
are reopened through their path with the service identity and required access.
The run fails before the swap if copied POSIX metadata alone would make a
replacement unusable.

Owning `journal.log` is necessary but does not make every mixed-ownership plan
safe. Preview checks the current process's uid, effective gid, supplementary
groups, the repository directory's gid and setgid behavior, and the uid/gid/mode
that each planned replacement must preserve. When the directory is not setgid,
the analysis conservatively models both POSIX-permitted creation outcomes: the
process effective gid and the parent-directory gid. A known credential mismatch
is printed as an apply-preflight warning in the read-only preview, including
`--dry-run`; dry-run stays read-only.

Apply builds its one plan under the held lock, repeats that check there,
and refuses a known mismatch before the first planned repository-content
mutation. The metadata sources in scope are the manifest when it will
be upgraded, the journal when it will be rewritten, each source TAR it will
rewrite, and the TARs that could become the newest metadata template after a
leading run of planned whole-archive removals during a checkpoint head move. A
rewrite source represents its successor because the replacement copies that
source's metadata. This second refusal can leave a newly created `repo.lock`,
but no planned content mutation. Files selected only for unlink do not need
their uid/gid/mode copied and are outside this second gate.

This read-only credential analysis cannot prove that a particular filesystem
will accept the later `fchown` and `fchmod` calls. An unexpected filesystem or
mount-policy rejection can therefore still stop a rewrite during apply,
possibly after an earlier planned action became durable; the failing source is
not replaced. Audit the relevant files for consistent service-account
ownership and correct accidental `sudo`-created rewrite sources before
applying a plan.

Start with a preview:

```console
$ froe compact /path/to/segmentstore --dry-run
```

Planning a large store takes minutes: it verifies the whole head tree,
replays the journal, and traces the reachable segment closure before it can
say anything. The run reports each of those steps on standard error while it
works, so a long wait is legible rather than silent; `--silent` turns the
reports off without hiding the plan, the warnings, or the confirmation
prompt. See [`cli-output.md`](cli-output.md).

Dry-run is a strict read-only operation: it writes no bytes, creates no files,
and does not create or acquire `repo.lock`. It validates the repository and
prints the exact actions, warnings, verified head, and the run's byte
estimates — what the sweep reclaims (conservative) and what the copy writes
into the fresh generation, so a run that trades a large rewrite for a small
reclaim says so before it is confirmed. Journal removals include the one-based physical
line, structured reason, optional record identifier, and an exact bounded
byte-string prefix; non-ASCII and control bytes are escaped rather than decoded
or sent to the terminal.

## The convergence gate

A selected copy is dropped from the plan when the planner proves it would
only replace a generation with an identical one: every data segment the head
reaches already carries the head's own compacted triple, no checkpoint is
selected for omission, no orphaned-version-history purge is selected — an
omission is content work, however compact the store already is — and the
journal already holds the single line a completed compaction leaves. The plan says so —
`the head is already fully compacted; the selected copy … was dropped` — and
the run performs the standalone sweep instead, so garbage never hides behind
the gate; a run with nothing at all left prints
`the store is already fully compacted; nothing to do` and mutates nothing. A
completed full compaction states the consequence in its summary:
`the store is now fully compacted; a repeat run will report nothing to do` —
or, when recovery backups remain on disk (the run's own journal backup
included), `… a repeat run has only recovery backups left to remove`,
because removing them is exactly what the next default run would do.
`--always-copy` overrides the gate for the operator who wants a rewrite
anyway; it is never needed for space.

The verdict is a triple-equality test against the head segment's own
generation, never an ordering or a store-wide maximum: old Java-written
archives can carry version-1-index synthesized full generations far ahead of
the real ones, generation arithmetic wraps, and killed-run residue stamped
ahead of the head sits outside the head's closure entirely — none of them
can confuse an equality.

## Orphaned version histories

Every plan reports the store's orphaned version histories: the
`nt:versionHistory` subtrees under `/jcr:system/jcr:versionStorage` whose
`jcr:versionableUuid` matches no live `jcr:uuid` outside version storage.
Structurally they are reachable content, so no generation sweep may touch
them; semantically they are garbage the moment their versionable is gone,
and on a long-lived store they pin the version payloads and inline binaries
of everything ever deleted. The report states what they hold — nodes,
inline binary bytes, external references, and how many histories carry
identifiers that do not parse and so were never classified — and what a
purge releases: the bulk segments only the purged histories reference, and
their share of the copy's node records (an estimate scaled from the head's
average; the plan's predicted copy cost is reduced by exactly this figure,
and the copy realizes it). When a purge is selected, both figures describe
that selection — a history the run keeps is not a saving the run delivers.
The bulk figure is an upper bound, printed as `up to`: a record shared
between a purged history and one the store keeps — Oak's writer dedups
identical frozen subtrees into shared records — keeps its blocks alive
without the plan-time walk being able to see it, and a retained
checkpoint's snapshot can pin blocks until it expires (the line names the
checkpoint count when that applies). The sweep that follows the copy frees
exactly what is actually unreferenced.

Removal is part of every full compaction: the run's one *content*
mutation, so it is gated by its own yes/no question — an interactive run
asks up front, `--yes` answers it, and
`--skip-purging-orphaned-version-histories` keeps the histories with the
reason stated in the report. The selected purge is listed as its own plan
action with the same irreversibility warning the journal retirement
carries, and confirmed with everything else. The copy simply declines to
enter the confirmed subtrees — the mechanism checkpoint retirement already
uses — and the reclaim pass that follows turns the omission into reclaimed
space. Deliberate boundaries:

* **Checkpoint snapshots keep everything they froze.** A purged history
  still resolves through a retained checkpoint, whose own copy of version
  storage is left intact; its storage returns when the checkpoint expires,
  and the plan says so. Internally the copy memoizes the affected ancestor
  records per scope, so a subtree shared between the head and a checkpoint
  can never leak one scope's shape into the other, whichever the walk
  reaches first.
* **Histories that freeze `nt:configuration` versionables are kept**, with
  a warning: configuration versioning is not this purge's business.
* **Advisory reference protection.** REFERENCE- or WEAKREFERENCE-typed
  values outside version storage naming a record inside a candidate demote
  it, with a warning — Oak does not enforce referential integrity, so this
  is best-effort protection for applications that store version references.
* **An optional age bound** (`--purged-history-minimum-age-days`) keeps
  histories whose newest version is younger than the bound, or carries no
  parsable creation date — the guard for content deleted moments ago and
  about to be restored from a package, whose recreated versionable would
  otherwise have re-attached the history by identifier. Passing the bound
  selects the purge without asking.
* **A full compaction is required**: tail reclamation retains the shared
  full generation, which could leave every released record on disk after a
  run that promised to remove them, so a `--tail` run never purges and its
  report says so.

The summary restates the purge —
`purged: 31,473 orphaned version histories (524,086 nodes omitted from the copy)` —
and `froe digest --exclude-subtree <path>` renders a digest with named
exclusions, so a before-digest can be compared against an after-digest with
the purge and nothing else excused.

One figure needs its explanation, and gets it in the plan, the summary,
and here. When retained checkpoints keep their snapshots of the purged
histories, the head's rewritten version storage stops sharing records with
those snapshots: every hash-bucket node on the path to a purged history
now exists in a head shape (without the history) and a checkpoint shape
(with it), where before the purge one shared record served both. The copy
therefore can write *more* node records than the pre-purge head reached —
on a real AEM store, a purge of 31,473 histories under 2 retained
checkpoints raised the copied node count from 18,796,595 to 18,816,318 —
and most of the purge's node-record saving is deferred until the
checkpoints expire, which the report's `(mostly deferred …)` clause
states. This is deliberate: rewriting a checkpoint's snapshot would make
an async indexer diff miss the deletions, leaving stale index entries
behind.

## Applying a plan

Without `--dry-run`, the command settles its questions first and then
plans exactly once, under the lock:

1. It surveys the store read-only — do any active archives lack an index,
   and are there recovery backups on disk — and settles the three default
   cleanups: the archive-index repair, the orphaned-version-history purge,
   and the recovery-backup removal. `--yes` answers yes to each; an
   interactive run asks each applicable question; each `--skip-*` flag
   answers no. A run that repairs never also removes backups: the repair
   writes the very `.bak` files a removal could otherwise delete, so their
   removal waits for the next run, and the run says so.
2. It acquires the exclusive Oak-compatible repository lock, runs any
   authorized index repairs, and builds the one plan from disk while
   holding it — so the plan the operator confirms is byte-for-byte the
   plan that applies, and nothing can change the store between the
   evidence and the decision.
3. It prints that plan and asks for confirmation while still holding the
   lock. The store is offline by precondition, so holding the lock through
   the prompt is a strengthening, not a discourtesy: an accidentally started
   Oak cannot open the store while the operator is reading.
4. It re-fingerprints the directory before the first mutation — a
   non-cooperating change during the prompt is refused, never silently
   replanned — applies the plan, verifies the fresh copy in full through the
   open session *before* the head is published or a single archive is
   unlinked, forces durable metadata updates to disk, and reopens the
   repository through fresh mappings for a final health check.

A plan with no mutations reports that result and releases the lock without
applying anything; `--dry-run` remains the way to preview without creating
or taking `repo.lock` at all.

`--yes` answers every question — the per-cleanup ones and the final
confirmation; it does not bypass the lock, fingerprint, validation, copy
verification, or final verification. A run scripted without `--yes` (no
answers arriving on standard input) plans but applies nothing: it is
cancelled at the final confirmation, and the cancellation names the flag
that would have answered.
Lock acquisition fails immediately if Oak/AEM or another writer is using the
repository. As with Oak's own repository lock, safety assumes every other
writer cooperates with the persistent `repo.lock` inode and does not unlink or
replace it behind the lock holder; the run rechecks that inode between mutation
phases, but cannot make an actively hostile process obey an advisory lock.

```console
# Interactive application: one question per applicable cleanup, then the
# plan and its confirmation.
$ froe compact /path/to/segmentstore

# The same checks and application with every question answered yes.
$ froe compact /path/to/segmentstore --yes
```

## What one run does

There is nothing to select. Every run performs the same sequence, and the
flags under [Opt-in behavior](#opt-in-behavior) add to it rather than replace
it:

| Stage | Behavior |
| --- | --- |
| copy | Deep-copies the head and every retained checkpoint into a fresh garbage-collection generation. Bulk segments are re-linked where they lie rather than copied, so binary content does not move. |
| journal | Discards parser-ignored lines, lines with invalid record identifiers, revisions whose head segment is absent, and unreadable *non-current* historical revisions — and then retires the readable ones too, leaving `journal.log` holding the single line naming the head the copy just published. |
| reclaim | Sweeps against the generation the copy created, retaining one, and rewrites or unlinks archives holding segments the new head does not reach. What the format will not let it move is reported rather than dropped. |
| stale archives | Removes superseded archive letters only after authenticating every active TAR entry and reconstructing its segment graph and binary-reference catalog from the indexed segment bytes, plus empty incomplete archives. Non-empty groups without that complete proof are preserved with a warning. |
| expired checkpoints | Omits checkpoints whose valid millisecond timestamp is strictly earlier than the planning time (`now > timestamp`) from the copy, so their content is never carried into the fresh generation. A missing or malformed timestamp is not selected by expiry and produces a warning, though the separate opt-in unreferenced policy can still select it. `--keep-expired-checkpoints` disables this one stage, for the operator who needs a run that reclaims space without touching checkpoint lifetimes. |
| stale temporaries | Removes only recognized interrupted-operation files whose contents are provably redundant. An ambiguous or non-redundant staging file is retained with a warning. |

Because expiry is realized by omission rather than by a second head update,
the head moves exactly once, and an expired checkpoint's content is reclaimed
by the same run that stopped carrying it.

There is one format-level effect to account for. If the repository still has
`store.version=1`, an actual checkpoint removal or archive rewrite adds an
explicit `atomically upgrade manifest to store.version=2` action to the plan.
Apply installs that upgrade atomically before writing version-two repository
state. The upgrade is one-way: froe does not provide a downgrade back to
version 1. There must be a checkpoint to retire or an archive to rewrite; a
run that finds neither leaves the manifest alone.

The temporary-file allowlist is deliberately exact:
`journal.log.compacting`, `journal.log.recovered`,
`journal.log.cleaning.NNN`, `manifest.cleaning.NNN`, a valid archive name
with a `.recovering` suffix, and
`<valid-archive-name>.cleaning.NNN`. A non-empty archive staging file is
removed only when it is byte-identical to an active archive; matching segment
payloads alone are insufficient because generation, graph, and binary-reference
metadata are also recovery evidence. A matching name is otherwise kept unless
the canonical state proves it redundant. In particular, a non-empty manifest
staging file is removable only when its bytes exactly match the canonical
manifest or, for a version-one canonical manifest, the deterministic
`store.version=2` upgrade of those canonical bytes. Divergent or unreadable
manifest staging is retained with a warning; an empty recognized staging file
is removable.

### Journal history

A syntactically valid journal line is not an orphan merely because it is old.
The run traverses each revision that still resolves and keeps its original
physical line bytes and order, including its tag and timestamp text. A
malformed or missing timestamp alone never makes a line removable. If a
rewrite is needed, the only normalization is to ensure that the installed
journal gives an originally unterminated final retained record one separating
`LF`. Existing `LF`, `CRLF`, and bare-`CR` terminators stay byte-exact. The
original is durably copied to a numbered `journal.log.bak.NNN` recovery backup
first, and successful CLI output names that exact backup.

The current selected head is special: if its record is not a node or its full
tree cannot be read—including inline binary blocks—the run fails instead of
silently falling back to an older revision. The retained root is re-verified
through a provider that excludes the planned removals before a byte moves, so
the run never exchanges readability of the revision it keeps for disk space.

A sweep that kept the history would reclaim approximately nothing on a
long-lived store, and that is why no run keeps it. Oak judges data segments by
their index generation triple alone and never rewrites `journal.log`; treating
every readable revision as an additional tracing root, as froe's standalone
sweep once did, protects nearly every segment those revisions reach on a store
whose journal holds tens of thousands of them. What is left is only garbage
that was never part of any persisted head.

Every run retires it. The copy rewrites the reachable head into a fresh
generation, reclaims the older ones without any history veto, and truncates
`journal.log` to the single line naming the head it just published. There is no
bound to set and no opt-in: retiring history is what a maintenance run does,
because the segments behind the older revisions are exactly what it reclaims.

The removed history is not recoverable from the store afterwards. The copy
appends its own journal line before the retirement, so the numbered
`journal.log.bak.NNN` holds the journal as it stood immediately before the
rewrite — every earlier revision plus the compacted head. That restores the
journal *file*, not the history: by the time it exists, the segments those
revisions named are already unlinked. Take a repository backup first if the
history matters.

A checkpoint this run retires is dropped by never entering it during the copy,
so it costs no second head update and leaves no older head behind to protect
its closure. Its content is unreachable from the generation the copy wrote and
is reclaimed by the same run's sweep — the case that previously needed a
compaction afterwards to become eligible at all. The outcome still reports the
logical checkpoint count separately from physical archive bytes, because a
checkpoint sharing every record with the live root frees nothing.

Before planning a head update, the run chooses the first archive
number above every physical Oak archive name in the directory. This includes
zero-byte files, invalid or unselected archive letters, and both the lettered
and letterless spellings of generation `a`; none can be silently reused. A
physical archive at number `4294967295` exhausts the namespace and is rejected
while planning, before mutation. If the certified first output number itself is
`4294967295`, an extraordinarily large checkpoint commit that crosses the
256 MiB rotation threshold cannot allocate a second archive. That later failure
is prefix-safe—the old journal head remains committed and the exclusive writer
never wraps or overwrites another archive—but may leave finalized, unreferenced
output for a subsequent run.

### Segment and TAR retention

Segment reclamation uses Oak's FULL-generation predicate with the generation
the copy just created as the reference, and retains one generation — the value
`SegmentGCOptions.setOffline()` uses. Data segments old enough under that
predicate are marked; unreferenced bulk segments are discovered from a single
store-wide reference graph. Nothing vetoes a mark on the strength of journal
history, because by then there is no journal history left to consult.

Marking a segment is not deciding to move it. An archive whose entries are all
marked is unlinked whole; an archive holding some is rewritten to its next
generation letter, however little of the file that frees — see [Why froe
rewrites archives Oak would leave
alone](#why-froe-rewrites-archives-oak-would-leave-alone). What stays behind is
what the format will not let the run move: an archive already at generation
`z`, one whose next generation pathname is occupied, or any archive Oak's
savings heuristic declines when `--archive-rewrite-policy oak-savings-gate` is
selected. The plan and the summary report that population as `identified but
retained` with its byte total, so a reclaimable estimate of zero can be told
apart from a store that holds no garbage at all.

The safety gate rejects the run rather than guessing when, for example, a
current-head segment appears reclaimable, index and segment-header generations
disagree, an active segment identifier occurs in more than one archive, an
active archive has no index metadata at all, or a *live* surviving data segment
would reference removed data.

Live is the operative word there. A segment the mark phase proved reclaimable
also survives whenever its archive cannot be rewritten — it is already at
generation `z`, its next generation pathname is occupied, or Oak's savings
heuristic was explicitly selected — and such a segment may point at other
reclaimed data — that is the ordinary state
Oak leaves behind every partial sweep, and nothing reachable reads it.
Rejecting on those would abort reclamation on precisely the stores it exists
for. What must not dangle is what is still reachable, and that is proved
separately and exactly: every retained journal root is re-verified through a
provider that excludes the planned removals before a single byte moves.

### An active archive with no index metadata

This one is worth naming, because it is the state a crashed Oak leaves and the
one an operator is most likely to meet. Oak writes an archive's `.gph`, `.brf`,
and index trailers only when it closes the archive, so a JVM killed with
`SIGKILL`, an out-of-memory kill, or a yanked container leaves its newest
archive complete but untrailered. froe's reader serves such an archive through
a recovery scan; the generation decisions the run makes are not allowed to rest
on a scan, because a scan silently drops what it cannot read and generation
froe deletes on the strength of it.

Without an authorized repair — `--skip-repairing-archive-indexes`, a
declined prompt, or a scripted run with no answers — the run refuses, and
the refusal counts *every* affected archive number, states why the index
was rejected, and says whether the newest one is among them — the
distinction between a killed writer, where the bytes are all still there,
and damage in the middle of a store, where they may not be. The refusal is
raised as soon as the archives are open, before the minutes-long
verification walks, and changes no archive, journal, or checkpoint. It
comes before the lock on the read-only preview path, and under it when a
caller prepares directly, in which case `repo.lock` itself may have been
created — that file is the only thing a refusing run can leave behind.

An authorized run — `--yes`, or yes at the prompt — [rebuilds those
indexes instead](#repairing-index-less-archives). `froe archives` reports
each archive's index state read-only, without the lock and without
changing anything.

An empty `dataNNNNN*.tar` is a related but separate artifact: it is the file a
writer creates just before it starts filling it, so a kill inside that window
leaves nothing but the name. It contributes no archive, blocks nothing, and
the stale-archive stage removes it as an empty incomplete archive.

If every segment in an archive is reclaimable, the whole archive can be
removed only when no other letter for that archive number could be promoted by
the removal. Otherwise it is rewritten to the next generation letter, however
little of the file that frees — see [Why froe rewrites archives Oak would
leave alone](#why-froe-rewrites-archives-oak-would-leave-alone), and
`--archive-rewrite-policy oak-savings-gate` to restore Oak's heuristic
instead. Rewrites use a non-active staging name and exclusively publish and
validate the new archive before the old file is removed. A partial rewrite is
still deferred when the archive is already at generation `z` or the next
generation pathname is occupied. Those deferrals are warnings, not repository
health failures, and the run names the archive, its reclaimable segment count
and its bytes so the residue is never silent.

Publishing a validated staging TAR uses an absent-only hard link in the same
directory, so it never overwrites a path that appeared after planning. Dry-run
cannot test hard-link support without writing; every rewrite plan warns about
the requirement. On a filesystem that rejects hard links, application can do
the staging work and then fail safely with the source still active and a
recognized `.cleaning.NNN` residue. Post-compaction archive rewrites performed
by ordinary `froe compact` use the same publication protocol and therefore
share this same-directory hard-link requirement.

A run that compacts appends exactly one `gc.log` line recording the cycle it
completed, and the final verification proves the file grew by that line and
nothing else.

## Why froe rewrites archives Oak would leave alone

Apache Oak's `TarReader.sweep` rewrites an archive only when the surviving
TAR-entry bytes fall below three quarters of the original — Java signed 32-bit
arithmetic, equality deferred. froe reproduces that arithmetic exactly, and by
default does not consult it.

The gate protects no invariant. It is evaluated *after* the whole-file removal
branch, so Oak already drops one hundred per cent of an archive with no gate at
all while refusing to drop twenty-four per cent of one, and the rewrite itself
is the same operation whatever volume it drops: the same survivor copy in
original file-position order, the same graph and binary-reference trailers
filtered from the existing ones, the same validated publication. What the gate
buys is input/output economics for an online collector competing with a running
repository.

froe reclaims offline, under the exclusive repository lock, because an operator
asked it to. On a store whose archives hold live binary content beside dead
node segments — the ordinary shape of an AEM repository, because compaction
re-links bulk segments where they lie instead of copying them — the surviving
bytes never fall below three quarters, so the gate declines those archives on
every run, forever. That is not hypothetical: under the older split of
compaction and cleanup, both of which applied the gate, a field report showed
642 MB across 130 archives correctly identified as reclaimable and
unreclaimable by any froe command.

So the default is `--archive-rewrite-policy every-reclaimable-archive`: any
archive holding a reclaimable segment is rewritten. `froe compact`'s own
reclaim pass always behaves this way and has no flag — a compaction that leaves
garbage no later command can remove defeats its own purpose. Pass
`--archive-rewrite-policy oak-savings-gate` to `froe compact` to leave behind
exactly what `oak-run compact` would.

The cost is the generation-letter namespace. An archive number carries the
letters `a` through `z`, a rewrite spends one, and only `froe compact` retiring
the number replenishes them. In practice the pressure is mild: after a
gate-less compaction a base archive holds nothing but referenced bulk segments,
so it is rewritten again only when binary content is actually deleted from the
repository, not on every garbage-collection run. An archive that does reach
`z` is deferred, named in a warning, and counted on the run's summary line, so
the residue a format limit forces is always visible.

## The questions and their skip flags

Every run performs the same sequence; three of its cleanups are settled as
yes/no questions first, and two flags stay opt-in overrides. The opt-ins
are documented above — `--always-copy`, because it overrides [the
convergence gate](#the-convergence-gate), and
`--remove-unreferenced-checkpoints` below. The questions — the
[orphaned-version-history purge](#orphaned-version-histories), the
archive-index repair, and the recovery-backup removal — are answered by
`--yes`, interactively at a prompt, or negatively by their `--skip-*`
flags; a scripted run without `--yes` receives no answers and applies
nothing.

### Repairing index-less archives

An authorized run rebuilds the index of an active archive that has none,
which is the state described under [the safety
gate](#an-active-archive-with-no-index-metadata). It asks first (`--yes`
answers; `--skip-repairing-archive-indexes` never repairs) because it
rewrites an archive, and because a store damaged in the middle rather than
at the tail is a case worth looking at before authorizing — the question
and the refusal both say which case this store is. Its full safety case —
scope, mutation ordering, interruption prefixes, resource bounds, and
known gaps — is
[`plans/0001-repair-archives/ARCHITECTURE.md`](plans/0001-repair-archives/ARCHITECTURE.md).

```console
$ froe compact /path/to/segmentstore --yes
```

Four things about it differ from the rest of the run.

**The read-only preview is deliberately partial.** Every index-dependent
decision — the segment sweep, checkpoint removal — is impossible until the
index exists, so a `--dry-run` with a repair pending names the repairs and
stops. An apply run performs the rebuilds under the exclusive lock *before*
planning — the answered question is the authorization — and then shows the
one full plan those repaired indexes made possible. Declining the final
confirmation does **not** undo the repairs: they are already durable, and
the CLI says so, on the cancelled path and even when the repaired store
turns out to need nothing else.

**The repository grows.** Every generation letter the rebuild reads is retired
to a `<archive>.bak` name — `<archive>.2.bak` and so on if one exists already —
so a repaired archive costs its own size again on disk, transiently twice.
The plan states the figure. Retiring the backups is the *next* run's work,
deliberately: repair and backup removal are never combined, because the run
that made the only copy of unrecoverable bytes must not be the run that
deletes it. A repairing run therefore keeps every recovery backup, states
the deferral, and the next run removes them once the store verifies.

**A version-1 store is raised to version 2.** A rebuilt archive carries a
version-2 binary-references trailer, so the manifest is upgraded first — but
only at the instant a rebuilt archive is about to become visible, never merely
because the flag was passed. The upgrade appears in the plan as
`UpgradeManifest`. It is one-way: an Oak older than 1.8 cannot open the store
afterwards.

**An archive no scan can read stops the run.** If any index-less number holds
bytes but yields no segment, the run refuses the whole run in the read-only
preview rather than rebuilding the others first — the run cannot complete
however it is retried, and the refusal names the file to move aside. Keep that
file: it is the only copy of whatever it holds.

The repair itself is reversible — every original is under a `.bak` — but the
sweep it unblocks is not. To separate the two, run interactively, answer
yes to the repair, decline the final confirmation (the rebuilds are
already durable at that point), run `froe check`, and only then compact.

### Unreferenced checkpoints

`--remove-unreferenced-checkpoints` retires checkpoints whose names do not
occur as string values under the content tree's `/:async` node. This is more
application-specific than timestamp expiry, so it requires explicitly asking
for it:

```console
$ froe compact /path/to/segmentstore \
    --remove-unreferenced-checkpoints \
    --dry-run
```

### Recovery backups

`journal.log.bak.NNN` and archive recovery backups named `<archive>.bak`,
`<archive>.N.bak`, `<archive>.ro.bak`, or `<archive>.N.ro.bak` are removed
by every run once its own question is answered — always as the run's last
mutation, after the store has verified — except by a run that also repairs
an archive index, which keeps them all and defers to the next run. `NNN`
is exactly three ASCII digits; an archive `N` is canonical decimal from 2
through 2147483647, without leading zeroes. `--skip-removing-recovery-backups`
keeps every backup, and the two retention flags narrow what the removal
may touch:

```console
# Keep the newest 3 per original target, and only ever remove backups at
# least 30 days old.
$ froe compact /path/to/segmentstore --yes \
    --backup-minimum-age-days 30 \
    --backup-keep-latest 3
```

Backups sit outside the archive byte figures, which count active archive names
only. A run that rebuilds an index therefore retires the original to a `.bak`
and grows the directory while reporting the archive total as unchanged, so the
summary states the bytes those retained backups still hold whenever any exist.

A backup is removable only if it is at least the requested age *and* falls
outside the newest count for the same original target; both default to
zero, so a plain authorized run removes every backup it can. All four
archive forms above are grouped under that original archive and compete
for the same slots, ordered by modification time; the unnumbered forms
have no special priority. Either retention flag stands alone. Future-dated backups
are never old; if modification times tie at the count boundary, the entire tie
is retained because numbered suffixes cannot safely establish creation order.
Any matching recovery-backup name denotes a managed file, so a directory,
symlink, or other non-regular object at such a name makes planning fail closed,
including during `--dry-run`; froe does not follow or remove it.

## Deliberate exclusions

The run has a narrow managed-file allowlist:

- The journal backup a run writes is retained by that run (the next run's
  authorized removal may retire it).
- `gc.log` is only ever appended to, by the single line described above; it is
  never truncated, rewritten, or removed.
- Applying acquires `repo.lock` and may create the empty lock file if
  absent, but never writes, truncates, or deletes its contents. A newly created
  Unix lock is hardened and synced under a non-active
  `.repo.lock.creating.*` name before an absent-only hard link publishes it as
  mode `0600`; an existing lock's mode is left untouched. Interrupted creation
  can leave a harmless staging link or file which repository discovery ignores
  and froe currently treats as an unknown, non-target path. Therefore apply,
  like every mutating froe command, requires same-directory hard-link and
  durable directory-fsync support when `repo.lock` is absent. It fails instead
  of using an unsafe fallback on an unsupported filesystem. Dry-run does not
  even acquire or create the canonical lock or a staging name.
- Unknown files and directories are never targets.
- External blob objects are outside the segment store and are never deleted;
  this is not blob garbage collection.

Recognized managed paths must be regular files. The run refuses symlinks and
other unexpected file types rather than following them. The repository
argument itself may be relative or pass through a symbolic-link alias: at API
entry it is resolved once to an absolute canonical directory, that target is
shown in the plan, and all later planning, locking, and mutation use the
captured path. Retargeting the alias afterward cannot redirect a prepared
maintenance session.

## Failures, deferrals, and reruns

The run is fail-closed. A missing manifest or journal, an invalid or unreadable
current head, unsafe generation metadata, duplicate active segment IDs, an
unexpected managed file type, or a repository change during planning causes an
error instead of a speculative deletion. In the CLI, a no-action preview
returns successfully without taking the lock.

Some residue is expected and safe:

- generation `z` or an occupied next archive name can defer an otherwise
  reclaimable archive (rerun once the stale-archive stage has removed a proven
  occupied leftover); under `--archive-rewrite-policy oak-savings-gate`, so can
  the 25% gate;
- malformed checkpoint metadata is not selected by timestamp expiry (although
  `--remove-unreferenced-checkpoints` can still retire that checkpoint);
  staging files that cannot be proved redundant are kept;
- a non-`NotFound` open error or identity/certification mismatch for a planned
  stale archive is fatal before checkpoint or journal head mutation: the
  confirmed proof cannot authorize a different or uninspectable inode. If a
  planned target is already absent, the run reports that externally satisfied
  but unconfirmed deletion separately and no retry is needed for that name. If
  unlinking a successfully recertified stale archive or other now-redundant old
  file fails, the retained target and operating-system detail are reported for
  a later retry. In both cases the validated repository remains usable, and
  the CLI calls the result partial and exits nonzero so unattended callers do
  not mistake another actor's deletion or a deferred deletion for this run's
  own complete success.

The command is designed to be rerun. A second pass can remove deletion residue
that remains or act after an operator resolves an occupied name; a target
already absent will simply disappear from the next plan. A clean second pass
reports no mutations. The whole command is not one filesystem transaction, so
a later error can follow an earlier durable action, but journal and manifest
swaps and archive rewrites use copy-on-write ordering. Reopen the repository,
inspect the warning or error, and rerun the same dry-run before applying again.

Archive readers remain mapped until the store-wide sweep finishes, so an
unlinked source does not necessarily release its blocks in time to fund the
next rewrite. The plan therefore prints the cumulative size of rewrite source
files as a working-space proxy and warns when reported filesystem availability
is lower. It is not a strict bound: journal backup/replacement files, a
checkpoint head TAR, manifest staging, filesystem metadata, sparse allocation,
quotas, and concurrent users of the same filesystem can all change the real
requirement. An orderly archive build, close, metadata-copy, or validation
error—including `ENOSPC`—leaves the original active archive in place and
removes the exact unvalidated `<archive>.cleaning.NNN` inode before returning,
provided its pathname still identifies that inode. Free space, reopen/verify
the repository, and rerun the preview.

Archive staging residue can still follow abrupt process death. It is also
retained intentionally after complete validation if a later publication step
fails, because at that point it is useful recovery evidence. Keep Oak/AEM
stopped and rerun dry-run first. If the residue is provably redundant, the
stale-temporary stage will plan its removal. If it is reported as
non-redundant and no final archive of the embedded name was published, it is a
non-active interrupted-write file: after confirming that the original active
source archive still exists, move that exact reported staging file to another
filesystem or remove it manually to recover space. Never delete an active
`data*.tar` archive or glob all `.cleaning.*` files; retain ambiguous residue as
recovery evidence. Then reopen/verify the repository and rerun the preview.

Planning is repository-wide rather than streaming. It memory-maps the active
archives and builds an in-memory segment-location table plus reachability,
history-protection, reference, and reclaim sets whose size grows with the
number of active segments and references, not just the number of TAR files.
As a rough planning figure on a typical 64-bit build, opening the repository
alone costs on the order of 70 bytes per active segment for index entries and
the global location map, before allocator overhead, caches, and the run's
additional sets and graph adjacency. Parsed-content caches are bounded and
archive payloads are not copied wholesale into the heap, but mapped pages still
consume address space and page cache as they are visited. Dry-run performs the
same planning analysis and is a useful baseline under the intended resource
limits; apply repeats that analysis under the lock and may need additional
transient writer and verification state, so the dry-run peak is not an upper
bound. An out-of-memory process kill is not an orderly error; keep
recovery headroom and an independent copy.

The failure suite injects deterministic I/O errors and abrupt `_exit` process
death around TAR publication, source unlink, journal backup/rename/fsync, and
manifest replacement, then performs a fresh read-only reopen. It does not
emulate a device that loses acknowledged cache writes, reorders durable I/O,
tears sectors, lies about flushes, or replays a damaged filesystem journal;
those scenarios require block-layer/VM power-cut testing. Keep an independent
recoverable copy for important repositories.

If the run refuses on a generation invariant, it refuses before the first
content mutation: no archive, checkpoint, or journal line has moved. An
authorized index repair is the one thing that can already have happened, and
its originals are under `.bak`. Read the named invariant, verify the repository
with `froe check`, and take a copy before rerunning — a refusal here means froe
could not prove the sweep safe, not that the store is known to be damaged.

## Library API

The core library exposes the same split between preview and locked apply:

```rust
use froe::{CompactionKind, CompactionOptions, PreparedCompaction, plan_compaction};
use std::path::Path;

fn main() -> froe::Result<()> {
    let directory = Path::new("/path/to/segmentstore");
    let options = CompactionOptions::default().with_compaction(CompactionKind::Full);
    let preview = plan_compaction(directory, &options)?;
    println!("{} planned actions", preview.actions().len());
    for removal in preview.journal_line_removals() {
        println!("journal line {}: {}", removal.line_number(), removal.reason());
    }

    // Reuse the preview's captured target so an alias cannot redirect the
    // lock acquisition between the two API calls.
    let prepared = PreparedCompaction::prepare(preview.directory(), options)?;
    // Interactive applications should compare/display prepared.plan() and
    // reconfirm if it differs from preview before calling apply().
    let outcome = prepared.apply()?;
    println!("verified head after the run: {}", outcome.head_after);
    println!("{} orphan segments removed", outcome.removed_segments());
    if let Some(path) = outcome.journal_backup_path() {
        println!("journal recovery backup: {}", path.display());
    }
    if !outcome.is_complete() {
        for failure in outcome.deletion_failures() {
            eprintln!("{}: {}", failure.file_name(), failure.error());
        }
    }
    Ok(())
}
```

Every one of these has a `_with_progress` twin —
`plan_compaction_with_progress`, `PreparedCompaction::prepare_with_progress`,
`PreparedCompaction::apply_with_progress`, `compact_with_progress` — taking a
`froe::progress::ProgressObserver` that is told which step is running and
how far it has got. The plain spellings above delegate to them with a
discarding observer, so observation costs nothing when nobody is watching
and never changes what an operation returns.

For unattended callers that do not need an external confirmation boundary,
`froe::compact(directory, options)` prepares under lock and applies the
authoritative plan directly.
