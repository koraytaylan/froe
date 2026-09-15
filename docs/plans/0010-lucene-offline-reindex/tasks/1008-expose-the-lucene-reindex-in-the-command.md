---
id: expose-the-lucene-reindex-in-the-command
title: Expose The Lucene Reindex In The Command With Its Binary-Text Flags
workstream: "0010"
kind: task
depends_on: [rebuild-lucene-indexes-offline]
gated: false
touches:
  - crates/froe-cli/src/command_line/index.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/index_reindex.rs
  - crates/froe-cli/tests/command_line_tests/index_reindex.rs
  - docs/index.md
  - docs/oak-segment-tar-feature-map.md
  - docs/cli-output.md
  - README.md
status: done
merged_as: "36d3ed9"
---
# Expose The Lucene Reindex In The Command With Its Binary-Text Flags

`froe index reindex` gains `--binary-text <marker|skip>` and the optional `--pre-extracted-text-directory DIRECTORY`, which together form the binary-text policy task 1007's library requires before it accepts any Lucene definition; with them the Lucene branch becomes user-facing, and its documentation lands in the same task as `CONTRIBUTING.md` requires. `--binary-text` is required for every Lucene definition, binaries or not — `skip` is the explicit choice for a definition that indexes none — so the operator's choice is always stated; the help text says what Oak does with a binary it cannot extract, that froe never extracts, that a binary without `jcr:mimeType` is never indexed whatever the policy, that `skip` reproduces Oak exactly for types Tika does not support while `marker` reproduces only the failed-extraction case, and that `--pre-extracted-text-directory` is consulted first with `--binary-text` as the fallback for blobs it does not cover.

**Steps:**

1. `command_line/index.rs`, the `Reindex` dispatch in `main.rs` (which destructures the variant field by field) and `index_reindex.rs`: the two flags, their validation, the plan rendering for a Lucene definition (rule count, document count estimate, the policy in force, the lane and its checkpoint, and the work-directory proxy task 1007's plan computes, rendered beside the reported free space and labelled a proxy rather than an upper bound), the `--from-head` reset rendered with its reason, and the summary.
2. Tests: a Lucene definition without `--binary-text` is refused naming the flag, whether named by `--index` or selected automatically beside a flagged property index; each policy reaches the library; `--pre-extracted-text-directory` without `--binary-text` is refused; the plan and summary rendering; reporting-stream invariants.
3. `docs/index.md`: the Lucene section — supported and refused features by name (the codec verdict — an explicit `codec` first, else `oakCodec` only for a fulltext-enabled definition, `Lucene46` refused — `valueRegex`, `similarityTags`, `useInSimilarity`, `dynamicBoost`, `function`, `compatVersion 1`, `maxFieldLength = 0`, two rules assigning different doc-value types to one `:dv` field name (refused at load where Oak keeps the first type and drops the later documents, the departure task 1005 records), an `nt:base` rule with a `nullCheckEnabled` property, a DATE property value that does not parse — one such value anywhere in the indexed subtree refuses the run at document time, before publication — a definition without `async`, a hybrid definition with `sync` or `unique` property definitions, custom `analyzers` children included, and a `tika` child accepted with a plan line), the lane rule and why `--from-head` resets rather than rebuilds, the binary policy, the residue a killed run leaves and the remedy (its froe-named run subdirectory under the work directory, which the next run refuses in an operator-named directory and warns about under the default; the operator removes it) — for a Lucene run a complete assembled segment, not only spill files — the suggester note, the runbook, the sizing expectations from the conformance phase, and the beta framing until the review task's evidence is frozen; `docs/oak-segment-tar-feature-map.md`: the `--reindex` command row **Implemented** for Lucene (beta until plan 0010's review freezes) with the refused features listed, and the command-surface Maintenance table's `froe index reindex` entry extended to Lucene; `docs/cli-output.md`: the Lucene reindex steps; `README.md`: the maintenance paragraph.

- **Done when:** every command-line test passes, every documented invocation runs as written, no guide claims a capability the code lacks, the refusals are documented by name, `git diff --check` is clean, and the stable host gate passes.
