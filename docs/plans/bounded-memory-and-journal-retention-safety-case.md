# Safety case: bounded memory and `--retain-journal-revisions`

The artifact [`high-risk-changes.md`](../high-risk-changes.md) requires for the
range `bdcbe55..HEAD`. In scope on three counts: it adds a task that makes
repository bytes unreachable by design (`--retain-journal-revisions`), it
rewrites how a write session holds and certifies what it wrote, and it
*loosens* a refusal on the destructive path — the last reaching the default
`--task segments` behaviour, not only the new flag.

Covers `crates/froe/src/cache.rs`, `crates/froe/src/store.rs`,
`crates/froe/src/tar_archive/archive.rs`, `crates/froe/src/tooling/{check,diff,search}.rs`,
`crates/froe/src/writer/{backup,cleanup,compaction,store_writer}.rs`, and
`crates/froe-export/src/{refresh,sqlite}.rs` as of the commit introducing this
file.

## Scope and retention

**Must survive, unconditionally.** The content root and every checkpoint
reachable from the persisted head; the binary content behind them; the
`manifest`; `repo.lock`; every file froe does not recognize.

**Must survive by default.** Every readable journal revision and its complete
segment closure. This is froe's standing divergence from Oak, which judges
data segments by their index generation triple alone
(`docs/analysis/write-cleanup.md` §5 and invariant 1) and never rewrites
`journal.log`. Nothing in this range changes that default.

**Opt-in retirement.** `--retain-journal-revisions N` keeps the newest `N`
revisions that actually resolve and removes the rest. The removed history is
**not recoverable**: the segment sweep runs before the journal rewrite in the
same run, so by the time `journal.log.bak.NNN` exists it names revisions whose
segments are already unlinked. The backup restores the journal *file*, not the
history. The flag selects `CleanupTask::Journal`, because the bounded lines must leave the journal in
the same run — see Guards.

**Deliberately not retained.** Payload bytes of segments this session wrote,
once they are durable in an archive. They were retained before; they are the
defect this range removes.

## Authoritative state

Unchanged by this range. The preview is advisory and discarded; the lock
boundary, the replan under the lock, and the re-verification of every retained
journal root against locked state are as before. Two additions bind to it:

* The retention bound reaches all three `analyze_journal` call sites — plan,
  pre-rewrite under the lock, and final post-mutation proof — so preview and
  apply share the predicate. A bound applied to only one would make the
  locked analysis disagree with the plan's retained roots and abort the run.
* `history_protected_reclaimable` is measured by replanning the sweep with the
  veto lifted. It is **reporting only**; it never feeds a mutation decision.

## Mutation and publication order

| Boundary / cutpoint | Preconditions | Published or durable change | Returned-error state | Abrupt-exit state | Reconciliation |
| --- | --- | --- | --- | --- | --- |
| Archive rotation (`close_archive_writer`) | segment appended; writer at threshold | Archive closed with trailers, fsynced | Complete archive on disk; journal unmoved | Same | Next open reads a complete archive |
| Archive reopen (new in this range) | archive durable per the row above | None — reopen is read-only | Complete archive, journal unmoved; session ends | Not reachable: no mutation follows the reopen within the boundary | Next open reads the archive normally |
| Session payload certificate (`validate_finalized_session_semantics`) | every session archive finalized | None; refuses before the journal append | Journal unchanged, archives intact | Journal unchanged | Retry replans from disk |
| Segment sweep (`apply_standalone_segment_cleanup`) | plan replanned under lock; sources certified | Successor archives published, sources retired | Typed partial outcome names retained/absent targets | Prefix-safe per the existing cleanup case | Next plan re-derives from disk |
| Journal rewrite with a retention bound | `journal.log.bak.NNN` durable; retained roots verified | `journal.log` replaced with the retained lines | Journal old or new, never partial | Same | Backup names the pre-rewrite content |

The reopen row is the only new boundary. It adds a failure point *after* a
durability transition, and it is prefix-safe in both directions: at rotation
it precedes any journal movement, and at session close `flush()` has already
appended the journal line for an archive that is itself complete. A zero-byte
file cannot reach it — `close()` returns `false` and the function returns
early.

## Guards

