# froe

Reader for Apache Jackrabbit Oak `segment-tar` ("TarMK") repositories — the
storage format used by Apache Jackrabbit Oak and Adobe Experience Manager.

This crate opens a segment store directly from disk, resolves the current
head state from the journal, and exposes the content tree for traversal and
extraction — without a running Oak instance. It is read-only by design: it
never takes the repository lock and never modifies any file, so it is safe to
point at a live repository or a backup.

See the workspace repository for the complete feature map and storage format
documentation, and the `froe-cli` crate for the command-line interface.

Licensed under the Apache License, Version 2.0.
