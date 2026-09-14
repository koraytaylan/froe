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
| Spill files written in the run's subdirectory under `--work-directory`, which `apply` creates before the first spill (after `open_prepared`; a cancelled confirmation leaves nothing behind); `index-reindex.before-spill-cleanup` fires once the sort has returned its iterator and before the first index record is appended, with every spill file still on disk | plan replanned under the lock; residue policy evaluated in the plan (the plan refuses a froe-named subdirectory left by an earlier run in an operator-named directory and warns about one under the default) | files outside the store only | subdirectory removed on return — `a_spill_failure_removes_the_run_subdirectory_and_leaves_the_store_unchanged` (0708) | leftover files in the run's subdirectory, never in the store — `a_death_before_spill_cleanup_leaves_no_file_in_the_store` (0708) | operator removes the subdirectory |
| Index records appended (`index-reindex.before-head-publish`), then the definition rewritten — hidden children replaced or dropped unless flagged `retainNodeInReindex`, `reindex` set to `false`, `reindexCount` incremented, `corrupt` removed, a counter's `seed` created when absent, `:disableIndexesOnNextCycle` under the disabler's verdict, a parked `async = async-reindex` property removed, every other property and visible child preserved by identity (under a reset: only the rewritten definition node, its hidden children gone, and the spine) | fresh archive number above every physical name, at the head's generation | new archives only, unreachable from the head | store unchanged plus unreferenced archives at the head's generation — `an_error_before_head_publish_leaves_the_head_and_every_definition_as_they_were` (0708) | same — `a_death_before_head_publish_leaves_the_head_resolving_the_old_records` (0708) | a later `froe compact` copies the live content into a fresh generation and retires every older archive, these included; they are never referenced by a checkpoint, having never been published (the superseded records of a *successful* run are the ones every lane checkpoint pins, which the Resources section states); they are not "interrupted-run residue" to the planner, which reserves that name for segments stamped *ahead* of the head |
| Head publication (`compare_and_set_head`, then `flush`; `index-reindex.after-head-publish-before-flush`) | every new subtree verified through the open session; the store was opened with `open_prepared` | `compare_and_set_head` changes nothing on disk; `flush` seals and fsyncs the archive, syncs the directory, validates the finalized session, then appends one journal line naming the new head | old head before the journal append, new head after it, never partial — `an_error_after_head_publish_before_flush_leaves_the_journal_naming_the_old_head` (0708) | same — `a_death_between_head_publish_and_flush_leaves_one_resolvable_head` (0708) | either head resolves; the loser's records are garbage |
| Applied-state verification (`index-reindex.before-applied-verification`) | head published | none | reports the mismatch, store already final — `a_failed_applied_state_verification_reports_rather_than_repairs` (0708) | not applicable — the head is published and durable before this boundary | rerun the check |

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
regression test the task that arms its cutpoint will add; task 0708 fills
those names in as it arms them.

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

| Guard and production callers | Named regression | Neutralization | Observed failing result |
| --- | --- | --- | --- |
| *To be filled by task 0709, one row per newly introduced or semantically changed refusal, preservation or publication guard, reaching every materially distinct production caller.* | | | |

#### Fault and subprocess tests

| Cutpoint | Fault model | Named test | Asserted prefix |
| --- | --- | --- | --- |
| *To be filled by task 0708 as it arms each cutpoint named in the mutation table above.* | | | |

#### Interoperability

*To be written by task 0714, recording the interop run of task 0712: the Oak
build, the image digest, and what Oak's own reindex of the same store agreed
with, compared with only the randomized approximate counters excused.*

#### Verification report

*To be written by task 0714: what was run, on what, with which results, and
what the coverage of the fault tests is and is not.*

#### Known gaps

*To be written by task 0714.*

#### Review

*To be written by task 0714, which freezes the range.*
