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

* **No abbreviations** in file, method, or variable names:
  `most_significant_bits`, not `msb`; `identifier`, not `id`;
  `record_number`, not `rec_no`. Universally standard acronyms (UUID,
  CRC32, JCR, TAR, JSON, CLI) are acceptable; ad-hoc shortenings are
  not. The cargo-mandated `src/` directory is the tolerated exception.
* **Minimal dependencies.** Everything in the tree must be strictly
  necessary and pull its weight. The crate manifests are the
  authoritative inventory; large format backends stay feature-gated.
  CRC32, Java string hashing, JSON encoding, and timestamp formatting
  are hand-implemented — a new dependency needs a reason those did not.
* **Documented, concisely.** Every module starts with a doc comment
  explaining what it models and the non-obvious facts of the format it
  implements. Public items carry doc comments. Inline comments state
  constraints the code cannot express — never what the next line does.
  Safety comments describe the exact mechanism and scope proved by the
  code, not a stronger idealized state.
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
  `main` stopped tracking released states after `v0.1.0` and nothing in
  the release workflow reads it; treat it as historical.
* Commits follow the conventional commit prefixes
  [`cliff.toml`](cliff.toml) groups into release notes — `feat:`, `fix:`,
  `harden:`, `perf:`, `refactor:`, `docs:`, `test:`, `chore:` — with
  bodies that explain *why*, in full sentences.
* A change to format-facing code cites the specification section (or the
  Java file and method) that justifies it, in the commit body or the
  code.

### Review discipline

High-risk final review uses a committed, frozen `BASE..HEAD` range and an
independent reviewer who did not implement the patch. Later edits
invalidate the affected review and gates. The authoritative review
procedure, evidence requirements, fallback when an independent reviewer
is unavailable, and follow-up-commit policy are in
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
