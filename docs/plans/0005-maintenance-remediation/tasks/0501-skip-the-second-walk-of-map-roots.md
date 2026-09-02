---
id: skip-the-second-walk-of-map-roots
title: Skip The Second Walk Of Map Roots
workstream: "0005"
kind: task
depends_on: []
gated: false
touches:
  - crates/froe/src/content/map.rs
  - crates/froe/src/content/node.rs
  - crates/froe/src/content/provider.rs
  - crates/froe/src/content/traversal.rs
  - crates/froe/tests/diagnostics_tests/budgets.rs
status: done
merged_as: "206ac95e0520f296c3d00cec215e4996a8b28036"
---
# Skip The Second Walk Of Map Roots

Every node visit walked its map root and its property list twice, once to count and once to read. On a 41-minute field run the verification walks were about a third of the time, so this was the first cut. Landed in `206ac95` (2026-08-19).

**Steps:**

1. Read map roots and property lists once per visit and carry the counts forward.
2. Keep the diagnostics budgets test honest about the number of reads a visit costs.

- **Done when:** the budgets test pins one read per map root per visit and the content tests are unchanged. Met at `206ac95`.
