# Architecture — Plan 0001

## 0001 — Archive Index Repair

### Record of a landed range

This plan landed before `docs/plans` adopted the Makina layout. It was restructured into that layout on 2026-09-02 so that every plan in this directory reads the same way: the tasks under `tasks/` were reconstructed from the commits that landed them, each closed with `status: done` and its landing commit in `merged_as`, and the original document is carried below as the frozen record. Nothing here is executable input; the range is history.

### Design summary

**Ordering is the argument.** A rebuild never overwrites in place: the replacement is staged as `<archive>.recovering`, validated by reopening it, and installed while every original letter is retired to `.bak`, with the install target's own original preserved through a hard link so a `.tar` under the target name exists at every instant. The manifest upgrade to `store.version=2` is the last durable change and happens only when a rebuild is about to become visible. Every prefix of an interrupted run therefore keeps the original bytes under some name, and the repair's own damage is always reversible by renaming a `.bak` back; the segment sweep it unblocks is not, which is why it stayed a separate task selection.

**One predicate, stated once.** "Is this archive number repairable" is derived by the preview from the open readers and by the locked path from the physical listing, both from the same scan's entry count. An earlier revision let the two disagree, and the divergence made a doomed run pay for durable rewrites first; the shared derivation is the fix and the reason the safety case states the predicate once.

**Footprints.** The `touches` below are the paths at landing time. Plan 0004's module splits moved most of them: `crates/froe/src/writer/cleanup.rs` became `crates/froe/src/writer/maintenance/**`, `store_writer.rs` became `crates/froe/src/writer/store_writer/**`, and the interop suite moved from `crates/froe-cli/tests/interop.rs` to `crates/froe-cli/tests/interop/`.

### Task graph

```
0101 rebuild index-less archives ──▶ 0102 keep it building on Windows ──▶ 0103 close the interop gate, write the safety case
```

### The frozen document

The safety case below was committed in `eaf3308` and amended in `af418bd` (plan 0005), which recorded the authorization surface after the `--skip-*` rework. It is carried here verbatim apart from heading levels and relative links.

---

## Safety case: `--repair-archive-indexes`

> Authorization surface as of the `--skip-*` rework: the repair is part of
> every run that needs one, gated by its own yes/no question (`--yes`
> answers it, `--skip-repairing-archive-indexes` declines it), and
> `--repair-archive-indexes` lives on as a hidden pre-authorizing
> compatibility spelling. The mechanism, ordering, and guarantees analyzed
> below are unchanged.

The artifact [`high-risk-changes.md`](../../high-risk-changes.md) requires for
the opt-in `--repair-archive-indexes` stage, which rebuilds the index of an active archive that
has none. In scope because it introduces destructive behavior on a path that
previously refused, and because it unblocks segment reclamation on stores that
were unreachable to it.

Covers `crates/froe/src/writer/maintenance.rs` and
`crates/froe/src/writer/store_writer.rs` as of the commit introducing this
file.

### Scope and retention

**What must survive.** Every content root reachable from the persisted head;
every readable journal revision and its closure; every checkpoint; every
binary reference; the manifest; and — specific to this task — the exact bytes
of every archive letter the rebuild reads.

**What repair may change.** Only archive numbers with no valid-index letter
and at least one letter that yields a segment. Nothing else: the head is not
moved, the journal is not written, no checkpoint is touched, and no archive
that already has a valid index is opened for writing.

**Retention mechanism.** A rebuild never overwrites in place. The replacement
is staged as `<archive>.recovering`, and every letter of the number is retired
to `<archive>.bak` (then `.2.bak`, …) at install. The install target's own
original is preserved through a hard link — or a copy where the filesystem has
no hard links — so a `.tar` under the target name exists at every instant.
Consequence: the repair's own damage is always reversible by renaming a `.bak`
back. The segment sweep it unblocks is *not* reversible, which is the residual
risk this case does not remove; the mitigation is that `--task segments` is a
separate selection and `froe check` sits between them.

**Default-safe versus opt-in retirement.** `RepairArchives` is not in the
default task set. Selecting it authorizes exactly: rebuilding index-less
archives, retiring their letters to `.bak`, and — only when a rebuild is about
to become visible — the one-way `store.version` 1→2 manifest upgrade. It does
*not* authorize deleting anything: `repair-archives` and `recovery-backups`
are refused in the same run precisely so the run that creates the only copy of
unrecoverable bytes cannot also delete it.

