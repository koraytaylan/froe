# froe-cli

The `froe` command line: inspect, extract from, and maintain Apache
Jackrabbit Oak `segment-tar` ("TarMK") repositories, as used by Apache
Jackrabbit Oak and Adobe Experience Manager.

Built on the [`froe`](https://crates.io/crates/froe) core library.
Inspection, extraction, and diagnostic commands (`summary`, `tree`,
`extract`, `check`, `difference`, `history`, `search-nodes`, …) are
read-only and safe against a live repository (like Oak, archives are
memory-mapped under the store's never-modify-in-place file protocol; a
process mutating archives outside that protocol would disturb froe and a
running Oak instance alike). Maintenance commands
(`compact`, `backup`, `restore`, `recover-journal`, `checkpoint`) take the
exclusive repository lock and modify the store, so they must run against a
*stopped* repository; each asks for confirmation first. The maintenance
commands require a Unix operating system entropy source and refuse to run
on Windows; the read-only commands work everywhere.

Licensed under the Apache License, Version 2.0.
