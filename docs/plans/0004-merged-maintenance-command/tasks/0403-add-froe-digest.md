---
id: add-froe-digest
title: Add froe digest
workstream: "0004"
kind: task
depends_on: [fix-the-two-write-path-defects]
gated: false
touches:
  - README.md
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/tooling_display.rs
  - crates/froe/src/checksum.rs
  - crates/froe/src/tooling/digest.rs
  - crates/froe/src/tooling/mod.rs
  - crates/froe/tests/digest_tests.rs
  - docs/cli-output.md
  - docs/oak-segment-tar-feature-map.md
status: done
merged_as: "f7a585a144c7094cad5dec5dccf4913d844ba373"
---
# Add froe digest

A canonical rendering of a repository's content, one line per node, property name, type, arity, value and binary checksum, so that two stores can be compared for content identity rather than for structural validity, with a 16 MiB insertion-order cache of inline-binary checksums. It is the instrument the write-path defects needed and the one every later interop phase is held to. Landed in `f7a585a` (2026-08-17).

**Steps:**

1. Define the rendering in `tooling/digest.rs` with a stability contract in `docs/cli-output.md`.
2. Add `froe digest` to the CLI and the feature map, and unit tests in `digest_tests.rs` with independently constructed expectations.

- **Done when:** two byte-different stores with identical content render identically, a one-value difference renders as one differing line, and the contract is documented. Met at `f7a585a`.
