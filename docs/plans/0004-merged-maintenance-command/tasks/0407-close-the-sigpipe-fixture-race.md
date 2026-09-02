---
id: close-the-sigpipe-fixture-race
title: Close The SIGPIPE Fixture Race
workstream: "0004"
kind: task
depends_on: [make-every-binary-copy-name-its-store]
gated: false
touches:
  - Cargo.toml
  - Cargo.lock
  - crates/froe-cli/Cargo.toml
  - crates/froe-export/Cargo.toml
  - crates/froe-cli/tests/diagnostic_command_tests.rs
status: done
merged_as: "0fae52ab59ee84abb4d79dd7d885f0e97c1f442f"
---
# Close The SIGPIPE Fixture Race

`v0.10.0` was tagged (`96552d0`), and the one flaky test in the release gate was fixed rather than re-run: `segment_hex_cli_uses_conventional_sigpipe_for_a_preclosed_stdout` built its pipe with inheritable descriptors, so a concurrently spawned process could hold the read end open. Characterized by asymmetry (1 failure in 15 parallel runs of the file, 0 in 20 runs alone) and fixed with `std::io::pipe`, which is close-on-exec at creation. Landed in `0fae52a` (2026-08-18).

**Steps:**

1. Bump the workspace to `0.10.0` with the breaking changes in the rationale.
2. Replace `libc::pipe` in the fixture with `std::io::pipe` and rerun the file 25 times in parallel.

- **Done when:** the test shows 0 failures in 25 parallel runs of its file, and the safety case records the characterization. Met at `0fae52a`.
