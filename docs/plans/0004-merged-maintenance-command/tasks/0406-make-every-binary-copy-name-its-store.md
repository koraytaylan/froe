---
id: make-every-binary-copy-name-its-store
title: Make Every Binary Copy Name Its Store
workstream: "0004"
kind: task
depends_on: [adopt-craft-standards-and-split-the-tree]
gated: false
touches:
  - crates/froe/src/writer/compaction/walk.rs
  - crates/froe/src/writer/record_writer/values.rs
status: done
merged_as: "bc377d08d5afd342d93a31d485bf4dd3ccc0a8b9"
---
# Make Every Binary Copy Name Its Store

The review pass found that the public two-argument `copy_binary_value` wrapper still defaulted to the cross-store-unsafe mode, an implicit default in the public API of exactly the shape the range had removed from one call site. The wrapper was removed and the explicit form took the plain name, so the defect cannot be reintroduced by reaching for a shorter name. Landed in `bc377d0` (2026-08-18).

**Steps:**

1. Remove the wrapper and make `BulkBlockSharing` a required argument with no default.
2. Update the compaction walk, the only remaining caller of the old name.

- **Done when:** no call site can copy a binary value without stating the store boundary, and the change is marked breaking. Met at `bc377d0`.
