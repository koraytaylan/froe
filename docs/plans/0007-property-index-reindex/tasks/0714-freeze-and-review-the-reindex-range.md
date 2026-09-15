---
id: freeze-and-review-the-reindex-range
title: Freeze And Adversarially Review The Reindex Range
workstream: "0007"
kind: task
depends_on: [wire-the-property-reindex-phase-into-the-suite, record-guard-neutralization-evidence, warn-about-pending-reindexes-in-compact]
gated: true
touches:
  - docs/plans/0007-property-index-reindex/ARCHITECTURE.md
  - docs/index.md
  - docs/oak-segment-tar-feature-map.md
status: done
merged_as: ""
---
# Freeze And Adversarially Review The Reindex Range

The high-risk gate: a committed, frozen `BASE..HEAD` range reviewed by passes that did not author it, with the verification report bound to exact commands and their own exit statuses, and the known gaps stated rather than waved at. Gated because the maintainer decides when the range is complete and which findings warrant follow-up commits.

**Steps:**

1. Commit the complete candidate, record base and head, confirm no untracked candidate files, run `git diff --check BASE..HEAD` in a clean worktree.
2. Run the stable and MSRV host gates, `scripts/oversized-files.sh`, the i686 width sentinel for `froe`, and `scripts/interop-fixture.sh`; record each command and its status in the safety case's verification report with the guide's five separations and, per claim, its platform, toolchain, test layer, fault model and asserted property (so each fault row records the cutpoint, the model — returned error or `_exit` — and the prefix it proves) — execution from cross-compilation, synthetic credentials from execution as root, process-exit or syscall injection from true power-loss ordering, file existence from durability, and froe-to-froe round trips from real Oak interoperability — and record the `property_reindex` run in the interoperability section: the exact Oak build, the direction, the operation, the froe-side edits made to the copy before the operation under test, the canonical-index check and the attempt on which it passed, and the verified post-state.
3. Run at least four adversarial lenses over the frozen range in clean worktrees: the state-root decision (can any path index an async definition from the wrong state?), reclaimability and identity preservation (can a rebuild drop a property or a visible child of a definition?), interruption prefixes against the code, and evidence wording. Verify every finding independently before it counts; record the pass as an automated pass, not a second person.
4. Address findings in follow-up commits, with each delta and the cumulative range re-verified; fill "Known gaps" (naming the fsync-capability gate, which has no synthetic regression, the moved-head compare-and-set, which has none either because the run holds `repo.lock` exclusively from `prepare` through `flush` so no second writer can move the head inside the window, every refusal the scope lists, the `--from-head` reset and every recorded departure: 0703's omitted `:count_*` properties and 0705's 32-bit seed draw; the standing environment axes 1010 also lists (no AEM build, no external blob store, no local macOS execution with CI's `macos-latest` job as the authority, no native Windows execution)) and "Review"; lift the beta framing tasks 0707 and 0710 wrote into the feature map's two rows and `docs/index.md`'s reindex section, replacing it with a pointer to the frozen evidence.

- **Done when:** the safety case carries the frozen range, the verification report with exit statuses and the five separations, the interoperability record, the review record with lenses and verdicts, and a known-gaps list, and every finding is either fixed in a follow-up commit or recorded as a gap with its reachability stated.
