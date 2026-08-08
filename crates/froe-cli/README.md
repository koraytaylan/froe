# froe-cli

The `froe` command line: inspect, extract from, and maintain Apache
Jackrabbit Oak `segment-tar` ("TarMK") repositories, as used by Apache
Jackrabbit Oak and Adobe Experience Manager.

Built on the [`froe`](https://crates.io/crates/froe) core library.
Inspection, extraction, and diagnostic commands (`summary`, `tree`,
`extract`, `check`, `difference`, `history`, `search-nodes`, …) are
read-only and safe against a live repository. Maintenance commands
(`compact`, `backup`, `restore`, `recover-journal`, `checkpoint`) take the
exclusive repository lock and modify the store, so they must run against a
*stopped* repository; each asks for confirmation first.

Licensed under the Apache License, Version 2.0.
