---
id: dump-lucene-indexes-to-the-filesystem
title: Dump Lucene Indexes To The Filesystem
workstream: "0008"
kind: task
depends_on: [read-lucene-segment-descriptors]
gated: false
touches:
  - crates/froe/src/index/lucene/dump.rs
  - crates/froe/src/progress.rs
  - crates/froe/src/tooling/output_directory.rs
  - crates/froe/src/tooling/mod.rs
  - crates/froe-export/src/output_file.rs
  - crates/froe-cli/src/command_line/index.rs
  - crates/froe-cli/src/main.rs
  - crates/froe-cli/src/index_display.rs
  - crates/froe-cli/tests/command_line_tests/index_transport.rs
  - crates/froe-cli/tests/command_line_tests/main.rs
  - crates/froe/tests/lucene_dump_tests.rs
  - crates/froe/src/index/lucene/mod.rs
  - docs/index.md
  - docs/oak-segment-tar-feature-map.md
  - docs/cli-output.md
  - README.md
status: planned
merged_as: ""
---
# Dump Lucene Indexes To The Filesystem

`froe index dump REPOSITORY --output DIRECTORY [--index PATH]…` writes every selected Lucene definition's `:data` and `:suggest-data` (and mount-decorated variants, reported but skipped) to the filesystem in oak-run's layout: `<output>/index-dumps/<base name>/<directory name>/…` with `index-details.txt`, where the base name and the directory names come from plan 0006's `layout.rs`; `<output>/index-dumps/index-definitions.json` rendered by plan 0006's `definitions_json.rs` in the variant an oak-run out-of-band build dumps with, which drops `:index-definition`, `:data` and `:suggest-data` and keeps `:status`, over exactly the selected definitions (the key set oak-run prints for `--index-paths`), because oak-run's importer requires the file and froe's import reads it back; and `<output>/index-dumps/indexer-info.properties`, at the exact level oak-run's importer reads it from (it scans that directory's direct children for `index-details.txt`, so `froe index import --input <output>/index-dumps` and oak-run's `--index-import-dir` both find the indexes). The properties file names one checkpoint for the whole directory while plan 0008's state rule is per definition, so it is written only when every dumped definition is asynchronous on the same lane and that lane's checkpoint resolves in the store (plan 0006's `dangling_checkpoints`). A selection that mixes lanes, includes a synchronous definition, or whose lane checkpoint is dangling still dumps the files as a backup but writes no `indexer-info.properties`, says so, and names the lanes or the dangling checkpoint — such a directory cannot be imported by oak-run either, which warns exactly that when the checkpoint is `head` — and the guide says to dump and import per lane with `--index`. The operation is read-only against the store; the output directory must not be inside the store; existing output is never overwritten, so an interrupted dump leaves a partial file set (and froe-named temporaries, removed on a returned error but not after abrupt death) that the operator must delete before rerunning — stated in the guide with that remedy. The command's documentation lands here.

**Steps:**

1. A first, separate `refactor:` commit moves the inside-the-repository predicate down into this crate, because `froe-export` depends on `froe` and a call the other way is a cycle cargo refuses: `tooling::output_directory::refuse_output_inside_repository(repository_path, directory)` carries the nearest-existing-ancestor canonicalization and the error text verbatim — the file-case rule in `create_export_output` stays where it is, deliberately: it canonicalizes the parent only and names a file rather than a directory, and each form has its own landed test, a duplication recorded at both sites — and `froe_export::output_file::create_export_directory` calls it before it creates the directory, so its doc comment, its two callers and `export_tests.rs`'s two tests are unchanged. Then `dump.rs`, whose output directory goes through that helper for the inside-the-store refusal (which is all it guards — it accepts an existing directory by contract, and `froe export` and the refresh depend on that) plus the dump's own `refuse_existing_dump_output(output)`, an emptiness check on `<output>/index-dumps` that is the never-overwrite guard 0806 records: `dump_lucene_indexes(repository, options: &DumpOptions)` — `DumpOptions` and `DumpOutcome` carrying private fields, the options built through `DumpOptions::new(selection, output)` and `with_*` setters as `CompactionOptions` is, so the integration tests construct them the way a downstream crate must — delegating to `dump_lucene_indexes_with_progress(repository, options, progress) -> DumpOutcome` streaming each file through `OakDirectory` to a temporary name and renaming it into place, fsyncing files and directories so a dump that returned is durable; `index-details.txt`, `index-definitions.json` and `indexer-info.properties` written last; the progress step is `dumping index files`, counting `WorkUnit::Files`.
2. `froe index check` gains the segment-level check and the document count column from task 0802 (this touches only the display module and the check dispatch).
3. The `Dump` variant in `command_line/index.rs`, its help text, and tests in a new `command_line_tests/index_transport.rs` registered in its `main.rs`: the read-only invariant by file snapshot (`directory_snapshot`, reachable in the `command_line_tests` binary through the `filesystem_snapshot` module task 0611 added to its `main.rs`), the refusal of an output path under the store, the never-overwrite rule, a partially populated output directory refused with a message naming the remedy, the layout names for `/oak:index/lucene`, the one-checkpoint rule for a mixed selection, for a synchronous definition and for a dangling lane checkpoint, and the reporting-stream invariants.
4. `crates/froe/tests/lucene_dump_tests.rs`: a synthetic store built by writing the committed sample index's files through the writer's binary path in both encodings; the dump must reproduce the original bytes, an observed dump equals an unobserved one (`an_observed_dump_equals_an_unobserved_one`), and an interrupted dump — a per-file write failing partway — leaves the froe-named temporaries removed and the completed file set intact, which is the regression the architecture's dump row names.
5. `docs/index.md`: the dump section — the layout, the one-checkpoint rule, the synchronous case (a backup, not importable), dumping per lane, the residue an interrupted dump leaves and the remedy (delete the output directory before rerunning), what `check` now proves at segment level, and the beta framing until the review task's evidence is frozen; `docs/oak-segment-tar-feature-map.md`: `--index-dump` **Implemented** (beta until plan 0008's review freezes), `froe index dump` added to the command-surface Read-only table, `--index-consistency-check` level 2 noted as covered by the segment-level check plus the judge, not by a native port of Lucene's own index checker; `docs/cli-output.md`: the `dumping index files` step in the per-command table and the dump pairing added to the observed-twin row with its named regression `an_observed_dump_equals_an_unobserved_one`; `README.md`: the dump in the read-only list; and widen `WorkUnit::Files`'s doc comment, which today scopes it to the repository directory, to cover files in an index dump or import directory too, since this task is the first to count those.

- **Done when:** a dump of both encodings reproduces the committed files byte for byte, the store snapshot is unchanged after a dump, the three metadata files are where oak-run's reader expects them, every documented invocation runs as written, and the stable host gate passes; byte identity with Oak's own dumper is task 0808's acceptance.
