# Contributing to froe

`froe` is an independent Rust implementation of Apache Jackrabbit Oak's
`segment-tar` ("TarMK") storage format — the repository format of Apache
Jackrabbit Oak and Adobe Experience Manager. Its contract is severe:
anything `froe` writes must leave the repository in a state a real AEM
instance starts and runs against without any problem. Every rule in this
document exists to protect that contract.

## The prime directive: format fidelity

The on-disk format is defined by what Oak's Java implementation writes
and tolerates — not by documentation, not by intuition, and not by the
Java source *comments* (which are wrong in places the code is not).

* Ground truth lives in [`docs/analysis/`](docs/analysis/): byte-exact,
  adversarially verified specifications extracted from the Java sources.
  Consult them **before** touching any parser or serializer. If a spec
  is silent on something you need, extend the spec first (citing the
  Java file and method), then implement.
* Load-bearing quirks are reproduced deliberately — signed UUID index
  ordering, validation constants copied across format versions, wrapping
  32-bit arithmetic, masked shift distances. Never "fix" one without
  proof from the Java sources that it is not observable behavior. The
  distilled list is in
  [`docs/storage-format.md`](docs/storage-format.md).
* Deliberate deviations are allowed only when they are strictly safer
  than Java (for example: in-memory recovery instead of writing
  `.ro.bak` files; returning errors on cyclic corrupt records where Java
  hangs). Every deviation must be documented at the deviation site and
  must never lose data Java would return, nor write bytes Java would not
  accept.

## Safety rules for the write path

* **Read-only means read-only.** Read paths never take the repository
  lock, never create files, never modify files — pointing `froe` at a
  live repository must always be safe.
* **Write paths take the lock first.** Every mutating operation acquires
  `repo.lock` exclusively before touching anything, exactly like Oak,
  so a running AEM instance and `froe` can never write concurrently.
* **Durability ordering is part of the format.** Segment data is forced
  to disk before the journal line referencing it is appended. Never
  reorder writes for convenience or speed.
* **Corrupt input returns errors.** No input bytes — however hostile —
  may cause a panic, an unbounded allocation, unbounded recursion, or an
  infinite loop. Walks over record graphs carry cycle bounds; sizes from
  files are validated before allocation; arithmetic on file-supplied
  values is checked or deliberately wrapping (matching Java), never
  implicitly overflowing.

## Choose the required proof before implementation

These paths are cumulative, and the strictest downstream effect controls
the classification. For example, a read-only planner or preview that can
authorize later deletion is high-risk even though the analysis itself
does not write.

* **Documentation, local refactors, and isolated read-only behavior**
  follow the code standards, applicable test layers, and stable host gate
  below.
* **Parsers and read-only format interpretation** additionally cite the
  Oak evidence, use independent fixtures, and run the portability checks
  relevant to the changed representation.
* **High-risk changes** are storage serializers or other changes that
  introduce, broaden, or reorder bytes published on disk, locking,
  recovery, durability, destructive behavior, or reachability of
  repository bytes. They require the safety case, fault matrix,
  load-bearing guard tests, cross-target checks, and frozen independent
  review in [`docs/high-risk-changes.md`](docs/high-risk-changes.md).

A narrow fix that preserves an established high-risk design may link to
its existing safety case and update only the affected invariant. Decide
the path before coding; do not discover the proof obligations after the
implementation is complete.

## Code standards

* **Minimal dependencies.** Everything in the tree must be strictly
  necessary and pull its weight. The crate manifests are the
  authoritative inventory; large format backends stay feature-gated.
  CRC32, Java string hashing, JSON encoding, and timestamp formatting
  are hand-implemented — a new dependency needs a reason those did not.
* **User-facing documentation moves with the code.** A change that adds,
  removes, or alters a capability updates the capability inventory in
  [`docs/oak-segment-tar-feature-map.md`](docs/oak-segment-tar-feature-map.md),
  the affected command's guide, and any remediation text, in the same
  commit. A guide that claims a capability is unimplemented after it
  ships, or omits a side effect the code now has — a one-way format
  upgrade, a file an interrupted run leaves behind — reads as correct and
  costs a review cycle to rediscover.
* **Idiomatic Rust, not transliterated Java.** The format semantics are
  exact; the implementation is not a line-by-line translation. Errors
  are `Result`s, not exceptions; ownership replaces defensive copying;
  iterators replace index loops where clearer.
* Rust edition 2024, workspace lints as configured in the root
  `Cargo.toml`: `missing_docs`, `unreachable_pub`, and clippy's
  `pedantic` group must all be clean.

