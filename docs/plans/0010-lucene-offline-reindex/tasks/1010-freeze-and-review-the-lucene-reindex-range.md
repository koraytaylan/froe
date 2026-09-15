---
id: freeze-and-review-the-lucene-reindex-range
title: Freeze And Adversarially Review The Lucene Reindex Range
workstream: "0010"
kind: task
depends_on: [prove-the-lucene-reindex-against-oak]
gated: true
touches:
  - docs/plans/0010-lucene-offline-reindex/ARCHITECTURE.md
  - docs/index.md
  - docs/oak-segment-tar-feature-map.md
status: done
merged_as: ""
---
# Freeze And Adversarially Review The Lucene Reindex Range

The high-risk gate: a committed, frozen `BASE..HEAD` range reviewed by passes that did not author it, with the verification report bound to exact commands and their own exit statuses, and the known gaps stated rather than waved at. Gated because the maintainer decides when the range is complete and which findings warrant follow-up commits.

**Steps:**

1. Commit the complete candidate, record base and head, confirm no untracked candidate files, run `git diff --check BASE..HEAD` in a clean worktree; the stable and MSRV gates; `scripts/oversized-files.sh`; the i686 width sentinel; `scripts/interop-fixture.sh` with every phase; each command and status recorded in this plan's safety case with the guide's five separations and, per claim, its platform, toolchain, test layer, fault model and asserted property (so each fault row records the cutpoint, the model — returned error or `_exit` — and the prefix it proves) — execution from cross-compilation, synthetic credentials from execution as root, injection from true power loss, file existence from durability, froe-to-froe from real Oak — and the `lucene_reindex` run recorded with the exact Oak build, direction, operation, the froe-side edits made to the copy before the operation under test, the canonical-index check and the attempt on which it passed, and verified post-state.
2. Adversarial lenses: analyzer fidelity beyond the vector corpus (which Unicode classes the corpus does not reach); rule resolution (inheritance, mixins, relative names, regular-expression edge cases); the lane rule and the `:status` timestamps; the binary policy's honesty in plan output and summary; evidence wording. Findings verified independently; recorded as an automated pass.
3. Follow-up commits, with each delta and the cumulative range re-verified; the beta framing tasks 1007 and 1008 wrote into the feature map's rows and `docs/index.md`'s Lucene section lifted and replaced with a pointer to the frozen evidence; the safety case's known gaps — including the fsync-capability gate, which has no synthetic regression, and the standing gaps every plan shares: no AEM build, no external blob store, no local macOS execution (CI's `macos-latest` job is the authority) and no native Windows execution, every refusal the scope lists (a codec verdict other than `oakCodec`, `valueRegex`, a definition without `async`, hybrid definitions, `analyzers` children, `similarityTags`, `useInSimilarity`, `dynamicBoost`, `function`, `compatVersion 1`, an `nt:base` rule with `nullCheckEnabled`, `maxFieldLength = 0`, and the document-time refusal of an unparseable DATE value), every recorded departure (the unobservable `oak.lucene.compressing-codec` system property, the `--from-head` reset, no text extraction, the accepted `tika` child — a plan line, nothing read from it — the refusal at load of conflicting `:dv` types, and `:suggest-data` removed and left for Oak's next suggester cycle rather than constructed), and the consumer-registered field providers Oak's document maker augments a document with, which froe cannot reproduce.

- **Done when:** this plan's safety case carries the frozen range, the verification report, the interoperability record, the review record and the known gaps, plans 0007's, 0008's and 0009's frozen sections are untouched, and every finding is fixed or recorded with its reachability.
