# Maintenance remediation: closing every gap the sgcp-aem-0001 runs exposed

Status: reviewed — two adversarial passes (21 findings) incorporated · Date: 2026-08-19 ·
Root-cause analysis: `/tmp/froe-compaction-rca.md`
(field runs of froe 0.10.0 against a 21.8 GiB AEM segment store; ten numbered observations).

## 1. What this plan must resolve

Two consecutive `froe compact --yes` runs on a production AEM store delivered ~0 savings in
~41 minutes each, while the operator knew the store carried tens of gigabytes of version-storage
garbage. The root causes were established and reproduced locally; none of them is store damage.
This plan turns each finding from "explained" into "fixed". The observations, restated as the
problems they are:

| # | Problem |
|---|---------|
| 1 | The run cannot deliver savings on this store: the reclaimable content is semantically orphaned but structurally reachable, and the run churns a 6.34 GiB generation swap for nothing. |
| 2 | Node count never drops; the garbage the operator cares about (orphaned version histories) is untouched, and the run never says why. |
| 3 | "opening archives" runs five times per apply. |
| 4 | "verifying the current head" runs four times per apply (~13 of ~41 minutes). |
| 5 | After reclamation, the open+verify pair runs twice back to back. |
| 6 | "predicting the shared binary content" reports no result — the one number (15.0 GiB shared in place) that explains the whole store. |
| 7 | "predicting the reclamation" reports no result; the post-confirmation re-analysis computes predictions and discards them unseen. |
| 8 | "estimated reclaimable" is wrong twice over: it omits the copy's own output (over-states), and it says nothing about the external blob store where the operator's 121 GiB actually lives. |
| 9 | A second run finds a full generation to reclaim — structurally inevitable today, and indistinguishable from unfinished work because the run never states that the store is already compact. |
| 10 | 31,473 orphaned version histories (proven by the operator's export query) survive every run; the merged command's promise of "one maintenance command" is not met for them. |

Constraints this plan honors:

* The repository's decided deviations stand (every-reclaimable-archive rewrite policy; one retained
  generation with the per-run reclaim-reference invariant).
* The workspace standards stand: no abbreviations, minimal dependencies, zero warnings,
  independent-implementation tests where a second decoder can check the first.
* The safety case (`docs/compact.md`) remains the contract; where a phase changes behavior, this
  plan names the exact sentence that must change and what replaces it.
* Every phase ends with acceptance criteria phrased as observable behavior, most of them against
  the sgcp-aem-0001 store shape, which the reproduction fixture models.

## 2. Shape of the work

Five phases, each independently shippable and testable, ordered so that the cheap
observability wins land first, the pipeline gets simpler before it gets a new feature, and the
one content-mutating feature arrives last on top of the cleaned-up structure.

| Phase | Title | Resolves |
|-------|-------|----------|
| 1 | Say what the run knows | 6, 7, 8 (reporting half of 1) |
| 2 | One lock, one plan, verification where it protects | 3, 4, 5 (and the structural half of 7) |
| 3 | Convergence: a run with nothing to do does nothing | 9 (churn half of 1) |
| 4 | Semantic garbage collection: orphaned version histories | 10, 2 (savings half of 1) |
| 5 | Field validation and the sgcp-aem-0001 runbook | 1, 8 (the 121 GiB question), interop gaps |

---

## Phase 1 — Say what the run knows

The analysis already computes every number the operator needed; it prints none of them. This
phase is output-only: no store-touching behavior changes, so it ships first and de-risks
everything after it (all later acceptance criteria read these lines).

### 1.1 Step results

* `predict_shared_bulk_segments` (`crates/froe/src/writer/maintenance/reclamation.rs:123`)
  returns a set today. Print its summary when the step completes:
  `predicting the shared binary content: … ; 73,967 pre-existing bulk segments (15.0 GiB) will be shared in place and retained`.
