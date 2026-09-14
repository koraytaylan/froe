---
id: enrich-the-fixture-for-indexes
title: Enrich The Generated Fixture For Index Coverage
workstream: "0006"
kind: task
depends_on: []
gated: false
touches:
  - crates/froe-cli/tests/interop/sling.rs
  - crates/froe-cli/tests/interop/phase_baseline.rs
status: planned
merged_as: ""
---
# Enrich The Generated Fixture For Index Coverage

The store `generate` produces already holds every index type Sling ships, but three storage shapes the later plans must rebuild are absent or thin: the reference index holds no references at all, the unique indexes cover only Sling's own system nodes, and no property index in the fixture was rebuilt by Oak from existing content, which is the exact oracle shape plan 0007 compares against. Add content through Sling itself — never through froe — so the shapes are authentically Oak-written.

**Steps:**

1. In `populate_content`: a `mix:referenceable` node and a sibling holding a `REFERENCE` property to it (`@TypeHint=Reference`) and a `WEAKREFERENCE` property (`@TypeHint=WeakReference`), so `/oak:index/reference/:references` and `:weakreferences` both gain an entry; a second referenceable node whose reference lives under `/jcr:system/jcr:versionStorage` is out of reach through Sling and is documented as covered by the unit tests instead.
2. A group with two members through Sling's user manager servlet, so `repMembers` (`rep:members`, `declaringNodeTypes = rep:MemberReferences`) indexes a multi-valued property under a declared type.
3. A property index definition posted under `/oak:index` *after* the content exists, with `reindex=true` (`@TypeHint=Boolean`) and a `propertyNames` value the content actually carries (`jcr:title`, posted with `@TypeHint=Name[]` so the stored type is the multi-valued `NAMES` that Oak's own definitions use and that the query planner reads strictly; the editor would convert a `STRING`, while the planner reads a `STRING` or a single `NAME` as empty). Oak rebuilds it inside that same commit: the reindex test is true for the flag and for a new definition alike, under the default `oak.indexUpdate.ignoreReindexFlags=false`; collecting the editors clears the flag, increments `reindexCount` and registers the editor; and the cycle then runs that editor from the missing state to the head before the commit completes. The fixture therefore cannot carry a definition that is still flagged — a still-flagged definition, which tasks 0710 and 0711 need, comes from a synthetic store or from a writer helper on a copy — and `generate` asserts instead that Oak rebuilt it (`reindex = false`, `reindexCount = 1`, `:index` present and non-empty): an Oak-rebuilt property index whose rebuild happened in one synchronous cycle.
4. Assert every added shape exists in the extracted store through the `froe node` command and the content API — the suite's existing means, since this task depends on no reader of plan 0006 — before `generate` records the digest baseline, in `phase_baseline.rs` beside `generate`, in the same style as `fixtures.rs`'s `assert_cleanup_fixture_built`.

- **Done when:** `generate` passes and the extracted store's `/oak:index/reference` holds both hidden children with at least one entry each, the new definition reads `reindex = false` and `reindexCount = 1` with a non-empty `:index`, and every existing phase still passes against the enriched fixture, and the stable host gate passes (`--all-features`, so the interop suite is compiled and linted).
