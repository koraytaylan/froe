---
id: report-declined-cleanup-and-bound-the-journal
title: Report Declined Cleanup And Bound The Journal
workstream: "0002"
kind: task
depends_on: []
gated: false
touches:
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/mutation.rs
  - crates/froe-cli/tests/command_line_tests.rs
  - crates/froe/src/writer/cleanup.rs
  - crates/froe/src/writer/store_writer.rs
  - docs/cleanup.md
  - docs/cli-output.md
status: done
merged_as: "45cd793acb5d79c7bdd2cb159daca94a0aee6d0f"
---
# Report Declined Cleanup And Bound The Journal

Cleanup used to be silent about garbage it found and declined to touch, and the journal could only grow. This task made the plan say what was found and why it was retained, and added `--retain-journal-revisions N`, which keeps the newest `N` revisions that resolve and removes the rest in the same run as the segment sweep, so the lines it un-roots leave the journal together with their segments. Landed in `45cd793` (2026-08-14).

**Steps:**

1. Report retained garbage with the reason it survives (`history_protected_reclaimable`, reporting only, never a mutation input).
2. Add the retention bound to `CleanupOptions` and thread it through all three `analyze_journal` call sites: plan, pre-rewrite under the lock, and the final post-mutation proof.
3. Refuse a bound without the journal task (`a_retention_bound_without_the_journal_task_is_refused`) and beside a checkpoint head update (`a_retention_bound_beside_a_checkpoint_head_update_is_refused_while_planning`).
4. Document the flag and the new plan lines in `docs/cleanup.md` and `docs/cli-output.md`.

- **Done when:** a run with `--retain-journal-revisions 1 --task journal,segments` leaves one journal line and a numbered backup, the two incoherent combinations are refused before mutation, and the plan names what it declined. Met at `45cd793`.
