# Plan 0010 — Lucene Offline Reindex — 🚧 In progress

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** 🚧 In progress.
- **Goal:** `froe index reindex` rebuilds Lucene definitions natively with Oak's document model, analyzer chain and numeric encodings reproduced exactly, installs the result with plan 0008's transport and bookkeeping, and a real Oak serves the same query results through it as through its own index.
- **Root cause:** plan 0009 writes valid indexes; only Oak's document semantics make them the indexes Oak expects, and those semantics — rules, analyzers, field types — are spread over `oak-search`, `oak-lucene` and `lucene-analyzers-common`.
- **Approach:** two specifications (documents, analysis), analyzers proved token-by-token against the image's classes, numeric encodings pinned by vectors, a document maker ported from Oak's own with typed refusals for unsupported features, the reindex operation on the established skeleton, the command's flags and documentation, an interop phase whose oracle is Oak's own reindex compared by enumeration plus live queries, and a frozen review; only fulltext-enabled definitions, the ones Oak writes with `oakCodec`, are in scope.
- **Progress:** 1/11 tasks done; 0 blocked; 0 dropped.
- **Integration:** `planned`; run —; base `develop` @ `314b9c704fef73636d40f3e7ec5ff2c839aa1870` plus plans 0006–0009 merged; validation base —; mode —; final integration —.
- **Exceptions:** Task 1002 found that the token-count filter's refusal of a limit below one, which its own text and task 1005's rely on, is a **later Lucene's** behaviour: in the pinned 4.7.2 a `maxFieldLength` of 0 silently empties every analyzed field. froe still refuses such a definition, but as its own choice rather than as a reproduction of an Oak refusal. (Coordinator-owned blocked/dropped reasons are recorded here.)
- **Outcome:** `froe index reindex` rebuilds Lucene indexes natively — Oak's document model, analyzer chain and numeric encodings reproduced exactly — and Oak serves the same query results from the froe-built index as from its own.

_Last updated: 2026-09-14, against `develop` @ `b0dbdd4`._
