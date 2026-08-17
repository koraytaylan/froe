# Command-line output and progress reporting

`froe` commands run for minutes against a real repository. This document
is the contract for what they say while they do it: which stream carries
what, when a report appears, what silence hides, and what a contributor
adding a new reported step owes.

The implementation is `froe::progress` (the observation API) and
`crates/froe-cli/src/progress.rs` (the renderer).

## The streams are not interchangeable

* **Standard output is data.** A compaction plan, a JSON lines export, a
  node listing, a segment dump, a consistency report. Nothing else ever
  goes there. Piping any command into a consumer gives exactly what it
  gave before progress reporting existed, byte for byte.
* **Standard error is everything else**: progress, status, warnings,
  errors, and confirmation prompts.

`reporting_never_reaches_the_standard_output_of_a_compaction_plan` pins this
by running the same plan under `--progress always`, under the default,
and under `--silent`, and requiring the three standard outputs to be
byte-identical.

## Three styles, chosen once at startup

| Style | When | What it looks like |
| --- | --- | --- |
| Animated | standard error is a terminal | one live line rewritten in place: `froe: searching segments [█████████████░░░░░░░░░░░░]  52% 64/123 0:00 eta 0:00` |
| Plain | standard error is a pipe, a file, or a CI log | the same information as whole lines, at most one every two seconds |
| Silent | `--silent` or `--progress never` | nothing |

Reporting can never change what a command does. Standard error is a pipe
often enough — `froe compact --yes 2>&1 | less`, quit early — and `main`
restores SIGPIPE to its terminating disposition so that piping *data*
into `head` ends quietly. Those two facts together once let a progress
line kill a destructive run between its mutations. The reporter's
writes, and only the reporter's, therefore suppress that signal: a closed
stream yields `EPIPE` and the reporter falls silent for the rest of the
run, rather than felling the process.

Two mechanisms, because the signal is not directed the same way on every
Unix. **Linux** raises it on the thread that wrote, so blocking it there
suffices. **Darwin** raises it on the *process* — XNU's `sys_generic.c`
write path calls `psignal(vfs_context_proc(ctx), SIGPIPE)` — and hands it
to any thread that does not have it masked; froe always has one, because
the ticker inherits an empty mask. A per-thread mask is therefore
structurally insufficient there, so on Darwin standard error's
`F_SETNOSIGPIPE` flag is set for the duration of the write instead — the
flag that same XNU path tests before raising anything. Both are restored
afterwards, including on unwind, so the conventional
terminate-on-closed-*standard-output* behaviour the CLI deliberately
keeps is untouched.

**The Darwin path is unverified on Darwin**: no macOS machine was
available, and `froe-cli` cannot even be cross-checked for that target
because its dependency graph bundles zstd and SQLite C sources. It was
nonetheless compiled, linted under `--deny warnings`, and run on this
host by copying the arm into a scratch crate carrying the workspace's
lint table with `target_vendor = "apple"` flipped to the host — enough to
establish that it builds, types, and passes the gate, but not that the
kernel behaves as `bsd/sys/fcntl.h` and `bsd/kern/sys_generic.c` say it
does. CI's `macos-latest` job is that evidence, and it fails loudly
either way: a wrong `fcntl` constant leaves `F_GETNOSIGPIPE` returning
-1 so no guard is built, and an ineffective suppression fails
`a_closed_standard_error_cannot_kill_a_destructive_compaction` there.

Reporting is deliberately conservative about the terminal:

* **No ANSI escape sequences at all.** The cursor moves with a carriage
  return and the line is cleared with spaces. There is no colour. A
  progress line can therefore never carry an escape sequence to a
  terminal, whatever a repository contains — the same property the
  diagnostic commands already had, extended to the reporter.
* **Repository-controlled text is sanitized** through
  `output::sanitize_terminal_text` before it is rendered, and every line
  is truncated to the terminal width, so a hostile archive name cannot
  wrap the line or defeat the erase.
* **The bar falls back to ASCII** (`[####----]`) where the locale does
  not declare UTF-8, rather than emitting block characters a terminal
  might render as mojibake. On Windows, Rust writes to a console through
  `WriteConsoleW`, so the code page cannot mangle them and the block
  characters are used.
* The terminal width comes from `COLUMNS`, then from `TIOCGWINSZ` on
  Unix, then from an assumed 80 columns.

## Numbers

Counts carry thousands separators on both streams: `18,796,598 nodes`.