**Unknown files.** Untouched. Repair reads only names matching Oak's archive
pattern; `.bak`, `.recovering`, and unrelated `*.tar` files are not inputs.

### Authoritative state

**Lock boundary.** `RepositoryLock::acquire` in
`PreparedCompaction::prepare_with_progress`. Every mutation in this task happens
after it and before the lock is dropped.

**Where the preview is discarded.** `plan_compaction` is advisory and strictly
read-only; `prepare` rebuilds the plan from disk under the lock. Unique to
this task, the preview is *deliberately partial*: while a repair is pending,
`index_available` is false and the checkpoint plan, stale-archive scan,
segment closure, segment plan, and stale-temporaries scan are all suppressed,
because each needs an index the repair has not yet created. The authoritative
plan is therefore always larger, and the CLI re-confirms it.

**Predicates shared between preview and apply.** One, deliberately:
"is this archive number repairable" — no valid-index letter, at least one
non-empty letter, and at least one segment the scan can read. The preview
derives it from the already-open readers (`unrepairable_archive_names`); the
locked path derives it from the physical listing
(`survey_indexless_archive_numbers`). Both read the same scan's entry count.
An earlier revision of this change had those two disagree, and the divergence
let a doomed run pay for durable rewrites first; the shared derivation is the
fix and the reason the predicate is stated once here.

**Facts rechecked under the lock.** Repository shape, manifest store version,
apply environment, apply identity, lock path identity, duplicate
`(number, letter)` pairs, repair-target ownership, and the repairability
survey — all after the lock and before the first rewrite.

### Mutation and publication order

Checks that can be computed without mutating are all performed before the
first transition they protect. The one check that cannot — the full plan —
is why the repair is the only mutation preceding planning.

| Boundary / cutpoint | Preconditions | Published or durable change | Returned-error state and named regression | Abrupt-exit state and named regression | Reconciliation |
| --- | --- | --- | --- | --- | --- |
| Manifest upgrade (`upgrade_manifest_atomically`) | Lock held; shape, version, environment, identity, duplicate-name and ownership checks passed; survey says ≥1 number repairable and 0 unrepairable; a staged rebuild is validated and about to install | `manifest` reads `store.version=2` | Manifest unchanged; staging file unlinked. `a_repair_that_installs_nothing_does_not_upgrade_the_manifest` | Manifest is old or new bytes, never partial (atomic replace). No named test — see gaps | Idempotent: a v2 manifest skips the step |
| Staging write (`<archive>.recovering`) | As above, plus no pre-existing non-empty staging residue | A new file that no archive discovery can see | Staging file unlinked, number untouched. `an_unrecoverable_archive_refuses_before_anything_is_rewritten` | Residue survives; the next run refuses rather than clobbering it. `a_failed_repair_reports_the_rebuilds_it_already_completed` | Operator moves it aside; `stale-temporaries` retains it as evidence |
| Staged validation (reopen, assert `!is_recovered()`) | Staging written and fsynced | Nothing published | Staging file unlinked. Covered by the same tests | Same as above | — |
| Install (`install_recovered_archive`) | Staged archive validated; metadata inherited from the replaced archive | Rebuilt archive under the target name; other letters renamed to `.bak` | Best-effort rollback of completed renames; target restored from its retained link | Mixed `.bak`/installed state possible; `.bak` copies always hold the original bytes | Inspect `.bak` names and rename back |
| Plan build (`build_plan`) | Repairs complete | Nothing | Refusal carries the completed rebuilds (`attach_completed_repairs`) | n/a — read-only | Rerun repairs only what is left |

**Interruption prefixes.** Killing froe during a repair leaves one of:
nothing changed; a `.recovering` file beside an untouched number; a mixed
`.bak`/installed state for one number, with the originals present under `.bak`.
No prefix loses archive bytes. Numbers are processed in ascending order and
each is independent, so a prefix is always "the first N numbers repaired, the
rest untouched".

