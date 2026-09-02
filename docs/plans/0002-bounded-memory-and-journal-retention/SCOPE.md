# Scope — Plan 0002

> Make every cache and memo in froe a function of configuration rather than of repository size, let a write session certify what it wrote by recorded CRC instead of retained bytes, and add the first policy-based retirement of reachable history, `--retain-journal-revisions`, with the safety case and the Oak evidence such a change requires.

## Why this plan

Field runs on large stores exhausted memory and file descriptors: read caches grew with the repository, a write session retained the payload bytes of every segment it wrote until the run ended and held a descriptor per session archive, and the recovery walk accumulated state in function locals. The range is high-risk on four counts named in its safety case: it adds a task that makes repository bytes unreachable by design, it rewrites how a session holds and certifies what it wrote, it loosens a refusal on the destructive default path, and (through the successor work it was later reconciled with) it changes how every walk terminates on a corrupt record graph.

The range ran from the `v0.7.0` bump to `v0.8.0` and continued for one day after it with record reuse, bulk-segment sharing and a tree-sized memo; the exact memo and the depth-limit removal that followed are plan 0003. The frozen safety case reports all of it because it was reconciled after plan 0003 landed, and it carries a superseded-note from plan 0004, which inverted two of its conclusions.

## In scope

- **Byte-ceilinged caches everywhere.** `BoundedCache::evict_to_budget` behind every read and write cache and every memo; a source-shape guard asserting that long-lived store state holds no store-scaled collection.
- **A session that certifies by CRC.** `validate_finalized_session_semantics` proves payload identity by recorded checksum; `FinalizedSessionArchiveCertificate` fingerprints by identity and metadata rather than holding a descriptor per archive; the archive reopen after rotation as a new, read-only boundary.
- **`--retain-journal-revisions N`.** Keeps the newest `N` revisions that resolve, requires the journal task in the same run, refuses beside a checkpoint head update, and counts only revisions that actually resolve.
- **Reporting what cleanup found and declined**, and the loosened survivor check that lets reclamation proceed when dead segments outlive the sweep.
- **Record reuse and binary sharing.** A writer reuses an identical value or template record it already wrote; compaction shares bulk segments instead of re-copying binary content.
- **The safety case**, the `--retain-journal-revisions` interop phase, and the revision of `docs/high-risk-changes.md` that made the review gate one the project can perform.

## Out of scope

- The exact compaction sharing memo and the removal of every depth limit: plan 0003, whose results the frozen document reports after reconciliation.
- Process RSS measurement (budgets are asserted against `cache_weight`), macOS execution, an armed cutpoint at the reopen boundary, and a ceiling on depth-proportional walk state; all recorded as gaps.
