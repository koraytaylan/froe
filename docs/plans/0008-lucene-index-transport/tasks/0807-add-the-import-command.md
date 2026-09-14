---
id: add-the-import-command
title: Add The Index Import Command
workstream: "0008"
kind: task
depends_on: [import-an-out-of-band-index, dump-lucene-indexes-to-the-filesystem]
gated: false
touches:
  - crates/froe-cli/src/command_line/index.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/index_import.rs
  - crates/froe-cli/tests/command_line_tests/index_transport.rs
  - docs/cli-output.md
  - docs/index.md
  - docs/oak-segment-tar-feature-map.md
  - README.md
status: planned
merged_as: ""
---
# Add The Index Import Command

`froe index import REPOSITORY --input DIRECTORY [--index PATH]… [--dry-run] [--yes]` with the compaction flow: lockless dry-run, prepare under the lock, the `LuceneImportPlan` printed, confirmation while holding the lock, apply, summary from the observed `LuceneImportOutcome`. The plan lists, per definition, the directories and files with byte counts, the checkpoint the index reflects and the lane it will be attached to, the bookkeeping that will change (`corrupt` and `indexImportState` cleared when present), and the note that `:suggest-data` will be absent until Oak's next suggester cycle. The rendering lives in its own `index_import.rs` beside `index_reindex.rs`, so neither file approaches the thousand-line gate. The command's documentation and runbook land here.

**Steps:**

1. The `Import` variant in `command_line/index.rs`, its dispatch in `crates/froe-cli/src/main.rs`, and its help text stating the offline preconditions and the state rule in operator terms ("the index must have been built at the checkpoint this store is at").
2. Plan and summary rendering in `index_import.rs`.
3. Tests: dry-run takes no lock; cancellation without `--yes`; `--yes` applies; the state-rule refusal is rendered with both roots and the failing definitions; the synchronous- and hybrid-definition refusals state their reasons; the definitions-file drift refusal names the differing property or child; the unknown-`indexPath` refusal names the directory; reporting-stream invariants through a named `reporting_never_reaches_the_standard_output_of_an_import_plan`.
4. `docs/cli-output.md`: the import steps with their units, the confirmation guard row, `reporting_never_reaches_the_standard_output_of_an_import_plan` added to the guard table, and both import pairings added to the observed-twin row with their named regressions `an_observed_import_plan_equals_an_unobserved_one` and `an_observed_import_equals_an_unobserved_one`, as the row pins both halves for compaction.
5. `docs/index.md`: the import section — the layout, the state rule and why it replaces bring-up-to-date, one directory per lane, why synchronous Lucene definitions are refused, how to build out of band with oak-run at the right checkpoint (`--checkpoint` with the lane's checkpoint name, read from `/:async/<lane>` on the stopped store), that AEM stays stopped from reading that name through the import because a running lane releases its previous checkpoint after every cycle, that `index-definitions.json` must agree with the store under the drift comparison and what an oak-run file legitimately differs in (`reindexCount`, `refresh`, `seed`, a cleared `corrupt` or `indexImportState`, and a `facets` subtree the build's document maker persisted), that no checkpoint is released, what the plan and summary print, the suggester note, the beta framing until the review task's evidence is frozen, and the runbook: stop AEM, `froe index dump` as a backup, `froe index import`, `froe index check`, start AEM, watch the lane; `docs/oak-segment-tar-feature-map.md`: the `--index-import` command row **Implemented** (beta until plan 0008's review freezes) and `froe index import` added to the command-surface Maintenance table; `README.md`: the maintenance paragraph.

- **Done when:** every command-line test passes, every documented invocation runs as written, no guide claims a capability the code lacks, `reporting_never_reaches_the_standard_output_of_an_import_plan` passes, and the stable host gate passes.