Byte counts are scaled to binary (IEC) units with one decimal place, truncated
toward zero so a figure never claims more than is there: `612.4 MiB`,
`54.7 GiB`. Counts below one kibibyte keep their exact value and the plain
noun — `512 bytes` — because at that size the exact number is both shorter and
more useful. Binary units, not decimal: the format is binary throughout, a
segment caps at 262144 bytes and an archive rotates at 256 MiB, and an operator
comparing froe's figure against `du` needs the ambiguity of "GB" gone. The
implementation is `froe::format_byte_size`, shared by the library warnings and
the CLI so one rule serves both.

Four renderings deliberately stay unscaled, because their exact integer is the
thing being reported rather than a size an operator is comparing:

* `froe summary`'s `archive bytes` and `froe segment`'s `size` print the scaled
  figure *and* the exact count, one labelled value per line;
* `froe archives` and `froe segments` print tabular rows whose fields are split
  on whitespace by downstream consumers, so no field may gain a token;
* `froe segment --debug`'s `Debug file {path}({length})` header reproduces
  Oak's `oak-run debug` output byte-for-byte;
* the `{-1 bytes}` / `{N bytes}` binary-size rendering in archive debug output
  reproduces Oak's `AbstractPropertyState.getBinarySize`, and the
  segment-size-limit diagnostic names an exact format boundary.

## Nothing is reported for work that finishes promptly

A step that completes within 300 milliseconds reports nothing at all —
not its start, not its completion. A command that simply did its job
stays quiet, scripted standard error keeps whatever it had, and only a
command that makes the operator wait explains itself.

The deferral measures the **operation**, not one step. A command that
opens a step per item — `check` opens one per revision — would otherwise
restart the delay on each and stay silent however long the whole run
took. Once the run has passed the delay, later steps report at once; in
the plain style they are then throttled across the run as well as within
one step, so a thousand short steps cannot contribute a thousand lines.

`--progress always` sets that delay to zero. Scripts wanting the reports
in a log, and tests asserting them, pass it.

An export streaming to a terminal's *standard output* shares the screen
with its own data, so it reports nothing; a redirected export keeps its
progress.

## What `--silent` hides, and what it never hides

`-s` / `--silent` suppresses progress reports and informational status
lines. `--quiet` is a hidden compatibility alias — `froe export --quiet`
predates uniform reporting — and means exactly the same thing.

It never suppresses:

* **errors** — `silence_never_hides_an_error`;
* **warnings** — a maintenance warning is a fact about the repository, not a
  progress report;
* **confirmation prompts** — a silenced destructive run still prints
  its plan and still asks, in full:
  `silence_never_hides_the_destructive_confirmation_prompt`;
* **any command's own output** on standard output.

`--silent` hides what froe is doing. It never hides what froe found or
what froe is about to change.

## What each command reports

Every command reports the archive scan behind it (`opening archives`),
because that is the first thing that takes time on a large store.
Beyond that:

| Command | Steps |
| --- | --- |
| `compact` (plan and locked replan) | `verifying the current head` (nodes), `analyzing journal revisions` (journal lines), `scanning for stale archives` (archives), `tracing segments reachable from the head` and `…from history` (segments), `predicting the shared binary content` (nodes) and `predicting the reclamation` (archives) — the read-only pass that lets the plan name the archives the run will remove or rewrite — `planning residue retirement` (archives, only when an interrupted earlier run left output ahead of the head), `scanning for stale temporary files` (files) |
| `compact` (apply) | `checking archive indexes for repair` (archives, only with `--repair-archive-indexes`, and before the plan exists; it counts every archive number examined, of which only the damaged ones are rebuilt), `opening archives`, `retiring interrupted-compaction residue` (archives, only when there is any), `removing stale archives` (files), `certifying source archives` (archives), `copying nodes into a fresh generation` (nodes), `reclaiming old generations` (archives), then the reopen / head verification / journal analysis triple — **twice**, once for the journal retirement and once for the final proof — and only then `removing stale temporary files` and `removing old recovery backups` (files), which are deliberately the last mutation of all |
| `compact` | `opening archives for writing`, `certifying source archives` (archives), `copying nodes into a fresh generation` (nodes), `reclaiming old generations` |
| `backup`, `restore` | `opening archives for writing` for the target (archives), `copying nodes` (nodes) |
| `checkpoint create\|remove\|remove-all\|remove-unreferenced` | `opening archives for writing` (archives) |
| `recover-journal` | `scanning segments for super-roots` (segments), `probing candidates for consistency` (revisions) |
| `check` | one step per revision — `checking revision N of M` — counting the nodes that revision's walk resolves |
| `search-nodes` | `searching segments` (segments) |
| `history` | `tracing revisions` (revisions) |
| `difference` | `comparing revisions` (nodes) |
| `export` | `exporting nodes` / `re-exporting changed nodes` (nodes); the command then reports its own outcome, naming the destination |
| `summary`, `journal`, `archives`, `segments`, `segment`, `node`, `tree`, `checkpoints`, `debug` | the archive scan only — these do no other work worth waiting for |

