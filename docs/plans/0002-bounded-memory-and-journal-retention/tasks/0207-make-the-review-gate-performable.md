---
id: make-the-review-gate-performable
title: Make The Review Gate Performable
workstream: "0002"
kind: task
depends_on: [prove-journal-retention-against-oak]
gated: false
touches:
  - CONTRIBUTING.md
  - docs/high-risk-changes.md
  - docs/releasing.md
  - Cargo.toml
  - Cargo.lock
  - crates/froe-cli/Cargo.toml
  - crates/froe-export/Cargo.toml
status: done
merged_as: "208ea06ee27f4722f15784e228b587bdfc86978d"
---
# Make The Review Gate Performable

`v0.8.0` was tagged (`4ff48c8`), and the high-risk guide's review step, which asked for a second person the project does not have, was rewritten as a gate the project can actually perform: an adversarial pass by an assistant that did not author the range, recorded with its stated weakness. Landed in `208ea06` (2026-08-14).

**Steps:**

1. Bump the workspace to `0.8.0` with the retention flag in the changelog rationale.
2. Rewrite the review section of `docs/high-risk-changes.md` and the matching lines of `CONTRIBUTING.md` and `docs/releasing.md`.

- **Done when:** the guide names a review procedure every later safety case in this directory follows. Met at `208ea06`.
