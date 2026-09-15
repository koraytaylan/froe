# Plan 0006 — Index Model and Inventory — ✅ Done

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

- **Status:** ✅ Done.
- **Goal:** give froe a specified, tested, read-only understanding of Oak's index definitions, async lanes, status nodes and index storage, expose it as `froe index list`, `froe index definitions` and `froe index check`, and stand up the Oak-side judge the later plans' evidence depends on.
- **Root cause:** froe ports oak-run's segment-store half and none of its `index` half; nothing in the crate models `/oak:index`, `/:async`, `:index`, `:data` or `:status`, and the interop suite has no way to ask Oak what an index contains.
- **Approach:** specification first (three analysis documents cited to the pinned Java), then Java-semantics primitives pinned by JDK-generated vectors, then the model and readers, then the inventory and the CLI, then the judge and the `index_inventory` phase against the real Sling store.
- **Progress:** 17/17 tasks done; 0 blocked; 0 dropped.
- **Integration:** `in progress`; run —; base `develop` @ `314b9c704fef73636d40f3e7ec5ff2c839aa1870`; validation base —; mode —; final integration —.
- **Exceptions:** task 0612's chain-to-completion-sentinel clause is **verified**. The chain ran to `all interop phases passed` on 2026-09-15 — every phase from `generate` through `recover`, 905 seconds — against the pinned image, and that run is the evidence the clause asked for. It had been unverified until then: earlier attempts were interrupted at three different phases and once by another workload on this shared host binding port 8080, which was read as a memory watchdog at the time and was more probably that same co-tenant, since one attempt during this review died with its judge container SIGTERMed in the same second another workload's containers were removed. A container start now waits out a port that is still held.
- **Outcome:** froe reads every index definition, async lane, status node and index-data structure a real Oak 1.90.0 store holds, lists and checks them read-only, dumps definitions in oak-run's JSON form, and the interop suite gains an Oak-side judge that proves the inventory against Oak's own printers.

_Last updated: 2026-09-14, against `develop` @ `1f6e4c7`._