The rest of this section is craft guidance. It is a set of heuristics
with a stated purpose, not a checklist to satisfy mechanically: a
reviewer may ask why a rule was not followed, and "following it here
made the code harder to read" is a complete answer. Where a heuristic
collides with Rust idiom or with format fidelity, the idiom and the
format win.

### Naming

* **No abbreviations** in file, method, or variable names:
  `most_significant_bits`, not `msb`; `identifier`, not `id`;
  `record_number`, not `rec_no`. Universally standard acronyms (UUID,
  CRC32, JCR, TAR, JSON, CLI) are acceptable; ad-hoc shortenings are
  not. The cargo-mandated `src/` directory is the tolerated exception.
* **Names reveal intent, not mechanism or type.** `reclaimed_bytes`
  beats `count`; `retained_generations` beats `filtered`. A name that
  needs the line after it to be understood is the wrong name. Loop and
  closure bindings are held to the same standard as fields — a
  three-line closure does not earn `x`.
* **Types are nouns, functions are verbs.** Predicates read as
  questions: `is_reachable`, `has_binary_references`, `can_reclaim`.
  Conversions follow the Rust API guidelines — `as_` for a borrowed
  view, `to_` for a copy, `into_` for a consuming conversion — because
  those prefixes carry cost information a reader relies on.
* **The same concept keeps the same word everywhere.** The format
  already has enough near-synonyms (segment, record, entry, blob); a
  port that renames a concept per module makes every cross-module read
  a translation exercise. When Oak's name is the clearest one, use
  Oak's name so the analysis documents and the code share a vocabulary.

### Functions

* **One thing, at one level of abstraction.** A function that both
  decides *what* to do and performs the byte-level *how* forces a
  reader to hold two altitudes at once. Split at that seam. The
  reliable smells are a comment introducing a block, a block whose
  locals are used nowhere else, and a name containing "and".
* **A hundred lines is the limit, and there is no escape hatch.**
  clippy's `too_many_lines` is the enforcement, and
  `#[allow(clippy::too_many_lines)]` is forbidden anywhere in the tree —
  production, fixtures, and tests alike. The lint counts physical lines
  between a function's braces, ignoring blank and comment-only lines, so
  the budget is real code. A function that needs more than that is
  telling you it holds more than one idea.
* **Twenty decision points, and seven levels of nesting.**
  `cognitive_complexity` and `excessive_nesting` in `clippy.toml` hold the
  two, and the comments there say what each one does and does not measure —
  the first counts branches without weighting them by depth, which is why
  the second exists. Production sits at 16 and 7 today, so both are
  ceilings rather than a cleanup project.
* **Split at the seams the function already has.** A long body almost
  always has phases separated by blank lines and a comment introducing
  each; those comments are the names of the functions hiding inside it.
  Extract them so the caller reads as the sequence it always was, with
  each step's *what* in its name and its *why* in its own doc comment.
  Where a linear order is load-bearing — a crash-safe mutation
  sequence, a durability protocol — the extracted steps stay in the same
  order in the same caller, so the ordering is still read top to bottom
  in one place. What must not survive is one screen's worth of code that
  a reader has to re-derive that structure from every time.
* **Few parameters, and none of them bare `bool`.** Past roughly three,
  group them into a struct — a caller passing five positional arguments
  cannot be reviewed for argument order. A boolean at a call site is
  unreadable (`compact(store, true, false)`); use a two-variant enum
  whose names say what each choice means. This matters most on the
  write path, where a transposed flag is a data-loss bug.
* **No side effects the name does not advertise.** A function named for
  a question does not mutate; a function named for a query does not
  write files, take the lock, or advance a generation. Where an
  operation must both compute and persist, the name says so
  (`write_and_flush`, not `prepare`). Read-only means read-only is the
  same rule enforced at function granularity.
* **Typed errors, never sentinel values.** Failure is a `Result` with a
  variant a caller can match on. `Option` means absence, never failure;
  `-1`, `0`, and empty collections never stand in for an error. A
  variant carries the values needed to report the fault — the offending
  offset, the expected and actual checksum — because a message the
  caller must reconstruct gets reconstructed wrongly.

### Comments

* **Documented, concisely.** Every module starts with a doc comment
  explaining what it models and the non-obvious facts of the format it
  implements. Public items carry doc comments. Safety comments describe
  the exact mechanism and scope proved by the code, not a stronger
  idealized state.
* **Comments explain why; the code explains what.** A comment
  restating the next line is noise that goes stale. In this crate the
  *why* is usually unavailable from the code at any quality — that Oak
  writes a signed comparison here, that a constant was copied unchanged
  into a later format version, that an arithmetic step wraps
  deliberately. Those comments are mandatory and cite the Java.
* **A comment compensating for a name is a rename.** If a line needs a
  gloss to say what a variable holds, the variable is misnamed. Fix the
  name and delete the comment.
