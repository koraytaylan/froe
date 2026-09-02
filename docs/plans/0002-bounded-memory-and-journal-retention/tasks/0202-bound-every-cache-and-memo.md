---
id: bound-every-cache-and-memo
title: Bound Every Cache And Memo
workstream: "0002"
kind: task
depends_on: [report-declined-cleanup-and-bound-the-journal]
gated: false
touches:
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/mutation.rs
  - crates/froe-cli/src/tooling_display.rs
  - crates/froe-export/src/refresh.rs
  - crates/froe-export/src/sqlite.rs
  - crates/froe/src/cache.rs
  - crates/froe/src/lib.rs
  - crates/froe/src/store.rs
  - crates/froe/src/tar_archive/archive.rs
  - crates/froe/src/tooling/check.rs
  - crates/froe/src/tooling/diff.rs
  - crates/froe/src/tooling/mod.rs
  - crates/froe/src/tooling/search.rs
  - crates/froe/src/writer/backup.rs
  - crates/froe/src/writer/compaction.rs
  - crates/froe/src/writer/store_writer.rs
status: done
merged_as: "77a6e7ebc15a4b205d861ca7f19b4c3c1da3ce40"
---
# Bound Every Cache And Memo

Every cache and memo that grew with the repository was put under a byte ceiling, the write session stopped retaining the payload bytes of segments already durable in an archive and started certifying them by recorded CRC, and the certificate stopped holding a descriptor per session archive. The tests assert that memory stays bounded, not merely that results are correct. Landed in `b436507` and `77a6e7e` (2026-08-14).

**Steps:**

1. Introduce `BoundedCache` with `evict_to_budget` and put the parsed-segment, string and template caches of both `Repository` and `ArchiveSet` behind it, with the writer's base-segment cache and the session read-back cache sized to the rotation threshold.
2. Replace retained session payload with `validate_finalized_session_semantics`, proving identity by CRC before the journal append, and fingerprint archives in `FinalizedSessionArchiveCertificate` by identity and metadata.
3. Add the source-shape guard `long_lived_store_state_holds_nothing_that_grows_with_the_repository` and the residency test `writing_more_segments_does_not_make_a_session_hold_more_bytes`.
4. Give the export refresh and the SQLite dictionary their budgets and reuse one `PackedRecordSet` per journal revision in `check`.

- **Done when:** each budgeted structure has a test that fails when its ceiling is bypassed, the session certificate refuses a foreign payload, and the descriptor count no longer scales with the session. Met at `77a6e7e`.
