---
id: survive-pluralization-and-a-failed-sling-post
title: Survive Pluralization And A Failed Sling Post
workstream: "0005"
kind: task
depends_on: [make-the-extra-cleanups-default-on-questions]
gated: false
touches:
  - .github/workflows/interop.yml
  - crates/froe-cli/tests/interop/fixtures.rs
  - crates/froe-cli/tests/interop/phase_maintenance.rs
  - crates/froe-cli/tests/interop/sling.rs
  - crates/froe-cli/tests/interop/store.rs
  - docs/interop.md
status: done
merged_as: "1a88a72b019e5370514026c4412bbf09edcb5287"
---
# Survive Pluralization And A Failed Sling Post

The suite matched `checkpoints` literally and broke on a one-checkpoint fixture, and a failed Sling POST during fixture generation was reported as a later, unrelated assertion. Both were fixed and the workflow's schedule adjusted. Landed in `1a88a72` (2026-09-01).

**Steps:**

1. Match the singular and plural forms of the plan's checkpoint line.
2. Fail fixture generation at the POST that failed, with its status and body.

- **Done when:** the suite passes on a one-checkpoint fixture and a refused POST fails the `generate` phase with the response quoted. Met at `1a88a72`.
