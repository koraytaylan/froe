# Safe repository cleanup

`froe cleanup` is conservative, offline maintenance for an existing
segment-tar repository. It removes journal entries that cannot name a readable
revision, reclaims eligible orphan segments, removes safe archive leftovers,
expires checkpoints, and deletes only staging files it can prove redundant.
It never creates or bootstraps a repository.

> **Run cleanup only while Oak/AEM is stopped, and keep a recoverable copy of
> important repositories.** Cleanup is covered by format-level and failure-path
> tests and by the interoperability suite ([`interop.md`](interop.md)), which
> applies it to a store written by Apache Jackrabbit Oak `oak-segment-tar`
> 1.90.0 — reclaiming orphan segments, a stale archive, an expired checkpoint
> and corrupt journal lines — and then boots Oak against the result, which
> serves a byte-identical content tree and logs none of its own repair
> messages. Not yet verified against a live instance: `store.version=1`
> stores, external blob stores, and Adobe AEM itself, which ships its own Oak
> build.

Apply is Unix-only and must run as the operating-system user that owns
`journal.log`—normally the Oak/AEM service account, not through `sudo`. This is
checked before `repo.lock` or any replacement file can be created. Rewritten
files preserve the source uid, gid, and permission bits, and cleanup refuses
before mutation if the repository filesystem cannot strictly fsync a directory.
Read-only `--dry-run` remains available without those apply preconditions.
Platform ACLs and extended attributes are not cloned; before publication,
replacement TARs, the journal, its recovery backup, and an upgraded manifest
are reopened through their path with the service identity and required access.
Cleanup fails before the swap if copied POSIX metadata alone would make a
replacement unusable.

Owning `journal.log` is necessary but does not make every mixed-ownership plan
safe. Preview checks the current process's uid, effective gid, supplementary
groups, the repository directory's gid and setgid behavior, and the uid/gid/mode
that each planned replacement must preserve. When the directory is not setgid,
the analysis conservatively models both POSIX-permitted creation outcomes: the
process effective gid and the parent-directory gid. A known credential mismatch
is printed as an apply-preflight warning in the read-only preview, including
`--dry-run`; dry-run stays read-only.

After taking the lock and rebuilding the authoritative plan, apply repeats
that check and refuses a known mismatch before the first planned repository-
content mutation. The metadata sources in scope are the manifest when it will
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
$ froe cleanup /path/to/segmentstore --dry-run
```

Planning a large store takes minutes: it verifies the whole head tree,
replays the journal, and traces the reachable segment closure before it can
say anything. Cleanup reports each of those steps on standard error while it
works, so a long wait is legible rather than silent; `--silent` turns the
reports off without hiding the plan, the warnings, or the confirmation
prompt. See [`cli-output.md`](cli-output.md).

Dry-run is a strict read-only operation: it writes no bytes, creates no files,
and does not create or acquire `repo.lock`. It validates the repository and
prints the selected task set, exact actions, warnings, verified head, and
conservative byte estimate. Journal removals include the one-based physical
line, structured reason, optional record identifier, and an exact bounded
byte-string prefix; non-ASCII and control bytes are escaped rather than decoded
or sent to the terminal.

## Applying a plan

Without `--dry-run`, cleanup uses two planning stages:

1. It builds and prints the same lock-free preview shown by dry-run.
2. After confirmation, it acquires the exclusive Oak-compatible repository
   lock and rebuilds the plan from disk.
3. If the authoritative locked plan differs, it prints the changed plan and
   asks for confirmation again.
4. It fingerprints the directory before the first mutation, applies the
   locked plan, forces durable metadata updates to disk, and reopens the
   repository through fresh mappings for a final health check.

If the first preview contains no mutations, the CLI reports that result and
returns without creating or taking `repo.lock`; there is no destructive plan
to authorize. Use the library's `PreparedCleanup` health-only apply when a
lock-protected final verification is specifically required.

`--yes` answers both possible confirmation questions automatically; it does
not bypass the lock, replan, fingerprint, validation, or final verification.
Lock acquisition fails immediately if Oak/AEM or another writer is using the
repository. As with Oak's own repository lock, safety assumes every other
writer cooperates with the persistent `repo.lock` inode and does not unlink or
replace it behind the lock holder; cleanup rechecks that inode between mutation
phases, but cannot make an actively hostile process obey an advisory lock.

```console
# Interactive application of the conservative defaults
$ froe cleanup /path/to/segmentstore

