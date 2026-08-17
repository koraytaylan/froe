# froe-cli

The `froe` command line: inspect, export from, and maintain Apache
Jackrabbit Oak `segment-tar` ("TarMK") repositories, as used by Apache
Jackrabbit Oak and Adobe Experience Manager.

Built on the [`froe`](https://crates.io/crates/froe) core library.
Inspection, export, and diagnostic commands (`summary`, `tree`,
`export`, `check`, `difference`, `history`, `search-nodes`, …) are
read-only and safe against a live repository (like Oak, archives are
memory-mapped under the store's never-modify-in-place file protocol; a
process mutating archives outside that protocol would disturb froe and a
running Oak instance alike). Mutating maintenance commands (`compact`,
`compact`, `backup`, `restore`, `recover-journal`, and checkpoint
mutation) take the exclusive repository lock, so they must run against a
*stopped* repository and ask for confirmation first. These commands require a
Unix operating-system entropy source and refuse to run on Windows; the
read-only commands work everywhere. If `repo.lock` is absent, every mutating
command also requires same-directory hard-link and durable directory-fsync
support to publish the new mode-`0600` lock safely; an unsupported filesystem
fails instead of using an unsafe fallback.

**Maintenance is verified against a real Oak instance**: every mutating
command round-trips through Apache Jackrabbit Oak `oak-segment-tar` 1.90.0 in
the interoperability suite — Oak writes the store, froe commits, checkpoints,
compacts (full and tail), removes checkpoints, cleans up, backs up, restores
and recovers the journal, and Oak then boots against each result and serves a
byte-identical content tree without logging any of its own repair messages.
Still unverified against a live instance: `store.version=1` stores, external
blob stores, native macOS or Windows execution, and Adobe AEM itself, which
ships its own Oak build. Maintenance still requires a stopped repository, and
keeping a copy before a destructive operation on irreplaceable data remains
ordinary prudence. `froe compact --dry-run` is strictly read-only: it neither
creates nor acquires `repo.lock`.

Two low-level interoperability diagnostics mirror Oak's segment tooling:

```console
froe segment /path/to/segmentstore SEGMENT-UUID --hex
froe debug /path/to/segmentstore data00000a.tar [data00001a.tar ...]
```

The first prints an Oak `SegmentDump`-compatible header, hexadecimal record
table, and raw bytes. Structurally corrupt data still produces the header, a
terminal-safe parse diagnostic, and the complete raw hex dump; only a segment
over the format's 256 KiB limit is refused before rendering. It also safely
escapes segment-info terminal controls and names unknown record types. The
second walks the current head from the super-root (so live JCR content appears
below `/root/`), attributes node, template, stored-property, and binary
block records to each named active archive, and prints its segment graph.
Archive arguments are canonical `data*.tar`
file names in the repository, not arbitrary paths. A missing archive is
reported and skipped, as is a superseded archive that is no longer active;
the graph always prints one row per segment, including empty data and bulk
rows. A valid stored graph is trusted; an absent or corrupt optional graph is
reconstructed from the archive's data-segment reference tables without
discarding independently derived path attribution.
STRING/STRINGS displays use Oak's first-value-only preview of 60 Java UTF-16
code units and full UTF-16 length (with an empty STRINGS value left unquoted).
Other scalar and array values render in full; a report that cannot retain the
full value fails with a typed budget error instead of printing an invented
omission summary. Scalar external binaries render Oak's `{-1 bytes}` without
resolving a long external identifier, and binary arrays retain their count.
By default each archive report is limited to 250,000 attribution rows, 64 MiB
of path/name/value text, 100,000,000 logical work units, 250,000 children
materialized from any one node, and 16 MiB of stored child/template-name bytes
per node, plus 250,000 pending child visits, 250,000 graph rows, and 1,000,000
graph edges. A refusal reports the configured and attempted bound. Each archive
argument currently performs its own bounded head traversal;
the command does not batch several archive names into one globally budgeted
walk.
Both commands remain strictly read-only and never create or acquire
`repo.lock`.

Licensed under the Apache License, Version 2.0.
