---
id: add-the-reindex-command
title: Add The Index Reindex Command
workstream: "0007"
kind: task
depends_on: [plan-prepare-and-apply-the-reindex]
gated: false
touches:
  - crates/froe-cli/src/command_line/index.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/index_reindex.rs
  - crates/froe-cli/tests/command_line_tests/index_reindex.rs
  - crates/froe-cli/tests/command_line_tests/main.rs
  - docs/cli-output.md
  - docs/index.md
  - docs/oak-segment-tar-feature-map.md
  - README.md
status: done
merged_as: "432e848df8579913df79c44b488aae9d3eb8f0a9"
---
# Add The Index Reindex Command

`froe index reindex REPOSITORY [--index PATH]… [--dry-run] [--yes] [--work-directory DIRECTORY] [--from-head] [--sort-budget-mebibytes N]`, following `froe compact`'s flow exactly: `--dry-run` plans read-only without the lock and prints the plan; otherwise the command prepares under the lock, prints the plan, asks for confirmation while holding the lock, applies, and prints a summary built from the observed outcome. A scripted run without `--yes` plans and cancels, naming the flag. The help text states the offline preconditions in the same words `compact` uses and states the one irreversible consequence: the previous index records become unreachable from the head but stay live through every checkpoint that references them (each lane's checkpoint does, by construction) and are reclaimed only by a `froe compact` run after those checkpoints are released; the summary names the checkpoints that pin them. The command's guide, feature-map row and README paragraph land in this task, as `CONTRIBUTING.md` requires of every capability.

**Steps:**

1. `command_line/index.rs`: the `Reindex` variant with the flags above, its dispatch in `crates/froe-cli/src/main.rs` (destructured field by field), `--from-head` documented as consulted only for a definition whose lane checkpoint is dangling or whose lane is absent from `/:async` (an intact lane ignores it) and as the explicit choice for a dangling lane checkpoint of a mirror- or unique-strategy definition (the lane's replay leaves every entry unchanged; the randomized `:count_*` estimates drift, which the help text says) and, for the counter, as the reset (hidden children removed so Oak's replay rebuilds it from scratch, since that replay would double a rebuilt counter whether or not froe ran — which the help text says), `--work-directory` mapped to `WorkDirectory::OperatorNamed` and its absence to `WorkDirectory::Default` (the system temporary directory, where the run spills into a subdirectory named from the canonical store directory's hash and locked for the run, so concurrent runs on other stores never collide and a live run is never mistaken for residue) with the same tmpfs warning `docs/interop.md` gives.
2. `index_reindex.rs`: the plan rendering (per definition: type, state root, entries, estimated bytes, warnings; a wildcard arm for `#[non_exhaustive]` actions this froe version does not know), the confirmation through `mutation::confirm`, the summary (`reindexed 3 indexes, reset 1: …; head <before> -> <after>`, keys per definition and the reset count from the outcome's typed variants).
3. Tests: dry-run takes no lock and writes nothing (filesystem snapshot), the confirmation cancels without `--yes`, `--yes` applies and the summary counts match the outcome, `--from-head` is required for a dangling or absent lane and resets the counter (hidden children gone, visible properties untouched, the summary naming the reset), a rerun of the reset reports `nothing to do` with no head move, reporting-stream invariants including a new `reporting_never_reaches_the_standard_output_of_a_reindex_plan` in `command_line_tests/index_reindex.rs` (a new file registered in its `main.rs`), modelled on the compaction one in `reporting.rs`.
4. `docs/cli-output.md`: the reindex steps and the confirmation guard row, the new reporting test in the guard table, and both reindex pairings added to the observed-twin row with their named regressions `an_observed_reindex_plan_equals_an_unobserved_one` and `an_observed_reindex_equals_an_unobserved_one`, as the row pins both halves for compaction.
5. `docs/index.md`: the reindex section — which types, which state each is rebuilt from and why, the `--from-head` choice and the counter's reset under it (Oak's replay after a lost checkpoint doubles every counter and Lucene definition on the lane regardless, so the reset lets Oak rebuild from scratch), the work directory and sort budget, the `EntryCheckBudget` refusal the publication tail raises and what an operator does about it, the residue a killed run leaves and the remedy (its froe-named run subdirectory under the work directory, which the next run refuses in an operator-named directory and warns about under the default; the operator removes it), the duplicate-unique refusal, every selection refusal the scope lists documented by name, what the plan prints, what the summary reports, that the old records stay live through every lane checkpoint that references them and are reclaimed only by a `froe compact` run after those checkpoints are released, and the beta framing until the review task's evidence is frozen; `docs/oak-segment-tar-feature-map.md`: the `--reindex` command row for the property family marked **Implemented** (beta until plan 0007's review freezes; froe extension: Oak has no offline property reindex that writes back), Lucene still **Planned**, and `froe index reindex` added to the command-surface Maintenance table; `README.md`: the maintenance paragraph gains the reindex.

- **Done when:** every command-line test passes, `reporting_never_reaches_the_standard_output_of_a_reindex_plan` holds, every documented invocation in `docs/index.md` runs as written, no guide claims a capability the code does not have, `git diff --check` is clean, and the stable host gate passes.
