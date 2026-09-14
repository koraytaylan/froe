---
id: write-the-transport-safety-case
title: Write The Lucene Import Safety Case
workstream: "0008"
kind: task
depends_on: []
gated: false
touches:
  - docs/plans/0008-lucene-index-transport/ARCHITECTURE.md
status: done
merged_as: "b82594080f44a285a959847961c3f7ee6a2ea508"
---
# Write The Lucene Import Safety Case

The import publishes new bytes under a definition, replaces its hidden children and moves the head; the dump writes files outside the store and must be provably read-only inside it. Write it first, as the `### Safety case` section of this plan's `ARCHITECTURE.md`, in the established form, as the living record the plan's later tasks complete.

**Steps:**

1. Scope and retention: what survives (everything outside the selected definitions; every visible property and child of a selected definition other than `reindex`, `reindexCount`, `corrupt` and `indexImportState`, which the import rewrites or clears as Oak's reindex and importer do, and the hidden `:disableIndexesOnNextCycle`, written under the disabler's predicate as the importer's data step writes it; `/:async`; every checkpoint — the import releases none, a recorded departure from oak-run's importer, whose fourth step releases the one `indexer-info.properties` names, because the only checkpoint froe accepts is a lane's), what is replaced (`:data`, `:status`, `:index-definition`, `:suggest-data` removed), what the dump may write (only under `--output`, which must not lie inside the store directory).
2. Authoritative state: the lockless preview; `PreparedLuceneImport::prepare` running the repository-shape check and the two pre-lock apply-identity gates, taking the lock, repeating both, replanning, fingerprinting, certifying the archive number and running the metadata-source gate as plan 0007 does, and `apply` opening through `WritableRepository::open_prepared` after the fingerprint and path-identity rechecks; the facts rechecked — the directory's `indexer-info.properties` checkpoint resolving, the root-identity rule per selected definition, each definition being asynchronous, the definition's type and lane, the definitions file's drift verdict, the head.
3. The existing `### Mutation and publication order` section of `ARCHITECTURE.md` is the safety case's table, cited by its heading rather than copied; this task fills in the regression each row will get.
4. Interruption prefixes, observed outcomes (files imported with byte counts and read-back verification), resources (one file streamed at a time; the largest file bounds temporary memory at one buffer; the store grows by the index size in the fresh archives; the file count is a proxy for the copy's duration and exhaustion leaves a safe prefix — a copy that fails on `ENOSPC` returns a typed error naming the file and offset, with the store unchanged and only unreferenced records at the head's generation; nothing is reclaimed until a compaction run after every checkpoint referencing the old `:data` — each lane's, by construction — has been released).
5. Section stubs — the guards table with the exact header the guide prescribes, a fault table (cutpoint, fault model, named test, asserted prefix), the rest in the prose form the landed cases use — for guards, fault tests, interoperability, verification report, known gaps and review.

- **Done when:** the section exists with every subsection the guide names, the mutation table has a row per boundary under the guide's headings, each row names its future regression, and the read-only claim of the dump is stated as a testable invariant (file snapshot before and after), `git diff --check` is clean, and the commit body records the omitted gates.
