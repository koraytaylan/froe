# Plan 0006 — Index Model and Inventory — 🚧 In progress

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** 🚧 In progress.
- **Goal:** give froe a specified, tested, read-only understanding of Oak's index definitions, async lanes, status nodes and index storage, expose it as `froe index list`, `froe index definitions` and `froe index check`, and stand up the Oak-side judge the later plans' evidence depends on.
- **Root cause:** froe ports oak-run's segment-store half and none of its `index` half; nothing in the crate models `/oak:index`, `/:async`, `:index`, `:data` or `:status`, and the interop suite has no way to ask Oak what an index contains.
- **Approach:** specification first (three analysis documents cited to the pinned Java), then Java-semantics primitives pinned by JDK-generated vectors, then the model and readers, then the inventory and the CLI, then the judge and the `index_inventory` phase against the real Sling store.
- **Progress:** 10/17 tasks done; 0 blocked; 0 dropped.
- **Integration:** `in progress`; run —; base `develop` @ `314b9c704fef73636d40f3e7ec5ff2c839aa1870`; validation base —; mode —; final integration —.
- **Exceptions:** — (coordinator-owned blocked/dropped reasons are recorded here).
- **Outcome:** froe reads every index definition, async lane, status node and index-data structure a real Oak 1.90.0 store holds, lists and checks them read-only, dumps definitions in oak-run's JSON form, and the interop suite gains an Oak-side judge that proves the inventory against Oak's own printers.

_Last updated: 2026-09-14, against `develop` @ `019489b`._