# The same checks and application without interactive questions
$ froe cleanup /path/to/segmentstore --yes
```

## Default tasks

With no `--task`, five conservative tasks run together:

| Task | Default behavior |
| --- | --- |
| `journal` | Removes parser-ignored lines, lines with invalid record identifiers, revisions whose head segment is absent, and unreadable *non-current* historical revisions. Every readable revision is retained. |
| `segments` | Runs a store-wide standalone FULL mark/sweep using the persisted head generation and two retained generations. It also protects the complete segment closure of every readable journal revision. |
| `stale-archives` | Removes superseded archive letters only after authenticating every active TAR entry and reconstructing its segment graph and binary-reference catalog from the indexed segment bytes, plus empty incomplete archives. Non-empty groups without that complete proof are preserved with a warning. |
| `expired-checkpoints` | Removes checkpoints whose valid millisecond timestamp is strictly earlier than the planning time (`now > timestamp`). A missing or malformed timestamp is not selected by expiry and produces a warning, though the separate opt-in unreferenced policy can still select it. All selected checkpoints are removed in one head update. |
| `stale-temporaries` | Removes only recognized interrupted-operation files whose contents are provably redundant. An ambiguous or non-redundant staging file is retained with a warning. |

Supplying any `--task` replaces this default set; repeat the option to select
more than one category:

```console
# Journal pruning only
$ froe cleanup /path/to/segmentstore --task journal

# Inspect only archive and temporary-file hygiene
$ froe cleanup /path/to/segmentstore \
    --task stale-archives \
    --task stale-temporaries \
    --dry-run
