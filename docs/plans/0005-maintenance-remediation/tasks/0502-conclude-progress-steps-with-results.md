---
id: conclude-progress-steps-with-results
title: Conclude Progress Steps With Results
workstream: "0005"
kind: task
depends_on: [skip-the-second-walk-of-map-roots]
gated: false
touches:
  - crates/froe-cli/src/progress/live.rs
  - crates/froe-cli/src/progress/mod.rs
  - crates/froe-cli/src/progress/render.rs
  - crates/froe/src/lib.rs
  - crates/froe/src/progress.rs
  - crates/froe/src/units.rs
status: done
merged_as: "d2f92e94fb9421c7ea74a9a341174245b35859ed"
---
# Conclude Progress Steps With Results

Phase 1 of the plan: a progress step can conclude with what it established, so `predicting the shared binary content`, `tracing segments reachable from the head` and `predicting the reclamation` print their results instead of ending silently. Landed in `d2f92e9` (2026-08-19).

**Steps:**

1. Add a conclusion to the progress step protocol and render it in both the live and the plain reporters.
2. Give the units module the formatting the result lines need.

- **Done when:** a step with a conclusion renders it once, in both progress modes, and a step without one renders as before. Met at `d2f92e9`.