`check` gets one step *per revision* rather than one bar across all of
them, because a healthy store pins every path at the first revision: a
single step counting revisions would sit at `0` of the whole journal for
the entire run, which is the silence this reporting exists to remove. The
revision's position rides in the description, where a step counting one
unit can still carry it. The declared total also respects `--revisions`,
so a bounded run never advertises a total it is forbidden to reach.

Known granularity limits, stated rather than hidden:

* `debug` reports no traversal step of its own. Its work is already
  bounded by explicit budgets, and it prints as it goes.
* `reclaiming old generations`, `removing checkpoints`, and the
  prospective-plan validation have no counter of their own — they are
  single indivisible commits — so they report the time they took rather
  than a count. The ticker still announces them and prints their
  completion, in both rendering styles.
* A step whose first work is one long call — the head verification of a
  node with hundreds of thousands of children must materialise them all
  before it can walk any — shows the clock moving with no count until
  that call returns.

## Rules for adding a reported step

1. **A function that reports owns its step.** Never wrap a step around a
   call that opens one of its own: the inner step would end the outer
   one, and two different counters would share a single report. This
   rule was written after `ObservationLog` caught exactly that in two
   apply-path phases.
2. **Report items completed, never started.** A bar that shows a hundred
   per cent while the last item is still being worked on is a lie a
   one-item step tells for its entire duration. Advance at the top of a
   loop with the index (items behind you), and report the exact total
   once after it. The same rule holds when the loop is an iterator the
   engine pulls lazily: pulling item N means items `0..N` are resolved,
   so report the count *before* incrementing it, and flush the final
   count once the engine has stopped pulling.
3. **Never declare a total the run cannot reach.** A step bounded by a
   limit declares the bounded figure, and a phase that is one indivisible
   commit declares no total at all rather than a total it will never
   advance toward.
4. **Report on a stride in an inner loop**, and flush the exact final
   count when the loop ends — `StrideCounter::finish` — or the last
   partial batch is never reported and a short step reports nothing.
5. **Bracket a fallible loop** with `progress::observe`, so an error can
   never leave a step open.
6. **A hook reads; it never decides.** Every call passes an
   already-computed value. Nothing may collect a collection to compute a
   total, choose what is visited, or change an order.

## Safety guards

