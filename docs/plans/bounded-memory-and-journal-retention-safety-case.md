# Safety case: bounded memory and `--retain-journal-revisions`

The artifact [`high-risk-changes.md`](../high-risk-changes.md) requires for the
range `bdcbe55..HEAD`, extended after `v0.8.0` by three further write-path
changes: record reuse in the writer, binary value sharing in compaction, and a
configurable sharing-memo budget. In scope on three counts: it adds a task that makes
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
history. The flag selects `CleanupTask::Journal`, because the bounded lines
must leave the journal in the same run — see Guards.

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
| Writer reuses an identical value or template record (`RecordWriter::write_string`, `write_template`; every write path — commit, backup, restore, compact) | `a_repeated_string_or_template_reuses_the_record_it_already_wrote` | Not neutralized — the test asserts identifier equality directly, which no disabling change leaves true | — |
| A block is shared only when it lives in a bulk segment (`copy_binary_value`) | interop `compact` and `compact --tail` phases | Not neutralized — the froe-authored unit fixtures all take the copy path, so only an Oak-written store exercises sharing | — |
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
* Compaction sharing memo 256 MiB by default and settable per run with
  `froe compact --memo-budget-mb`, at roughly 112 bytes per node in the head; verified-subtree memo 192 MiB; recovery
  visited memo 128 MiB; SQLite string dictionary 64 MiB.

Planning is *not* bounded by configuration. On the default `--task segments`
path, under the lock, peak store-scale sets are `head_data_segments`,
`protected_history_segments`, the plan's `reclaimable`, and the second
marking pass's own `references` and `reclaimable` — five, where before this
range there were three. An applied run plans twice (preview and authoritative).

Temporary disk is bounded by the live set rather than by the store, and the
two record-reuse changes cut what "live set" means in practice. Measured on
4000 identically shaped nodes: the authored store fell 3.35x and its compacted
output 2.80x once the writer stopped re-writing the same value and template
records. Compaction no longer copies binary content at all when the source
blocks live in bulk segments, which on a store written by Oak is most of the
bytes.

What remains is the sharing memo. It is an evicting FIFO, and a miss re-copies
a shared subtree, so a budget below the tree inflates the output; the
observable symptom is a copied-node count climbing past the number of nodes
the head contains, which is what a 256 MiB budget did on an 18.8M-node
repository. The budget is now a per-run flag so it can be matched to the tree.
No test measures the amplification at a given budget.

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

Re-verified twice after `v0.8.0` for the later write-path changes, most
importantly binary value sharing: `compact` and `compact --tail` passed with
sharing active, so Oak served the exact baseline tree from bulk segments froe
referenced rather than rewrote. That run also replaced the `cleanup` phase's
reclaimable condition, which had built itself by restoring the pre-compaction
gen-0 archive — an assumption that only held while froe copied binaries, and
which failed loudly once it did not. The phase now writes 2000 nodes at
generation zero and never links them to a head.

This is the evidence the session rewrite rests on: the certificate now proves
payload identity by recorded CRC rather than by retained bytes, and Oak read
back byte-identical trees.

`--retain-journal-revisions` is exercised too, by a phase added for it. Oak's
own fixture carried three journal revisions; froe bounded the journal to one
and swept the segments behind the other two; Sling then booted the result and
served the exact baseline tree from the single revision froe kept, with one
line and its numbered backup on disk. This is the operation that most needed
Oak evidence — it is the only one that destroys *reachable* history by policy
rather than by Oak's generation predicate, so froe agreeing with its own
reachability rules would have proved nothing about it.

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
| `cargo test -p froe-cli --features interop -- --ignored --test-threads=1 interop_full` | 0 (chain including the `journal_retention` phase) |

Separations the guide asks for explicitly: everything above is **execution**,
not cross-compilation, except the i686 row, which is a **compile-only width
sentinel**. No test runs as root; identities are synthetic. Fault coverage is
**process-level error injection**, not power-loss ordering — no test asserts
true write-ordering under power loss. `interop_full` is **real Oak
interoperability**, not a froe-to-froe round trip.

## Known gaps

1. **Record reuse and value sharing have no neutralization evidence.** Both
   guards are asserted by tests that pass or fail on identity rather than on a
   refusal, so the disabling experiment the guide asks for does not apply
   cleanly; value sharing additionally has no unit coverage at all, because
   every froe-authored fixture puts blocks in data segments and takes the copy
   path. Interop is the only thing exercising it.
2. **The loosened survivor check reaches the default path.** `--task segments`
   can now proceed where it previously refused. One synthetic fixture on the
   default task set covers it and fails when the stricter check is restored;
   no real store has exercised it.
3. **No RSS measurement.** Budgets are asserted against `cache_weight`, which
   is an approximation. A leak outside the budgeted structures would not be
   caught.
4. **macOS untested.** Only CI covers it.
5. **The reopen boundary has no armed fault cutpoint.** It is argued
   prefix-safe above rather than tested by injection.
6. **The source-shape guard has known blind spots.** It matches literal type
   substrings against single-line field text of three named structs, so it
   does not see `VecDeque`, a field whose type rustfmt wrapped across lines, a
   collection behind a type alias, or state inside a nested struct such as
   `write_state: Mutex<WriteState>`. It also cannot see accumulators held in
   function locals, which is the class the `recover-journal` defect belonged
   to. It raises the cost of reintroducing the defect; it does not prevent it.
7. **`CleanupAction::PruneJournal` gained a required field.** The enum is
   `#[non_exhaustive]`, which does not make its variants so; a downstream
   struct-pattern match on that variant is source-breaking. Record it in the
   version bump rationale.
8. **Per-node fan-out is unbounded** by content shape — `child_node_entries`
   returns an owned `Vec`. Bounded by the widest single node, not by the
   store; fixing it needs an additive streaming API.
