# Safety case: the merged maintenance command

For the range `52fa39d..ec8d2ec` — `v0.9.0` through the version bump and the
two fixes that follow it — under the rules in [`high-risk-changes.md`](../high-risk-changes.md).
It succeeds
[`bounded-memory-and-journal-retention-safety-case.md`](bounded-memory-and-journal-retention-safety-case.md),
which remains the case for the write session, the caches, and the walks, and
which this range supersedes on exactly two points: journal history is no
longer retained by default, and the retained-generation count is one rather
than two.

In scope on four counts.

* **Reclamation became unconditional.** A run frees bytes no earlier froe
  command could free, because the archive rewrite no longer consults Oak's
  savings gate. `froe cleanup` is gone; there is one command.
* **The retained-generation count dropped to one.** Head safety no longer
  follows from the generation predicate the way `write-cleanup.md` §11
  invariant 2 states it for the online default of two.
* **Journal history is retired on every run.** Revisions that still resolve —
  reachable history, not garbage — are removed by policy, and the segments
  behind them are swept in the same run.
* **Two write-path defects are fixed that produced a store passing every
  structural check.** A backup carried the content tree without its binary
  blocks whenever a copy crossed a store boundary, and two nodes differing
  only in a slot's arity could share a template record and decode their values
  at the wrong arity afterwards. `RecordWriter::copy_binary_value` now takes
  the store boundary as an argument with no default, so the first of those
  cannot be reintroduced by reaching for a shorter name.

Covers `crates/froe/src/writer/maintenance/**`,
`crates/froe/src/writer/compaction/**`,
`crates/froe/src/writer/store_writer/**`, `crates/froe/src/writer/backup/**`,
`crates/froe/src/writer/record_writer/nodes.rs`,
`crates/froe/src/tooling/digest.rs`, and `crates/froe-cli/src/mutation.rs`.

## Scope and retention

**Must survive, unconditionally.** The content root and every checkpoint the
run does not retire; the binary content behind them; the `manifest`;
`repo.lock`; every file froe does not recognize.

**No longer retained — the inversion this range makes.** Journal history. The
previous case listed "every readable journal revision and its complete segment
closure" as surviving by default, with retirement behind
`--retain-journal-revisions`. That is now what a run *does*: after a
successful run `journal.log` holds one line, naming the head the copy
published. There is no flag that keeps more. The library's
`CompactionOptions::with_journal_revision_retention` survives, but it is read
only on the path where no compaction ran — see Known gaps.

The removed history is **not recoverable from the store**. The sweep runs
before the journal rewrite in the same run, so by the time
`journal.log.bak.NNN` exists it names revisions whose segments are already
unlinked. The backup restores the journal *file*, not the history.

**Deliberately dropped from the copy.** The records exclusively reachable from
a checkpoint the run retires. Expiry is realized by never entering the
checkpoint during the deep copy
(`compaction/walk.rs`, `an_omitted_checkpoint_leaves_its_exclusive_records_uncopied`),
not by removing it from the live head and compacting afterwards, so the head
moves exactly once. A subtree the retired checkpoint shares with the live root
is still copied through the live root
(`omitting_a_shared_subtree_still_copies_it_through_the_live_root`).

**Deliberately not retained.** Generations older than the one the copy just
wrote, without a savings gate on the archives that hold them.

## Authoritative state

The preview is advisory and discarded. `PreparedCompaction::prepare` takes
`repo.lock`, replans from disk, and `reject_changed_directory` refuses a
directory whose fingerprint moved between the two. Three facts are rechecked
against locked state rather than carried from the preview:

* **Every reclaim source is certified again.** `preflight_reclaim_sources_with_progress`
  reads the record table and payload bytes of each archive holding a candidate
  and pins them by CRC. It runs before the copy appends anything, not merely
  before the sweep consumes it, so a retry against a pre-existing defect does
  not durably append another full copy before failing; the proof then travels
  to the reclaim pass. On a version-one store
  about to be upgraded, `apply_prepared` runs a second all-source pass first,
  so a bad source cannot leave even a compatible manifest transition behind.
