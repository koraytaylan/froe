# Oak interop test suite

End-to-end tests verifying that froe reads stores written by Apache
Jackrabbit Oak, writes stores that Oak reads, and performs maintenance
operations that leave the store in a state Oak boots against cleanly.

The suite uses an Apache Sling image (Apache-2.0) as a real Oak instance —
no Adobe/AEM license is involved. Sling boots Oak with TarMK by default,
so the store is byte-for-byte what a production Oak repository produces.

The image is **pinned by manifest digest** in
`crates/froe-cli/tests/interop.rs`, because the claim in the README names an
Oak build and a mutable tag could be re-pushed with a different one. The
suite also asserts the `oak-segment-tar` version inside the image, so a
substitution fails loudly rather than silently redefining what was verified.

Setting `FROE_INTEROP_CANARY=1` runs against the floating `:14` tag instead.
The two modes answer different questions: the pinned run asks whether froe
still interoperates with the build the claim names, and the floating run asks
whether the ecosystem has moved underneath it. On a canary run, the Oak
version assertion failing is the useful result.

## Prerequisites

- **podman** installed and runnable by the current user.
- **Network access** to pull the pinned Sling image (once).
- **froe** built: `cargo build --release`.

## Running

```console
# All phases in dependency order:
$ scripts/interop-fixture.sh

# A single phase (generate runs first automatically):
$ scripts/interop-fixture.sh compact

# Direct cargo invocation:
$ cargo test -p froe-cli --features interop -- --ignored --test-threads=1

# A single phase via cargo:
$ cargo test -p froe-cli --features interop -- --ignored interop::read
```

Tests are `#[ignore]`d by default so they don't run in the normal
`cargo test` gate. The `interop` feature flag gates compilation; without
it the test file is empty.

## Dependency chain

The phases run in a strict dependency chain. Each phase depends on the
previous one and aborts the chain on failure. There is no point testing
a later phase if an earlier one is broken — the later phases use the
earlier phases' output as input.

```
generate
   │  Sling writes the Oak store fixture
   ▼
read
   │  froe reads the Oak store
   │  If this fails: froe cannot read Oak's format. No write-path
   │  verification is meaningful without a working reader.
   ▼
commit
   │  froe adds nodes with typed properties to the content tree via
   │  the library's commit API, then Sling reads them back
   │  If this fails: froe cannot write content that Oak reads — the
   │  core interop claim. No point testing checkpoint, compact, cleanup,
   │  backup, or recover if the writer can't produce content Oak reads.
   ▼
checkpoint
   │  froe writes a checkpoint (metadata-only write-path test)
   │  If this fails: the writer's checkpoint machinery is broken,
   │  which affects cleanup's expired-checkpoint test and compact's
   │  checkpoint preservation.
   ▼
compact
   │  froe compacts a copy, Sling boots against the result
   │  If this fails: cleanup's multi-generational fixture cannot be
   │  built (it uses two compactions to advance the generation).
   ▼
cleanup
   │  froe cleanup against a gen 0→1→2 store with orphans, stale
   │  archives, expired checkpoints, and corrupt journal lines
   │  If this fails: the write path's plan-and-apply machinery is broken.
   ▼
backup
   │  froe backup + restore, Sling boots against the result
   │  Independent of compact/cleanup but later because lower-risk.
   ▼
recover
   │  froe recover-journal after deleting journal.log
   │  Last because it is the most destructive (deletes the journal).
```

## What each phase proves

### generate

Boots Sling with TarMK, populates content under `/content/interop`
(folders, ordered folders, multi-value properties, an inline binary),
churns content (create + delete 20 subtrees × 5 children × 3 rounds) to
produce orphaned segments, and stops cleanly. The resulting store is the
shared fixture for all later phases.

### read

froe reads the Oak-written store: `summary`, `tree`, `check`,
`search-nodes`, and `export` (json-lines). All must succeed. This is the
foundation — every later phase uses froe's reader to verify results.

### commit

froe adds nodes with typed properties (String, Long, Boolean) to the
content tree via the library's commit API
(`rewrite_node_with_child_edits`), writing a new subtree under
`/content/interop/froe-written/node`. Then Sling boots against the
modified store and verifies Oak reads the froe-written nodes back
correctly — the same JSON Sling would serve for any other node.

This is the core interop claim: froe writes content that Oak reads.
If this fails, there is no point testing checkpoint, compact, cleanup,
backup, or recover — the writer cannot produce content Oak reads.

Depends on `read` (to verify).

### checkpoint

