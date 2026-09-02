---
id: make-the-extra-cleanups-default-on-questions
title: Make The Extra Cleanups Default-On Questions
workstream: "0005"
kind: task
depends_on: [split-the-files-the-line-gate-refuses]
gated: false
touches:
  - README.md
  - crates/froe-cli/src/command_line.rs
  - crates/froe-cli/src/compaction.rs
  - crates/froe-cli/src/compaction_report.rs
  - crates/froe-cli/src/compaction_summary.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/mutation.rs
  - crates/froe-cli/src/output.rs
  - crates/froe-cli/tests/command_line_tests/compaction.rs
  - crates/froe-cli/tests/command_line_tests/compaction_decisions.rs
  - crates/froe-cli/tests/command_line_tests/main.rs
  - crates/froe-cli/tests/command_line_tests/reporting.rs
  - crates/froe-cli/tests/command_line_tests/support.rs
  - crates/froe-cli/tests/interop/phase_maintenance.rs
  - crates/froe-cli/tests/interop/phase_recovery.rs
  - crates/froe/src/lib.rs
  - crates/froe/src/writer/maintenance/indexless_refusal.rs
  - crates/froe/src/writer/maintenance/mod.rs
  - crates/froe/src/writer/maintenance/planning/mod.rs
  - crates/froe/src/writer/maintenance/planning/version_storage.rs
  - crates/froe/src/writer/maintenance/prepared.rs
  - crates/froe/src/writer/maintenance/reclamation.rs
  - crates/froe/src/writer/maintenance/stale_archives.rs
  - crates/froe/src/writer/maintenance/surveys.rs
  - crates/froe/src/writer/mod.rs
  - crates/froe/src/writer/store_writer/repair.rs
  - docs/cli-output.md
  - docs/compact.md
  - docs/interop.md
  - docs/oak-segment-tar-feature-map.md
  - docs/plans/0001-repair-archives/ARCHITECTURE.md
status: done
merged_as: "af418bd22432fe8c89505857b427a06eafd314db"
---
# Make The Extra Cleanups Default-On Questions

Decision point 1 of the plan chose an opt-in purge flag. Two days later the rule was inverted for every extra cleanup: the purge, the archive-index repair and recovery-backup removal are default-on questions the run asks, `--yes` answers all of them, and each has a `--skip-*` opt-out; the old flags survive as hidden pre-authorizing spellings. The repair safety case's authorization surface was restated. Landed in `af418bd` (2026-08-21).

**Steps:**

1. Model each extra cleanup as a decision with a question, an answer under `--yes` and a `--skip-*` decline, in `compaction_decisions.rs` tests.
2. Rewrite the plan report and summary so a declined question is stated rather than silent.
3. Update the interop phases, the docs and the repair safety case's header note.

- **Done when:** `froe compact --yes` performs every extra cleanup the plan finds, each `--skip-*` declines exactly one, and the hidden compatibility spellings still parse. Met at `af418bd`.
