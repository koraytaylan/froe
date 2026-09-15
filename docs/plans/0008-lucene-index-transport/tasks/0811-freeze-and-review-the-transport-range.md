---
id: freeze-and-review-the-transport-range
title: Freeze And Adversarially Review The Transport Range
workstream: "0008"
kind: task
depends_on: [wire-the-lucene-transport-phases-into-the-suite, arm-import-fault-cutpoints]
gated: true
touches:
  - docs/plans/0008-lucene-index-transport/ARCHITECTURE.md
  - docs/index.md
  - docs/oak-segment-tar-feature-map.md
status: done
merged_as: ""
---
# Freeze And Adversarially Review The Transport Range

The high-risk gate: a committed, frozen `BASE..HEAD` range reviewed by passes that did not author it, with the verification report bound to exact commands and their own exit statuses, and the known gaps stated rather than waved at. Gated because the maintainer decides when the range is complete and which findings warrant follow-up commits.

**Steps:**

1. Commit the complete candidate, record base and head, confirm no untracked candidate files, run `git diff --check BASE..HEAD` in a clean worktree.
2. The stable and MSRV gates, `scripts/oversized-files.sh`, the i686 width sentinel, `scripts/interop-fixture.sh`; each command and status in the verification report with the guide's five separations and, per claim, its platform, toolchain, test layer, fault model and asserted property (so each fault row records the cutpoint, the model — returned error or `_exit` — and the prefix it proves) — execution from cross-compilation, synthetic credentials from execution as root, injection from true power loss, file existence from durability, froe-to-froe from real Oak — and the `lucene_dump` and `lucene_import` runs recorded in the interoperability section with the exact Oak build, direction, operation, the froe-side edits made to the copies before the operation under test, and verified post-state.
3. Adversarial lenses over the frozen range: the state rule (can any input attach an index to a state it does not reflect?), the file round trip (can a length, key or listing disagree with what Oak reads?), interruption prefixes against the code, and evidence wording; findings verified independently; the pass recorded as automated.
4. Follow-up commits, with each delta and the cumulative range re-verified; known gaps (naming the fsync-capability gate, which has no synthetic regression, the moved-head compare-and-set, which has none either because the import holds `repo.lock` exclusively from `prepare` through `flush` so no second writer can move the head inside the window, every refusal the scope lists — synchronous and hybrid definitions, definition drift, mixed lanes — and every recorded departure: `:suggest-data` skipped, no checkpoint released, the single-blob encoding written, `dirListing` in name order, the file's `refresh` not copied; the standing environment axes 1010 also lists (no AEM build, no external blob store, no local macOS execution with CI's `macos-latest` job as the authority, no native Windows execution)) and review sections filled; the beta framing tasks 0803, 0805 and 0807 wrote into `docs/index.md` and the feature map's rows lifted and replaced with a pointer to the frozen evidence.

- **Done when:** the safety case carries the frozen range, the verification report, the interoperability record, the review record and the known gaps, and every finding is fixed or recorded with its reachability.
