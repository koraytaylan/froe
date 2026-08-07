# froe-cli

The `froe` command line: inspect and extract data from Apache Jackrabbit Oak
`segment-tar` ("TarMK") repositories, as used by Apache Jackrabbit Oak and
Adobe Experience Manager.

Built on the [`froe`](https://crates.io/crates/froe) core library. All
commands are read-only: the repository lock is never taken and no file is
ever modified.

Licensed under the Apache License, Version 2.0.
