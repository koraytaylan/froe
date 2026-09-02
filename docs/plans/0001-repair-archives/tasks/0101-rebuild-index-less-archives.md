---
id: rebuild-index-less-archives
title: Rebuild Index-Less Archives
workstream: "0001"
kind: task
depends_on: []
gated: false
touches:
  - crates/froe/src/tar_archive/archive.rs
  - crates/froe/src/writer/cleanup.rs
  - crates/froe/src/writer/store_writer.rs
  - crates/froe-cli/src/inspection.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/mutation.rs
  - crates/froe-cli/tests/interop.rs
  - docs/cleanup.md
  - docs/interop.md
status: done
merged_as: "09c50aa9bca873423c7a7fc47b198d5ff75645c3"
---
# Rebuild Index-Less Archives

A writer killed mid-archive leaves a `.tar` whose index trailer was never written; Oak rebuilds such an index at its own next start, froe refused the store. This task gave `froe cleanup` an opt-in `--repair-archive-indexes` stage that rebuilds the index of an active archive that has none, staged as `<archive>.recovering`, installed with every original letter retired to `.bak`, and, only when a rebuild is about to become visible, the one-way `store.version` upgrade from 1 to 2. Landed in `09c50aa` (2026-08-13).

**Steps:**

1. Survey index-less numbers from the physical listing under the lock (`survey_indexless_archive_numbers`) with the repairability predicate the preview derives from its open readers (`unrepairable_archive_names`): no valid-index letter, at least one non-empty letter, at least one readable segment.
2. Scan each letter for tar headers, rebuild the index into a staging file, fsync it, reopen it and assert the staged archive is not itself flagged as recovered.
3. Install by hard link (copy where the filesystem has none), retire the other letters to `.bak`, then upgrade the manifest atomically; roll back completed renames best-effort on a returned error.
4. Refuse `repair-archives` beside `recovery-backups` in `validate_options`, recheck shape, version, environment, identity, duplicate names and target ownership under the lock, and carry completed rebuilds into any later refusal (`attach_completed_repairs`).
5. Name a regression per guard: `selecting_repair_with_nothing_to_repair_changes_no_byte`, `a_repair_that_installs_nothing_does_not_upgrade_the_manifest`, `an_unrepairable_archive_refuses_before_anything_is_rewritten`, `a_failed_repair_reports_the_rebuilds_it_already_completed`, `repair_archives_and_recovery_backups_cannot_run_together`, and the zero-length-letter trio.

- **Done when:** a store with one index-less archive is repaired under the lock with its originals retained under `.bak`, the manifest reads `store.version=2` only when a rebuild installed, and every named regression passes. Met at `09c50aa`.
