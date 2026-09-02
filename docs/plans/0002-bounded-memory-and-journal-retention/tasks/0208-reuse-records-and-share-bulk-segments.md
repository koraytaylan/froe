---
id: reuse-records-and-share-bulk-segments
title: Reuse Records And Share Bulk Segments
workstream: "0002"
kind: task
depends_on: [make-the-review-gate-performable]
gated: false
touches:
  - crates/froe/src/cache.rs
  - crates/froe/src/writer/record_writer.rs
  - crates/froe-cli/tests/interop.rs
status: done
merged_as: "d14951fdfd274fb6971b7505f5d626c129f34967"
---
# Reuse Records And Share Bulk Segments

The writer re-wrote an identical value or template record every time it met one, and compaction copied every binary block even when the block already lived in a bulk segment that survives. Record reuse cut an authored store of 4000 identical nodes by 3.35x and its compacted output by 2.80x; bulk-segment sharing meant that on an Oak-written store most bytes are referenced rather than rewritten. The interop `cleanup` phase's reclaimable condition, which had depended on froe copying binaries, was rebuilt. Landed in `93858c1` and `d14951f` (2026-08-14).

**Steps:**

1. Key value and template records in the session cache so `write_string` and `write_template` return the record already written (`a_repeated_string_or_template_reuses_the_record_it_already_wrote`).
2. Share a block in `copy_binary_value` only when it lives in a bulk segment; copy otherwise.
3. Rebuild the interop `cleanup` phase to write 2000 nodes at generation zero never linked to a head, so its garbage does not depend on binaries being copied.

- **Done when:** the two unit regressions pass, and `compact` and `compact --tail` pass against Oak with sharing active. Met at `d14951f`.