```

Task selection is isolation, not merely output filtering. For example,
`--task journal` does not open the normal writable-store lifecycle or repair,
rewrite, or delete archives as a side effect.

There is one format-level effect to account for when selecting tasks. If the
repository still has `store.version=1`, an actual checkpoint removal (from
either checkpoint task) or a `segments` archive rewrite adds an explicit
`atomically upgrade manifest to store.version=2` action to the plan.
Apply installs that upgrade atomically before writing version-two repository
state. The upgrade is one-way: cleanup does not provide a downgrade back to
version 1. Merely selecting either task is not enough to change the manifest;
there must be a checkpoint to remove or an archive to rewrite. Journal-only,
stale-archive, stale-temporary, and recovery-backup passes do not upgrade it.

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
Cleanup traverses each revision that still resolves and keeps its original
physical line bytes and order, including its tag and timestamp text. A
malformed or missing timestamp alone never makes a line removable. If a
rewrite is needed, the only normalization is to ensure that the installed
journal gives an originally unterminated final retained record one separating
`LF`. Existing `LF`, `CRLF`, and bare-`CR` terminators stay byte-exact. The
original is durably copied to a numbered `journal.log.bak.NNN` recovery backup
first, and successful CLI output names that exact backup.

The current selected head is special: if its record is not a node or its full
tree cannot be read—including inline binary blocks—cleanup fails instead of
silently falling back to an older revision. Segment cleanup also checks the
prospective archive set against every retained readable journal root, so the
default pass does not exchange historical readability for disk space.

**Expect the default pass to reclaim approximately nothing on a long-lived
store, and understand that this is the design rather than a failure.** Oak
judges data segments by their index generation triple alone and never rewrites
`journal.log`; froe additionally treats every readable revision as a tracing
root. A store whose journal holds tens of thousands of resolvable revisions
therefore protects nearly every segment those revisions reach, and only garbage
that was never part of any persisted head can be reclaimed. The plan and the
final summary both state the size of that protection: how many data segments
the head does not reach but history does, and what this same sweep would free
if that history were retired. The second figure is measured by replanning with
the veto lifted rather than estimated beside it, because the veto holds bulk
segments only through the data segments that reference them—on a store of
inline binaries the data segments are a rounding error next to the content
behind them—and because releasing more of an archive can carry it over the
rewrite gate below. Pricing it costs one extra marking pass over the archives
whenever the veto protects anything.

Two ways retire it. A full `froe compact` rewrites the reachable head into a
fresh generation, reclaims the older ones without any history veto, and
truncates `journal.log` to a single line; it is the reliable option and the
one to reach for first. Alternatively `--retain-journal-revisions N` keeps only
the newest `N` resolvable revisions, removes the older lines, and releases
their closure to the ordinary generation predicate—`N=1` leaves the current
head as the only root, which is the closest standalone cleanup comes to Oak's
own retention. The bound selects the journal task, because un-rooting a line
without removing it would leave the run refusing its own plan; naming an
explicit task set that excludes `journal` is refused rather than silently
widened. It is also refused while planning when the same run would remove
checkpoints: that removal installs a new head and appends its journal line, so
a bound counted afterwards would retire the head the plan retained and abort
the apply with the checkpoint removal already committed. Remove the
checkpoints first, then bound the journal in a second run. The removed history
is not recoverable afterwards. The segment sweep runs before the journal
rewrite in the same run, so the numbered `journal.log.bak.NNN` copy restores
the journal *file* but not the history: by the time it exists it names
revisions whose segments are already unlinked. Take a repository backup first
if the history matters. Whether the bound
actually frees bytes still depends on the store: the released segments must be
old enough for the generation predicate, and the archives holding them must
still clear the 25% rewrite gate below.

Checkpoint removal is a logical head update. The default segment sweep is
planned and applied first against the pre-removal roots, so it does not claim
bytes that become unreachable only after that new head is installed. The old
head remains readable journal history and standalone cleanup deliberately
protects its complete closure, so removing the checkpoint does not by itself
make those bytes eligible on a later cleanup pass. A subsequent full compaction
(or another explicit history-retirement workflow) is required to retire that
history and reclaim eligible storage. The outcome therefore reports the
logical checkpoint count separately from physical archive bytes.

Before planning a checkpoint head update, cleanup chooses the first archive
number above every physical Oak archive name in the directory. This includes
zero-byte files, invalid or unselected archive letters, and both the lettered
and letterless spellings of generation `a`; none can be silently reused. A
physical archive at number `4294967295` exhausts the namespace and is rejected
while planning, before mutation. If the certified first output number itself is
`4294967295`, an extraordinarily large checkpoint commit that crosses the
256 MiB rotation threshold cannot allocate a second archive. That later failure
is prefix-safe—the old journal head remains committed and the exclusive writer
never wraps or overwrites another archive—but may leave finalized, unreferenced
output for a subsequent cleanup pass.

### Segment and TAR retention

Standalone segment cleanup uses Oak's FULL-generation predicate with the
current persisted committed head as the reference and retains two generations.
Data segments old enough under that predicate may be marked; unreferenced bulk
segments are discovered from a single store-wide reference graph. Readable
journal-history data segments are an additional keep-veto and never a reason
to reclaim something.

Marking a segment is not deciding to move it. An archive is rewritten only when
dropping its marked entries saves more than a quarter of the file—Oak's gate,
reproduced with Java's integer arithmetic—and an archive whose entries are all
marked is unlinked whole instead. Real stores interleave live and dead segments,
so a scattered handful per archive is normally identified and then left in
place. The plan and the summary report that population as `identified but
retained` with its byte total, so a reclaimable estimate of zero can be told
apart from a store that holds no garbage at all.

The safety gate rejects cleanup rather than guessing when, for example, a
current-head segment appears reclaimable, index and segment-header generations
disagree, an active segment identifier occurs in more than one archive, an
active archive has no index metadata at all, or a *live* surviving data segment
would reference removed data.

Live is the operative word there. A segment the mark phase proved reclaimable
also survives whenever the rewrite gate declines to rewrite its archive, and
such a segment may point at other reclaimed data — that is the ordinary state
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
a recovery scan; the generation decisions cleanup makes are not allowed to rest
on a scan, because a scan silently drops what it cannot read and generation
cleanup deletes on the strength of it.

Without `--task repair-archives`, cleanup refuses, and the refusal counts
*every* affected archive number, states why the index was rejected, and says
whether the newest one is among them — the distinction between a killed
writer, where the bytes are all still there, and damage in the middle of a
store, where they may not be. The refusal happens while planning: no archive,
journal, or checkpoint is changed. It is raised before the lock on the
read-only preview path, and under it when a caller prepares directly, in which
case `repo.lock` itself may have been created — that file is the only thing a
refusing run can leave behind.

Selecting [`repair-archives`](#repairing-index-less-archives) makes cleanup
rebuild those indexes instead. `froe archives` reports each archive's index
state read-only, and the tasks that do not consult generations — `journal`,
`stale-archives`, `stale-temporaries` — still run against an affected store
without the repair task.

An empty `dataNNNNN*.tar` is a related but separate artifact: it is the file a
writer creates just before it starts filling it, so a kill inside that window
leaves nothing but the name. It contributes no archive, blocks nothing, and
`stale-archives` removes it as an empty incomplete archive.

If every segment in an archive is reclaimable, the whole archive can be
removed only when no other letter for that archive number could be promoted by
the removal. Otherwise it is rewritten to the next generation letter only
when the surviving TAR-entry bytes pass Oak's exact Java signed-32-bit 25%
savings calculation; at ordinary archive sizes this means *below* 75% of the
original entry bytes, and equality is deferred. Rewrites use a non-active
staging name and exclusively publish and validate the new archive before the
old file is removed. Partial rewrites are also deferred when the archive is
already at generation `z` or the next generation pathname is occupied. These
deferrals are warnings, not repository health failures.

Publishing a validated staging TAR uses an absent-only hard link in the same
directory, so it never overwrites a path that appeared after planning. Dry-run
cannot test hard-link support without writing; every rewrite plan warns about
the requirement. On a filesystem that rejects hard links, application can do
the staging work and then fail safely with the source still active and a
recognized `.cleaning.NNN` residue. Post-compaction archive rewrites performed
by ordinary `froe compact` use the same publication protocol and therefore
share this same-directory hard-link requirement.

Standalone cleanup never appends to or changes `gc.log`; that log is reserved
for completed compaction cycles.

## Opt-in tasks

Three categories are intentionally outside the defaults.

### Repairing index-less archives

`--task repair-archives` rebuilds the index of an active archive that has
none, which is the state described under [the safety
gate](#an-active-archive-with-no-index-metadata). It is not a default because
it rewrites an archive, and because a store damaged in the middle rather than
at the tail is a case worth looking at before authorizing. Its full safety
case — scope, mutation ordering, interruption prefixes, resource bounds, and
known gaps — is
[`plans/repair-archives-safety-case.md`](plans/repair-archives-safety-case.md).

```console
$ froe cleanup /path/to/segmentstore \
    --task repair-archives \
    --task journal --task segments --task stale-archives \
    --task expired-checkpoints --task stale-temporaries
