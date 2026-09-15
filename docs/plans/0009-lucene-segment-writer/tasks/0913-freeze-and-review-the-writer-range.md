---
id: freeze-and-review-the-writer-range
title: Freeze And Adversarially Review The Writer Range
workstream: "0009"
kind: task
depends_on: [wire-the-writer-conformance-phase-into-the-suite]
gated: true
touches:
  - docs/plans/0009-lucene-segment-writer/ARCHITECTURE.md
  - docs/oak-segment-tar-feature-map.md
status: done
merged_as: ""
---
# Freeze And Adversarially Review The Writer Range

The writer publishes no repository bytes on its own — nothing in this plan touches a store — so the high-risk safety case is owed by plan 0010, where the writer's output is installed. What this range owes is the review the contributing guide asks of every format parser and writer: cited evidence, independent fixtures, a round trip through a reader that does not share the writer's assumptions (Lucene's own, through the judge), and a frozen review of the range, recorded in this plan's `ARCHITECTURE.md` where every landed plan keeps its verification report and review.

**Steps:**

1. Commit the complete candidate, record base and head, confirm no untracked candidate files, run `git diff --check BASE..HEAD` in a clean worktree; the stable and MSRV gates; `scripts/oversized-files.sh`; the i686 width sentinel (the codec does 32-bit arithmetic in packed integers and file pointers); `scripts/interop-fixture.sh`; each command and status recorded with the guide's five separations and, per claim, its platform, toolchain, test layer, fault model and asserted property (the fault model recorded as not applicable, since this range arms no cutpoint and publishes no repository bytes) — execution from cross-compilation, synthetic credentials from execution as root, injection from true power loss, file existence from durability, froe-to-froe from real Oak — and the `lucene_writer_conformance` run recorded with the exact Oak build, the direction (froe-to-Oak: Lucene inside the image reads what froe wrote), the operation exercised and the verified post-state.
2. Adversarial lenses: encoding boundaries (every place a size is a `vint`, a block is 128, a term is 32,766 bytes); the equivalence oracle's blind spots (what `enumerate` does not print); the memory bound; evidence wording. Findings verified independently; recorded as an automated pass.
3. Follow-up commits, with each delta and the cumulative range re-verified; record the verification report, a `### Known gaps` list naming the standing environment axes 1010 also lists (no AEM build, no external blob store, no local macOS execution with CI's `macos-latest` job as the authority, no native Windows execution), and the review as `### Verification report`, `### Known gaps` and `### Review` sections of this plan's `ARCHITECTURE.md`; the coordinator summarizes them in the plan's status; lift the beta framing task 0910 wrote into the feature map's writer row, replacing it with a pointer to the frozen evidence.

- **Done when:** the range is frozen with its verification report and review recorded in `ARCHITECTURE.md`, and every finding is fixed in a follow-up commit or recorded as a known gap.
