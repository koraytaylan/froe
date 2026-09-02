---
id: fix-the-two-write-path-defects
title: Fix The Two Write-Path Defects
workstream: "0004"
kind: task
depends_on: [make-compact-the-one-maintenance-command]
gated: false
touches:
  - crates/froe/src/writer/record_writer.rs
  - crates/froe/src/writer/backup.rs
  - crates/froe/src/writer/compaction.rs
  - crates/froe/src/writer/mod.rs
  - docs/analysis/write-record-writers.md
status: done
merged_as: "7acd73e59747eaab8b99e5809c241626a632bc13"
---
# Fix The Two Write-Path Defects

Two defects produced stores that passed every structural check. A preserved multi-valued slot could share a template record with a single-valued one and decode its values at the wrong arity afterwards; and a backup carried the content tree without its binary blocks whenever a copy crossed a store boundary, because bulk-segment sharing was decided by segment kind alone. Landed in `ae7aad7` and `7acd73e` (2026-08-17).

**Steps:**

1. Make `property_slot_tag` the one computation that decides a slot's arity, used by both the `TemplateKey` dedup cache and `write_template_record`, with an exhaustive match.
2. Copy binary blocks when a copy crosses a store boundary and share them only within one store; document the rule in `docs/analysis/write-record-writers.md`.
3. Pin both with `a_preserved_multi_valued_slot_never_shares_a_template_with_a_single_valued_one` and `a_backup_carries_binary_content_that_lived_in_a_bulk_segment`.

- **Done when:** both regressions fail under the restored defect and pass fixed, and a cross-store backup resolves every binary from its own store. Met at `7acd73e`.
