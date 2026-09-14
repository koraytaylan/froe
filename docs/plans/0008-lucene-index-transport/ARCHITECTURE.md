# Architecture — Plan 0008

## 0008 — Lucene Index Transport

Requires plans 0006 and 0007 merged (the readers, the judge, the reindex operation's lock-and-verify skeleton, the definition bookkeeping).

### Ground truth

The file formats are the Lucene 4.7.2 ones `oak-lucene` vendors at the pinned commit and exports as version `4.7.2-oak2`: the commit file `segments_N` and its optional `segments.gen` pointer, the per-segment `.si` descriptor, the compound file's table of contents, and the codec header every one of those opens with. Task 0802 specifies all five as a section of `docs/analysis/index-lucene-storage.md` before froe reads a byte, and plan 0009's `docs/analysis/lucene-4-7-codec.md` cites that section for the write side. The repository-side behaviour — Oak's own dumper, oak-run's Lucene importer and the four-step import protocol around it, the local index directory with its `index-details.txt` and `indexer-info.properties`, Oak's own document count over a directory, the importer's definition-refresh step, the fresh `uid` and the format version a fresh index gets — is specified by plan 0006's `docs/analysis/index-definitions.md` (task 0601 step 4: `uid` a `STRING` of decimal epoch milliseconds, read back strictly) and `docs/analysis/index-lucene-storage.md`.

### The state rule that replaces bring-up-to-date

oak-run's importer runs four numbered steps on a live store: switch the definition's lane to `temp-<lane>`, import the data, bring the index up to date from the indexed checkpoint to the lane's current checkpoint with Lucene's own writer — the step that reverts the lane — and release the checkpoint. The third step needs an index writer froe does not have until plan 0009; the lane switch and the revert inside that third step exist only because another indexer is running. Offline, with the head frozen under `repo.lock`, the equivalent guarantee is a precondition rather than a repair: the index directory's `indexer-info.properties` names the checkpoint it reflects; froe resolves that checkpoint in the store and requires its root record to be, by identity, the root of the selected definition's lane checkpoint — evaluated per definition (checkpoints share the content root's record: froe's `create_checkpoint` stores the content root's own record identifier as the checkpoint's `root`, as Oak's own checkpoint creation does, and froe's memo-based compaction preserves that sharing). A definition is asynchronous when it carries an `async` property, and its lane is the one `async` value that is neither `sync` nor `nrt` — `async=[async, nrt]` is asynchronous on lane `async`; a definition without an `async` property is synchronous, which is the mode test Oak's own cycle applies when it selects the definitions for a run; an `async` holding only `sync` or `nrt` is one Oak refuses and task 0605 reports as a typed error. The import serves asynchronous definitions only: Oak documents `async` as required for a Lucene definition, and oak-run's own importer never completes the synchronous case — its catch-up step skips the `sync` lane and leaves the definition on `async = temp-sync` — so there is no Oak reference behaviour to match and no oracle to prove against; a synchronous Lucene definition is refused by the import with that reason, and the dump still writes its files as a backup but no `indexer-info.properties`. When the roots match, no catch-up is owed and the lane's own next cycle continues correctly from its checkpoint. When they do not, the import is refused with the two roots named. The lane's own checkpoint is the one to build at, and `froe index dump` writes it into `indexer-info.properties`, so a dump followed by an import on the same store round-trips and an out-of-band build by Oak's editors at that same checkpoint imports; a checkpoint created at the head can never satisfy the rule, because every lane cycle commits the index changes after taking its checkpoint, so the head's root always differs from the lane's. The import releases no checkpoint: the only checkpoint it accepts is a lane's, which the lane owns (a deliberate departure from oak-run's importer, whose fourth step releases that checkpoint unconditionally unless `oak.index.importer.preserveCheckpoint` is set, recorded at the site). One `indexer-info.properties` names one checkpoint for the whole directory while the rule is per definition, so a dump writes the file only when every dumped definition is on the same lane and that lane's checkpoint resolves, and otherwise writes none and names the lanes (or the dangling checkpoint); an import evaluates the rule per selected definition and refuses the directory naming every definition the checkpoint does not fit. Operators dump and import per lane with `--index`, and AEM stays stopped throughout, because a running lane releases its previous checkpoint after every cycle.

### Modules

```
crates/froe/src/index/lucene/
├── codec_header.rs      codec header and footer reading (magic 0x3fd76c17, name, version)
├── segments.rs          segments.gen, segments_N, per-segment .si; the commit-file model
├── compound.rs          .cfe/.cfs table of contents and sub-file reader
├── check.rs             structural check added beside task 0611's blob check: listing vs segments,
│                        document and deletion counts, headers, codec name
└── dump.rs              dump_lucene_indexes -> DumpOutcome: files out, index-details.txt, indexer-info.properties, index-definitions.json
crates/froe/src/index/definitions_json_reader.rs   index-definitions.json read back (bounded reader; stubbed by plan 0006)
crates/froe/src/writer/index/
├── lucene_directory.rs  OakDirectoryWriter: files in, single-blob encoding, dirListing
└── lucene_import/
    ├── mod.rs           re-exports; the state rule, definition drift, bookkeeping
    ├── plan.rs          plan_lucene_import, LuceneImportOptions, LuceneImportPlan
    ├── prepared.rs      PreparedLuceneImport (the wiring unit test lives here)
    └── apply.rs         apply, LuceneImportOutcome, and the cutpoints task 0806 arms
crates/froe/src/writer/fault_injection/lucene_import.rs   cutpoints
crates/froe/src/writer/record_writer/values.rs   write_binary_stream(Read) — streaming binaries
crates/froe/src/index/inventory.rs                the Lucene document count filled in (0802)
crates/froe-cli/src/index_display.rs               the check's document-count column (0803)
crates/froe-cli/src/index_import.rs              plan/confirm/apply flow of the import command
(the operation is the capability the feature map inventories; the readers, the directory writer and the definitions reader are its components)
crates/froe/src/tooling/output_directory.rs               refuse_output_inside_repository, moved down
                                                          here by 0803's refactor: commit
crates/froe-cli/tests/interop/judge/Consistency.java      the judge classes this plan adds (0808)
crates/froe-cli/tests/interop/judge/OutOfBandBuild.java   (0809)
```

The import follows plan 0007's open protocol: the repository-shape check and the two pre-lock apply-identity gates, the lock with the path-identity check, replan, fingerprint, certified archive number and the metadata-source gate in `prepare`; the fingerprint and path-identity rechecks and then `open_prepared` in `apply`, so the store is untouched until the first index record is appended; the dump and the lockless plan open read-only.

### Mutation and publication order

| Boundary / cutpoint | Preconditions | Published or durable change | Returned-error state and named regression | Abrupt-exit state and named regression | Reconciliation |
| --- | --- | --- | --- | --- | --- |
| Dump output written under `--output` (files outside the store; the store is only read) | the output path is outside the store and `<output>/index-dumps` is empty | files outside the store only | froe-named temporaries removed, the partial file set left (0803's interrupted-dump test) | not applicable — no cutpoint is armed in the dump, which writes nothing inside the store; an abrupt exit leaves the partial file set and its temporaries | the operator deletes the output directory; the never-overwrite refusal blocks a rerun over it |
| Import directory validated | files listed, `segments_N` parsed, `.si` per segment, every listed file present, every codec header valid, `index-definitions.json` parsed and free of definition drift against the store under Oak's own comparison (extended by the properties an oak-run build rewrites: `reindexCount`, `refresh`, a created `seed`, a cleared `corrupt` and `indexImportState`, and a `facets` subtree the file adds), every selected definition asynchronous and not hybrid | none | refusal names the file, the drifting property or child or the synchronous or hybrid definition (regression named by 0806) | not applicable | fix the directory or the definition |
| State rule checked under the lock | checkpoint resolves; root identity equals the attachment state | none | refusal names both roots (0806) | not applicable | rebuild at the right checkpoint |
| `:data` records appended (`lucene-import.mid-file-copy`) | fresh archive number above every physical name, at the head's generation | new archives only, unreachable from the head | store unchanged plus unreferenced archives (0806) | same (0806) | the next `froe compact` copies the live content into a fresh generation and retires every older archive |
| Definition rewritten, then the head published (`lucene-import.before-head-publish`, `lucene-import.after-head-publish-before-flush`; `reindex` set to `false` and `reindexCount` set to the file's value plus one, `corrupt` and `indexImportState` removed, as Oak's own cycle clears the one and the importer's definition-refresh step removes the other; `:disableIndexesOnNextCycle` set under the disabler's predicate as the importer's data step sets it, `:status` replaced with a fresh `uid` and `:version` from the fresh-index format-version rule, `:index-definition` replaced by the visible-state clone, the pre-existing hidden children dropped as Oak's own definition updater drops them when it installs the file's node wholesale, so a stored `:suggest-data` goes and is never re-imported), spine rewritten, head published (`compare_and_set_head`, then `flush`) | every file read back through `OakDirectory` byte-identically from the open session; the three apply-identity gates passed in `prepare` and the store was opened with `open_prepared` after the fingerprint and path-identity rechecks | `flush` seals and fsyncs the archive, syncs the directory, validates the session, then appends one journal line | old head before the append, new head after it (0806) | same (0806) | either head resolves |
| Applied-state verification (`lucene-import.before-applied-verification`) | head published | none | mismatch reported (0806) | not applicable | rerun the check |

### Task graph

```
0801 safety case (first)
0801 ─► 0802 table-of-contents specification and readers (also flips plan 0006's entry-count assertion)
0802 ─► 0803 dump, check and their documentation
0801 ─► 0804 OakDirectory writer
0801, 0802, 0803, 0804 ─► 0805 import operation (its round-trip test uses the dump)
0801, 0805 ─► 0806 cutpoints and guard evidence (covers the dump's guards too)
0803, 0805 ─► 0807 import command and its documentation
0803 ─► 0808 lucene_dump phase
0807, 0808 ─► 0809 lucene_import phase
0809 ─► 0810 suite wiring and run record
0806, 0810 ─► 0811 review (gated; records the interop runs)
```

No code lands before the safety case: 0802 and 0804 depend on 0801. Tasks 0803 and 0807 both extend the command line, so 0807 follows 0803; tasks 0802 and 0803 both register modules in `crates/froe/src/index/lucene/mod.rs` in that order; 0804 and 0805 both edit `crates/froe/src/writer/index/mod.rs` in that order. The plan's `ARCHITECTURE.md` is edited by 0801, 0806 and 0811 only; the interop phases record their runs through 0811. User-facing documentation lands with each capability (0803, 0807).

### Safety case

This plan is high-risk under `docs/high-risk-changes.md`, and its safety case lives here, as a section of this file, in the same place plans 0001, 0002 and 0004 keep theirs. Task 0801 writes the section before any code lands; task 0806 fills its fault and guard tables; task 0811 freezes the range, records the interoperability runs of tasks 0808 and 0809, the verification report and the review, and lists the known gaps. Plan 0010 keeps its own `### Safety case` section that cites this one for the `:data` write and the `OakDirectory` preconditions, plan 0007's for the publication boundaries, and adds only the rows the native reindex introduces, so this section is never edited after 0811 freezes it. Until 0801 lands, this paragraph is the placeholder.
