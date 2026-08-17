# Safety case: bounded memory and `--retain-journal-revisions`

The artifact [`high-risk-changes.md`](../high-risk-changes.md) requires for the
range `bdcbe55..HEAD`, extended after `v0.8.0` by record reuse in the writer,
binary value sharing in compaction, an exact compaction sharing memo replacing
the evicting one, and the removal of every depth limit — which rewrote all six
walks over records from recursion to an explicit heap stack. In scope on four
counts: it adds a task that makes repository bytes unreachable by design
(`--retain-journal-revisions`), it rewrites how a write session holds and
certifies what it wrote, it *loosens* a refusal on the destructive path — the
last reaching the default `--task segments` behaviour, not only the new flag —
and it changes how every walk terminates on a corrupt record graph.

Covers `crates/froe/src/cache.rs`, `crates/froe/src/store.rs`,
`crates/froe/src/content/{map,traversal}.rs`,
`crates/froe/src/tar_archive/archive.rs`, `crates/froe/src/tooling/{check,diff,search}.rs`,
`crates/froe/src/writer/{backup,maintenance,compaction,store_writer}.rs`, and
`crates/froe-export/src/{refresh,sqlite}.rs`.

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
history. Retirement is now unconditional rather than flag-selected, because the retired lines
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
| Segment sweep (`apply_standalone_segment_cleanup`) | plan replanned under lock; sources certified | Successor archives published, sources retired | Typed partial outcome names retained/absent targets | Prefix-safe per the existing case | Next plan re-derives from disk |
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

The point of the range. Every cache below is a function of configuration
rather than of repository size — with three deliberate exceptions, all stated
as such rather than hidden: the compaction sharing memo and the verifier's
certificate set, both proportional to the tree by design, and the walk state
that replaced the depth caps, now proportional to depth. See "The two store-proportional memos" and "What
removing the depth caps costs".

* Read caches: 192 MiB parsed segments, 48 MiB strings, 48 MiB templates.
* Writer base-segment cache: 192 MiB. Session read-back cache: twice the
  archive rotation threshold (512 MiB by default), sized so the archive being
  written is always resident.
* Recovery visited memo 128 MiB; SQLite string dictionary 64 MiB. Two memos
  are no longer among these — the compaction sharing memo and the
  verified-subtree memo are both exact and unbudgeted, and are treated
  separately below.

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

### The two store-proportional memos

The compaction sharing memo used to be an evicting FIFO under a 256 MiB
budget. A miss did not cost one duplicated subtree; it re-walked the subtree,
and misses nested, so the copied-node count could climb past the number of
nodes the head contains — which is what the default budget did on an 18.8M-node
repository. Copying each distinct node once is an invariant, and it was being
enforced by a byte budget an operator had to guess.

It is now exact: `RewrittenNodes`, an open-addressed table that never evicts,
keyed on records interned to a `u32` segment index and packed into a `u64`, so
an entry is two `u64`s rather than two 24-byte `RecordIdentifier`s.
`compacted_nodes` therefore equals the number of distinct node records
reachable from the head, pinned by
`a_shared_subtree_is_copied_once_however_deep_the_sharing_nests` and
`a_deep_copy_copies_each_distinct_node_exactly_once`.

The cost of that guarantee is residency proportional to the tree: 16 bytes a
slot, between 35 and 46 bytes a node depending where the table sits between
growths (measured 44 at a million entries and 35 at four million; pinned as
table occupancy, not RSS, by
`the_exact_memo_costs_a_bounded_number_of_bytes_a_node`). An 18.8M-node head
needs 2^25 slots, or 512 MiB. The rejected alternative — the same map keyed on
`RecordIdentifier` — measured 104 to 115 bytes a node, or roughly 2.1 GB on the
same head, which is why interning and packing are load-bearing rather than an
optimization.

The verifier's certificate set is the second, for the same reason and with the
same shape. `NodeTreeVerifier` held an evicting 192 MiB `BoundedCache`, which
on that same 18.8M-node repository retained about a sixth of the tree; a miss
re-walked the subtree below it, and the walk reported one node per miss, so
`froe compact` announced 56,389,743 nodes for a head holding 18,796,598. It is
now `PackedRecordSet` — the same interned, open-addressed, never-evicting
table, keys only, so 8 bytes a slot and about 11.4 a node: 256 MiB of slots for
that head, with a 384 MiB peak across the final doubling, against the 204 MiB
the evicting cache actually occupied. The count is now asserted equal to the
number of certificates the walk issued, and a re-walk would trip the set's
duplicate-insert assertion rather than inflate a number, so the defect cannot
return quietly. `NodeTreeVerifier::with_memo_budget` is gone; no knob replaces
it, for the reason below.

`--memo-budget-mb`, `compact_with_memo_budget` and
`COMPACTION_MEMO_BYTES_PER_NODE` are gone with it. No knob replaces them: the
figure an operator would have needed is a distinct-set cardinality unavailable
before the walk, and a ceiling could only fire mid-copy, after segments had
already streamed to the sink. If a tree is ever found that does not fit, the
answer is a measurement first.

