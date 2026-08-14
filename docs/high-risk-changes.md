# High-risk change safety and review guide

This guide applies when a change introduces, broadens, or reorders bytes
published in the storage format, locking, recovery, durability, or
destructive behavior, or can make repository bytes unreachable. It
expands the cumulative decision paths in
[`CONTRIBUTING.md`](../CONTRIBUTING.md). A narrow fix that preserves an
existing design may cite its existing safety case and update only the
affected invariant.

The objective is to make the first review about whether the design is
correct, not about discovering its proof obligations one at a time.

## Write the safety case first

During implementation, keep the safety case in the pull request, commit
series, or a design document and update it with the code. Before final
review, include that evidence in the committed range or freeze it as an
immutable artifact or permalink whose content hash and revision are
recorded. It covers:

* **Scope and retention.** Name the content roots, history, checkpoints,
  binary references, metadata, backups, and unknown files that must
  survive. Separate default-safe work from opt-in retirement. If users
  select subtasks, state the side effects each one authorizes; task
  isolation is part of the public contract.
* **Authoritative state.** Identify the lock boundary, the point where a
  preview is discarded and state is replanned or revalidated, and every
  fact that must be checked again when used. A preview is advisory. When
  preview exists, preview and apply share policy predicates; apply repeats
  them against locked state.
* **Mutation and publication order.** Perform predictable format,
  credential, namespace, ownership, and source-certification checks before
  the first transition they can protect. If an exact check depends on an
  earlier mutation, document that safe prefix and repeat the check before
  its dependent mutation. Stage, sync, reopen, and semantically validate
  replacement bytes, then publish them with a format-appropriate atomic
  sequence and sync the containing directory. Retire a separately named
  predecessor only after its replacement is durable; document cases where
  atomic replacement publishes and retires in one operation. Hold or
  recheck descriptor/path identity immediately before destructive path
  operations.
* **Interruption prefixes.** For each meaningful durability or publication
  boundary, define the old, new, or monotonic-prefix state visible after a
  returned error and after abrupt process death. Returned errors report
  observed completed mutations and any durability uncertainty; a dead
  process cannot report, so the next inspection or retry must recognize
  and safely reconcile the on-disk prefix.
* **Observed outcomes.** Build externally relevant counts and result
  records from observed committed operations, not a stale plan or the
  existence of an intended destination. Preserve materially different
  outcomes as typed state; for cleanup that can include unchanged,
  removed, rewritten, already absent, and retained-with-error. Never infer
  state from diagnostic text.
* **Resources.** State worst-case bounds or scaling for time, memory, open
  files, and temporary disk. Identify estimates that are only proxies and
  explain how resource exhaustion leaves a safe, reportable prefix.

A compact mutation table makes ordering reviewable:

| Boundary / cutpoint | Preconditions | Published or durable change | Returned-error state and named regression | Abrupt-exit state and named regression | Reconciliation |
| --- | --- | --- | --- | --- | --- |
| Example boundary | Checks completed before it | Bytes or namespace made visible | Observed prefix, uncertainty, and test | Discoverable prefix and test, or reason not applicable | Inspection or retry behavior |

“Meaningful boundary” means a durability, publication, authority, or
irreversibility transition—not every source line or syscall wrapper.

## Make the tests prove the case

### Safety guards

For every newly introduced or semantically changed refusal,
preservation, or publication guard, record this table in the safety case
or pull request:

| Guard and production callers | Named regression | Neutralization | Observed failing result |
| --- | --- | --- | --- |
| Guard name and every distinct phase | Exact test name | Change that disabled only this guard | Failure that proved the guarded property |

The regression reaches every materially distinct production caller or
phase. One test is sufficient only when callers converge through the same
production wrapper and separate wiring assertions prove their inputs
reach it correctly. A helper test is supplemental; use a filesystem- or
process-backed fixture when that boundary is part of the behavior.
Neutralize experiments serially in an isolated source state with labelled
logs, then restore the code and rerun the original test. Use a separate
target directory when build artifacts could contaminate the result.

### Fault and subprocess tests

Place fault cutpoints at the meaningful boundaries from the mutation
table. Cover both returned errors and abrupt process exit where their
states differ. As applicable, freshly reopen and assert:

* the exact old, new, or monotonic on-disk prefix;
* retained content, metadata, and recovery material;
* journal/head readability and repository consistency;
* lock reacquisition and safe retry; and
* the typed partial outcome returned to callers.

A child harness proves that the intended scenario ran and completed its
assertions. Exit zero is not sufficient because a filtered-out test also
exits zero. Use a full exact test name plus a scenario-specific sentinel
status or artifact emitted only after assertions. Every cutpoint is armed
by a named test or removed. For multi-stage protocols, assert only bytes
or events observed since the preceding boundary; retain cumulative
transcripts for diagnostics, not later-stage matching.

### Environment and scale

Use synthetic identities, widths, timestamps, and boundary values so a
predicate test does not depend on the runner being root, non-root, 32-bit,
or 64-bit. Also cover the representative production wiring from real
metadata or process state into that predicate.

For a new algorithm, or a change that introduces nested whole-store
traversal or alters scan complexity, state the asymptotic cost. Prefer a
deterministic operation/traversal counter or a bounded-cache assertion.
Benchmarks are supplementary unless they have a stable tracked threshold;
minor changes to an already-covered traversal may cite the existing
evidence.

## Verify portability deliberately

The current MSRV comes from `workspace.package.rust-version` in
[`Cargo.toml`](../Cargo.toml). CI and release targets come from
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) and
[`.github/workflows/release.yml`](../.github/workflows/release.yml); do
not copy their inventories into design notes.