* `trace_head_closure` (`maintenance/planning/segments.rs:21`) knows the closure; join it with the
  archive indexes it already loaded and print the composition:
  `tracing segments reachable from the head: … ; the head reaches 6.3 GiB of node data and 15.0 GiB of shared binary blocks`.
* `predict_post_compaction_reclamation` (`planning/segments.rs:138`) produces the sweep plan.
  Print its totals when the step completes:
  `predicting the reclamation: … ; the sweep removes 26 archives (6.3 GiB) and rewrites 0 archives (0 bytes of entries)`.

### 1.2 An honest estimate

`estimated_reclaimable_bytes` (`maintenance/planning/listing.rs:144-209`) sums removals and
rewrite-eligible entry bytes and never subtracts the generation the copy writes. Replace the
single figure with three, printed in the plan footer:

```
the copy writes ≈6.3 GiB into generation (925,346,compacted)
the sweep reclaims ≈6.3 GiB of archives and entries
estimated net change: ≈0.0 GiB
```

`predicted_copy_output_bytes` = the summed index entry sizes of the data segments in the head
closure, minus the exclusive bytes of any subtree the copy omits (checkpoints today, purged
version histories after Phase 4). For a head froe already compacted this is exact (the copy is a
deterministic fixed point, observed in the field as 6.34 GiB → 6.34 GiB); for a fragmented head
it is an upper bound (the dense copy writes less); for the *first* compaction of an Oak-written
dense head the copy additionally writes stable-identifier blocks (≈24 bytes per node), so the
line prints `≈` and the bound direction rather than pretending precision. Keep `estimated_reclaimable_bytes` as an
internal field; the printed contract becomes the three-line form.

### 1.3 The external blob store, named

The operator's 121 GiB expectation lived in the datastore, which compaction can never touch, and
nothing in the output said so. During the planning verification walk (which already reads every
property), sum external binary references: Oak blob identifiers carry a `#<length>` suffix in the
common `FileDataStore`/`OakFileDataStore` format; sum lengths where the suffix parses, count-only
where it does not. Print once in the plan footer:

`content references 31,404 external binaries (≈118.7 GiB) in the blob store; compaction never affects those bytes — that space is reclaimed by blob-store garbage collection after content deletion`

### 1.4 Documentation and tests

* `docs/cli-output.md` gains every new line with its stability contract.
* CLI tests assert each line against fixtures where the numbers are known exactly
  (the reproduction store: shared bulk 0, copy output 1.5 MiB, net ≈ +0.4 MiB on the first run —
  the acceptance test for the estimator is precisely the case that today prints
  "estimated reclaimable: 1.1 MiB" and then grows the store).
* Independent-implementation check: the external-binary total must equal the DuckDB sum of
  `binary_reference` lengths over `froe export` output for the same fixture.

Acceptance: observations 6 and 7 have result lines; observation 8's estimate is sign-correct on
the growth fixture and names the blob store explicitly.

---

## Phase 2 — One lock, one plan, verification where it protects

### 2.1 Problem

The apply path runs the full analysis twice (read-only preview, then authoritative replan under
the lock: `froe-cli/src/mutation.rs:124` and `:150`) and verifies the head four times (preview,
replan, journal phase `apply/journal_phase.rs:28`, final state `apply/compaction_phase.rs:220`).
On sgcp-aem-0001 that is ~10.5 minutes of a ~41-minute run re-deriving what the same locked
process derived minutes earlier. Five archive opens are the visible symptom.

### 2.2 Design

* **Apply acquires the lock first, plans once, confirms while holding it.** The preview/replan
  split existed to avoid holding the lock during operator think-time, but the store is offline by
  precondition, and holding `repo.lock` during confirmation is *stronger*: an accidentally
  started Oak cannot open the store mid-confirmation. `--dry-run` keeps the lock-free read-only
  path unchanged. The "authoritative plan differs" flow (`announce_authoritative_plan`) disappears;
  `--repair-archive-indexes` runs its repairs under the same lock before the single planning pass,
  so the plan the operator confirms is always the plan that applies. The directory fingerprint
  check between plan and apply stays as a cheap belt inside the locked session.