* **The reclaim rule is re-evaluated over the head's own closure.**
  `validate_reclaim_reference_invariant` walks the transitive segment closure
  of the current head and refuses if any data segment in it is reclaimable
  under the exact rule the mark phase will consume, or if its index and header
  generations disagree. At one retained generation this is what replaces the
  margin the predicate used to provide; it runs during planning, before any
  mutation.
* **The retained journal root is verified through a provider that excludes the
  planned removals**, before a byte moves, and again after the run against a
  freshly reopened store.

## Mutation and publication order

The ordering inside the compaction phase is the safety argument. The copy is
purely additive: it opens a certified archive number above every physical
archive name in the directory (`WritableRepository::open_prepared`), so it
creates files rather than extending them, and touches no existing byte. A
failure or refusal anywhere between the copy's start and the head move
therefore leaves the store as the *pre-copy* phase left it, plus orphan
archives a later run retires. Within the compaction phase nothing is unlinked
until the head is published, and by then every segment the new head reaches
lives in the generation the sweep will retain.

That claim is scoped to the compaction phase deliberately. `apply_pre_copy_mutations`
runs before it and does unlink — stale archives, and the residue of a killed
earlier run — so an interruption after that point does not leave the store
byte-identical to its starting state. It leaves it in the prefix that row
describes, which is why the residue retirement is ordered first: a retry
converges instead of accumulating one more orphan generation.

| Boundary / cutpoint | Preconditions | Published or durable change | Returned-error state | Abrupt-exit state | Reconciliation |
| --- | --- | --- | --- | --- | --- |
| Manifest upgrade (`upgrade_manifest_atomically`) | all active sources certified; lock path identity rechecked | `manifest` replaced atomically, directory synced | Manifest old or new, never partial; no archive touched | Same | Next plan reads whichever manifest is present |
| Pre-copy mutations (`apply_pre_copy_mutations`) | plan replanned under lock; sources certified | Stale archives unlinked; a killed run's residue retired | Typed partial outcome names retained and already-absent targets | Prefix-safe: each name is published or unlinked, never both | Next plan re-derives from disk; retiring residue first is what makes a retry converge |
| Deep copy (`apply_compaction_phase`, through `writer.finish()`) | fresh generation number certified above every physical archive name | New archives only; no existing byte changed | Store unchanged plus orphan archives | Same | `an_interrupted_compaction_is_retired_by_the_next_run` |
| **Head publication** (`compare_and_set_head` then `flush`) | copy complete and durable | `journal.log` gains the compacted head line | Refuses if the head moved under the run; nothing unlinked | Old or new head line, never partial | The copy's archives are reachable or orphaned; either is safe |
| Post-copy reclaim sweep (`reclaim_old_generations_with`) | head published; sources certified | Successor archives published by absent-only hard link, sources retired | Typed partial outcome; the new head is already durable | Old or new archive, never both; `postcomp_removal_to_rewrite_error_and_process_crash_leave_a_healthy_prefix` | Next run sweeps what this one left |
| `gc.log` append | sweep complete, directory synced | Exactly one line describing the cycle | File byte-identical or grown by that line | Same | `verify_gc_log_delta` proves the delta on the same run |
| Journal retirement (`rewrite_journal_for_run`) | `journal.log.bak.NNN` durable; retained root verified | `journal.log` replaced with the single retained line | Journal old or new, never partial | Same | Backup names the pre-rewrite content — the file, not the history |
| Applied-state verification (`verify_applied_state`) | every mutation above returned | None — fresh reopen and proof | Reports the mismatch; store already in its final state | Not applicable | Rerun the preview |
| Residue retirement (`retire_run_residue`) | applied state verified | Recovery and staging material unlinked | Typed partial outcome | Names lie outside active archive discovery, so no prefix can invalidate the verified state | Next run retires the remainder |