| Guard and production callers | Named regression | Neutralization | Observed failing result |
| --- | --- | --- | --- |
| Cache byte ceiling (`BoundedCache::evict_to_budget`; every read and write cache, the compaction and verifier memos) | `writing_more_segments_does_not_make_a_session_hold_more_bytes` | Early `return` in `evict_to_budget` | `payload residency exceeded the ceiling: 20736 bytes held against a 4096-byte budget` |
| Long-lived state holds no store-scaled collection (source-shape guard over `WritableRepository`, `Repository`, `ArchiveSet`) | `long_lived_store_state_holds_nothing_that_grows_with_the_repository` | Added `regression_probe: HashMap<SegmentIdentifier, Vec<u8>>` to `WritableRepository` | `long-lived store state gained an unbounded collection: WritableRepository::regression_probe` |
| Session payload certificate (`validate_finalized_session_semantics`, via `flush` and the prepared-cleanup path) | `a_session_payload_the_writer_never_produced_fails_closed` | `if actual_crc != expected_session.payload_crc` → `if false` | Test failed at the `expect_err`; the foreign payload reached the journal append |
| Retention bound requires the journal task (`validate_options`; `plan_cleanup`, `PreparedCleanup::prepare`, `cleanup`) | `a_retention_bound_without_the_journal_task_is_refused` | Refusal condition → `if false` | Test failed at `expect_err`: the bound planned without pruning the lines it un-rooted |
| Retention bound refuses beside a checkpoint head update (`build_plan_collecting`) | `a_retention_bound_beside_a_checkpoint_head_update_is_refused_while_planning` | Refusal condition → `if false` | Test failed at `expect_err`: the plan proceeded toward an apply that aborts after committing the checkpoint removal |
| Retained journal roots stay readable (`validate_prospective_segment_plan`, first half) | `prospective_plan_refuses_a_survivor_that_references_a_planned_removal`, plus the armed cutpoint `cleanup.before-prospective-retained-root-verification` | Pre-existing; unchanged by this range | — |
| Live survivors must not dangle (`validate_prospective_segment_plan`, second half — **loosened here**; default `--task segments` path) | `a_dead_survivor_pointing_at_a_removed_segment_is_handled` | Removed `\|\| reclaimable.contains(&identifier)`, restoring the stricter check | `a dead survivor pointing at removed garbage must not refuse the plan: InvalidFormat { details: "surviving data segment 84dbbc74… references segment 595eb34c…, which the cleanup plan would remove" }` |
| Retention bound counts only revisions that resolve (`journal_retention_boundary`) | `a_bound_counts_only_revisions_that_actually_resolve` | Counted every line whose segment exists, ignoring the verdict | `a readable revision was retired to make room for an unreadable one` — the older readable revision removed as `BeyondRetention` beside the unreadable one |
| Zero-budget memos reach the same verdict (`NodeTreeVerifier`, `Compactor`, session cache) | `an_evicting_memo_costs_reads_but_never_changes_the_verdict`, `a_deep_copy_with_no_sharing_memo_still_produces_the_whole_tree`, `a_session_serves_its_own_segments_from_disk_when_nothing_is_cached` | Budget set to 0 by the test itself | Not applicable — the starved configuration *is* the experiment |

Neutralizations ran serially against an isolated target directory, each
followed by restoring the pristine source and rerunning the original test.

## Resources

The point of the range. Worst-case residency is now a function of
configuration, not of repository size:

* Read caches: 192 MiB parsed segments, 48 MiB strings, 48 MiB templates.
* Writer base-segment cache: 192 MiB. Session read-back cache: twice the
  archive rotation threshold (512 MiB by default), sized so the archive being
  written is always resident.
* Compaction sharing memo 256 MiB; verified-subtree memo 192 MiB; recovery
  visited memo 128 MiB; SQLite string dictionary 64 MiB.

Planning is *not* bounded by configuration. On the default `--task segments`
path, under the lock, peak store-scale sets are `head_data_segments`,
`protected_history_segments`, the plan's `reclaimable`, and the second
marking pass's own `references` and `reclaimable` — five, where before this
range there were three. An applied run plans twice (preview and authoritative).

Temporary disk is likewise unbounded by configuration. The compaction sharing
memo is now an evicting FIFO, and a miss re-copies a shared subtree including
its binary payload, so a store with many checkpoints can stage an output
larger than its source before `reclaim_old_generations` retires anything.
No test measures that amplification.

Still proportional to the store, and unavoidable without an on-disk index:
one decoded index entry per segment (~40 B), one `segment_locations` entry per
segment (~25 B, now reserved up front rather than grown by doubling), one
`SessionSegment` locator per segment written (Copy, ≤ 24 B, pinned by
`a_session_locator_owns_no_heap_and_stays_small`), and one journal entry per
journal line.

Open files no longer scale: `FinalizedSessionArchiveCertificate` fingerprints
by identity and metadata instead of holding a descriptor per session archive,
which previously reached `EMFILE` on a large compaction after its work was
done.

