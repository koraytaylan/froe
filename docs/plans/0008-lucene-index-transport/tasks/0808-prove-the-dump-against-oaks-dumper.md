---
id: prove-the-dump-against-oaks-dumper
title: Prove The Dump Against Oak's Own Dumper
workstream: "0008"
kind: task
depends_on: [dump-lucene-indexes-to-the-filesystem]
gated: false
touches:
  - crates/froe-cli/tests/interop/phase_lucene_transport.rs
  - crates/froe-cli/tests/interop/main.rs
  - crates/froe-cli/tests/interop/judge/Consistency.java
  - crates/froe-cli/tests/interop/phase_judge.rs
  - crates/froe-cli/tests/interop/phase_recovery.rs
  - crates/froe/src/index/lucene/segments.rs
  - crates/froe/tests/lucene_segments_tests.rs
  - docs/analysis/index-lucene-storage.md
  - docs/interop.md
status: done
merged_as: "b80db37"
---
# Prove The Dump Against Oak's Own Dumper

The `lucene_dump` phase asks the strongest question a read-only transport can be asked: are the bytes froe reads out of `:data` the bytes Oak reads out of `:data`? The judge's `dump` subcommand (task 0614, Oak's own dumper reading `:data` out of the same store) is the oracle, and the judge gains a consistency checker beside it. The phase lives in a new `phase_lucene_transport.rs`, registered in `main.rs`, which task 0809's phase shares; one file per plan's phases keeps each under the thousand-line gate.

**Steps:**

1. Add `judge/Consistency.java` with `consistency <store> <indexPath> <level> <workDirectory>` running `oak-lucene`'s own index consistency checker — whose constructor requires the work-directory root, and whose two levels are the blobs-only pass and the full one, the `1`/`2` mapping being oak-run's printer's — and printing the checker's own result dump; the container mounts a writable work directory large enough for the full level's local copy of the index.
2. The phase: `froe index dump` over the same fixture and one judge dump per `indexPath` it selected — the loop is explicit because plan 0010 later adds a second Lucene definition to the fixture, so the comparison is per definition directory, not one against one; the full level's index-check status is asserted to have been reached and clean, not merely that the blob pass printed a verdict (the checker runs Lucene's own index checker only when the blob pass came out clean); every file under each definition's data directory must be byte-identical (name set equal, contents equal); `index-details.txt` must carry the same `indexPath` and directory mappings; `checkindex` must be clean on froe's output; `numdocs` on froe's output must equal the document count `froe index check` computes from `segments_N` and the per-segment `.si` for the same index, as Oak's own document count composes it (`:status/indexedNodes` is a per-cycle counter and is not compared).
3. Assert the store snapshot is byte-identical after the phase (the dump is read-only).

- **Done when:** the phase passes against the pinned image with every file byte-identical, and corrupting one byte of froe's dump before the comparison makes the phase name that file, and the stable host gate passes (`--all-features`, so the interop suite is compiled and linted).
