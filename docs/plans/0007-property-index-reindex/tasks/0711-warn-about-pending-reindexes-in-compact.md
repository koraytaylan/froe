---
id: warn-about-pending-reindexes-in-compact
title: Warn About Pending Reindexes In The Compaction Plan
workstream: "0007"
kind: task
depends_on: [add-the-reindex-command]
gated: false
touches:
  - crates/froe/src/writer/maintenance/planning/mod.rs
  - crates/froe/src/writer/maintenance/plan.rs
  - crates/froe-cli/tests/command_line_tests/compaction.rs
  - docs/compact.md
status: done
merged_as: "10ebb86b1c0ef1264f2337d34b2ce14824c08d8f"
---
# Warn About Pending Reindexes In The Compaction Plan

An operator running `froe compact` before restarting AEM has, at that moment, the one fact that predicts a multi-hour startup: whether any definition under `/oak:index` carries `reindex=true`. The planner already walks the head; reading the definitions is cheap. Add an advisory warning to the compaction plan — never an action — naming each flagged definition and its type, and pointing at `froe index reindex` for the supported types. This changes no byte the run writes and no decision it makes. The plan's warnings are plain strings today (`warnings: Vec<String>` behind `warnings() -> &[String]` in `maintenance/plan.rs`, rendered by `compaction_report.rs` as `froe: warning: …` on standard error), and this task keeps that shape rather than introducing a typed variant that would change the public accessor.

**Steps:**

1. In `planning/mod.rs`, collect flagged definitions through `froe::index` and push one formatted warning per definition (`planning/mod.rs` is 827 lines and gains the scan and the warnings; `plan.rs`, at 959 lines against the thousand-line gate, gains only the documentation of the new warning on `warnings()`; no in-module tests are added, the tests being command-line tests) — the line reads `pending reindex: /oak:index/<name> (<type>; froe index reindex rebuilds it offline)` or, for an unsupported type, the same line naming the type as unsupported.
2. Render nothing new: the existing warning loop already prints on standard error, so the plan's standard-output contract stays byte-identical whether or not a definition is flagged.
3. Tests: a store with a flagged definition prints the warning; a store without prints nothing new; the observed plan equals the unobserved plan.
4. `docs/compact.md`: one paragraph after the dry-run paragraph that enumerates what a plan prints, stating that a plan naming a definition with `reindex=true` warns about it and points at `froe index reindex`, and that the warning is advisory — no action, no byte changed; and while in the file, repoint its stale `#opt-in-behavior` link at the `#the-questions-and-their-skip-flags` heading that replaced that section.

- **Done when:** the warning appears for a flagged definition and nowhere else, every existing compaction test still passes byte-for-byte, and the stable host gate passes.