```

Four things about it differ from every other task.

**The plan arrives in two stages.** Every index-dependent decision — the
segment sweep, checkpoint removal — is impossible until the index exists, so
while a repair is pending the read-only preview names the repairs and stops.
The full plan appears only at the second, lock-protected confirmation, and the
banner says so rather than blaming an outside writer. Declining at that second
prompt does **not** undo the repair: it has already happened, and the CLI says
that too. `--yes` answers both prompts, so a scripted run authorizes the
rebuild and everything the replan then finds.

**The repository grows.** Every generation letter the rebuild reads is retired
to a `<archive>.bak` name — `<archive>.2.bak` and so on if one exists already —
so a repaired archive costs its own size again on disk, transiently twice.
The plan states the figure. Retiring the backups is a *separate, later* run of
`--task recovery-backups`, deliberately: repair and recovery-backups are
refused together, because the run that made the only copy of unrecoverable
bytes must not be the run that deletes it.

**A version-1 store is raised to version 2.** A rebuilt archive carries a
version-2 binary-references trailer, so the manifest is upgraded first — but
only at the instant a rebuilt archive is about to become visible, never merely
because the task was selected. The upgrade appears in the plan as
`UpgradeManifest`. It is one-way: an Oak older than 1.8 cannot open the store
afterwards.

**An archive no scan can read stops the run.** If any index-less number holds
bytes but yields no segment, cleanup refuses the whole run in the read-only
preview rather than rebuilding the others first — the run cannot complete
however it is retried, and the refusal names the file to move aside. Keep that
file: it is the only copy of whatever it holds.

The repair itself is reversible — every original is under a `.bak` — but the
segment sweep it unblocks is not. Repair, run `froe check`, and only then
cleanup, if you want those separated.

### Unreferenced checkpoints

`unreferenced-checkpoints` removes checkpoints whose names do not occur as
string values under the content tree's `/:async` node. This is more
application-specific than timestamp expiry, so it requires explicit selection:

```console
$ froe cleanup /path/to/segmentstore \
    --task unreferenced-checkpoints \
    --dry-run
