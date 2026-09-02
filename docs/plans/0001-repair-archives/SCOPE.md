# Scope — Plan 0001

> Let the maintenance run rebuild the index of an active archive that has none, so a store a killed writer left behind can be maintained offline without first handing it back to Oak, with every original byte retained and the step gated by an explicit authorization.

## Why this plan

Oak rebuilds a missing archive index on its own next start: `TarReader` falls back to a full scan of the file and writes the index back. froe refused such a store outright, which left segment reclamation unreachable on precisely the stores that most needed it, the ones a killed writer had just left behind. The change is high-risk under `docs/high-risk-changes.md` because it introduces destructive behaviour on a path that previously refused, so it was the first range in this repository to carry a safety case, and that case's largest admitted gaps (no guard-neutralization evidence, no abrupt-exit fault harness) set the bar the later plans were held to.

The range landed over 2026-08-13 and 2026-08-14 as three commits: the mechanism, a Windows build repair, and the interoperability phase that the safety case rests on.

## In scope

- **The repair mechanism.** Survey index-less archive numbers from the physical listing under the lock with the same repairability predicate the preview derives from its open readers; scan each letter for tar headers; rebuild the index into `<archive>.recovering`; reopen and validate the staged file; install it by hard link (a copy where the filesystem has none) while retiring every original letter to `.bak`; and only when a rebuild is about to become visible, upgrade `manifest` from `store.version=1` to `2` atomically.
- **Authorization and exclusion.** `RepairArchives` is opt-in, and `validate_options` refuses it beside `recovery-backups` so the run that creates the only copy of unrecoverable bytes cannot also delete it.
- **Guards with named regressions.** Manifest upgrade gated on a pending repair, store version checked before repair, cross-number duplicate segments refused, unrepairable numbers refused before any rewrite, staging residue never clobbered, zero-length letters skipped.
- **Interoperability.** The `repair` phase: Oak's own JVM killed with `SIGKILL` while holding an archive, exit 137 asserted, exactly one index-less archive confirmed, repaired by froe, then Oak booted against the result serving the byte-identical baseline tree and logging none of its own repair messages.
- **The safety case**, written in the shape `docs/high-risk-changes.md` prescribes.

## Out of scope

- An abrupt-exit fault harness at each cutpoint and disabled-guard runs for each guard; both were recorded as gaps and addressed by the fault-injection work of plan 0004.
- Narrowing the whole-run refusal when one archive is unreadable; fail-closed by policy.
- `store.version=1` stores beyond the one-way upgrade, external blob stores, and Adobe AEM itself.