## Guards

| Guard and production callers | Named regression | Neutralization | Observed failing result |
| --- | --- | --- | --- |
| Reclamation is complete (`ArchiveRewritePolicy::EveryReclaimableArchive`; `reclaim_old_generations_with`, the standalone sweep) | `compact_then_cleanup_leaves_nothing_unreachable_on_a_binary_heavy_store`, `repeated_compaction_and_cleanup_never_accumulates_unreachable_segments`, `one_merged_run_compacts_and_reclaims_in_a_single_pass` | Reclaim pass restored to Oak's savings gate | `round 0 left 11 segments the journal cannot reach` — the field report reproduced |
| Head reaches nothing reclaimable (`validate_reclaim_reference_invariant`; every planned sweep) | `a_head_reaching_a_reclaimable_generation_is_refused_without_mutation` | **Not recorded** — see Known gaps | — |
| Reclaim sources are certified before they are consumed (`preflight_reclaim_sources_with_progress`, `certify_active_archives_with_progress`; the copy's sweep, the standalone sweep, the manifest upgrade) | `segment_source_certificate_rejects_a_survivor_payload_crc_mismatch`, `segment_source_certificate_rejects_exact_graph_or_brf_omissions`, `segment_source_certificate_precedes_a_whole_archive_removal` | Pre-existing; extended to the merged path by this range | — |
| Staging and source identity hold across publication (`sweep.before-publish-link`, `sweep.before-source-unlink`, `remove-planned-file.before-final-identity`) | `sweep_rejects_a_staging_path_substituted_after_validation`, `sweep_does_not_unlink_a_source_path_substituted_after_certification`, `planned_file_removal_does_not_unlink_a_substituted_staging_inode` | Pre-existing | — |
| A block is shared only within one store (`BulkBlockSharing`, a required argument of `copy_binary_value` with no default; `deep_copy_tree_across_stores_with_progress` for backup and restore) | `a_backup_carries_binary_content_that_lived_in_a_bulk_segment` | Restored the segment-kind-only rule | `SegmentNotFound` — the target resolves a reference into a store it does not have |
| One computation decides a slot's arity (`property_slot_tag`; `write_template_record` and the `TemplateKey` dedup cache, so every write path) | `a_preserved_multi_valued_slot_never_shares_a_template_with_a_single_valued_one` | Restored the two independent computations | The single-valued node inherited the multi-valued node's template record |
| A retired checkpoint's exclusive records are not copied (`compact_super_root_omitting`) | `an_omitted_checkpoint_leaves_its_exclusive_records_uncopied`, `a_copy_that_omits_a_checkpoint_reproduces_every_other_child_exactly`, `omitting_a_shared_subtree_still_copies_it_through_the_live_root` | Not neutralized — the tests assert record presence and absence directly | — |
| `gc.log` grew by exactly this cycle's line (`verify_gc_log_delta`; every applied run) | asserted on every applied-run test through `verify_applied_state` | Not neutralized | — |

Every reclamation assertion above is made against
`segments_unreachable_from_the_journal`, a reachability oracle in
`crates/froe/tests/reclamation_completeness_tests.rs` written from the storage
format alone: it reads `journal.log`, walks segment header reference tables,
and consults no generation triple, archive index, graph trailer, or any part
of the planner it judges. `a_dry_run_plan_predicts_exactly_the_archives_the_run_sweeps`
binds the preview to that same oracle, and
`a_generation_z_archive_reports_the_residue_it_cannot_reclaim` pins what the
format forces the run to leave.

## Fault and subprocess tests

Cutpoints armed around the merged path, each by a named test in
`crates/froe/src/writer/fault_injection/`: `sweep.{before,after}-publish-link`,
`sweep.{before,after}-publish-directory-sync`,
`sweep.{before,after}-source-unlink`, `sweep.{before,after}-staging-unlink`,
`remove-planned-file.before-final-identity`, `journal.{before,after}-rename`,
`journal.{before,after}-{pre,post}-rename-directory-sync`,
`manifest.before-rename`, `manifest.before-post-rename-directory-sync`, and
the three `cleanup.before-*-verification` points.

`postcomp_removal_to_rewrite_error_and_process_crash_leave_a_healthy_prefix`
is the one this range adds: it covers the sweep that now runs *after* a head
publication, where the previous case's rows all covered a sweep that ran
before one. Each fault test freshly reopens the store and asserts the exact
on-disk prefix, journal readability, and the typed partial outcome.

## Resources

Certification went wide rather than deep. Certifying a segment now reads the
record table and payload bytes of the archive holding it, so the shared
provider — whose caches take a write lock on a miss — is consulted only to
follow a `0xF0`-class blob identifier out of the segment being read. That is
what let the pass parallelize over `std::thread::scope` with a shared position
counter and no added dependency. Measured on a 16-core host: 3227 MB/s on one
thread, 23101 on eight, 33526 on sixteen — near-linear to eight, then
memory-bandwidth bound. Compaction's source certificate now stands for its
reclaim pass instead of being re-derived over identical bytes.

**A run requires headroom equal to its live set.** There is no sweep-only
mode: the copy writes the live content into a fresh generation before anything
is freed, so a store with less free space than its live content can no longer
be reclaimed by froe at all. This is the largest capability the merge cost,
and it is deliberate — it is also the reason the plan prints the cumulative
size of rewrite sources as a working-space proxy and warns when reported
availability is lower.

Memo residency, cache budgets, and walk state are unchanged by this range and
remain covered by the previous case. The one addition is digest's 16 MiB
insertion-order cache of inline-binary checksums, recorded there.

## Interoperability

Closed for this range. `scripts/interop-fixture.sh`, exit 0, 383.84 s, against
`docker.io/apache/sling@sha256:8722cd66ae0758e50784ac21df836c8f8d9e443d105e1a4292a4cb7f810a8cc9`,
Oak build under test **oak-segment-tar 1.90.0**, `store.version=2`. Direction
froe-to-Oak for every maintenance phase. Every phase passed, and every boot
asserted Oak logged none of its own repair messages, so Oak consumed the store
as froe wrote it rather than reconstructing it.

Two phases carry this range specifically:

* **reclaim** now asserts what the merge exists to do — a partially dead
  archive rewritten to its next generation letter with a survivor subset and
  reconstructed `.gph`, `.brf` and `.idx` trailers, which Oak then reads
  without logging a repair. Under the savings gate that archive was never
  rewritten at all, so this assertion could not previously exist.
* **journal_retention** runs a plain `froe compact` — no flag — and requires
  that every revision but the head it wrote is gone, the segments behind them
  swept, and Oak serving the exact baseline tree from the single revision
  froe kept.

Every mutating phase is additionally held to a `froe digest` rendering taken
before and after, with each line that differs required to fall inside the
delta the phase declares.

## Verification report

Each claim binds to one command and that command's own exit status. Linux
x86-64 host; stable `1.97.1`; MSRV `1.89.0` from `Cargo.toml`. Run against the
tree at `ec8d2ec`.

| Command | Status |
| --- | --- |
| `cargo +1.89.0 fmt --all -- --check` | 0 |
| `cargo +1.89.0 clippy --workspace --all-targets --all-features -- -D warnings` | 0 |
| `cargo +1.89.0 test --workspace --all-features --release --no-fail-fast` | 0, 722 tests |
| `RUSTDOCFLAGS="-D warnings" cargo +1.89.0 doc --workspace --all-features --no-deps` | 0 |
| `RUSTFLAGS="-D warnings" cargo +1.89.0 check -p froe --all-targets --all-features --target i686-unknown-linux-gnu` | 0 |
| `cargo +stable fmt --all -- --check` | 0 |
| `scripts/oversized-files.sh` | 0 |
| `cargo +stable test --workspace --all-features --no-fail-fast` | 0, 722 tests |
| `cargo +stable test --workspace --all-features --release --no-fail-fast` | 0, 722 tests |
| `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings` | 0 |
| `RUSTDOCFLAGS="-D warnings" cargo +stable doc --workspace --all-features --no-deps` | 0 |
| `scripts/interop-fixture.sh` | 0, every phase, 384 s |

The separations the guide asks for explicitly: everything above is
**execution**, not cross-compilation, except the i686 row, which is a
**compile-only width sentinel**. No test runs as root; identities are
synthetic. Fault coverage is **process-level error injection**, not power-loss
ordering. `interop-fixture.sh` is **real Oak interoperability**, not a
froe-to-froe round trip.

Two results are worth recording because both would have reddened CI rather
than the host gate, and both were caught by running the full matrix locally
before the tag rather than after it.

`clippy::duplicated_attributes` fires at 1.89 and not at stable 1.97.1, so a
duplicated `#[cfg(unix)]` introduced by a test split left the stable host gate
green while both MSRV legs would have failed to compile four test targets.

`segment_hex_cli_uses_conventional_sigpipe_for_a_preclosed_stdout` failed once
in four full runs of the gate, and only in the debug profile. Its fixture
built a pipe with `libc::pipe`, whose descriptors are inheritable, so one of
the eight other processes this file spawns concurrently could inherit the read
end and hold the pipe open — after which the dump wrote successfully and the
test reported that froe had not used SIGPIPE. Characterized by the asymmetry
rather than by inspection: 1 failure in 15 parallel runs of the file, 0 in 20
runs of the test alone, 0 in 8 single-threaded runs. Fixed in `ec8d2ec` with
`std::io::pipe`, which is close-on-exec at creation; 0 failures in 25 runs
after. It is recorded here because a flaky test in a release gate is an
evidence defect, not a nuisance: it teaches a maintainer to re-run rather than
to read.

## Known gaps

1. **A journal retention bound is accepted and ignored under a compaction.**
   `rewrite_journal_for_run` reads `options.journal_revision_retention` only
   on the branch where no compaction ran; when one did, the journal is
   retired to the compacted head line and the bound is never consulted.
   `validate_options` refuses two other incoherent combinations — a bound
   without the journal task, and repair beside recovery-backup retirement —
   so refusing rather than silently choosing is this codebase's pattern. The
   direction is safe (the run retires more than asked, never less) and no CLI
   path can reach it, since the CLI always sets a compaction kind and no
   longer exposes the bound.
2. **The reclaim reference invariant has no neutralization evidence.**
   `a_head_reaching_a_reclaimable_generation_is_refused_without_mutation`
   names the guard and asserts the refusal, but no disabling experiment is
   recorded for it. This is the guard that replaced the margin two retained
   generations used to provide, so it is the most load-bearing refusal in the
   range and the one whose evidence is thinnest.
3. **Journal history is unrecoverable by design.** Nothing in the store
   restores it after a run. An operator who needs it needs a repository copy
   taken beforehand, which the CLI states and this document repeats because it
   is the one irreversible consequence of the merge.
4. **A store without headroom cannot be reclaimed at all.** See Resources. No
   test asserts the behavior at genuine exhaustion; `ENOSPC` is covered as an
   injected error during a rewrite, not as a store that cannot fit its own
   live set.
5. **The digest is froe-side evidence.** It renders what froe reads, so a
   defect symmetric between froe's writer and its reader would produce
   identical renderings before and after. Only the Oak-side content snapshot
   in the interop suite covers that axis, and it covers 21 entries rather than
   the whole tree.
6. **macOS is untested locally.** Only CI covers it, and the maintenance path
   is Unix-specific, so it is a first-class target rather than an afterthought.
7. **No RSS measurement**, unchanged from the previous case: budgets are
   asserted against `cache_weight`, an approximation.
8. **Unchanged from the previous case and still open:** the reopen boundary
   has no armed fault cutpoint; the source-shape guard has known blind spots;
   per-node fan-out is unbounded by content shape; walk state is
   depth-proportional with no ceiling.
9. **Not covered by any run here:** `store.version=1` stores, external blob
   stores, native Windows execution, and Adobe AEM itself, which ships its own
   Oak build.

## Review

**An automated adversarial pass, not a second person.** Performed 2026-08-18
over `52fa39d..ec8d2ec` with a clean worktree, by an assistant that did not
author the range's behavioral changes. The range reviewed is the one this
document covers, including `abf2104`, which the pass produced and which was
re-verified after it landed. Recorded as the
guide requires, with the weakness the guide names: the pass was briefed by the
author's own commit messages, so it inherits the author's framing of what the
change is for, and it cannot notice a question nobody thought to ask. It is
not a substitute for a second person.

Four lenses, run in sequence, each finding verified against the code before it
was recorded. One produced a fix rather than a gap, per step 4 of the guide;
the other two are recorded above:

* **The write-path fixes — is either incomplete, or does a sibling path carry
  the same defect?** The arity fix holds: `property_slot_tag` has one
  definition and exactly two callers, the `TemplateKey` constructor and
  `write_template_record`, and its match over `PropertyValuesToWrite` is
  exhaustive, so a new variant is a compile error rather than a silent
  single-valued default. The bulk-sharing fix holds at every froe call site,
  but the public two-argument `copy_binary_value` wrapper still defaulted to
  the cross-store-unsafe mode — an implicit default in the public API, which
  is the shape of the defect the range had just removed from one call site.
  Addressed in `abf2104`: the wrapper is gone, and the explicit form takes
  the plain name.
* **Reclamation completeness — can the sweep free something reachable, and is
  the oracle sound?** `segments_unreachable_from_the_journal` resolves journal
  heads and walks `referenced_segments` out of parsed segment headers, so its
  reached set is an over-approximation of true reachability: it can under-report
  garbage, never falsely accuse the planner. That is the safe direction for a
  completeness assertion, and it matches the granularity of the thing it
  judges, since the sweep reclaims segments rather than records. Record-level
  garbage inside a live segment is out of its scope and is the copy's business,
  judged separately by `compacted_nodes` and the digest.
* **Interruption prefixes — does each row of the mutation table hold?** Read
  against `apply_prepared` and `apply_compaction_phase` directly. Confirmed:
  the copy opens a certified archive number above every physical name, so it
  creates rather than extends; source certification precedes the copy, not
  merely the sweep; `compare_and_set_head` and `flush` precede every unlink
  within the phase; the sweep precedes the journal rewrite, which is why the
  `.bak` restores the file and not the history. This lens caught an
  overstatement in this document's own first draft — "leaves the store as it
  was" ignored `apply_pre_copy_mutations`, which unlinks before the copy
  begins — now corrected in the section above.
* **Evidence wording — does any claim here or in the range's commit messages
  overstate what was proved?** Produced gap 1, where an accepted option is
  silently not honored rather than refused. Two limits of this document are
  stated rather than left implicit: the throughput figures in Resources are
  the author's measurements carried forward from the commit message, not
  re-measured here, and the neutralization results in the Guards table are
  likewise the author's, reproduced from the commits that recorded them. The
  Verification report rows are the only measurements taken by this pass, and
  every one was executed rather than inferred.

**What this pass did not cover.** The range holds 57 commits, of which about
fifty are refactors that moved code without changing behavior. Those were not
read line by line; they were judged by outcome — both gates green, the
file-length rule holding, and the interoperability suite passing on the
resulting tree. A behavioral change hidden inside a commit labelled `refactor:`
would not have been caught by this pass. The five commits dated 2026-08-18 —
the clippy fix, the documentation repair, the `copy_binary_value` change, the
version bump, and the SIGPIPE fixture fix — were authored by the same
assistant that performed this pass, so for those it is a self-review and not
independent evidence.