* **Verification moves to where it protects.** Today's four full walks include *none* between
  the copy and the reclamation — the only window in which a defective copy is still fully
  recoverable, because every source archive is untouched until the sweep starts
  (`apply_compaction_phase`: copy → head swap → reclaim, first full walk only afterwards). The
  two post-reclamation walks arrive after the store they could have saved is gone. The new
  arrangement: a **pre-publication walk of the fresh copy** — reported as
  `verifying the compacted copy` — runs through the open writable session after the copy
  finishes and *before the head swap itself* (the implementation strengthened this beyond the
  reviewed draft, which placed the walk after the swap), so a copy defect refuses the run while
  the journal still names the old head and recovery is nothing at all; the
  journal phase's post-hoc full walk is dropped (its raw-journal byte checks,
  `retained_compacted_head_line` and `verify_retained_journal_lines`, stay exactly where they
  are); the final fresh-reopen walk in `verify_applied_state` remains the proof of the published
  store. Three full walks per run — one fewer than today, and for the first time the dangerous
  window is covered.

Target pipeline per apply run: open (plan, under lock) → `verifying the current head` (walk #1)
→ analysis → plan printed → confirmation → writable open (apply) → copy →
`verifying the compacted copy` (walk #2, before any unlink) → reclaim → journal rewrite →
fresh reopen → `verifying the current head` (walk #3) → summary. Three opens, three full walks,
each with a distinct job.

### 2.3 Safety-case changes

`docs/compact.md` §"After taking the lock and rebuilding the authoritative plan…" is rewritten:
there is no rebuilt plan; the section instead states that planning, confirmation and apply happen
under one continuously held lock and one directory fingerprint. The crash-safety argument in
`apply/compaction_phase.rs:35-42` is untouched (copy is additive, head publishes before any
unlink). Fault-injection tests (`writer/fault_injection`) gain publication-boundary probes: an
injected error and a process death between the copy's verification and the head's publication
each leave the store additive-only and reopening at its old head — including a purging run,
whose interrupted copy must leave every history resolvable. The journal-truncation window keeps
its existing probes: after the rewrite the store is already the final store, so a crash before
the final verification loses only the verification, which the next open performs anyway.

### 2.4 Acceptance

On the reproduction store, an apply run's `--progress always` transcript contains exactly three
`opening archives` lines (four when a manifest upgrade adds its pre-apply certification pass,
`maintenance/apply/mod.rs:63`), exactly two `verifying the current head` lines, and exactly one
`verifying the compacted copy` line; wall-clock on sgcp-aem-0001 drops by ≈7 minutes (the replan
analysis disappears; one redundant post-reclamation walk is traded for the protective
pre-reclamation walk); the confirmation prompt appears after the only plan print; the
fault-injected corrupt copy refuses before any source archive is touched. Observations 3, 4, 5
close; observation 7's discarded re-computation is structurally gone.

---

## Phase 3 — Convergence: a run with nothing to do does nothing

### 3.1 Problem

Full compaction unconditionally copies the head and reclaims the previous generation. On an
already-compact store that is a 6.34 GiB, ~22-minute no-op that then *presents* one generation of
"reclaimed" archives, which reads as leftover work (observation 9). The planner already has a
no-copy path (`options.compaction_kind == None` routes to `plan_standalone_segments`,
`planning/segments.rs:222`); nothing selects it automatically.

### 3.2 Design

Introduce a planner decision, `compaction_disposition`, computed from state the planner already
holds:

**AlreadyCompact** if and only if all of:
1. every data segment in the head closure carries the head segment's own generation triple with
   `is_compacted` set (triple equality against the index generations, never ordering and never a
   store-wide maximum — old Java-written archives can carry version-1-index synthesized full
   generations far ahead of the real ones, generation arithmetic wraps, and killed-run residue
   stamped ahead of the head must neither block the gate nor survive it: the existing residue
   sweep retires it in the same no-copy run);
2. no checkpoint is selected for omission this run (expired or unreferenced, after policy);
3. no version history is selected for purge (Phase 4);
4. `journal.log` already holds exactly one line naming the head — a belt only, since any write
   after a compaction already fails condition 1: non-compacting writes stamp the head generation
   with the compacted flag cleared (`store_writer/repository/writes.rs:211`).

Otherwise **Copy**. (As built, a `--tail` run uses the same triple-equality
predicate rather than a pair-analogous one: tail output leaves the retained
full generation's segments carrying their old triples, so a tail run only
ever gates on a store that is already *fully* compacted — the conservative
direction, since the gate's only power is to drop a copy.)

When AlreadyCompact, the copy is dropped from the plan and the remaining garbage — orphan
pockets, stale archive letters, stale temporaries, recovery backups under policy — routes through
the existing standalone sweep. If that plan is also empty, the existing empty-plan path prints:

`the store is already fully compacted; nothing to do`

and exits without mutation. A run that does copy ends its summary with the forward-looking
statement that makes observation 9's confusion impossible:

`the store is now fully compacted; a repeat run will report nothing to do`

Escape hatch: `--always-copy` forces a fresh generation regardless (operator wants a rewrite,
for example after suspected mapping-level corruption). Flag documented as never needed for
space.

### 3.3 What the gate must never do

The gate skips only the *copy*. Any store where today's planner produces a non-swap action
(rewrite, removal, stale letter, temporary, backup, journal retirement beyond one line,
checkpoint expiry, purge) must produce the same action under the gate. Property test: for a
corpus of fixture stores, plan-with-gate equals plan-without-gate minus exactly the
{copy, previous-generation sweep, journal single-line rewrite} triple, and only when the four
conditions hold. The reclaim-reference invariant continues to run in both dispositions.

### 3.4 Acceptance

Second run on the reproduction store: no mutation, exit 0, the fully-compacted line, ~7 minutes
on sgcp-aem-0001 (analysis and verification only — verification is the product's assurance and
stays; a faster no-op is explicitly out of scope). First run's summary carries the
forward-looking statement. Observation 9 closes; observation 1's churn half closes.

---

## Phase 4 — Semantic garbage collection: orphaned version histories

### 4.1 Problem and stance

31,473 `nt:versionHistory` subtrees under `/jcr:system/jcr:versionStorage` have a
`jcr:versionableUuid` that matches no live `jcr:uuid`. They are reachable, so no structural
garbage collector — Oak's or froe's — may touch them, yet they are garbage by the only
definition the operator cares about, and they pin ~15 GiB of shared binary blocks plus external
blob references. If `froe compact` is the one maintenance command, this belongs in it as a
*named, confirmed, explicitly semantic* phase — not smuggled into structural GC.

### 4.2 Detection (always on, both dry-run and apply)

Detection is two ordered passes, so the memory bound holds regardless of tree-visit order.
As built the *content* pass runs first — adversarial review found that certifying version
storage first lets a record shared between live content and a frozen subtree be memo-skipped
out of the live walk, silently orphaning a live history — so the order is:

* the main planning verification walk covers the head's content tree first, *pruned by path* at
  `/jcr:system/jcr:versionStorage`, recording every live `jcr:uuid` it sees — identifiers
  normalized to lowercase on both sides, so the set and the independent oracle (§4.6) cannot
  diverge on case;
* a version-storage pre-scan — the same verifier continuing into exactly what the pruning left
  out — collects every `nt:versionHistory`'s `jcr:versionableUuid` parsed to a 16-byte
  identifier, together with each history's subtree node count (the count the report and the
  summary need). On the field store this subtree is ≈3.5M nodes, ≈36 seconds at observed walk
  rates. A third short walk (`verifying the checkpoints`) then covers the super-root and
  checkpoint snapshots, cheap because the memo already holds what they share with the head.
  Live matches are resolved only after both censuses exist, so registration order cannot
  matter.
  Resident memory is therefore bounded by the *history* count — 16 bytes plus map overhead per
  history, ≈40 MiB per million histories — never by the store's referenceable-node count. The
  bounded-memory safety case
  (`docs/plans/bounded-memory-and-journal-retention-safety-case.md`) gains a paragraph stating
  this bound. Malformed identifier strings are skipped and counted into a warning.

What remains after the walk is the orphan set. The plan always reports it, and every figure in
the report is either exact or carries its bound direction (illustrative numbers):

```
orphaned version histories: 31,473 (their versionables no longer exist)
  holding 612,598 nodes, 8.2 GiB of inline binary content, and 28,911 external binary references
  a purge releases up to 33,102 bulk segments (8.1 GiB) and about 1.4 GiB of node records (realized by the copy)
  purge with --purge-orphaned-version-histories; removal is permanent and is listed above when selected
```

(As built, three refinements to that block. The bulk figure is printed as `up to` — a ceiling —
because adversarial review proved the per-record attribution over-counts when a record is shared
between a purged history and a kept one, which Oak's writer-side deduplication of identical
frozen subtrees makes plausible on real stores; a retained checkpoint's snapshot can additionally
pin blocks, and the line names the checkpoint count when that applies. When a purge is
*selected*, both the bulk figure and the node-record estimate describe that selection rather
than the full orphan set — a history the run keeps is not a saving the run delivers. And the
plan's predicted copy cost is reduced by exactly the node-record estimate. The sweep itself
frees exactly what is unreferenced regardless of the prediction.)

How each figure is computed — and why a segment-difference is deliberately *not* the estimator:
on a compacted store, orphan-history records interleave with live records inside the same
segments (the deep copy packs subtree boundaries and deduplicated templates and strings
together), so "segments reached only through orphans" is nearly empty and a segment-granular
difference collapses toward zero while the real saving is realized by the copy simply writing
those records no more. Instead:

* history, node, and external-reference counts are exact (pre-scan and property reads);
* inline binary content is the sum of the orphan subtrees' inline binary property lengths —
  exact, and the same semantics as the operator's export query;
* released bulk segments are the bulk blocks referenced only by purged-history binary values,
  computed by tagging bulk references live-versus-orphan during the walks. As built the figure
  is an *upper bound*, not a floor: attribution is per certified record, and a record shared
  between a purged history and one the store keeps (Oak's writer dedups identical frozen
  subtrees) is only ever attributed to the first history the walk certifies it under, so its
  blocks can be counted releasable when the kept history still holds them. A block shared with
  a live binary's blocks stays either way; the sweep frees exactly what is actually
  unreferenced, whatever the plan predicted;
* the node-record saving is a scaled estimate — the orphans' node count times the head's
  average data bytes per node — labeled "about" and realized exactly by the copy that runs
  (implementation refinement: per-record byte sums would require record-extent arithmetic the
  figure does not need; the scaled estimate serves the same plan-time purpose and the summary
  reports the realized delta).

### 4.3 Removal (explicit opt-in)

`--purge-orphaned-version-histories` adds plan lines naming the action with counts and the same
irreversibility warning style the journal retirement uses. With `--dry-run` the combination
stays strictly read-only: the purge actions are listed in the plan, no confirmation is asked, no
lock is taken. Implementation reuses the existing
copy-time omission mechanism: checkpoint omission already declines to enter subtrees during
`deep_copy_super_root_with_progress` (`writer/compaction/walk.rs:96-105`); generalize the
omission set from "child names of the checkpoints container" to "a set of subtree roots",
populated with the confirmed orphan history nodes; the plan object carries that root set — and,
learned in implementation, the *context-dependent ancestor* records beside it: the chain from
the content root down to each omission point is memoized per scope inside the copy, because a
subtree shared between the head and a retained checkpoint would otherwise leak one scope's
shape into the other through the copy's memo, whichever the walk reached first (the
checkpoint-scoping test caught exactly that). Both the apply and the digest-exclusion
derivation read the plan's sets. After omission, prune
version-storage intermediate nodes (`xx/yy/zz`) left childless — recursively, under version
storage only — so the copy does not carry millions of empty directory nodes. Purge selection
forces the Copy disposition (Phase 3 condition 3), and the flag refuses to combine with
`--tail`: tail reclamation retains the shared full generation, so the released segments could
survive the very run that promised to remove them — full compaction is the purge's vehicle.

One optional narrowing flag ships with it:

* `--purged-history-minimum-age-days <days>` — purge only histories whose newest version was
  created at least this long ago (age from the version's `jcr:created`). Unset means no age
  bound. This guards the mistaken-recent-delete window: content deleted minutes ago and about to
  be restored from a package re-attaches its history by identifier, and an age bound keeps such
  histories out of the purge set without the operator curating identifiers by hand.

Why opt-in rather than default-on: recreating a versionable with its old identifier (a content
package reinstall) re-attaches the surviving history by identifier match; purging forfeits that
re-attachment. That behavior difference is invisible to a structural check, so the operator must
choose it. The plan report (4.2) is always printed precisely so the flag gets discovered.

### 4.4 Safety rails

* **Advisory inbound-reference scan.** Purge candidates are only known when the main walk ends,
  so intersecting them with inbound references cannot ride that walk without retaining every
  REFERENCE value in memory. Instead, purge-flagged runs make one dedicated property-only pass
  *after* candidate determination: it reads REFERENCE- and WEAKREFERENCE-typed values outside
  version storage and matches them against the candidates' internal identifiers (history and
  version `jcr:uuid` values, collected during the pre-scan — bounded by the candidate set, ≈10
  MiB at the field store's shape). Any hit demotes that history from the purge set with a
  per-history warning. Oak does not enforce referential integrity, so this is best-effort
  protection for custom applications that store version references — safe by default,
  overridable never. Cost ≈2–3 minutes on the field store, paid only when the flag is given; the
  bounded-memory paragraph covers the retained set.
* **Retained checkpoints.** Orphan-ness is judged against the head alone, deliberately: a
  versionable alive only inside a retained checkpoint's snapshot does not protect its history,
  because the checkpoint carries its own version-storage snapshot and loses nothing. Such a
  history is omitted from the head but its bytes survive until that checkpoint retires, and the
  purge's plan action says so:
  `N retained checkpoints keep their own snapshots of these histories; that storage returns when the checkpoints expire`.
* **Scope, version 1.** Exactly `nt:versionHistory` under `/jcr:system/jcr:versionStorage`.
  Histories whose versionable identifier belongs to an `nt:configuration` are detected, counted,
  and excluded with a warning. `/jcr:system/jcr:activities` is untouched *by construction* — it
  is a different subtree than the one the purge walks, so as built there is no detection or
  warning for it, just as there is none for any other content the purge never visits. Retention
  pruning of *live* histories (keep the last N versions) is future work, not this plan.

### 4.5 Contract changes

* `docs/compact.md`'s digest sentence — "every node, property name, type, arity, value and
  binary checksum must be unchanged outside the checkpoints the run is meant to retire" — becomes
  "… outside the checkpoints the run is meant to retire *and the version histories the operator
  confirmed for purge*". The exclusion list handed to the digest comparison is derived from the
  confirmed plan, never hand-written.
* `froe digest` today has no notion of excluding a subtree, so the mechanism is a named work
  item, not an assumption: digest canonicalization accepts a set of exclusion path prefixes, the
  interop harness threads the plan-derived set through, and a digest with exclusions states them
  in its own header so two digests are never compared blind.
* The summary reports the content delta the operator asked about in observation 2:
  `nodes: 18,796,598 -> 18,184,000 (removed 31,473 orphaned version histories holding 612,598 nodes)`.

### 4.6 Tests

* **Independent-implementation oracle:** on a fixture with versionables created and then
  partially deleted, the planner's orphan set must equal the field query's logic implemented a
  second time. As built the orphan oracle applies that logic through the read-only content API
  inside the test (no DuckDB dependency in CI), and the *export* half of the original idea
  proves the external-binary census instead: distinct identifiers, measured bytes, and
  unmeasured references derived from `froe export` JSON lines must match the plan's printed
  census figures.
* **Interop:** Oak 1.90.0 writes versionable content, deletes some versionables; froe purges and
  compacts; Oak boots the result, serves the surviving tree, resolves version history for
  surviving versionables, and the before/after digests match modulo exactly the confirmed
  exclusions. A variant seeds large binaries so purged histories release shared bulk segments and
  the store measurably shrinks.
* **Safety-rail tests:** inbound REFERENCE demotion; checkpoint-shared history reporting;
  intermediate-node pruning leaves sibling histories intact; empty purge set produces a byte-identical
  store.
* **Crash injection:** kill between copy and journal rewrite with purge selected; the store still
  opens at the old head with all histories present (the copy is additive until publish — inherited
  argument, now tested for the purge path).

### 4.7 Acceptance

On the sgcp-shaped fixture: detection always prints the orphan report when there is anything to
report; with the flag, node count drops by the orphans' node count, the store shrinks by at
least the released-bulk figure (asserted as a lower bound — an exact upper bound would be
hostage to archive padding and alignment), Oak opens and serves the result, and a repeat run
converges to `nothing to do`. As built a store with zero orphans and zero malformed identifiers
prints no orphan block at all rather than an explicit `orphaned version histories: 0` — silence
about a non-finding, consistent with every other section of the plan. Observations 10 and 2
close; observation 1's savings half closes.

---

## Phase 5 — Field validation and the sgcp-aem-0001 runbook

### 5.1 Interop gaps this store exposed

`docs/compact.md` lists "external blob stores, and Adobe AEM itself" as unverified. The field
store is exactly that shape. Add a fixture whose content mixes inline binaries (bulk segments,
some shared, some dead) with external references carrying `#<length>` suffixes, and assert
Phase 1's composition and blob-store lines plus Phase 4's release of newly unshared bulk against
it. AEM-proper remains out of scope for automation; the runbook below is its manual counterpart.

### 5.2 Runbook (the plan for the actual server)

1. Upgrade froe on sgcp-aem-0001 to the release carrying Phases 1–4.
2. `sudo systemctl stop aem6`, then `froe compact <store> --dry-run`: confirm the report shows
   ≈31,473 orphaned histories and the composition lines match the RCA's figures
   (≈6.3 GiB node data, ≈15.0 GiB shared binaries, external footprint ≈ the datastore). AEM has
   run since the RCA snapshot (the export already showed 526 archives and a slightly different
   node count), so expect the figures to drift a little, not to match exactly.
3. Apply with `--purge-orphaned-version-histories --backup-minimum-age-days 0 --backup-keep-latest 1`
   (retires the 11.8 GiB of stale recovery backups in the same run). Expected: node count drops by
   the orphans' share, the store shrinks by the predicted segment bytes, summary ends
   "fully compacted".
4. Start AEM; run blob-store garbage collection (the purge unreferenced its external binaries —
   this is where the operator's 121 GiB figure actually shrinks).
5. Optional proof: a repeat `froe compact` reports `nothing to do` in ≈7 minutes and mutates
   nothing — observation 9's counter-demonstration.

### 5.3 Release mechanics

Planned as a 0.11.0 / 0.12.0 split; as built, all five phases land together as 0.11.0 — the
purge was implemented, reviewed, and interop-proven in the same range, so holding it back would
have shipped the version that reports the garbage without the version that removes it. Phase 2
reshapes the public `PreparedCompaction` flow (`prepare` now runs selected repairs and builds
the one plan under the lock; there is no advisory-preview API), marked with the repository's
breaking-change commit convention. Changelog entries per cliff conventions; `docs/compact.md`, `docs/cli-output.md`, `docs/interop.md`, and
the feature map updated in the phase that changes them.

---

## 6. Decision points (recommendation first)

1. **Purge default:** opt-in flag (recommended, §4.3) versus default-on-with-confirmation. The
   report is always-on either way; only removal needs the flag.
2. **`--always-copy` name** for the gate override (recommended) — alternatives: `--force-copy`.
3. **No-op runs still verify** (recommended): the ~7-minute floor is the product's assurance; a
   `--skip-verification` fast path is rejected as contrary to froe's contract.
4. **Confirmation under the held lock** (recommended, §2.2): a deliberate UX change, documented.
5. **Purged external blob identifier list** (`--emit-purged-blob-references <file>`): deferred;
   blob-store garbage collection discovers them independently. Revisit if operators want
   targeted datastore verification.

## 7. Risk register

| Risk | Mitigation |
|------|------------|
| Identifier-set memory on giant stores | Bound is 16 bytes × version histories (not referenceable nodes, §4.2); measured on the 18.8M-node fixture; bounded-memory safety case updated. |
| Purge removes something an application needed | Opt-in flag, always-on plan report, advisory inbound-reference demotion, irreversibility warning, backup guidance unchanged. |
| Pipeline restructure breaks the repair flow | Repairs run under the same lock before the single plan; the changed-plan reprint path is deleted with its tests replaced, not orphaned. |
| Moving the post-copy verification | The full walk moves *earlier* (pre-reclamation), where refusal is still recoverable; the raw-journal byte checks stay; fault injection proves both the corrupt-copy refusal and the `journal.log.bak` + `recover-journal` window (§2.3). |
| Gate wrongly skips needed work | Gate only ever removes the swap triple from a plan; property test over the fixture corpus (§3.3). |
| Estimate bounds mislead | Bound direction printed; sign-correctness test on the growth fixture (§1.4). |
| Digest-contract weakening | Exclusions derive mechanically from the confirmed plan; the interop digest still covers every byte outside them. |
| Generation arithmetic near wrap-around | Gate uses triple equality, never ordering (§3.2). |
| Purge under tail rules strands released bytes | The flag refuses `--tail` outright (§4.3). |
| A just-deleted versionable is purged moments before its restore | Optional age bound on the purge set (§4.3), plus the opt-in default. |

## 8. The matrix: every observation, its fix, its proof

| # | Mechanism | Phase | Proof |
|---|-----------|-------|-------|
| 1 | Purge releases the real garbage; gate ends the churn; estimator and composition lines tell the truth; runbook finishes the job at the datastore | 1,3,4,5 | Fixture store shrinks by predicted bytes; repeat run is a no-op; runbook step 4 |
| 2 | Purge plus node-count delta line | 4 | Summary shows the drop and names the removed histories |
| 3 | Single locked plan; merged post-verification | 2 | Exactly three `opening archives` lines per apply |
| 4 | Three purposeful walks: plan, pre-reclamation copy check, published store | 2 | Exactly two `verifying the current head` lines plus one `verifying the compacted copy` line |
| 5 | Journal phase keeps byte checks only; one reopen+walk after the rewrite | 2 | A single open+verify pair follows reclamation |
| 6 | Shared-bulk result line | 1 | Line asserted in CLI tests; documented |
| 7 | Sweep prediction result line; replan deleted | 1,2 | Line asserted; no post-confirmation analysis exists |
| 8 | Net-change triple; external blob footprint line | 1 | Growth fixture prints ≈0/negative net; blob line matches export-derived sum |
| 9 | Convergence gate; forward-looking summary line | 3 | Second run: `nothing to do`, zero mutations |
| 10 | Semantic detection always on; confirmed purge; digest-modulo-exclusions; interop proof | 4 | Orphan set equals the export-SQL oracle; Oak boots the purged store |

## 9. Explicitly out of scope

Blob-store garbage collection itself (Oak/AEM runs it); retention pruning of live version
histories; online operation; changes to index version 1 generation synthesis (verified during
the RCA as matching Oak's own `SegmentDataV12`/index-entry semantics, and not operative here).
