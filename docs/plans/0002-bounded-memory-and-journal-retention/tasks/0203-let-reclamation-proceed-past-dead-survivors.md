---
id: let-reclamation-proceed-past-dead-survivors
title: Let Reclamation Proceed Past Dead Survivors
workstream: "0002"
kind: task
depends_on: [bound-every-cache-and-memo]
gated: false
touches:
  - crates/froe/src/store.rs
  - crates/froe/src/writer/cleanup.rs
  - crates/froe/src/writer/store_writer.rs
  - docs/cleanup.md
status: done
merged_as: "f4a3f4ff91759b0a76ae73c53c91dd8902d6e076"
---
# Let Reclamation Proceed Past Dead Survivors

The prospective-plan check refused any survivor that referenced a planned removal, including survivors that were themselves dead and about to be removed, so a store with dead segments outliving the sweep could never be reclaimed. The check was loosened on the default `--task segments` path so that only live survivors must not dangle, and `docs/cleanup.md` states exactly which surviving references the gate still rejects. Landed in `a036482` and `f4a3f4f` (2026-08-14).

**Steps:**

1. Split `validate_prospective_segment_plan` into the retained-root half (unchanged) and the survivor half, which now ignores survivors the plan removes.
2. Pin the loosening with `a_dead_survivor_pointing_at_a_removed_segment_is_handled`, which fails when the stricter check is restored.
3. State the accepted and rejected reference shapes in `docs/cleanup.md`.

- **Done when:** a synthetic store with a dead survivor pointing at removed garbage is reclaimed on the default task set, and a live survivor pointing at a planned removal is still refused. Met at `f4a3f4f`.