```

Because any `--task` replaces the defaults, enumerate the five default tasks
as well if this rule should be added to a full pass.

### Recovery backups

Existing `journal.log.bak.NNN` and archive recovery backups named
`<archive>.bak`, `<archive>.N.bak`, `<archive>.ro.bak`, or
`<archive>.N.ro.bak` are never deleted by default. Removing them requires both
an age floor and a per-target count floor. `NNN` is exactly three ASCII digits;
an archive `N` is canonical decimal from 2 through 2147483647, without leading
zeroes:

```console
# Backups only: keep the newest 3 per original target and consider only
# backups at least 30 days old.
$ froe cleanup /path/to/segmentstore \
    --task recovery-backups \
    --backup-min-age-days 30 \
    --backup-keep-latest 3
```

Backups sit outside the archive byte figures, which count active archive names
only. A run that rebuilds an index therefore retires the original to a `.bak`
and grows the directory while reporting the archive total as unchanged, so the
summary states the bytes those retained backups still hold whenever any exist.

A backup is removable only if it is at least the requested age *and* falls
outside the newest count for the same original target. All four archive forms
above are grouped under that original archive and compete for the same slots,
ordered by modification time; the unnumbered forms have no special priority.
The two policy flags must be supplied together. Supplying them enables
`recovery-backups` in addition to the selected task set; without any `--task`,
that means backup cleanup is added to the five defaults. Future-dated backups
are never old; if modification times tie at the count boundary, the entire tie
is retained because numbered suffixes cannot safely establish creation order.
Any matching recovery-backup name denotes a managed file, so a directory,
symlink, or other non-regular object at such a name makes planning fail closed,
including during `--dry-run`; cleanup does not follow or remove it.

## Deliberate exclusions

The default pass has a narrow managed-file allowlist:

- Existing recovery backups are retained (although a journal rewrite creates
  a new numbered backup).
- `gc.log` is byte-checked before and after and is never updated by standalone
  cleanup.
- Applying cleanup acquires `repo.lock` and may create the empty lock file if
  absent, but never writes, truncates, or deletes its contents. A newly created
  Unix lock is hardened and synced under a non-active
  `.repo.lock.creating.*` name before an absent-only hard link publishes it as
  mode `0600`; an existing lock's mode is left untouched. Interrupted creation
  can leave a harmless staging link or file which repository discovery ignores
  and cleanup currently treats as an unknown, non-target path. Therefore apply,
  like every mutating froe command, requires same-directory hard-link and
  durable directory-fsync support when `repo.lock` is absent. It fails instead
  of using an unsafe fallback on an unsupported filesystem. Dry-run does not
  even acquire or create the canonical lock or a staging name.
- Unknown files and directories are not cleanup targets.
- External blob objects are outside the segment store and are never deleted;
  this is not blob garbage collection.

Recognized managed paths must be regular files. Cleanup refuses symlinks and
other unexpected file types rather than following them. The repository
argument itself may be relative or pass through a symbolic-link alias: at API
entry it is resolved once to an absolute canonical directory, that target is
shown in the plan, and all later planning, locking, and mutation use the
captured path. Retargeting the alias afterward cannot redirect a prepared
cleanup session.

## Failures, deferrals, and reruns

Cleanup is fail-closed. A missing manifest or journal, an invalid or unreadable
current head, unsafe generation metadata, duplicate active segment IDs, an
unexpected managed file type, or a repository change during planning causes an
error instead of a speculative deletion. In the CLI, a no-action preview
returns successfully without taking the lock.

Some residue is expected and safe:

- the 25% gate, generation `z`, or an occupied next archive name can defer an
  otherwise reclaimable archive (rerun after `stale-archives` removes a proven
  occupied leftover);
- malformed checkpoint metadata is not selected by timestamp expiry (although
  the explicitly selected `unreferenced-checkpoints` policy can still remove
  that checkpoint); staging files that cannot be proved redundant are kept;
- a non-`NotFound` open error or identity/certification mismatch for a planned
  stale archive is fatal before checkpoint or journal head mutation: the
  confirmed proof cannot authorize a different or uninspectable inode. If a
  planned target is already absent, cleanup reports that externally satisfied
  but unconfirmed deletion separately and no retry is needed for that name. If
  unlinking a successfully recertified stale archive or other now-redundant old
  file fails, the retained target and operating-system detail are reported for
  a later retry. In both cases the validated repository remains usable, and
  the CLI calls the result partial and exits nonzero so unattended callers do
  not mistake another actor's deletion or a deferred deletion for cleanup's
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
`stale-temporaries` task will plan its removal. If it is reported as
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
the global location map, before allocator overhead, caches, and cleanup's
additional sets and graph adjacency. Parsed-content caches are bounded and
archive payloads are not copied wholesale into the heap, but mapped pages still
consume address space and page cache as they are visited. Dry-run performs the
same planning analysis and is a useful baseline under the intended resource
limits; apply repeats that analysis under the lock and may need additional
transient writer and verification state, so the dry-run peak is not an upper
bound. An out-of-memory process kill is not an orderly cleanup error; keep
recovery headroom and an independent copy.

The failure suite injects deterministic I/O errors and abrupt `_exit` process
death around TAR publication, source unlink, journal backup/rename/fsync, and
manifest replacement, then performs a fresh read-only reopen. It does not
emulate a device that loses acknowledged cache writes, reorders durable I/O,
tears sectors, lies about flushes, or replays a damaged filesystem journal;
those scenarios require block-layer/VM power-cut testing. Keep an independent
recoverable copy for important repositories.

If segment cleanup refuses a generation invariant that an explicitly expired
or unreferenced checkpoint is keeping reachable, run that checkpoint task by
itself and verify the repository. Its previous head remains protected history;
use full compaction when the operator is ready to retire that history and
physically reclaim it.

## Library API

The core library exposes the same split between preview and locked apply:

```rust
use froe::{CleanupOptions, PreparedCleanup, plan_cleanup};
use std::path::Path;