**Proxy, not a measurement.** `cache_weight` is an approximation of resident
bytes; it decides eviction, not correctness. No test measures process RSS.

## Interoperability

Closed for this range. `cargo test -p froe-cli --features interop -- --ignored
--test-threads=1 interop_full`, exit 0, 428 s, against
`docker.io/apache/sling:14`, Oak build under test **oak-segment-tar 1.90.0**.
Direction froe-to-Oak for every maintenance phase; Oak served the *exact*
baseline tree after full compaction, tail compaction, all three checkpoint
removals, and cleanup, and after `repair` with Oak's own JVM killed by
`SIGKILL` while holding an archive. `backup` and `recover` passed.

This is the evidence the session rewrite rests on: the certificate now proves
payload identity by recorded CRC rather than by retained bytes, and Oak read
back byte-identical trees.

`--retain-journal-revisions` is **not** exercised by the interop suite. See
gaps.

## Verification report

Each claim binds to one command and that command's own exit status. Linux
x86-64 host; MSRV `1.89.0` from `Cargo.toml`.

| Command | Status |
| --- | --- |
| `cargo +1.89.0 fmt --all -- --check` | 0 |
| `cargo +1.89.0 clippy --workspace --all-targets --all-features -- -D warnings` | 0 |
| `cargo +1.89.0 test --workspace --all-features --release --no-fail-fast` | 0 |
| `RUSTDOCFLAGS="-D warnings" cargo +1.89.0 doc --workspace --all-features --no-deps` | 0 (failed first run on a dangling intra-doc link; fixed in `83a3ad0`) |
| `RUSTFLAGS="-D warnings" cargo +1.89.0 check -p froe --all-targets --all-features --target i686-unknown-linux-gnu` | 0 |
| `cargo test --workspace --all-features` (stable) | 0, 672 tests |
| `cargo test -p froe-cli --features interop -- --ignored --test-threads=1 interop_full` | 0 |

Separations the guide asks for explicitly: everything above is **execution**,
not cross-compilation, except the i686 row, which is a **compile-only width
sentinel**. No test runs as root; identities are synthetic. Fault coverage is
**process-level error injection**, not power-loss ordering — no test asserts
true write-ordering under power loss. `interop_full` is **real Oak
interoperability**, not a froe-to-froe round trip.

## Known gaps

1. **Independent human review waived by maintainer exception.** The review of
   this range was performed by automated reviewers against the frozen
   candidate, not by a second person. It found and corrected a fabricated
   evidence row, an unarmed regression on the loosened refusal, and a
   retention-counting defect that retired a readable revision at `N ≥ 2`;
   those fixes are in `cca15be`. The maintainer has exercised the exception
   [`high-risk-changes.md`](../high-risk-changes.md) provides and released the
   range without a second reviewer. Recorded here because the exception is a
   decision, not an absence — a later reader is entitled to know which
   evidence rests on review by a person and which does not.
2. **`--retain-journal-revisions` has no interoperability evidence.** No
   interop phase bounds the journal and then asks Oak to boot the result. It
   is the one new destructive operation with no Oak-verified post-state.
3. **The loosened survivor check reaches the default path.** `--task segments`
   can now proceed where it previously refused. One synthetic fixture on the
   default task set covers it and fails when the stricter check is restored;
   no real store has exercised it.
4. **No RSS measurement.** Budgets are asserted against `cache_weight`, which
   is an approximation. A leak outside the budgeted structures would not be
   caught.
5. **macOS untested.** Only CI covers it.
6. **The reopen boundary has no armed fault cutpoint.** It is argued
   prefix-safe above rather than tested by injection.
7. **The source-shape guard has known blind spots.** It matches literal type
   substrings against single-line field text of three named structs, so it
   does not see `VecDeque`, a field whose type rustfmt wrapped across lines, a
   collection behind a type alias, or state inside a nested struct such as
   `write_state: Mutex<WriteState>`. It also cannot see accumulators held in
   function locals, which is the class the `recover-journal` defect belonged
   to. It raises the cost of reintroducing the defect; it does not prevent it.
8. **`CleanupAction::PruneJournal` gained a required field.** The enum is
   `#[non_exhaustive]`, which does not make its variants so; a downstream
   struct-pattern match on that variant is source-breaking. Record it in the
   version bump rationale.
9. **Per-node fan-out is unbounded** by content shape — `child_node_entries`
   returns an owned `Vec`. Bounded by the widest single node, not by the
   store; fixing it needs an additive streaming API.
