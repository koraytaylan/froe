---
id: state-the-as-built-contracts
title: State The As-Built Contracts
workstream: "0005"
kind: task
depends_on: [prove-the-gate-and-the-purge-against-oak]
gated: false
touches:
  - README.md
  - docs/cli-output.md
  - docs/compact.md
  - docs/interop.md
  - docs/oak-segment-tar-feature-map.md
  - docs/plans/0002-bounded-memory-and-journal-retention/ARCHITECTURE.md
  - docs/plans/0005-maintenance-remediation/ARCHITECTURE.md
  - Cargo.toml
  - Cargo.lock
  - crates/froe-cli/Cargo.toml
  - crates/froe-export/Cargo.toml
status: done
merged_as: "9f03c7196c6141ca36a15c80c6f85d4a72e82c78"
---
# State The As-Built Contracts

The plan document was committed with every as-built refinement recorded against the reviewed draft (the pre-swap walk, the content-first census order, the `up to` bulk figure, the per-scope memo, the silence on a zero-orphan store), `docs/compact.md`'s digest sentence and its authoritative-state section were rewritten, the bounded-memory case gained the three new store-proportional structures, and the workspace was bumped to `0.11.0`. Landed in `fa6486e` and `9f03c71` (2026-08-19).

**Steps:**

1. Rewrite the contract sentences the plan named, in `docs/compact.md`, `docs/cli-output.md`, `docs/interop.md` and the feature map.
2. Record the resource bounds of the census, the purge sets and the split memo in plan 0002's safety case.
3. Bump to `0.11.0` with the breaking `PreparedCompaction` flow in the rationale.

- **Done when:** every sentence the plan said must change has changed, and the plan states each as-built deviation beside the reviewed design. Met at `9f03c71`.