Termination no longer rests on the depth bound either. A self-referential
record graph is refused exactly, by an ancestor set, at the record that closes
the cycle (`a_cyclic_source_is_refused_at_the_record_that_closes_the_cycle`).

### What removing the depth caps costs

Every walk over records — compaction, the verifier, the recovery gate, the
content traversal, the diff, and the map enumerator — now carries its own
stack on the heap and imposes no depth limit. The caps were not bounding what
they claimed: three of the six stood in for cycle detection that did not
exist, and none could bound the stack, since 4000 levels of the compaction
walk needs ~2.8 MiB in release against the 2 MiB a spawned thread receives.
A legitimate 3000-deep tree aborted the process with SIGABRT instead of being
refused, through public API, where SIGABRT cannot unwind and the caller gets
no report.

The trade is that a constant-bounded amount of walk state became a
depth-proportional one. Measured on `verify_node_tree`
(`measure_deep_chain_walk_footprint`): **~175 bytes a level** — the ancestor
set, the frame stack, and the path buffer — so 17 MiB at 100,000 levels and
65 MiB at 400,000, where `MAXIMUM_CHECK_DEPTH` previously capped it at about
700 KB. It is bounded now only by how many distinct records the store holds,
because a repeat is refused as a cycle.

For an ordinary repository this is negligible: content trees run a few hundred
levels, about 70 KB. It takes a store shaped as a long chain — corrupt or
adversarial — to make it large. But the gap should be named precisely rather
than waved at:

* `check.rs` and `backup.rs` have **no work budget at all**, so nothing bounds
  the term there.
* `traversal.rs` and `diff.rs` do have budgets, but they charge against node
  counts (1e9), not residency, so they would allow tens of gigabytes of walk
  state before firing.

The instrument that would close this is the one this document's own rule
already names — a budget charged against the resource actually consumed. Here
that resource is the walk's own state, and a ceiling on it can be read from
the host rather than guessed, which is what separates it from the depth caps
just removed. Not implemented.

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
bytes; it decides eviction, not correctness. No test measures process RSS. The
compaction memo's per-node figure is pinned as table occupancy for that reason:
resident size tracked it within a few bytes a node when measured in isolation,
but varies with allocator reuse when measured alongside other tests.

## Interoperability

Closed for this range. `cargo test -p froe-cli --features interop -- --ignored
--test-threads=1 interop_full`, exit 0, 428 s, against
`docker.io/apache/sling:14`, Oak build under test **oak-segment-tar 1.90.0**.
Direction froe-to-Oak for every maintenance phase; Oak served the *exact*
baseline tree after full compaction, tail compaction, all three checkpoint
removals, and cleanup, and after `repair` with Oak's own JVM killed by
`SIGKILL` while holding an archive. `backup` and `recover` passed.

Re-verified after every depth limit was removed and all six walks were
rewritten to carry their own stack. Every phase passed against real Oak, and
each boot asserted Oak logged none of its repair messages — so Oak consumed
the store as the rewritten compactor wrote it, rather than reconstructing it.
That run is the load-bearing evidence for the walk rewrites: the exact sharing
memo changes which records a compacted store contains, and only Oak reading it
back can show the result is the store Oak expects.

Re-verified twice before that, after `v0.8.0`, for the earlier write-path
changes, most importantly binary value sharing: `compact` and `compact --tail` passed with
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
| `cargo +1.89.0 test --workspace --all-features --release --no-fail-fast` | 0, 683 tests |
| `RUSTDOCFLAGS="-D warnings" cargo +1.89.0 doc --workspace --all-features --no-deps` | 0 (failed first run on a dangling intra-doc link; fixed in `83a3ad0`) |
| `RUSTFLAGS="-D warnings" cargo +1.89.0 check -p froe --all-targets --all-features --target i686-unknown-linux-gnu` | 0 |
| `cargo test --workspace` (stable) | 0, 681 tests |
| `cargo test -p froe-cli --features interop -- --ignored --test-threads=1 interop_full` | 0, every phase, 376 s |

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
9. **Walk state is depth-proportional and has no ceiling.** Removing the depth
   caps was right — they refused valid stores while doing none of the jobs
   they appeared to do, and aborted the process rather than refusing — but it
   left ~175 bytes a level of walk state bounded only by how many distinct
   records the store holds. `check.rs` and `backup.rs` have no work budget at
   all; `traversal.rs` and `diff.rs` charge theirs against node counts rather
   than residency. Negligible on any real content tree (a few hundred levels,
   ~70 KB), material only on a store shaped as a long chain. Measured by
   `measure_deep_chain_walk_footprint`; see "What removing the depth caps
   costs".
10. **The exactness oracles share a primitive.** Every distinct-reachable-node
    count that cross-checks `compacted_nodes` is built on
    `NodeState::child_node_entries()`, the same call the walks use, so a
    defect there would make walk, oracle and re-read agree on one wrong
    number.
