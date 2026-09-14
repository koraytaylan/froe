---
id: add-digest-property-prefix-exclusion
title: Add A Property-Prefix Exclusion To The Digest
workstream: "0006"
kind: task
depends_on: [split-the-command-line-module]
gated: false
touches:
  - crates/froe/src/tooling/digest.rs
  - crates/froe/tests/digest_tests.rs
  - crates/froe-cli/src/command_line.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/tooling_display.rs
  - crates/froe-cli/tests/command_line_tests/diagnostics.rs
  - docs/oak-segment-tar-feature-map.md
  - docs/compact.md
  - docs/cli-output.md
  - README.md
status: done
merged_as: "dd04fe7ad16a93222910558ff6aafb9ea127504a"
---
# Add A Property-Prefix Exclusion To The Digest

Oak's approximate counters write `:count_<random uuid>` properties whose names, presence and values are all drawn from a random generator, so two indexes built from identical content differ on them by design. Plan 0007's oracle — froe's rebuilt index must render identically to Oak's own rebuild — needs a way to excuse exactly those properties and nothing else. Extend `froe digest` with `--exclude-property-prefix PREFIX` (repeatable), applied to every node in the rendering, stamped into the digest as a header line so a digest with an exclusion can never be compared blind against one without, mirroring how `--exclude-subtree` already stamps `#excluded`. The flag is documented wherever `--exclude-subtree` is documented today, in the same task.

**Steps:**

1. `digest_repository_excluding` gains a property-prefix set; emit a `#excluded-properties` header listing the sorted prefixes, and skip matching properties in `emit_node` while still counting them separately in `DigestSummary` (`excluded_properties`), so the summary line says how much was excused (`digest.rs` is 688 lines and `tooling_display.rs` 843; new tests stay in-module only while each file remains under the thousand-line gate); the `digest_repository_excluding` signature changes, which the 0.x version allows (`digest_repository` keeps its own and delegates with an empty prefix list), and `DigestSummary` gains `#[non_exhaustive]` in the same commit so the next field is not another break.
2. `compare_digests` treats the new header like any other line, so mismatched exclusions surface as a difference.
3. CLI flag, help text, the `Command::Digest` dispatch in `main.rs` (which destructures the variant field by field and passes each flag explicitly), and the summary line on standard error naming the excluded property count.
4. Tests: a store whose `:index` nodes carry `:count_*` properties digests identically before and after those properties are perturbed when the prefix is excluded, and differently when it is not; the header round-trips through `parse_digest`; a store with no `:count_*` property digested with and without the exclusion differs only in the header line; the CLI test in `command_line_tests/diagnostics.rs`, beside `digest_excludes_exactly_the_named_subtree`, pins the flag, the header and the summary line.
5. Documentation: the digest's flag list in `docs/oak-segment-tar-feature-map.md`, the digest paragraph in `docs/compact.md`, the `README.md` mention of `--exclude-subtree` and the digest summary's contents enumerated in `docs/cli-output.md` each gain the new flag or the excluded-property count.

- **Done when:** the two-digest test pair passes in both directions, a full digest and an excluding digest of the same store are reported as different because of the header alone, every place that documents `--exclude-subtree` or the digest summary documents the new flag or count, and the stable host gate passes.
