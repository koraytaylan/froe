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

## Code standards

* **No abbreviations** in file, method, or variable names:
  `most_significant_bits`, not `msb`; `identifier`, not `id`;
  `record_number`, not `rec_no`. Universally standard acronyms (UUID,
  CRC32, JCR, TAR, JSON, CLI) are acceptable; ad-hoc shortenings are
  not. The cargo-mandated `src/` directory is the tolerated exception.
* **Minimal dependencies.** Everything in the tree must be strictly
  necessary and pull its weight. Current policy: `memmap2` in the core
  crate; `clap` and `libc` (SIGPIPE handling) in the CLI. CRC32, Java
  string hashing, JSON encoding, and timestamp formatting are
  hand-implemented — a new dependency needs a reason those did not.
* **Documented, concisely.** Every module starts with a doc comment
  explaining what it models and the non-obvious facts of the format it
  implements. Public items carry doc comments. Inline comments state
  constraints the code cannot express — never what the next line does.
* **Idiomatic Rust, not transliterated Java.** The format semantics are
  exact; the implementation is not a line-by-line translation. Errors
  are `Result`s, not exceptions; ownership replaces defensive copying;
  iterators replace index loops where clearer.
* Rust edition 2024, workspace lints as configured in the root
  `Cargo.toml`: `missing_docs`, `unreachable_pub`, and clippy's
  `pedantic` group must all be clean.

## Testing standards

Tests are the port's proof of fidelity. Every change keeps all of these
green, and new functionality arrives with all three layers:

1. **Unit tests with hand-crafted bytes.** Every parser and serializer
   is exercised against fixtures written out by hand from the
   specifications — including corrupt, truncated, hostile, and
   boundary-value inputs.
2. **Independent-encoder round-trips.** Integration tests build
   repositories through `crates/froe/tests/support/` — a separate
   encoder sharing no production code with the reader or writer — so a
   self-consistent bug in one cannot hide in the other. Writer changes
   additionally round-trip through the *reader*: everything written must
   be read back identically.
3. **End-to-end store tests.** Mutating operations run against complete
   synthetic repositories on disk and assert the full post-state:
   archives reopen with valid indexes, the journal resolves, content is
   intact, and read-only invariants (no lock, no writes) hold where
   promised.

Run the full gate before any commit:

```console
$ cargo fmt --all -- --check
$ cargo test --workspace
$ cargo test --workspace --release   # overflow behavior differs by profile
$ cargo clippy --workspace --all-targets   # zero warnings
$ cargo doc --workspace --no-deps          # zero warnings
```

## Workflow

* Development happens on `develop`; `main` holds released states.
* Commits follow conventional commit prefixes (`feat:`, `fix:`,
  `docs:`, `chore:`) with bodies that explain *why*, in full sentences.
* A change to format-facing code cites the specification section (or the
  Java file and method) that justifies it, in the commit body or the
  code.

## License

Apache-2.0. By contributing you agree to license your work under the
same terms. Apache Jackrabbit Oak is a trademark of the Apache Software
Foundation; `froe` is an independent implementation, not an Apache
project.