fn main() -> froe::Result<()> {
    let directory = Path::new("/path/to/segmentstore");
    let options = CleanupOptions::default();
    let preview = plan_cleanup(directory, &options)?;
    println!("selected tasks: {:?}", preview.tasks());
    println!("{} planned actions", preview.actions().len());
    for removal in preview.journal_line_removals() {
        println!("journal line {}: {}", removal.line_number(), removal.reason());
    }

    // Reuse the preview's captured target so an alias cannot redirect the
    // lock acquisition between the two API calls.
    let prepared = PreparedCleanup::prepare(preview.directory(), options)?;
    // Interactive applications should compare/display prepared.plan() and
    // reconfirm if it differs from preview before calling apply().
    let outcome = prepared.apply()?;
    println!("verified head after cleanup: {}", outcome.head_after);
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
`plan_cleanup_with_progress`, `PreparedCleanup::prepare_with_progress`,
`PreparedCleanup::apply_with_progress`, `cleanup_with_progress` — taking a
`froe::progress::ProgressObserver` that is told which step is running and
how far it has got. The plain spellings above delegate to them with a
discarding observer, so observation costs nothing when nobody is watching
and never changes what an operation returns.

For unattended callers that do not need an external confirmation boundary,
`froe::cleanup(directory, options)` prepares under lock and applies the
authoritative plan directly.