* **No commented-out code.** Version control keeps it. A disabled test
  is either deleted or marked `#[ignore]` with a reason and a linked
  issue.

### Structure and design

* **Related things stay close.** A helper sits directly below its only
  caller; a type's inherent `impl` sits next to the type. Vertical
  distance implies unrelatedness, so distance between two things that
  must change together is a defect. A module that has grown past the
  point where related things can stay close is a module to split.
* **A thousand lines is the limit for a file.** `scripts/oversized-files.sh`
  enforces it and CI runs it, because clippy has no lint for this —
  `too_many_lines` measures a function's body, not a module. Split at the
  seams the file already has, and move each test to the module it now
  belongs with. A directory module (`foo/mod.rs` plus submodules) is the
  usual shape; an inherent `impl` may be divided across them, so a large
  type does not force a large file.
* **One reason to change per module and per type.** A type that
  models a format structure does not also own I/O scheduling or
  progress reporting.
* **Ask which way the dependency points before moving a module.** A name
  like `maintenance_fault_injection` looks like it wants to live under
  `maintenance/`, but its cutpoints are called from the tar writer and the
  store writer too — modules `maintenance` is built on. Moving it in would
  have made them reach into a private module above them. `journal_maintenance`
  was the opposite: every caller was already inside `maintenance/`, so
  moving it in only removed reach nobody was using.
* **DRY, with two deliberate exceptions.** Duplication is the default
  maintenance hazard, and shared logic belongs in one place. But the
  independent encoder in `crates/froe/tests/support/` duplicates
  production encoding *on purpose* — sharing code with production would
  destroy the property that makes it evidence — and validation
  constants repeated across format versions are duplicated because Oak
  duplicates them. Both are documented at the site; neither is ever
  "cleaned up".
* **Tell, don't ask.** Behavior lives with the data it operates on. A
  caller that reaches through two accessors to compute something the
  owner could answer wants a method on the owner instead.
* **Abstract on the second implementation, not the first.** Traits and
  generics earn their place when there is a real second implementer or
  a test needs a seam. A trait with one implementation, a builder for a
  struct with two fields, or a configuration knob no caller sets is
  speculative generality — the same instinct minimal dependencies
  exists to resist. YAGNI applies to internal structure, not to format
  coverage: the format's own complexity is not optional.

## Testing standards

Tests are the port's proof of fidelity. Every change keeps all applicable
layers green. New functionality explains why any layer does not apply:

1. **Unit tests with hand-crafted bytes.** Every parser and serializer
   is exercised against fixtures written out by hand from the
   specifications — including corrupt, truncated, hostile, and
   boundary-value inputs.
2. **Independent-encoder round-trips.** Integration tests build
   repositories through `crates/froe/tests/support/` — a separate
   encoder that shares no production encoding or parsing code with the
   reader or writer, its single production import being
   `checksum::crc32`, which known-answer vectors pin independently — so a
   self-consistent bug in one cannot hide in the other. Writer changes
   additionally round-trip through the *reader*: everything written must
   be read back identically.
3. **End-to-end store tests.** Mutating operations run against complete
   synthetic repositories on disk and assert the full post-state:
   archives reopen with valid indexes, the journal resolves, content is
   intact, and read-only invariants (no lock, no writes) hold where
   promised.
4. **Oak/AEM interoperability for substantial writer changes.** Before a
   new format-writing or maintenance feature is called production-ready,
   open its output with a real compatible Oak/AEM version and exercise
   froe against Oak-produced input. If that environment is unavailable,
   record the gap explicitly and keep the feature labelled beta; a
   froe-to-froe round trip is not a substitute. Record the exact Oak/AEM
   build, producer-to-consumer direction, operation exercised, and
   verified post-state.

### Tests are first-class code

The standards above apply to test code without discount — a test suite
this large is read far more often than it is written, and a test nobody
can read is a test nobody dares change when the format demands it.

* **One concept per test, named for the concept.** The name states the
  property being pinned, so a failure line alone tells a reader what
  broke: `rejects_index_entry_past_archive_end`, not `test_index_3`.
  Several assertions that together pin one property are fine; two
  unrelated properties in one test are two tests, because the first
  failure hides the second.
* **Independent and repeatable.** Tests share no mutable state, run in
  any order, and depend on no wall-clock time, no ambient environment,
  and no leftover directory from a previous run. Anything written goes
  to a temporary directory the test owns and removes.
* **Fixture-building belongs in helpers; the property belongs in the
  test.** A reader should see the arrangement summarized and the
  assertion in full, not thirty lines of byte-array setup obscuring one
  comparison. Helpers are named for what they produce.