| Guard and production callers | Named regression | Neutralization | Observed failing result |
| --- | --- | --- | --- |
| Reports never reach standard output. Every command; exercised through `compact --dry-run`, the plan an operator confirms a destructive run from. | `reporting_never_reaches_the_standard_output_of_a_compaction_plan` | `Reporter::new` constructs with `std::io::stdout()` instead of `std::io::stderr()`. | The reported and silent standard outputs differ: every progress line is prepended to the plan. Four unrelated export tests also fail, their data streams contaminated. |
| Silence never hides a destructive confirmation. `mutation::confirm`, reached by every mutating command; exercised through an interactive `compact --silent`. | `silence_never_hides_the_destructive_confirmation_prompt` | `confirm` writes the prompt through `Reporter::status`, which silence suppresses. | The test times out after 15s waiting for a prompt that never arrives, with the plan printed and the process blocked on an invisible question. |
| The observed twin of an operation returns what the plain spelling returns. `plan_compaction` / `plan_compaction_with_progress`, and the same pairing for `compact`, `backup`, and the readers. | `an_observed_plan_equals_an_unobserved_one`, `an_observed_compaction_equals_an_unobserved_one`, `an_observed_backup_equals_an_unobserved_one` | `plan_compaction` stops delegating to `plan_compaction_with_progress` and appends a warning of its own. | `assertion left == right failed: observation must not change the plan an operator confirms`, with the drifted warning shown in the diff. |
| The reported sequence is well formed: advances inside a begin/end pair, counts never decreasing, never overshooting a declared total. Every observable operation. | `ObservationLog`, asserted on every call of all six `progress_api_tests` | (found a live defect rather than needing one injected) | `rewriting the journal: counts must not run backwards (0 after 8)` — two apply-path phases whose step wrapped inner steps. Fixed by rule 1 above. |
| An animated line is erased before another writer takes the stream. `Reporter::while_suspended`, used by `mutation::confirm`. | `suspending_erases_the_live_line_before_the_borrower_writes` | (covered by the prompt guard's neutralization) | — |
| Reporting never changes a command's outcome. `RenderState::write_line`, the reporter's only path onto the stream; reached by every command, exercised through a destructive `compact --yes` whose standard error is a pipe with no reader. | `a_closed_standard_error_cannot_kill_a_destructive_compaction` | `write_line` writes without `without_sigpipe`. | The child dies of signal 13 (`ExitStatus(unix_wait_status(13))`) with the journal rewrite undone and `journal.log.bak.000` absent. |
| A bounded run never labels a revision past its own bound. `check_consistency_with_progress`, reached by `froe check --revisions N` over a journal containing an unresolvable line. | `a_bounded_check_never_labels_a_revision_past_its_bound` | The label uses `checked_revisions` rather than `checked_revisions.min(examinable)`. | `a bounded run advertised "checking revision 3 of 2", past its own bound`. |
| A step whose work spans several calls keeps one running total. `NodeTreeVerifier::verify_with_progress`, used per root by the planner's projected node count. | `a_verifier_reporting_several_roots_keeps_one_running_total` | The verifier resumes from `0` instead of its carried total. | `verifying the current head: counts must not run backwards (2 after 10)`. |
| A run of short steps is neither silenced by a per-step deferral nor spammed into a log. `ReporterInner::render`. | `many_short_steps_are_neither_silenced_nor_spammed` | (bounded by the two assertions in the test itself, either side of the behaviour) | — |
| A long step with no counter of its own still announces itself and completes when standard error is a log. `Ticker`, in the plain style; reached by compaction's reclamation sweep and the checkpoint removal. | `a_plain_step_that_never_advances_still_announces_and_completes` | `Ticker::start` is spawned only for `Style::Animated`, as it originally was. | The captured stream is empty: a ten-minute phase leaves no announcement and no completion line. |
| `check` reports the nodes a revision resolves, not a revision counter that cannot move. `check_one_path` → `verify_subtree`. | `every_observable_reader_reports_through_the_same_trait` | `check_one_path` is passed a fresh `VerifiedNodeCount::new(&mut DiscardedProgress)` instead of the step's counter. | `the revision step counts the nodes it resolved, not a frozen revision count: None`. |

Each neutralization was applied on its own, to an otherwise clean tree,
built into a separate target directory, and reverted before the next.

What the inertness guards do **not** prove: both spellings of an
operation share one body, so no test at this level can show that the
shared body ignores its observer. That property is structural and is held
by rule 5 and by review of the hooks themselves — every one is an
inserted call on a value the operation had already computed.

## Verification

Executed on Linux x86-64 (`x86_64-unknown-linux-gnu`), each command run
on its own and its own exit status recorded, on both the stable and the
MSRV (1.89.0) toolchains:

| Command | stable | MSRV 1.89.0 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | 0 |
| `cargo test --workspace --all-features --no-fail-fast` | 0 | 0 |
| `cargo test --workspace --all-features --release --no-fail-fast` | 0 | 0 |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | 0 |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps` | 0 | 0 |

Integer width is the one axis this change adds: the reporter does
percentage, rate, estimate, and bar arithmetic on counts, and
`progress::count` converts collection lengths. Every one of those uses
explicit `u64`/`u128` operands and checked or saturating conversions
rather than `as`, and `percentages_and_bars_stay_within_their_bounds`
pins the extremes (`u64::MAX` against small and equal totals) so an
overshooting count is capped rather than wrapped.

* `RUSTFLAGS="-D warnings" cargo +stable check -p froe --all-targets
  --all-features --target i686-unknown-linux-gnu` — exit 0. This covers
  every library-side hook and `progress::count`.
* The same probe for `froe-cli` is **not available on this host**: its
  dependency graph bundles the zstd and SQLite C sources through
  `froe-export`, and neither a 32-bit C runtime (`gcc -m32` cannot link:
  no `Scrt1.o`) nor a Windows cross toolchain (`lib.exe`) is installed.
  Recorded as unexecuted rather than substituted with a host-only check.
  The renderer's own width-sensitive arithmetic is nevertheless
  target-independent by construction, as above, and is exercised by unit
  tests on the host.
* `terminal_width_from_ioctl` is `#[cfg(unix)]` FFI. It is written with
  an explicitly typed `libc::winsize` and `usize::from(u16)` — no `as` —
  and the `#[cfg(not(unix))]` arm of `supports_unicode_bar` is a
  constant. CI's `windows-build` job (`cargo check --workspace
  --all-targets` at 1.89) is the authority that the non-Unix arms
  compile, and its macOS gate the authority for the other Unix.

Not executed here: native macOS and Windows runs, and any Oak/AEM
interoperability run. The interoperability surface is unchanged — this
change writes no repository bytes and reorders no mutation — so no new
interoperability evidence is owed.
