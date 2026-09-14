---
id: arm-import-fault-cutpoints
title: Arm Fault And Process-Death Cutpoints Around The Import
workstream: "0008"
kind: task
depends_on: [import-an-out-of-band-index, write-the-transport-safety-case]
gated: false
touches:
  - crates/froe/src/writer/fault_injection/lucene_import.rs
  - crates/froe/src/writer/fault_injection/mod.rs
  - crates/froe/src/writer/fault_injection/test_support.rs
  - crates/froe/src/writer/index/lucene_import/apply.rs
  - crates/froe/tests/lucene_import_guard_tests.rs
  - docs/plans/0008-lucene-index-transport/ARCHITECTURE.md
status: planned
merged_as: ""
---
# Arm Fault And Process-Death Cutpoints Around The Import

The import's cutpoints follow the pattern task 0708 set for the reindex: `#[cfg(test)]` fault and process-death points at the four boundaries step 1 names (`before-head-publish`, `after-head-publish-before-flush`, `mid-file-copy`, `before-applied-verification`), armed by named tests in the child harness with its scenario-specific exit codes (`CRASH_EXIT_CODE` at the cutpoint, `VERIFIED_EXIT_CODE` after the error child's assertions), so every guard row in the safety case quotes an observed failing result and every interruption prefix has an observed outcome.

**Steps:**

1. Cutpoints `lucene-import.before-head-publish`, `lucene-import.after-head-publish-before-flush`, `lucene-import.mid-file-copy` (a returned error while streaming the largest file), `lucene-import.before-applied-verification`; returned-error and abrupt-exit variants where the states differ, through the child harness in `test_support.rs`, which gains an import child entry point and scenario dispatch beside the compaction and reindex ones.
2. Named tests in `fault_injection/lucene_import.rs`, each freshly reopening the store: before publication the old head, the old `:data`, every checkpoint still present, and unreferenced archives at the head's generation that the next `froe compact` reclaims (the test runs it); after publication the old head in the journal and the new records unreachable — the same on-disk prefix, stated as such; a mid-copy error leaves the store unchanged and reports the file and byte offset reached. After every death variant the parent reacquires `repo.lock`, reruns the import from the same input directory and asserts the retry publishes the same post-state (the import has no work directory and leaves no residue outside the store).
3. Write `crates/froe/tests/lucene_import_guard_tests.rs` with one test per guard, named for the property, reaching `plan_lucene_import` or `PreparedLuceneImport::apply` — guard regressions with neutralization evidence for: the state rule, per definition; the synchronous-definition refusal; the one-checkpoint refusal of a mixed selection; the output-inside-store refusal of the dump; the never-overwrite rule; the unknown directory mapping refusal; the unknown-`indexPath` refusal; the definitions-file drift refusal (including the `corrupt`-, `indexImportState`- and `seed`-tolerant acceptances, the added-`facets`-subtree acceptance and the removed-child refusal); the checkpoint-set preservation (no checkpoint released); the disabler's flag written only under its predicate; the repository-shape check, the journal-owner gate and the metadata-source gate (the metadata-source twin's module test task 0715 added, the existing `validate_apply_identity_for_uid` module test, a synthetic-store shape test, and the in-crate wiring unit test task 0805 wrote in `lucene_import/prepared.rs` over 0715's observation seam, as 0709 records the reindex's; the fsync-capability gate stays in the known gaps task 0811 writes); the hybrid definition refusal; the fingerprint recheck; the path-identity recheck; the certified archive number; the moved-head compare-and-set — the last four through `PreparedLuceneImport`, the import's own production caller, with the moved-head row carved out of the neutralization requirement as task 0709 carves out the reindex's, for the same reason: the import holds `repo.lock` exclusively across the same window, so no second writer can move the head, and its row points at the known-gaps entry task 0811 writes; the header validation before any write; the read-back verification before publication. Record the rows in the safety case's fault and guards tables under the header the guide prescribes — guard and production callers, named regression, neutralization, observed failing result, quoting the failure and never the accept condition, as task 0709 records the reindex's; the dump's two guards reach `dump_lucene_indexes` through 0803's tests — the output-inside-store refusal in `tooling::output_directory::refuse_output_inside_repository`, whose second production caller is `create_export_directory`, its behaviour-preserving phase covered by `export_tests.rs`'s two landed tests, which the row cites beside the dump's, and the never-overwrite refusal in the dump's own `refuse_existing_dump_output` — and this task records their neutralization evidence. Perform the neutralization experiments serially against a separate target directory with labelled logs; restore and rerun after each.

- **Done when:** every cutpoint is armed by a named test that fails when removed, every guard row except the moved-head refusal has an observed failing result recorded from a serial neutralization, and the process-death tests prove the scenario ran through the harness's scenario-specific exit codes (`CRASH_EXIT_CODE` at the cutpoint, `VERIFIED_EXIT_CODE` after the assertions) and the parent's fresh reopen, and the stable host gate passes.