* **Assert the observable post-state, not the implementation path.** A
  test that pins internal call order breaks on every refactor and
  proves nothing about the format.

### Make safety tests load-bearing

High-risk tests exercise every materially distinct production phase or
caller; a helper test alone is not evidence that the protection is wired
into production. The authoritative guard-neutralization, fault-harness,
environment-wiring, scale, and mutation-test requirements are in
[`docs/high-risk-changes.md`](docs/high-risk-changes.md).

The same proof principle applies at every tier to newly introduced or
semantically changed refusal and resource-limit guards: add a named test
that reaches the production-facing entry, fails when only that guard is
neutralized, and pins the exact boundary or typed error. Helper tests may
supplement that wiring proof. A declared work or memory budget documents
its accounting unit, charges every operation or allocation in that unit,
and has an exact limit/limit-plus-one regression.

Public configuration and diagnostic types must be usable as documented
from a downstream crate. If a type is deliberately non-exhaustive, provide
constructors or builders for every supported configuration; if an error is
internal, keep it crate-private or document the public path that can
produce it. Add an external integration test whenever Rust visibility or
type construction is part of the contract.

## Verification and portability

The executable CI matrix in [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
is authoritative for automated platform coverage. Before requesting
review, every code change runs the stable host gate:

```console
$ cargo +stable fmt --all -- --check
$ cargo +stable test --workspace --all-features --no-fail-fast
$ cargo +stable test --workspace --all-features --release --no-fail-fast
$ cargo +stable clippy --workspace --all-targets --all-features -- -D warnings
$ RUSTDOCFLAGS="-D warnings" cargo +stable doc --workspace --all-features --no-deps
```

Documentation-only changes with no generated or code-facing effect may
run only the relevant documentation, formatting, and diff checks; record
the omitted gates. Before final review, the relevant CI matrix must also
be green. If no CI run is available, local MSRV host tests may support
preliminary review, but they do not replace native macOS or Windows jobs;
record those platform axes as unexecuted. High-risk and
portability-sensitive changes also run the MSRV, cross-target, and
integer-width checks in
[`docs/high-risk-changes.md`](docs/high-risk-changes.md). Code behind
`cfg(test)` is part of the target's compilation surface. Record whether
each platform was executed or only cross-compiled, and list relevant
environments not exercised.

Diff checks cover only tracked paths. Before checking an uncommitted
change, ensure `git status --short` contains no intended `??` path (stage
new files or add them with intent-to-add), then run `git diff --check
HEAD`. For committed work, check the complete review range with `git diff
--check BASE..HEAD`.

## Workflow

* Development happens on `develop`, the default branch. Releases are cut
  and tagged there, following [`docs/releasing.md`](docs/releasing.md).
  `main` tracks the latest released commit: the release workflow
  fast-forwards it after the crates reach the registry, so it is a
  pipeline-maintained pointer rather than a branch anyone commits to. Open
  changes against `develop`.
* Commits follow the conventional commit prefixes
  [`cliff.toml`](cliff.toml) groups into release notes — `feat:`, `fix:`,
  `harden:`, `perf:`, `refactor:`, `docs:`, `test:`, `chore:` — with
  bodies that explain *why*, in full sentences.
* A change to format-facing code cites the specification section (or the
  Java file and method) that justifies it, in the commit body or the
  code.
* **Leave code cleaner than you found it — in its own commit.** Renaming
  a confusing local, deleting a stale comment, or splitting a function
  that has outgrown its name is welcome as you pass through. Keep those
  edits in a `refactor:` commit separate from the behavior change,
  because a format-facing diff that also carries unrelated cleanups
  costs a reviewer the ability to see what actually changed on disk —
  and on a high-risk path, that reviewer is the last line of defense.
  A cleanup commit asserts that no byte written to disk changed.

### Review discipline

High-risk final review uses a committed, frozen `BASE..HEAD` range and a
reviewer that did not author the patch — normally an adversarial automated
pass, since this project is single-maintainer. Later edits invalidate the
affected review and gates. The authoritative review procedure, evidence
requirements, what an automated pass can and cannot stand in for, and the
follow-up-commit policy are in
[`docs/high-risk-changes.md`](docs/high-risk-changes.md).

What a reviewer owes in return — that a finding asserting Oak behavior
cites the Java rather than an analysis document, that a claim about the
language or a public API quotes the definition, that validating a
committed range isolates it, and how severity and adversarial
verification are calibrated — is in
[`docs/reviewing.md`](docs/reviewing.md). A false finding costs the
author a cycle just as a missed defect costs a release.

## License

Apache-2.0. By contributing you agree to license your work under the
same terms. Apache Jackrabbit Oak is a trademark of the Apache Software
Foundation; `froe` is an independent implementation, not an Apache
project.
