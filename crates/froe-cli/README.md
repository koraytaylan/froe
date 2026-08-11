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

Licensed under the Apache License, Version 2.0.