In the commands below, replace `<MSRV>` with the current value from
`Cargo.toml`, `<TARGET>` with the target triple, and `<PACKAGE>` with the
affected package. These placeholders avoid duplicating changing project
metadata. The examples use POSIX-style environment assignments. In
PowerShell, set `$env:RUSTFLAGS = "-D warnings"` or
`$env:RUSTDOCFLAGS = "-D warnings"` before the corresponding command and
remove the variable afterward.

Install both toolchains:

```console
$ rustup toolchain install stable --profile minimal --component rustfmt,clippy
$ rustup toolchain install <MSRV> --profile minimal --component rustfmt,clippy
```

Run the stable host gate from `CONTRIBUTING.md`, then the high-risk gate:

```console
$ cargo +<MSRV> fmt --all -- --check
$ cargo +<MSRV> test --workspace --all-features --no-fail-fast
$ cargo +<MSRV> test --workspace --all-features --release --no-fail-fast
$ cargo +<MSRV> clippy --workspace --all-targets --all-features -- -D warnings
$ RUSTDOCFLAGS="-D warnings" cargo +<MSRV> doc --workspace --all-features --no-deps
```

Code under `cfg`, platform-specific imports, and `cfg(test)` code are all
part of the target compilation surface. A library-only check is
insufficient when tests contain platform code. Keep item, import, helper,
and test `cfg` expressions aligned; use `cfg(unix)` only for behavior
genuinely common to Unix targets.

Treat FFI aliases as target-dependent in width and signedness. This
includes types such as `mode_t`, `time_t`, `c_long`, `off_t`, `uid_t`, and
`gid_t`. Use explicitly typed Rust operands and checked conversions into
target-dependent fields. Do not use `as` unless losslessness is proved and
documented; Clippy cannot replace a target-width audit.

For portability-sensitive work, run `check` and Clippy on stable and the
MSRV for one representative of each affected OS, ABI, integer-width, or
conditional-compilation family, plus any release target with distinct
affected code. Include all targets, tests, and features. When integer
width matters, include a representative 32-bit target such as
`i686-unknown-linux-gnu`; it is a compile-only width sentinel, not a
released runtime platform.

```console
$ rustup target add --toolchain stable <TARGET>
$ rustup target add --toolchain <MSRV> <TARGET>
$ RUSTFLAGS="-D warnings" cargo +stable check -p <PACKAGE> --all-targets --all-features --target <TARGET>
$ cargo +stable clippy -p <PACKAGE> --all-targets --all-features --target <TARGET> -- -D warnings
$ RUSTFLAGS="-D warnings" cargo +<MSRV> check -p <PACKAGE> --all-targets --all-features --target <TARGET>
$ cargo +<MSRV> clippy -p <PACKAGE> --all-targets --all-features --target <TARGET> -- -D warnings
```

Cross-probe packages whose dependencies support the host/target
combination. Packages with bundled C libraries or other target-native
build dependencies may require the matching native CI runner or an
explicit cross compiler. Record any unavailable probe rather than
silently substituting a library-only or host-only check. CI remains the
authority for its native matrix.

## Freeze and review the evidence

Before final review:

1. Stop concurrent edits, commit the complete candidate, and use a clean
   worktree. Record the exact base and head commits, confirm there are no
   untracked candidate files, and run `git diff --check BASE..HEAD`.
   Uncommitted reviews are preliminary because ordinary diffs and
   checksums can omit untracked bytes.
2. Provide the frozen safety case, mutation/fault tables, exact
   verification commands, and known blind spots. If this evidence is not
   committed, record an immutable artifact or permalink, its revision,
   and a content hash. A later edit invalidates the affected final review
   and gates; review of a moving diff is preliminary.
3. Review the frozen candidate with a reviewer that did not author it,
   auditing production code, tests, subprocess/fault harnesses, public API
   and documentation, and the wording of evidence claims—not only the
   happy-path implementation. This project is single-maintainer, so that
   reviewer is normally an adversarial automated pass rather than a second
   person: several reviewers over the frozen range, each briefed on a
   distinct lens, each finding independently verified before it counts.

   Record which it was. An automated pass is weaker in a specific way — it
   is briefed by the author, so it inherits the author's framing of what
   the change is for, and it cannot notice a question nobody thought to
   ask. It is not a substitute for a second person and must not be
   described as one. It is what this project can actually sustain, and a
   gate nobody performs protects nothing.

   Do not skip the lens that audits the evidence wording. On the v0.8.0
   range it caught a safety-case row citing a test's *accept* condition as
   an observed failure, and behind it a regression that passed in both
   branches — a guard on the destructive path that was documented as armed
   and was not.
4. Address review findings in follow-up commits so the reviewed base stays
   stable unless amendment or squashing is explicitly requested. Review
   both each delta and the cumulative result.

The verification report binds each claim to the exact command, that
command's own exit status, platform, toolchain, test layer, fault model,
and asserted property. Avoid pipelines, or enable pipeline-failure
propagation and record the producing command's status rather than the
formatter's. The report explicitly separates:

* execution from cross-compilation;
* synthetic credentials from execution as root;
* process-exit or syscall injection from true power-loss ordering;
* file existence from durability; and
* froe-to-froe round trips from real Oak/AEM interoperability.

For interoperability, also record the exact Oak/AEM build, whether the
direction was Oak-to-froe or froe-to-Oak, the operation exercised, and the
verified post-state.

Avoid “all paths” or “no remaining issues” unless that exact scope was
reviewed. List the relevant environments and properties not exercised.
