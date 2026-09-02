# Scope — Plan 0005

> Close every gap two field runs of froe 0.10.0 against a 21.8 GiB AEM segment store exposed: say what the run knows, plan once under the lock and verify where it protects, converge on an already-compact store, purge orphaned version histories as a named semantic phase, and prove all of it against a real Oak.

## Why this plan

Two consecutive `froe compact --yes` runs on a production AEM store delivered about zero savings in about 41 minutes each, while the operator knew the store carried tens of gigabytes of version-storage garbage. The root-cause analysis produced ten numbered observations, none of them store damage: the reclaimable content was semantically orphaned but structurally reachable, the run churned a 6.34 GiB generation swap for nothing, it re-derived under the lock what it had derived minutes earlier, it computed the numbers that explained the store and printed none of them, and its estimate was wrong in sign. The plan was reviewed adversarially twice (21 findings incorporated) before implementation.

Five phases were planned as a `0.11.0` and `0.12.0` split; as built, all five landed together as `0.11.0` over 2026-08-19, followed by the file splits the line gate demanded, the `--skip-*` rework that made every extra cleanup a default-on question, and the interop-suite hardening that a one-checkpoint fixture and a failed Sling post forced.

## In scope

- **Phase 1, say what the run knows.** Step results for shared bulk segments, head composition and the predicted sweep; the three-line net-change estimate; the external blob store named with its footprint.
- **Phase 2, one lock, one plan.** Apply acquires `repo.lock` first, plans once, confirms while holding it; the full walk of the fresh copy runs before the head swap, where a defect is still free to refuse; three opens and three walks per run, each with a distinct job.
- **Phase 3, convergence.** `compaction_disposition` gates the copy on triple equality of every head segment's generation with the compacted flag set, no checkpoint or history selected, and a one-line journal; `--always-copy` as the escape hatch.
- **Phase 4, orphaned version histories.** Two-pass detection bounded by the history count, an always-on report with bound directions stated, a confirmed purge realized by copy-time omission with context-dependent ancestors memoized per scope, the advisory inbound-reference demotion, the age bound, and `froe digest --exclude-subtree` so the interop comparison excludes exactly the confirmed set.
- **Phase 5, field validation.** The interop phases for the gate and the purge against Oak 1.90.0, the fixture with external references, and the runbook for the field store.
- **After the plan.** The line-gate splits, the `--skip-*` rework superseding decision point 1, and the interop-suite fixes.

## Out of scope

- Blob-store garbage collection itself, retention pruning of live version histories, online operation, and index version 1 generation synthesis.
- A faster no-op run: the verification floor is the product's assurance and stays.
- `--emit-purged-blob-references`: deferred, blob-store garbage collection discovers them independently.
