# froe

A Rust implementation of Apache Jackrabbit Oak's `segment-tar` ("TarMK")
storage format — the repository format used by Apache Jackrabbit Oak and
Adobe Experience Manager.

This crate opens a segment store directly from disk, without a running Oak
instance. The reading API ([`store`], [`content`], [`tooling`]) is read-only
and safe against a live repository: it neither takes the lock nor writes.
Like Oak, it memory-maps archives and relies on the store's
never-modify-in-place file protocol; an external process that truncates or
rewrites an archive would disturb both froe and a running Oak instance.

The mutating writing API ([`writer`]) covers commits, checkpoints, applying
`cleanup`, compaction, backup, restore, and journal recovery. It takes the
exclusive repository lock and produces stores byte-for-byte compatible with
Oak (apart from the documented extreme-subnormal `double_to_text` rendering
residue), so a subsequent AEM start consumes the result cleanly. Planning
`cleanup` is the read-only exception and never takes the lock. Run mutations
only against a *stopped* repository. The writer currently requires a Unix
operating-system entropy source and therefore refuses to open on Windows. If
`repo.lock` is absent, opening any writer also requires same-directory
hard-link and durable directory-fsync support to publish the new mode-`0600`
lock safely; an unsupported filesystem fails closed.

**The writing API is beta**: it is verified against byte-exact
specifications extracted from the Oak sources and an extensive test
suite, but has not yet been validated end-to-end against stores produced
by — or consumed by — a real Oak/AEM instance. Until that
interoperability round-trip lands, take a copy of your repository before
writing to data you care about. The reading API carries no such caveat.

See the workspace repository for the complete feature map and storage
format documentation, including the cleanup safety guide, and the `froe-cli`
crate for the command-line interface.

[`store`]: https://docs.rs/froe/latest/froe/store/
[`content`]: https://docs.rs/froe/latest/froe/content/
[`tooling`]: https://docs.rs/froe/latest/froe/tooling/
[`writer`]: https://docs.rs/froe/latest/froe/writer/

Licensed under the Apache License, Version 2.0.
