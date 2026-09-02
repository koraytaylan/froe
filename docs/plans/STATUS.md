# Plans — roll-up board

One row per plan. Task status is authored in each plan's `tasks/*.md` frontmatter and summarized by its `STATUS.md`.

Plans are numbered by creation order. Plans 0001 through 0005 record the ranges that landed between 2026-08-13 and 2026-09-01, before this directory adopted the Makina layout; they were restructured into it on 2026-09-02, their tasks reconstructed from the commits that landed them and their safety cases and design documents carried verbatim inside their `ARCHITECTURE.md`. They are records, not executable input.

| Plan | Title | Status | Tasks | Outcome | Status doc |
|---|---|---|---|---|---|
| 0001 | Archive Index Repair | ✅ Done | 3/3 | the repair stage landed with its safety case and the `repair` interop phase; plan 0005 later made it a default-on question of `froe compact` with the same mechanism, ordering and guarantees. | [status](0001-repair-archives/STATUS.md) |
| 0002 | Bounded Memory and Journal Retention | ✅ Done | 9/9 | every cache is a function of configuration, the session certificate proves payload identity by recorded CRC, `--retain-journal-revisions` shipped in `v0.8.0` with an Oak-proved phase, and the writer stopped rewriting identical records and re-copying binary content. | [status](0002-bounded-memory-and-journal-retention/STATUS.md) |
| 0003 | Exact Compaction Sharing Memo | ✅ Done | 5/5 | compaction copies each distinct node once with a memo costing 16 bytes a slot, every walk carries its own stack and refuses a self-referential graph at the closing record, and the verifier's count equals its certificate count by construction. | [status](0003-exact-compaction-sharing-memo/STATUS.md) |
| 0004 | Merged Maintenance Command | ✅ Done | 9/9 | `v0.10.0` ships one maintenance command with unconditional reclamation, a content digest, a safety case reviewed adversarially, and an interop suite that holds every phase to a declared delta against a digest-pinned Sling image. | [status](0004-merged-maintenance-command/STATUS.md) |
| 0005 | Maintenance Remediation | ✅ Done | 8/8 | `v0.11.0` says what it knows, plans once under the lock, verifies the copy before publishing it, reports `nothing to do` on an already-compact store, purges orphaned version histories as a confirmed default-on question, and proves the gate and the purge against a real Oak with digest exclusions derived from the confirmed plan. | [status](0005-maintenance-remediation/STATUS.md) |
