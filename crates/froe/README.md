# froe

A Rust implementation of Apache Jackrabbit Oak's `segment-tar` ("TarMK")
storage format — the repository format used by Apache Jackrabbit Oak and
Adobe Experience Manager.

This crate opens a segment store directly from disk, without a running Oak
instance. The reading API ([`store`], [`content`], [`tooling`]) is
read-only and safe against a live repository — it never takes the lock and
never writes. Like Oak itself it memory-maps archives, relying on the
store's file protocol (existing archive bytes are never modified in
place); a process mutating archives outside that protocol would disturb
froe and a running Oak instance alike. The writing API ([`writer`]) — commits, checkpoints,
compaction, backup, restore, journal recovery — takes the exclusive
repository lock and produces stores byte-for-byte compatible with what Oak
writes, so a subsequent AEM start consumes the result cleanly; run it only
against a *stopped* repository.

See the workspace repository for the complete feature map and storage
format documentation, and the `froe-cli` crate for the command-line
interface.

[`store`]: https://docs.rs/froe/latest/froe/store/
[`content`]: https://docs.rs/froe/latest/froe/content/
[`tooling`]: https://docs.rs/froe/latest/froe/tooling/
[`writer`]: https://docs.rs/froe/latest/froe/writer/

Licensed under the Apache License, Version 2.0.
