# Analysis documents

The byte-exact specifications this workspace was implemented from,
extracted from the Apache Jackrabbit Oak `oak-segment-tar` Java sources
and the official storage documentation, then adversarially cross-verified
against the Java code. They are kept as contributor reference material;
the distilled format description lives in
[`../storage-format.md`](../storage-format.md) and the feature analysis
in [`../oak-segment-tar-feature-map.md`](../oak-segment-tar-feature-map.md).

## The Java these documents were read from

These documents are *derived*. The prime directive in
[`../../CONTRIBUTING.md`](../../CONTRIBUTING.md) makes the Java implementation
the ground truth, and that includes being ground truth over this directory: a
claim about Oak behaviour is settled by reading the named Java method, not by
re-reading the summary of it here.

The specifications were extracted from and cross-verified against
`https://github.com/apache/jackrabbit-oak.git` at commit
`4984c4cf26a7ca58ae9ce12c63190b7f492bda78` (2026-08-07), where the
`oak-segment-tar` module declares version `2.5-SNAPSHOT`. That is a trunk
snapshot rather than a release tag, so pin the commit, not the version, when
recording evidence. A blob-filtered clone is enough to read the sources and is
a fraction of the size:

```console
$ git clone --filter=blob:none https://github.com/apache/jackrabbit-oak.git
$ git -C jackrabbit-oak checkout 4984c4cf26a7ca58ae9ce12c63190b7f492bda78
```

Not all relevant behaviour lives in `oak-segment-tar`. Property rendering and
value conversion — the semantics behind what a diagnostic prints and when it
fails — are in `oak-store-spi`
(`plugins/memory/AbstractPropertyState.java`, `plugins/value/Conversions.java`),
and node-state rendering is there too. Searching only the segment module will
miss them.

When extending a specification, cite the Java file and method, and record the
commit you read if it differs from the one above; a citation that names no
revision cannot be re-checked once trunk moves.

## Stating the scope of a documented behaviour

Where a behaviour is specific to one property type, format version, or code
path, say what it does **not** cover. A correct sentence about a narrow case
invites generalisation to the whole class, and that generalisation then gets
implemented or asserted in review.

The concrete example this convention comes from: Oak renders an unavailable or
corrupt `BINARY` as `{-1 bytes}` because `AbstractPropertyState.getBinarySize`
catches the exception. That is BINARY-only. Every other property type renders
through `property.getValue(type)` with no exception handling, so a malformed
`LONG` or `DOUBLE` propagates a `NumberFormatException` out of
`Conversions.toLong`/`toDouble` and aborts the command. A note that records only
the `{-1 bytes}` behaviour reads as though Oak tolerates every malformed scalar,
which it does not.

Read-path specifications:

| Document | Subsystem |
| --- | --- |
| [`tar-layer.md`](tar-layer.md) | Archive files: entries, index, graph, binary references, recovery |
| [`segment-layer.md`](segment-layer.md) | Segment headers, reference tables, record addressing |
| [`record-layer.md`](record-layer.md) | Value, list, and map record encodings |
| [`node-layer.md`](node-layer.md) | Node, template, and property records; super-root and checkpoints |
| [`filestore-layer.md`](filestore-layer.md) | Repository directory, journal, manifest, locking, opening |
| [`tooling-inventory.md`](tooling-inventory.md) | Complete feature inventory of `oak-segment-tar` |
| [`read-tooling.md`](read-tooling.md) | Check, diff, revisions, history, search, debug tools |
| [`official-documentation.md`](official-documentation.md) | Distillation of the official Oak format documentation, as an independent witness |

Write-path specifications:

| Document | Subsystem |
| --- | --- |
| [`write-record-writers.md`](write-record-writers.md) | Record serialization for every record kind |
| [`write-segment-buffer.md`](write-segment-buffer.md) | Building segment buffers: header, tables, flush, segment info |
| [`write-tar-archives.md`](write-tar-archives.md) | Writing archives: headers, trailers, rotation, durability |
| [`write-filestore.md`](write-filestore.md) | The read-write store lifecycle, locking, and durability ordering |
| [`write-compaction.md`](write-compaction.md) | Full and tail compaction, generation arithmetic, offline tool |
| [`write-cleanup.md`](write-cleanup.md) | Mark-and-sweep cleanup and reclaim predicates |
| [`write-commit-path.md`](write-commit-path.md) | Node builder, merge, and checkpoint operations |
| [`write-backup-restore-recovery.md`](write-backup-restore-recovery.md) | Backup, restore, and journal recovery |

Facts in these documents cite the Java file and constant they came from.
Where the official documentation and the code disagree, the code is
authoritative and the disagreement is called out.