**Resources.** Time and I/O scale with the *index-less* archives only, not the
store: each is memory-mapped and scanned for tar headers, twice on a repair
run (survey, then rebuild). Payload bytes are not materialized by the survey.
Temporary disk is the size of the largest repaired archive; permanent disk
grows by the total size of the repaired archives until `recovery-backups`
retires the `.bak` files. The plan states that figure, and it is not a proxy —
it is the sum of the letters that will be retired. Exhaustion during a rebuild
leaves the staging file, which the next run refuses on and the operator clears.

### Guards

| Guard and production callers | Named regression | Neutralization | Observed failing result |
| --- | --- | --- | --- |
| Manifest upgrade gated on a pending repair (`repair_before_planning`) | `selecting_repair_with_nothing_to_repair_changes_no_byte`, `a_repair_that_installs_nothing_does_not_upgrade_the_manifest` | Not neutralized — see gaps | — |
| Store version checked before repair (`check_manifest` in `prepare_with_progress`) | `a_store_from_a_newer_oak_is_refused_before_any_repair` | Not neutralized — see gaps | — |
| Cross-number duplicate segments refused in the preview (`reject_cross_number_duplicate_active_segments`) | `a_refusal_after_a_repair_says_the_repair_already_happened` | Not neutralized — see gaps | — |
| Unrepairable numbers refused before any rewrite (`survey_indexless_archive_numbers`, `unrepairable_archive_names`) | `an_unrepairable_archive_refuses_before_anything_is_rewritten` | Not neutralized — see gaps | — |
| Repair/recovery-backups mutual exclusion (`validate_options`) | `repair_archives_and_recovery_backups_cannot_run_together` | Not neutralized — see gaps | — |
| Repair-target ownership preflight (`validate_repair_target_identity`) | `a_repair_target_this_process_cannot_match_refuses_before_rewriting` | Not neutralized — see gaps | — |
| Staging residue not clobbered (`existing_staging_residue`) | `a_failed_repair_reports_the_rebuilds_it_already_completed` | Not neutralized — see gaps | — |
| Zero-length letters skipped for selection and install (`select_writable_generation`, `install_target_generation`) | `an_empty_archive_file_does_not_break_opening_for_writing`, `an_empty_archive_file_is_never_deleted_by_opening_for_writing`, `an_empty_archive_number_is_never_reallocated_over_its_own_residue` | Not neutralized — see gaps | — |

### Interoperability

Closed, and it is the evidence this case rests on most. The `repair` phase of
the interoperability suite ([`interop.md`](../../interop.md)) kills Oak's own JVM
with `SIGKILL` while it holds an archive open, asserts the container exited
137, confirms exactly one archive has no index, repairs it with this task, and
then boots a real Oak against the result — which serves the byte-identical
baseline tree and logs none of its own repair messages, so it consumed froe's
rebuilt index rather than reconstructing one.

Producer-to-consumer direction: Oak → froe → Oak. Oak build:
`oak-segment-tar` 1.90.0, from the digest-pinned Apache Sling image named in
`crates/froe-cli/tests/interop.rs`. Operation exercised:
`froe compact --repair-archive-indexes` alongside the five default tasks.
Verified post-state: every archive indexed, `froe check` passing, Oak serving
the baseline tree.

Not covered by that loop, unchanged from the rest of the suite:
`store.version=1` stores, external blob stores, and Adobe AEM itself.

### Known gaps

* **No guard-neutralization evidence.** The table above names a regression per
  guard but does not record a disabled-guard run for each, which
  `high-risk-changes.md` asks for. The guards were instead exercised by three
  adversarial review passes over the diff; that is a different kind of
  evidence and a weaker one for this specific property.
* **No abrupt-exit fault harness.** The interruption prefixes above are
  reasoned from the code and from the rollback's own structure, not proved by
  killing froe at each cutpoint. This is the largest gap. It is tolerable only
  because every prefix retains the original bytes under a `.bak` name, and it
  should be closed before the task loses any beta framing.
* **`attach_completed_repairs` collapses non-`InvalidFormat` errors** into
  `InvalidFormat`. No in-tree caller matches on the variant; a downstream one
  could. Source-compatible to fix later.
* **Redundant scanning.** An index-less number is scanned by the survey and
  again by the rebuild. Memory-mapped and payload-free, but it extends the
  locked window.
* **The whole-run refusal is a policy choice, not a proof.** A store with one
  unreadable archive refuses the entire repair-selected run rather than the
  repair task alone. Fail-closed and deliberate; whether it should narrow is
  open.