froe creates a checkpoint with a 1-second lifetime against the Oak
store. A metadata-only write-path operation (logical head update) that
exercises the writer's checkpoint machinery against a store froe didn't
write. If this fails, cleanup's expired-checkpoint test and compact's
checkpoint preservation can't be trusted.

Depends on `commit` (the writer can already produce content Oak reads).

### compact

froe compacts a copy of the store. The journal is truncated to the head
first, so the churned subtrees' segments are true orphans (no journal
history protects them). Compaction deep-copies only reachable records,
dropping the orphans. Sling boots against the compacted store and the
binary round-trips byte-for-byte.

Depends on `read` (to verify the compacted store) and `commit` (to trust
the writer).

### cleanup

froe cleanup against a multi-generational store with:

- **727 orphan segments** (67 MB): built by compacting twice (gen 0→1→2),
  then restoring the original gen-0 archive at a higher archive number.
  Those gen-0 segments are 2 full generations behind the head, not
  protected by journal history (truncated), and not referenced by
  surviving gen-2 segments. Compact's built-in cleanup already ran and
  can't see them; only standalone cleanup finds and reclaims them. This
  mirrors a real scenario: a crashed online compaction leaving a stale
  archive behind, or an old backup archive restored to the directory.
- **1 stale archive**: a copied newer-letter duplicate of the active
  archive — the on-disk condition Oak's own compaction leaves behind.
- **1 expired checkpoint**: created by froe with a 1-second lifetime.
- **2 corrupt journal lines**: a no-space line (ParserSkippedNoSpace)
  and an invalid-record-identifier line (InvalidRecordIdentifier).

froe cleanup removes all four conditions. Sling boots against the
cleaned store.

Depends on `compact` (to build the gen 0→1→2 fixture).

### backup

froe backup copies the store head into a fresh target directory. froe
restore copies that backup into another store. Sling boots against the
restored store and content is preserved.

Depends on `read` and `commit`.

### recover

Deletes `journal.log`, then runs `froe recover-journal` to rebuild it
from the segments. The recovered journal resolves, `froe check` passes,
and Sling boots against the recovered store.

Depends on `read`.

## How the orphan-segment gap was closed

A fresh Oak store has only generation 0. froe's `segments` cleanup task
uses Oak's FULL-generation predicate, which reclaims segments whose
`full_generation` is 2+ behind the head. A single-generation store has
nothing old enough to reclaim.

The natural way to advance generations is `froe compact`, but compact's
built-in cleanup (with `retained_generations=1`) reclaims old-gen
segments during compaction itself. So after compacting, old archives are
already gone.

The solution: compact twice (gen 0→1→2), **then** restore the original
gen-0 archive to the store directory. Those gen-0 segments are 2 full
generations behind the gen-2 head, not protected by journal history
(truncated), and not referenced by surviving segments. Compact's
cleanup already ran and can't see them. Only standalone cleanup finds
and reclaims them — 727 orphan segments, 67 MB.

This mirrors a real production scenario: Oak runs online compaction
(gen 0→1→2), but an old backup archive from gen 0 is restored to the
directory, or a crashed online compaction left a stale archive behind.

## CI

`.github/workflows/interop.yml` runs the suite on three occasions, because
they answer different questions:

- **Push, path-filtered** on the write path, the suite, and the workflow —
  the froe-side axis, where a regression is possible. Pinned digest.
- **Monthly schedule** against the floating tag — the environment axis, which
  can break with no froe commit at all: a new Oak build in the image, a new
  runner image, a new stable compiler. This is what a timer is actually for;
  a weekly cadence added nothing the push filter did not already cover.
- **Manual dispatch**, for re-verifying deliberately.

A failing run opens or comments on an `interop`-labelled issue, because a
scheduled failure produces no pull request and would otherwise sit unnoticed.

`.github/workflows/release.yml` runs the suite as a release gate: the release
notes assert that maintenance is verified against a named Oak build, so the
publishing job depends on the suite passing at the tagged commit rather than
on a run from some earlier day.

## Implementation

The tests live in `crates/froe-cli/tests/interop.rs`, behind the
`interop` feature flag. The shell script
`scripts/interop-fixture.sh` is a thin wrapper around `cargo test`.

Podman orchestration uses `std::process::Command` to shell out to
`podman run`, `podman volume`, and `podman stop/rm` — the same commands
the previous shell script used, but from Rust with structured
assertions. The Sling image is `docker.io/apache/sling:14` (Apache-2.0);
it boots Oak with TarMK by default.

The froe binary is resolved via `env!("CARGO_BIN_EXE_froe")`, so the
tests always run against the freshly built binary, not whatever is on
`$PATH`.