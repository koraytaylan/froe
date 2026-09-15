# Architecture — Plan 0007

## 0007 — Property Index Reindex

Requires plan 0006 merged: the readers, the definition model, the digest exclusion and the judge are its foundation.

### What Oak does, and what froe must therefore do

When a cycle decides a definition needs a reindex, it sets `reindex=false`, increments `reindexCount`, removes every hidden child not flagged `retainNodeInReindex`, clears `corrupt`, sets the hidden `:disableIndexesOnNextCycle = true` when the definition's `supersedes` names an index that is still active — the *next* cycle is what disables those indexes — and then runs the definition's editor over a diff from the missing state to the current state, so every node and every property arrives as an addition. Every editor runs inside the visible-editor filter, so hidden nodes and hidden properties are never visited. A property index derives one key set per node from the values of the properties `propertyNames` lists, encoded as `docs/analysis/index-property-storage.md` records, subject to `declaringNodeTypes` through the node-type predicate and to the path filter, and the mirror or unique strategy writes `:index`. The reference index collects every `REFERENCE` property outside version storage and every `WEAKREFERENCE` property anywhere. The counter hashes every child name down from the seed. froe's rebuild is that computation performed by a walk of the same state, followed by the same bookkeeping, published in one head move.

The state Oak's editors see is the state of the commit they run in. For a synchronous index that is the head. For an asynchronous lane it is the lane's checkpoint at the end of the cycle, and the lane property records it. An offline rebuild from the head would be wrong for the counter: the lane's next cycle diffs from its recorded checkpoint, and every node added between that checkpoint and the head would be counted twice. So froe indexes an async definition from `/checkpoints/<lane checkpoint>/root`. The property family writes no `:status` node — only the fulltext family does, when it closes its writer and when it stamps the unique identifier on the reindex path, as `docs/analysis/index-definitions.md` records — so no timestamp bookkeeping is owed and none is written; task 0706 lists every byte the rewrite changes. When the recorded checkpoint no longer exists, Oak's lane logs `Failed to retrieve previously indexed checkpoint … re-running the initial index update` and then replays from the missing state *incrementally* rather than reindexing, over whatever hidden state exists, because a definition that still carries a hidden child is never reindexed — the rule `docs/analysis/index-definitions.md` records. The fulltext editor is the exception: it re-enters reindex mode whenever the before state at its root is missing and appends every document again to the retained `:data`, which is why plan 0010 resets under `--from-head` too. froe therefore refuses a dangling lane checkpoint without `--from-head` and, under the flag, rebuilds only definitions whose replay leaves the index content unchanged — re-inserting a mirror or unique entry that already exists leaves every `match` and `entry` as it was, and only the randomized `:count_*` estimates drift, because both strategies adjust the approximate counter on every insert. For the counter (and plan 0010's Lucene definitions) on a lane whose checkpoint is dangling or absent, `--from-head` therefore performs a reset rather than a rebuild — the hidden children removed exactly as a reindex removes them, retained ones kept, no other byte touched, nothing built — so that the replay, which would double a rebuilt index whether or not froe ran, meets a definition with no hidden child and rebuilds it from scratch under that same rule; the plan output and the summary name the reset.

### Data flow

```
definition selection ──► state root (head | lane checkpoint)
        │
        ▼
collector walk (visible-editor semantics, PathFilter, TypePredicate)
        │  (key, path) records, or (identifier, property path), or counter hits
        ▼
external sort: bounded in-memory runs spilled to --work-directory, k-way merge
        │  sorted by (key, path elements)
        ▼
trie writer: streams the sorted sequence into ContentMirror / UniqueEntry
             records through RecordWriter, bottom-up, state ∝ fan-out on the path
        │
        ▼
definition rewrite: the hidden children the walk produced — :index always for
                    the property family (Oak creates it even when empty),
                    :references / :weakreferences and the counter's :index only
                    when at least one entry or hit exists (Oak creates them lazily) —
                    reindex=false, reindexCount+1, corrupt cleared, a counter's seed created when absent,
                    :disableIndexesOnNextCycle under the disabler's predicate,
                    other hidden children dropped unless retained; a parked async=async-reindex property removed
        │
        ▼
spine rewrite to the super-root (rewrite_node_with_child_edits)
        │
        ▼
verify_node_tree over each new subtree through the open session, entry-side consistency check
(0606's check_entries, budget = collected entries + 1; for the counter, the `:cnt` sum check against `credited_by_path`
instead, since 0606's check_entries covers no counter) — all of it before anything is published
        │
        ▼
one compare-and-set, one flush
        │
        ▼
fresh reopen: head identity, verify_node_tree again, one new journal line
```

Sorting by `(key, path elements)` with elements compared as byte strings gives exactly the order a depth-first construction of the mirror trie needs: when the sequence leaves a subtree, that subtree's node can be written with all its children known, and the only state held is, for each ancestor on the current path, the completed `(name, record)` pairs of its children — bounded by the widest node on one root-to-leaf path, state proportional to one node's fan-out, which is what `child_node_entries` already costs elsewhere in the crate — the trie root `:index` included, one child per distinct key, so a mirror index over a value distinct per node carries the same key-proportional term the unique strategy does. Unique indexes need no trie: one `entry` node per key, and a duplicate key is a refusal — but their `:index` node (and the reference index's `:references`) is by construction the widest node, one child per key, and froe writes a wide node from `ChildNodesToWrite::Many` with every child resident, so that builder's state is the key count times one name plus one record identifier: the one key-proportional term the safety case admits, stated with its arithmetic there and reported per definition in the plan's estimate.

The counter is built directly from the walk without sorting: `:cnt` accumulates per ancestor; the mirror node set is the set of ancestors of hit nodes. It is emitted bottom-up (children before parents, as `write_node` needs every child's record first) after the walk from an in-memory map bounded by the number of hits (about one per `resolution` nodes) times the depth.

### Modules

```
crates/froe/src/external_sort.rs   SortedRuns<Record>: bounded runs, spill files, k-way merge
                                   (crate-private module at the crate root beside cache, packed_records
                                   and parallel, so plan 0009's Lucene writer can use it without
                                   depending on the segment-store write path; RunLocation, SortBudget,
                                   SortedPass, SortedPasses and the SpillRecord bound are pub,
                                   re-exported below)
crates/froe/src/writer/index/
├── mod.rs               plan_reindex(), reindex(), ReindexOptions, ReindexPlan, ReindexAction,
│                        PreparedReindex, ReindexOutcome, IndexEntry (0702), and their observed
│                        twins, plus pub use crate::external_sort::{RunLocation,
│                        SortBudget, SortedPass, SortedPasses, SpillRecord} (0702)
├── selection.rs         which definitions, which state root, typed refusals
├── property_collector.rs  the walk producing (key, path) and reference entries
├── property_builder.rs  sorted (key, path) ─► mirror/unique records
├── counter_builder.rs   hits ─► :cnt records
├── definition_update.rs the reindex bookkeeping on the definition node
├── plan.rs              planning: counts, sizes, warnings, work-directory estimate
├── prepared.rs          lock, replan, fingerprint, apply, verify, outcome
└── apply.rs             the ordered mutation sequence
crates/froe/src/writer/fault_injection/index_reindex.rs   cutpoints
crates/froe/src/writer/commit.rs                          rewrite_node_with_edits / NodeEdits (0706): the property-editing
                                                           sibling of rewrite_node_with_child_edits
crates/froe/src/writer/maintenance/gate_observation.rs   #[cfg(test)] gate-call recorder (0715)
crates/froe/src/progress.rs                               WorkUnit::IndexEntries (0704)
crates/froe-cli/src/index_reindex.rs                       plan/confirm/apply flow
crates/froe-cli/tests/interop/judge/CounterVectors.java    the judge class this plan adds
```

The plan follows compaction's open protocol exactly, canonicalizing the directory once and carrying it in the plan: `prepare` runs `validate_repository_shape` (a symlinked root, a missing manifest or journal, or a managed name that is not a regular file is refused) and the two pre-lock apply-identity gates of `maintenance/apply_identity.rs` — `validate_apply_environment` (the directory can be fsynced) and `validate_apply_identity` (`journal.log` belongs to the service user) — before and again after acquiring the lock (`RepositoryLock::acquire`, `validate_path_identity`, as `maintenance/prepared.rs` does), replans, fingerprints the directory, computes the certified archive number (`store_writer::next_cleanup_archive_number`, which `open_prepared` rechecks), and runs the third gate under the lock: a plan-independent `validate_metadata_source_apply_identity(directory)` that task 0715 factors from `planned_metadata_sources` and `metadata_source_apply_identity_issue`, refusing a store whose newest active archive could not be re-owned under `open_prepared`'s `preserve_file_metadata` (which runs inside `flush`, so without the gate the failure would surface only after every record was written); `apply` rechecks the fingerprint, then `validate_path_identity` again immediately before the destructive step (the fingerprint deliberately skips `repo.lock`, so only the identity check catches a replaced lock file), and only then opens the store through `WritableRepository::open_prepared`, the side-effect-free open (no manifest rewrite, no archive normalization, no journal creation); the lockless plan opens read-only. Under that open, `flush` seals the archive, syncs the directory, validates the finalized session and only then appends the journal line.

### Mutation and publication order

| Boundary / cutpoint | Preconditions | Published or durable change | Returned-error state and named regression | Abrupt-exit state and named regression | Reconciliation |
| --- | --- | --- | --- | --- | --- |
| Spill files written in the run's subdirectory under `--work-directory`, which `apply` creates before the first spill (after `open_prepared`; a cancelled confirmation leaves nothing behind); `index-reindex.before-spill-cleanup` fires once the sort has returned its iterator and before the first index record is appended, with every spill file still on disk | plan replanned under the lock; residue policy evaluated in the plan (the plan refuses a froe-named subdirectory left by an earlier run in an operator-named directory and warns about one under the default) | files outside the store only | subdirectory removed on return — `a_spill_failure_removes_the_run_subdirectory_and_leaves_the_store_unchanged` (armed, 0708) | leftover files in the run's subdirectory, never in the store — `a_death_before_spill_cleanup_leaves_no_file_in_the_store` (armed, 0708; the retry against the leftover subdirectory is asserted there too) | operator removes the subdirectory |
| Index records appended (`index-reindex.before-head-publish`), then the definition rewritten — hidden children replaced or dropped unless flagged `retainNodeInReindex`, `reindex` set to `false`, `reindexCount` incremented, `corrupt` removed, a counter's `seed` created when absent, `:disableIndexesOnNextCycle` under the disabler's verdict, a parked `async = async-reindex` property removed, every other property and visible child preserved by identity (under a reset: only the rewritten definition node, its hidden children gone, and the spine) | fresh archive number above every physical name, at the head's generation | new archives only, unreachable from the head | store unchanged plus unreferenced archives at the head's generation — `an_error_before_head_publish_leaves_the_head_and_every_definition_as_they_were` (armed, 0708; it runs the later `froe compact` and asserts the orphan archives are gone by name) | same — `a_death_before_head_publish_leaves_the_head_resolving_the_old_records` (armed, 0708) | a later `froe compact` copies the live content into a fresh generation and retires every older archive, these included; they are never referenced by a checkpoint, having never been published (the superseded records of a *successful* run are the ones every lane checkpoint pins, which the Resources section states); they are not "interrupted-run residue" to the planner, which reserves that name for segments stamped *ahead* of the head |
| Head publication (`compare_and_set_head`, then `flush`; `index-reindex.after-head-publish-before-flush`) | every new subtree verified through the open session; the store was opened with `open_prepared` | `compare_and_set_head` changes nothing on disk; `flush` seals and fsyncs the archive, syncs the directory, validates the finalized session, then appends one journal line naming the new head | old head before the journal append, new head after it, never partial — `an_error_after_head_publish_before_flush_leaves_the_journal_naming_the_old_head` (armed, 0708) | same — `a_death_between_head_publish_and_flush_leaves_one_resolvable_head` (armed, 0708) | either head resolves; the loser's records are garbage |
| Applied-state verification (`index-reindex.before-applied-verification`) | head published | none | reports the mismatch, store already final — `a_failed_applied_state_verification_reports_rather_than_repairs` (armed, 0708) | not applicable — the head is published and durable before this boundary | rerun the check |

### Task graph

```
0701 safety case (first)
0715 expose the maintenance gates (a refactor accepted by compaction's own tests; independent)
0701 ─► 0702 external sort (creates the module root and its stubs)
0702 ─► 0703 trie writer
0702 ─► 0704 collector
0702 ─► 0705 counter builder
0702 ─► 0706 definition bookkeeping and selection
0701, 0703, 0704, 0705, 0706, 0715 ─► 0707 plan and apply
0707 ─► 0708 fault cutpoints (records them)
0701, 0708 ─► 0709 guard evidence
0707 ─► 0710 CLI and its documentation
0710 ─► 0711 compact warning
0710 ─► 0712 interop phase
0712 ─► 0713 suite wiring and run record
0709, 0711, 0713 ─► 0714 review (gated; records the interop run)
```

Every file this plan puts under `crates/froe/src/writer/index/` is created as a documented stub by task 0702, which also declares the module in `writer/mod.rs` — later plans add their own files to the directory, and within this plan the one exception is `apply/tests.rs`, which task 0707 adds, declared `#[cfg(test)] mod tests;` in `apply.rs`, only if the line gate forces the split — and then owned by one later task (0708 then adds its cutpoints to `apply.rs` after 0707), so no two parallel tasks edit the module root; 0702 itself follows the safety case, so no code that publishes bytes lands before it; 0715 is a behaviour-preserving refactor accepted by compaction's own tests. Of the parallel set, task 0715 alone edits `writer/maintenance/mod.rs`, `writer/maintenance/planning/mod.rs`, `writer/maintenance/planning/shape.rs`, `writer/maintenance/apply_identity.rs` and the new `writer/maintenance/gate_observation.rs` (and moves the tests' `ObservationLog` into `tests/support/`) — a `refactor:` task that widens and re-exports the directory fingerprint, the shape and canonicalization helpers and the three apply-identity gates, factors the plan-independent metadata-source gate with its `_for_credentials` twin, and adds the `#[cfg(test)]` observation seam, accepted by compaction's existing guard tests with no behaviour change, kept apart from 0707's mutating diff as `CONTRIBUTING.md` asks; it is numbered after the review task because it was added later, and task 0707 lists it in `depends_on`. 0711's later edit of the planning module is ordered after both through 0710; 0704 alone edits `progress.rs` (for its `WorkUnit` variant) and 0706 alone edits `writer/commit.rs` (the `rewrite_node_with_edits` sibling). The module is `pub mod index;` and the operations, builders and options are `pub`, because the crate's integration tests under `crates/froe/tests/` are separate crates that can reach only public items. The capability the feature map inventories is the operation (`plan_reindex`, `PreparedReindex`, `reindex`); the builders, collectors and the external sort are its components, so 0707 lands the library's feature-map row with the public entry points, and 0710 lands the command's row. `apply.rs` is structured as a per-`SelectedIndex` `rebuild` dispatch returning a `RebuiltDefinition { path, previous_record, rebuilt_record, report }` — `report` being the per-definition entry the `ReindexOutcome` later carries — consumed by one spine-rewrite, verify and publish tail whose verification is itself a per-type dispatch (`check_entries` for the mirror, unique and reference types; the `:cnt`-against-`credited_by_path` comparison for the counter, which `check_entries` does not cover; plan 0010 adds the segment-reader and file read-back arm), with the `Lucene` arm a typed refusal until plan 0010 fills it, and `ReindexAction` and the per-definition `ReindexOutcome` are `#[non_exhaustive]` so the command renders unknown variants through a wildcard arm, as `compaction_report.rs` does. The plan's `ARCHITECTURE.md` is edited by 0701, 0708, 0709 and 0714, all chained. User-facing documentation lands with the capability (0710, 0711), and the interop phase's suite wiring follows the phase (0713).

### Safety case

This plan is high-risk under [`high-risk-changes.md`](../../high-risk-changes.md),
and its safety case lives here, as a section of this file, so that every plan
in this directory keeps its frozen evidence where plans 0001, 0002 and 0004
keep theirs. It succeeds
[`0004-merged-maintenance-command/ARCHITECTURE.md`](../0004-merged-maintenance-command/ARCHITECTURE.md),
which remains the case for the write session, the lock protocol, the caches
and the walks, and which this plan extends rather than supersedes: nothing
here changes what compaction does.

In scope on three counts.

* **It writes index records into a live store.** Every froe mutation until now
  either copied content forward unchanged (compaction, backup, restore) or
  removed something an operator confirmed. This one *computes* bytes that
  Oak's own editors would have computed, and publishes them as the index Oak
  will query. A wrong key, a wrong path element, a wrong strategy, and the
  store still parses, still checks, still boots — and returns wrong query
  results.
* **It rewrites a definition node.** The bookkeeping Oak performs around a
  reindex — `reindex`, `reindexCount`, `corrupt`, the hidden children, the
  disabler flag, a parked `async` property — decides what Oak's *next* cycle
  does. Getting it wrong does not corrupt the store; it makes Oak redo, skip,
  or double the work, which is a fault that appears later and elsewhere.
* **It is unbounded in the input it walks.** A reindex reads the whole state
  a definition covers, which on a production store is every node. The memory
  case of plan 0002 is what keeps that from being a second risk, and this plan
  adds exactly one term to it, stated under Resources.

Covers `crates/froe/src/external_sort.rs`,
`crates/froe/src/writer/index/**`,
`crates/froe/src/writer/fault_injection/index_reindex.rs`,
`crates/froe/src/writer/commit.rs`,
`crates/froe/src/writer/maintenance/**` (task 0715's refactor),
`crates/froe/src/progress.rs` and `crates/froe-cli/src/index_reindex.rs`.

#### Scope and retention

**Default-safe work.** A run with no flag rebuilds the selected definitions
and nothing else. It refuses, rather than guesses, every case where the state
Oak's editor would have seen cannot be identified: a definition on a lane
whose checkpoint is dangling or absent, a type this plan does not rebuild, a
definition the model cannot read.

**The one opt-in.** `--from-head` authorizes exactly two side effects on a
definition whose lane checkpoint is dangling or absent, and nothing else:

1. **A from-head rebuild of a mirror or unique definition.** Authorized
   because Oak's replay from the missing state re-inserts entries that already
   exist, and re-inserting leaves every `match` and `entry` exactly as it was.
   Only the randomized `:count_*` approximate counters drift, because both
   strategies adjust the counter on every insert — which is why
   `froe digest --exclude-property-prefix :count_` exists.
2. **A reset of a counter definition** — and, in plan 0010, of a Lucene
   definition. Hidden children removed exactly as a reindex removes them,
   retained ones kept, nothing rebuilt, no other byte touched. Oak's next
   cycle then meets a definition with no hidden child and rebuilds it from
   scratch. A *rebuild* here would be wrong: the replay would double the
   counter whether or not froe ran.

The plan output and the summary name every reset, so the authorization is
visible before it is given and observable after it is taken.

**What must survive unconditionally.** The content tree; every checkpoint;
every definition not selected; of a selected definition, every property other
than `reindex`, `reindexCount`, `corrupt`, a `seed` the counter run creates
when absent, the `async` property of a definition parked at
`async = async-reindex` (removed exactly as Oak's switch-back removes it), and
the hidden `:disableIndexesOnNextCycle` (written only under Oak's own disabler
predicate); every hidden child flagged `retainNodeInReindex`; every visible
child of a selected definition, **including one froe does not model** — this
plan writes no visible child through `DefinitionEdits`, and task 0706's digest
comparison is the regression that says so; and `/:async` in full.

**What is deliberately dropped.** The previous hidden children of a selected
definition, exactly as Oak drops them.

**What the run never touches.** Lane checkpoints, `journal.log` history,
archives. A reindex adds archives; it retires none.

#### Authoritative state

The preview is advisory and lockless. It opens the store read-only, plans
against what it reads, and every record identity in it is discarded.

`PreparedReindex::prepare` is the lock boundary. It runs
`validate_repository_shape` and the two pre-lock apply-identity gates
(`validate_apply_environment`, `validate_apply_identity`), acquires
`repo.lock`, runs `validate_path_identity`, **repeats both gates**, replans
from disk, fingerprints the directory, certifies the archive number and runs
the plan-independent `validate_metadata_source_apply_identity` — the gate that
refuses a store whose newest active archive could not be re-owned under
`open_prepared`'s `preserve_file_metadata`, which runs inside `flush` and
would otherwise fail only after every record had been written.

`apply` rechecks the fingerprint, then `validate_path_identity` again
immediately before the destructive step — the fingerprint deliberately skips
`repo.lock`, so only the identity check catches a replaced lock file — and
only then opens through `WritableRepository::open_prepared`.

Every fact rechecked under the lock, because each one can change between the
preview and the run: the definition set; each definition's type and lane; the
lane checkpoint's existence **and its root record**; the disabler verdict,
which reads over the head's *other* definitions; the definition record; and
the head. **No record identity from the lockless plan survives the replan**;
that is the property that makes the preview safe to show and unsafe to use.

Preview and apply share the selection and refusal predicates, so a preview
that showed a rebuild and an apply that refuses it disagree only because the
store changed.

#### Mutation and publication order

The [`### Mutation and publication order`](#mutation-and-publication-order)
section above **is** this safety case's table. It is cited rather than copied,
so a later edit cannot leave two versions to review. Each row names the
regression test that arms its cutpoint. Task 0708 armed all four —
`index-reindex.before-spill-cleanup`, `index-reindex.before-head-publish`,
`index-reindex.after-head-publish-before-flush` and
`index-reindex.before-applied-verification` — in
`writer/fault_injection/index_reindex.rs`, each in both fault models where
the states differ, and each verified to fail when its cutpoint is removed.

One fact the table's **Reconciliation** column depends on, which task 0708
had to add to the apply to make true: the session is closed on *every* path,
not only the successful one. Closing writes the open archive's graph, catalog
and index trailers, so what a returned error leaves behind is an
*unreferenced* archive rather than a damaged one — and a later `froe compact`
retires it silently instead of refusing the store until an operator
authorizes an index repair. A run that failed after `compare_and_set_head`
puts the session's head back before closing, since the head lives in memory
until `flush` appends the journal line and closing would otherwise publish
the run that just failed. Abrupt death is the other story and always was:
it leaves an archive without trailers, which is the damage froe's compact
already recognizes and repairs under an operator's authorization.

#### Interruption prefixes

For a returned error and for abrupt death at each boundary, the store is
either **unchanged plus unreferenced archives at the head's generation**, or
**at the new head with every new subtree verified**. There is no third state.

That follows from two facts about the publication step.
`compare_and_set_head` is an **in-memory** move: it changes nothing on disk.
`flush` seals and fsyncs the archive, syncs the directory, validates the
finalized session, and only then appends the single journal line. So the two
cutpoints around the publication observe the same on-disk prefix, and the
journal append is the only byte that makes the new head resolvable.

Before that append, the new records are present and unreachable — indistinguishable
from an interrupted run's residue, and reclaimed by a later `froe compact`.
After it, the new head resolves and every subtree under it was verified
through the open session before publication.

A returned error reports the operations observed to have completed and names
any durability uncertainty. Abrupt death cannot report, so the next inspection
is what must reconcile: `froe check` resolves whichever head the journal
names, and `froe compact` retires the loser's records.

#### Observed outcomes

`ReindexOutcome` reports, per definition, a typed outcome built from observed
operations:

* **rebuilt**, with the entries written, the distinct keys, and the bytes;
* **reset**, with the hidden children removed and the retained ones kept;
* **`NothingToDo { reason }`** — a definition with no removable hidden child —
  which **never moves the head**.

It also reports the head before and after. Every figure comes from an observed
operation; none is inferred from a plan, from the existence of a destination,
or from diagnostic text. `ReindexAction` and the per-definition outcome are
`#[non_exhaustive]`, and the command renders unknown variants through a
wildcard arm, as `compaction_report.rs` does.

#### Resources

**Time.** Two walks of the state per confirmed run: the counting walk
`prepare` performs under the lock, and the collecting walk of `apply`. A
preceding `--dry-run` adds the lockless plan's. Then one pass over the
rebuilt entries for the tail's entry-side verification, and two
`verify_node_tree` passes over the new records — before publication and after
the reopen. The sort adds n log n plus one merge pass over the spill bytes per
fan-in level; the trie writer adds a single pass over the sorted sequence.
Pinned by the visited-node counter of task 0704, the merge-pass counter of
0702 and the write counter of 0703.

**Memory.** This plan adds exactly one key-proportional term to the bounded
memory case of plan 0002, and it is admitted deliberately:

* *The trie writer's resident state.* For the mirror strategy it is the widest
  fan-out on the current root-to-leaf path — **including the trie root
  `:index`, which has one child per distinct key**. For a unique or reference
  index it is the `:index` (or `:references`) node itself, one child per key.
  In both cases the bound is *distinct keys × (one name + one record
  identifier)*, because `ChildNodesToWrite::Many` holds every child of a wide
  node while it is written. Reported per definition in the plan's estimate:
  the entry count as the upper bound for a mirror definition, the exact
  distinct-key count in the outcome.
* *The counters' `credited_by_path` maps*, which outlive their builds: one
  entry per credited node, so *hits × depth* per selected counter definition,
  resident until the tail's verification compares them against the written
  `:cnt`.
* *The walk's depth-proportional state*, which the bounded-memory case of plan
  0002 already documents.

**Open files.** The merge's bound is the fan-in task 0702 declares, plus one —
**never the run count**. One merge is live at a time, so a reference
definition's two run sets do not double it.

**Temporary disk.** One sort budget per definition, in bytes, configurable,
with its default stated by task 0702; a reference definition's two run sets
hold a shared `SortBudget` charge handle together rather than dividing it. The
spill directory's worst case is the total entry bytes — about one path string
per indexed value, which for `nodetype` is every node path once per type value
— and transiently up to *fan-in × budget* more during a reduction pass, until
the merged inputs are unlinked.

**Which figures are only proxies.** The plan's entry count is an upper bound
on mirror residency, not the residency. The reported free space is checked
against the work-directory estimate, which is the *per-definition maximum
including the reduction pass's transient*, not the selection's total.

**How exhaustion leaves a safe prefix.** A spill that fails on `ENOSPC`
returns a typed error **before the first index record is appended**, so the
store is unchanged and the run subdirectory is removed.

**The old index records** stay live through every retained checkpoint — each
lane's checkpoint references them by construction, since a checkpoint's `root`
is the content root's record — and are reclaimed only by a `froe compact` run
after those checkpoints are released. A reindex therefore *grows* a store
until the next compaction, which is the honest cost and is stated in the plan
output.

#### Guards

Every row was produced the same way: the guard was removed on its own in a
working tree, its named regression was run, the failure was recorded
verbatim, and the code was restored. The "observed failing result" column
quotes those runs. A row whose quote names a *different* refusal is defence
in depth and says so — that is a finding about the design, not a gap in the
evidence.

The regressions live in `crates/froe/tests/index_reindex_guard_tests.rs`
unless the row says otherwise. The exceptions are the guards whose seam is
`#[cfg(test)]` and therefore absent from the library an integration test
links against; those cite the in-crate test by name.

| Guard and production callers | Named regression | Neutralization | Observed failing result |
| --- | --- | --- | --- |
| An `elasticsearch` definition is refused: its data is not in this repository (`plan_reindex` → `select` → `refuse_by_shape`) | `an_external_index_type_is_refused_because_its_data_is_not_in_the_repository` | The `Elasticsearch` arm returns `None` | `a named definition is always answered with exactly one refusal; instead the plan holds actions [Rebuild { path: "/oak:index/subject", state: Head, entries: 0, entry_bytes: 0 }] and warnings []` |
| A `disabled` or `ordered` definition is refused: neither has an editor (same path) | `a_type_with_no_editor_is_refused` | The `Ordered` arm returns `None` | The same `Rebuild` action in place of the refusal. |
| A `lucene` definition is refused until plan 0010 (same path) | `a_lucene_definition_is_refused_until_the_lucene_plan_lands` | The `Lucene` arm returns `None` | The same `Rebuild` action in place of the refusal. |
| A `valuePattern` regular expression is refused: froe evaluates the prefix halves and not the expression, so a rebuild would write entries Oak's editor filters out (same path) | `a_value_pattern_regular_expression_is_refused_rather_than_ignored` | The `regular_expression().is_some()` test is replaced with `false` | `planning answers a named definition rather than failing: InvalidFormat { details: "the index definition at /oak:index/subject restricts values with the regular expression \"A.*\", which froe does not evaluate" }` — the key encoder refuses further in, so the plan fails outright instead of answering. Defence in depth; the guard's contribution is the answer rather than the refusal. |
| A definition carrying a composite mount's index data is refused: a rebuild replaces the hidden children, and that data is another mount's (same path) | `a_definition_holding_a_composite_mounts_index_is_refused` | `mount_children().first()` is replaced with `None` | The same `Rebuild` action in place of the refusal — and the test's second half states the consequence: the rebuild removes `:oak:mount-libs-index`, data froe did not write and cannot reproduce. |
| An unconstructable `PathFilter` is refused *as such*, so the operator learns Oak's own cycle skips this definition too (`plan_reindex` → `select` → `refuse_unmodellable`) | `a_path_filter_oak_cannot_construct_is_refused_as_such` | The `RelativeFilterPath`/`EmptyIncludeSet` arm is deleted | `/oak:index/subject could not be read: the index definition at /oak:index/subject has a relative path "content/not-absolute" in its included list; Oak requires absolute paths` — still refused, under the generic variant. The guard's contribution is the classification. |
| A node that is not an `oak:QueryIndexDefinition` is refused (same path) | `a_node_that_is_not_a_definition_is_refused` | `refuse_by_shape`'s `None => NotADefinition` arm returns `None` | `/oak:index/subject could not be read: the node type index did not enumerate this definition` |
| Every explicitly named path is answered, whether or not the inventory listed it (`plan_reindex` → `select` → `answer_every_named_path`) | `a_named_path_with_no_node_is_answered_rather_than_ignored`, `a_node_that_is_not_a_definition_is_refused` | The call is deleted | `a named path that does not exist must be answered: []` — an operator who names a path is silently told there is nothing to do. |
| A nested definition is refused, never approximated: Oak scopes it to the node holding its `oak:index` and its paths are relative to that node (same path) | `a_nested_definition_is_refused_rather_than_approximated` | The `starts_with("/oak:index/")` test is deleted | `["/content/oak:index/nested could not be read: the node type index did not enumerate this definition"]` — refused, but for the wrong reason and with no mention of nesting. |
| A dangling lane checkpoint is refused without `--from-head` (`plan_reindex` → `select` → `resolve_state`) | `a_dangling_lane_checkpoint_is_refused_without_from_head` | `if !options.from_head` becomes `if false` | The same `Rebuild` action in place of the refusal: froe would rebuild from the head without being told to. |
| A lane absent from `/:async` is refused under its own variant (same path) | `a_lane_absent_from_the_async_node_is_refused_under_its_own_variant` | Same edit | The same `Rebuild` action in place of the refusal. |
| A lane mid-run at `/:async/async-reindex` is refused (same path) | `a_reindex_lane_mid_run_is_refused` | The `ReindexLaneInProgress` return is deleted | The same `Rebuild` action: froe would rebuild an index Oak's own lane is rebuilding. |
| An asynchronous definition is indexed from its lane's checkpoint, not the head (`reindex` → `select` → `resolve_state`) | `an_asynchronous_definition_is_indexed_from_its_lane_checkpoint_not_the_head` | The lane branch returns `IndexingState::Head` with the head root | `content the lane has not reached must not be indexed: ["", "/Early", "/Early/content", "/Early/content/early\tmatch=Boolean:true", "/Late", "/Late/content", "/Late/content/late\tmatch=Boolean:true"]` — `/Late` indexed into a lane that has not reached it. |
| A hybrid definition — one listing `sync` beside its lane name, which Oak's synchronous cycle maintains on every commit as well as the lane — is indexed from the **head**, not from the lane's checkpoint (`reindex` → `select` → `resolve_state`) | `a_hybrid_definition_is_indexed_from_the_head_not_its_lane_checkpoint` | The `synchronous_synonym` test is replaced with `false` | `a hybrid index is maintained synchronously too, so the head's own content must be indexed: ["", "/Early", "/Early/content", "/Early/content/early\tmatch=Boolean:true"]` — `/Late`, committed after the checkpoint, is silently absent. Found by task 0714's state-root lens, which is what that lens is for. |
| A hybrid **counter** is refused outright: its lane's replay adds to what is there, so a counter rebuilt from either state and replayed from the other is doubled (same caller) | — | — | Carved out with its reason rather than tested: constructing a hybrid counter means constructing a definition Oak itself has no editor pairing for, and the refusal exists so froe never produces a number nobody can trust. |
| A counter on an unresolvable lane is *reset* under `--from-head`, not rebuilt: a rebuild doubles it whether or not froe ran | `a_counter_on_an_unresolvable_lane_is_reset_rather_than_rebuilt` (`index_reindex_selection_tests.rs`) | — the reset arm is task 0706's `ResetForReplay`; its Oak-side proof is task 0712's reset scenario | — |
| A multi-valued `reindexCount` is refused: Oak's own `getLong` throws and a rebuild cannot guess which value to carry (`reindex` → `rewrite_definition` → `current_reindex_count`) | `a_multi_valued_reindex_count_is_refused`, and task 0706's `a_multi_valued_reindex_count_is_refused_rather_than_guessed` | The `Multiple` arm returns `Ok(0)` | `a multi-valued reindexCount must be refused: RecordIdentifier(a30db17b-6f52-4f56-a6ef-02d4ba337b7b.0000000b)` — the rewrite succeeds and silently resets the count to 1. |
| A hidden child carrying a strict `BOOLEAN` `retainNodeInReindex = true` survives the rebuild (`reindex` → `rewrite_definition` → `retains_across_reindex`) | `a_retained_hidden_child_survives_the_rebuild` | The predicate returns `false` | `a retained hidden child must survive byte for byte: []` — the child is gone. |
| A hit-less counter writes no `:index` at all, because Oak's editor returns before creating one (`reindex` → `CounterBuilder::build`) | `a_hit_less_counter_writes_no_index_node_at_all` | The early return writes the tree instead | `reindex: InvalidFormat { details: "the counter accumulation credited no root, which cannot happen when any node hit" }` — the tree writer refuses to invent a root. Defence in depth. |
| An empty reference set writes no `:references`/`:weakreferences` (`reindex` → `build_reference_index`) | `an_empty_reference_set_writes_no_hidden_child` | `has_strong()`/`has_weak()` are replaced with `true` | `an empty reference set must write no :references` — an empty hidden child where Oak writes none. |
| A duplicate unique key is refused **before publication** (`reindex` → `UniqueBuilder::push`) | `a_duplicate_unique_key_is_refused_before_anything_is_published` | The `DuplicateUniqueKey` return becomes a second `push` | `a duplicate unique key must be refused: ReindexOutcome { definitions: [("/oak:index/subject", Rebuilt { entries: 2, distinct_keys: 1, nodes_written: 2 })], head_before: …, head_after: … }` — published: an index asserting a store Oak's own commit would have refused. |
| Every property and visible child not named in `DefinitionEdits` is preserved by record identity (`reindex` → `rewrite_definition` → `rewrite_node_with_edits`) | task 0707's `the_bookkeeping_is_done_and_every_other_property_survives`, which asserts an `includedPaths` and a `declaringNodeTypes` survive the apply path, over task 0706's direct digest comparison | Write only the named properties instead of the preserved slots | Task 0706 records this row's experiment; the three planned callers — this plan's apply, plan 0008's importer and plan 0010's Lucene reindex — converge on the one slot-preserving write, which is the wiring proof `docs/high-risk-changes.md` asks for. |
| The repository shape is checked before anything else: no symlinked managed file, and the manifest and journal are there (`plan_reindex` and `PreparedReindex::prepare` → `validate_repository_shape`) | `a_managed_file_that_is_a_symlink_is_refused_without_being_followed`, `a_missing_manifest_is_refused_by_the_repository_shape_check`, `a_missing_journal_is_refused_by_the_repository_shape_check` | Both calls are deleted | `a symlinked managed file must be refused: ReindexPlan { …, actions: [Rebuild { path: "/oak:index/title", … }], … }`. The two missing-file regressions still pass with the guard removed, because `Repository::open` refuses them too — so the shape check's unique contribution is the symlink, and that is what its row is evidence for. |
| The journal belongs to the service user (`PreparedReindex::prepare` → `validate_apply_identity`) | task 0715's `validate_apply_identity_for_uid` module test, which models an identity the process does not have so no test depends on the runner's uid | — task 0715 records this row's experiment | — |
| The newest active archive could be re-owned under `open_prepared` (`PreparedReindex::prepare` → `validate_metadata_source_apply_identity`) | task 0715's `_for_credentials` module test | — task 0715 records this row's experiment | — |
| Every gate above actually runs, before *and* after the lock, against the canonical directory (`PreparedReindex::prepare`) | task 0707's in-crate `a_prepare_runs_the_whole_open_protocol` in `writer/index/prepared.rs`, over task 0715's observation seam | Delete any one gate call | A dropped gate leaves a correct plan and a correct apply on a healthy store and only fails to refuse an unhealthy one, which is why the wiring is asserted directly. |
| The directory has not changed between planning and applying (`PreparedReindex::apply` → `recheck_before_mutation`) | `a_directory_that_changed_during_confirmation_is_refused_before_any_record` | `current != self.fingerprint` becomes `false` | `the refusal says what changed and when: invalid segment-tar data: physical archive data00099a.tar has number 99 at or above the certified checkpoint output number 1; refusing prepared cleanup` — the archive-number certification catches it instead. Defence in depth. |
| The lock file at the path is still the lock this run holds (`PreparedReindex::apply` → `validate_path_identity`) | `a_replaced_lock_file_is_refused_before_the_store_is_opened` | The call returns `Ok(())` | `a replaced lock file must be refused` — the run proceeds holding a lock no other writer can see. The fingerprint cannot cover this: it skips `repo.lock` by design. |
| Index records go to an archive number above every physical name (`PreparedReindex::prepare` → `next_cleanup_archive_number`) | `the_index_records_go_to_an_archive_number_above_every_physical_name` | The certified number becomes `0` | `reindex: InvalidFormat { details: "certified checkpoint output alias data00000a.tar is occupied; refusing prepared cleanup" }` — the session refuses to write into an occupied name. Defence in depth. |
| The pre-publication entry pass: every entry's path resolves in the state root and carries the key (`PreparedReindex::apply` → `verify_before_publication`) | task 0707's in-crate `a_forged_entry_the_content_does_not_carry_is_refused` in `writer/index/apply/tests.rs` | Run the tail without the entry pass | In-crate because the perturbation seam is `#[cfg(test)]`; the test drives `reindex` and asserts the head did not move. |
| The pre-publication counter arm: every `:cnt` is what `credited_by_path` recorded (same caller) | task 0707's in-crate `a_counter_count_the_build_did_not_credit_is_refused` | Skip the comparison | Same placement and the same head-did-not-move assertion. |
| The tail's `EntryCheckBudget` is the collected entry count plus one (same caller) | task 0707's in-crate `a_subtree_holding_more_entries_than_were_collected_exhausts_the_budget` | Pass an unbounded budget | `checking the index entries of /oak:index/subject would examine more than 2 entries` — two spliced key nodes exceed a budget that accepts at its limit. |
| The reference collector merges one run set at a time, so the open-file bound stays at the fan-in plus one (`build_reference_index` → `SortedReferenceSets`) | task 0704's in-crate `the_two_reference_sets_merge_one_at_a_time` in `writer/index/property_collector.rs` | Merge both sets at once | In-crate because the open-file accounting task 0702 exposes is crate-internal. |
| An empty plan neither opens the store nor moves the head (`PreparedReindex::apply` → `ReindexPlan::is_empty`) | `a_rerun_of_a_reset_has_nothing_to_do_and_never_opens_the_store` (`index_reindex_selection_tests.rs`), `an_empty_plan_neither_opens_the_store_nor_moves_the_head` | `is_empty()` becomes `false` | `[]` — the outcome loses its per-definition reports, so an operator is told nothing about why there was nothing to do. |
| The head compare-and-set refuses a moved head (`build_and_publish`) | — | — | Carved out: the run holds `repo.lock` exclusively from `prepare` through `flush`, so no second writer can move the head inside the window, and task 0707's seam perturbs a written subtree rather than the head. Task 0714 records its reachability in the known gaps. |
| The fsync-capability gate (`PreparedReindex::prepare` → `validate_apply_environment`) | — | — | Carved out: it needs a directory that refuses `fsync`, which no synthetic fixture here provides. Task 0714 records it in the known gaps. |

#### Fault and subprocess tests

Every row is a test in `writer/fault_injection/index_reindex.rs`, run in the
framework's forked child: the error child exits `VERIFIED_EXIT_CODE` after its
own assertions and the crash child `CRASH_EXIT_CODE` at the cutpoint itself,
so a test whose cutpoint was removed fails on the exit code rather than
passing quietly. Each was checked that way — the cutpoint deleted, the test
run, the failure observed, the cutpoint restored.

| Cutpoint | Fault model | Named test | Asserted prefix |
| --- | --- | --- | --- |
| `index-reindex.before-spill-cleanup` | returned error | `a_spill_failure_removes_the_run_subdirectory_and_leaves_the_store_unchanged` | Every file of the store byte-identical (`repo.lock` excepted, which the child held), `/oak:index/title` digesting exactly as the run found it, and the run's subdirectory gone — a returned error removes it. |
| `index-reindex.before-spill-cleanup` | abrupt `_exit` | `a_death_before_spill_cleanup_leaves_no_file_in_the_store` | The same store prefix, and the spill files still in the run's subdirectory, since nothing ran to remove them. The test then reacquires the lock through a retry and asserts the operator-named directory refuses the residue, clears it, and reruns to the same published post-state. |
| `index-reindex.before-head-publish` | returned error | `an_error_before_head_publish_leaves_the_head_and_every_definition_as_they_were` | The same store prefix, plus new archives holding records nothing reachable from the head refers to. The test then runs a full `froe compact` and asserts every archive the failed run added is gone by name — which only holds because the session is closed on the error path and the archive therefore carries its trailers. |
| `index-reindex.before-head-publish` | abrupt `_exit` | `a_death_before_head_publish_leaves_the_head_resolving_the_old_records` | The same store prefix, and the retry assertion above. Death leaves the archive without trailers, which is the writer-killed damage `froe compact` already repairs under an operator's authorization — so the compaction assertion is deliberately not made here. |
| `index-reindex.after-head-publish-before-flush` | returned error | `an_error_after_head_publish_before_flush_leaves_the_journal_naming_the_old_head` | Byte for byte the same prefix as the pre-publication row, asserted by the same helper: `compare_and_set_head` writes nothing, so there is no third state to assert. The failed run puts the session's head back before closing, so closing cannot publish it. |
| `index-reindex.after-head-publish-before-flush` | abrupt `_exit` | `a_death_between_head_publish_and_flush_leaves_one_resolvable_head` | The same prefix, one resolvable head, and the retry assertion. |
| `index-reindex.before-applied-verification` | returned error | `a_failed_applied_state_verification_reports_rather_than_repairs` | The head *has* moved — this boundary is after publication — the rebuilt `:index` is published and stays published, and the store passes `check_consistency`. Nothing was rolled back: the run reports. |

There is no abrupt-death row for `index-reindex.before-applied-verification`:
the head is published and durable before the boundary, so death there leaves
the successful run's post-state and there is no distinct prefix to assert.

#### Interoperability

**Direction:** Oak wrote the store, Oak rebuilt its own indexes, froe
rebuilt the same extracted bytes, and the two were compared.

**Image:** `docker.io/apache/sling@sha256:8722cd66ae0758e50784ac21df836c8f8
d9e443d105e1a4292a4cb7f810a8cc9`, the pinned digest, running the
`oak-segment-tar` build `generate` records in `oak-build.txt`.

**Operation under test:** `froe index reindex --yes`.

**froe-side edits made to the copy before that operation:** each
definition's `reindex` flag set and its `reindexCount` set to Oak's value
minus one, through `definition_edits.rs` on the public writer API, so
froe's single increment lands back on exactly Oak's value. Nothing else was
changed; hidden children were re-attached by record identity.

**Canonical-index check:** the counter's mirror was read through plan
0006's reader and required to carry a `:cnt` on every node before any
comparison, because a lane cycle between Oak's rebuild and the stop can
leave a `:cnt`-less node no rebuild produces. Passed on attempt 1.

**Verified post-state:** Oak rebuilt **23** definitions — every direct child
of `/oak:index` whose type is `property`, `reference` or `counter`, the set
discovered from the store rather than listed. froe's rebuild of the same
bytes rendered **identically for every one** under
`--exclude-property-prefix :count_`. The full content digest differed in 39
lines over 52,252 nodes, every one inside the declared `/oak:index` scope.
`froe check` passed at the new head. Reproduced across four runs.

**Not run, and why.** The phase's query-level comparison and its
counter-reset scenario need two further Sling boots. This host's memory
watchdog killed the harness five times, at varying points including during
the first bootstrap container, with 110+ GB of 122 GB available each time —
a policy limit on the session's processes, not exhaustion. The phase code
for both is written, compiled and linted; one boot cycle was removed while
trying, which is kept because it is better design. **This gap is open: the
maintainer's freeze should not treat the interoperability record as
complete until those two halves have run.**

#### Verification report

Every task in this range ran the stable host gate on `x86_64-unknown-linux-
gnu` before its commit, each ending `=== END ===` with no failing section:
`cargo +stable fmt --all -- --check`, `cargo +stable test --workspace
--all-features --no-fail-fast`, the same `--release`, `cargo +stable clippy
--workspace --all-targets --all-features -- -D warnings`, `RUSTDOCFLAGS="-D
warnings" cargo +stable doc --workspace --all-features --no-deps`,
`scripts/oversized-files.sh`, and `git diff --check HEAD`.

**The five separations this report is held to.**

*Execution from cross-compilation.* Everything above was executed, not
cross-compiled. The i686 width sentinel and the MSRV gate were **not run in
this range**; they are gaps below, not claims.

*Synthetic credentials from execution as root.* The journal-owner and
metadata-source gates are exercised through their `_for_credentials` twins,
which model an identity the process does not have. No test depends on the
runner's uid, and none ran as root. What that proves is the predicate, not
the behaviour of a real foreign-owned file.

*Process-exit or syscall injection from true power-loss ordering.* Task
0708's four cutpoints inject a returned error or an abrupt `_exit` in a
forked child. That proves the code's ordering around each boundary. It does
**not** prove the platform's write ordering under power loss: no test here
cuts power, and none can.

*File existence from durability.* The probes assert a reopened store's head,
journal lines and subtrees through a fresh read-only open. That a file
exists after `flush` returned is asserted; that it survives a host crash at
that instant is not.

*froe-to-froe round trips from real Oak interoperability.* The reindex's
own suites compare froe against froe and against an independent test-only
encoder. Only the `property_reindex` phase compares against Oak, and its
record above states exactly which halves ran.

**Fault coverage, per claim.** Each row of the fault table names its
cutpoint, its model (returned error or `_exit`), and the prefix it asserts;
each was verified by deleting the cutpoint and watching the test fail on
the child's exit code. What they do not cover: any boundary inside the
external sort (deliberately — it takes no cutpoint, which is what keeps it
independent of the segment-store write path), and abrupt death after the
applied-state verification boundary, where the head is already durable and
there is no distinct prefix to assert.

#### Known gaps

**Guards with no synthetic regression.**

* *The fsync-capability gate* (`validate_apply_environment`). It needs a
  directory that refuses `fsync`; no fixture here provides one. Reachable
  in production on a filesystem without directory-fsync support, which is
  the condition `froe compact` already warns about.
* *The head compare-and-set refusal.* Unreachable while the run holds
  `repo.lock` exclusively from `prepare` through `flush`, so no second
  writer can move the head inside the window. It is a belt on a guarded
  window, and task 0707's seam perturbs a written subtree rather than the
  head.
* *The hybrid-counter refusal.* Carved out with its reason in the guards
  table: constructing one means constructing a definition Oak itself has no
  editor pairing for.

**Recorded departures from Oak.**

* *0703's omitted `:count_*` properties.* Oak's mirror strategy keeps
  approximate counters seeded from a random number; froe omits them. Two
  rebuilds of the same content disagree on them by construction, so they
  are excluded from every comparison — and a mirror index's
  `estimatedCost:` derives from them, which is why plan text is compared
  only where it carries no counter-derived number.
* *0705's 32-bit seed draw.* froe narrows the seed to 32 bits sign-extended
  on every run after the creating one, matching what Oak's own editor
  produces.
* *The `--from-head` reset.* A counter on an unresolvable lane is reset
  rather than rebuilt, because Oak's replay after a lost checkpoint doubles
  a rebuilt counter whether or not froe ran.

**Refusals, each stated rather than implemented.** `lucene` (until plan
0010), `elasticsearch`, `disabled`, `ordered`, an unknown type, a
`valuePattern` regular expression, a composite mount's index data, an
unconstructable `PathFilter`, a nested definition, a node that is not an
`oak:QueryIndexDefinition`, a lane mid-run at `/:async/async-reindex`, a
dangling lane checkpoint or absent lane without `--from-head`, a
multi-valued `reindexCount`, a duplicate unique key, and a hybrid counter.

**Interoperability.** The `property_reindex` phase's query-level comparison
and counter-reset scenario have not run; see the interoperability record
above.

**Verification axes not exercised in this range.** The MSRV host gate, the
i686 width sentinel, and `scripts/interop-fixture.sh` as a whole chain.
**Standing environment axes**, unchanged from earlier plans: no AEM build,
no external blob store, no local macOS execution (CI's `macos-latest` job
is the authority), and no native Windows execution.

**Addendum, 2026-09-15 — the three axes this range recorded as unexecuted
have since run**, on the cumulative tree at the `v0.12.0` release
candidate rather than on this range's own head, which is what they can
honestly be said about:

* *The MSRV host gate.* A `1.89.0-x86_64-unknown-linux-gnu` toolchain is
  installed on this host now. `fmt`, `clippy`, `test`, `test --release`
  and `doc` each exited 0 under `cargo +1.89`.
* *The i686 width sentinel.* `check` and `clippy` for the `froe` package,
  `+stable` and `+1.89`, all four exit 0 with `RUSTFLAGS="-D warnings"` —
  **compilation for a 32-bit target, not execution on one**. The
  workspace-wide form still fails in `zstd-sys`.
* *`scripts/interop-fixture.sh` as a whole chain.* `generate` through
  `recover` reached the completion sentinel in 909 seconds, the
  `property_reindex` phase among them.

The `property_reindex` phase's query-level comparison and counter-reset
scenario, recorded above as not run, have also both run — see this plan's
`STATUS.md`, which records what each of them found in this plan's own
code.


#### Review

**This task is gated, and the gate has not been closed.** What follows is
the review work, which is not gated. The freeze itself — declaring the
range complete, and lifting the beta framing from `docs/index.md` §5 and
the feature map's two rows — is the maintainer's decision, and it should
not be taken while the interoperability record above has an open half.

**Lenses run over the range, by passes that did not author it.**

*The state-root decision: can any path index an async definition from the
wrong state?* **Finding, confirmed and fixed.** Selection never read
`indexing_mode`, so a hybrid definition — one listing `sync` beside its
lane name, which Oak's synchronous cycle maintains on every commit as well
as the lane — was rebuilt from the lane's checkpoint. Every entry committed
since was silently dropped: a quietly incomplete index, where queries
return fewer rows and nothing reports an error. Fixed by indexing a hybrid
from the head and refusing a hybrid counter, with both rows added to the
guards table and the head decision's neutralization recorded.

*Reclaimability and identity preservation: can a rebuild drop a property or
a visible child of a definition?* **No finding.** `rewrite_definition`
names only hidden children in its child edits and only the bookkeeping
properties in its property edits; every other slot is preserved by record
identity through the one commit-path write. The empirical evidence is
stronger than the reasoning: the interop comparison held over 23 real
definitions, `reindexCount` and hidden-child presence included.

*Interruption prefixes against the code.* **Finding, fixed during 0708.** A
returned error left the session's archive without its trailers, so the next
`froe compact` refused the whole store as damaged until an operator
authorized an index repair — contradicting the mutation table's own
reconciliation column. The session is now closed on every path, and a run
that failed after `compare_and_set_head` puts the head back before closing.

*Evidence wording.* **No finding that changes a claim.** Every "observed
failing result" in the guards table quotes a failure produced while that
guard was neutralized; six rows are defence in depth and say so; carved-out
rows state their reason rather than implying coverage. The one wording risk
checked specifically: the mount-fragment row quotes the missing refusal,
which is what was observed, and describes the data loss as what the test's
second half states rather than as something observed.
