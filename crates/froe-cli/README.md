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
applying `cleanup`, `backup`, `restore`, `recover-journal`, and checkpoint
mutation) take the exclusive repository lock, so they must run against a
*stopped* repository and ask for confirmation first. These commands require a
Unix operating-system entropy source and refuse to run on Windows; the
read-only commands work everywhere. If `repo.lock` is absent, every mutating
command also requires same-directory hard-link and durable directory-fsync
support to publish the new mode-`0600` lock safely; an unsupported filesystem
fails instead of using an unsafe fallback.

**Maintenance commands are beta**: the write path is verified against
byte-exact specifications extracted from the Oak sources and an extensive
test suite, but has not yet been validated end-to-end against stores
produced by — or consumed by — a real Oak/AEM instance. Until that
interoperability round-trip lands, take a copy of your repository before
running any maintenance command against data you care about. The
read-only commands carry no such caveat. `froe cleanup --dry-run` is also
strictly read-only: it neither creates nor acquires `repo.lock`.

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
bulk-block records to each named active archive, and prints its segment graph.
Archive arguments are canonical `data*.tar`
file names in the repository, not arbitrary paths. A missing archive is
reported and skipped, as is a superseded archive that is no longer active;
the graph always prints one row per segment, including empty data and bulk
rows. A valid stored graph is trusted; an absent or corrupt optional graph is
reconstructed from the archive's data-segment reference tables without
discarding independently derived path attribution.
Exceptionally large non-binary property displays are summarized rather than
materialized; normal STRING/STRINGS displays use Oak's fixed default of 60
Java characters (with an empty STRINGS value left unquoted, like Oak), while
binary bulk-block attribution still examines every block. Reports retain at
most 250,000 attribution rows and 64 MiB of path/name/value text; exceeding a
limit fails with the attempted and configured sizes instead of risking an
unbounded allocation. External binaries report that their size is unavailable
when no blob store is configured.
Both commands remain strictly read-only and never create or acquire
`repo.lock`.

Licensed under the Apache License, Version 2.0.
