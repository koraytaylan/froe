# Analysis documents

The byte-exact specifications this workspace was implemented from,
extracted from the Apache Jackrabbit Oak `oak-segment-tar` Java sources
and the official storage documentation, then adversarially cross-verified
against the Java code. They are kept as contributor reference material;
the distilled format description lives in
[`../storage-format.md`](../storage-format.md) and the feature analysis
in [`../oak-segment-tar-feature-map.md`](../oak-segment-tar-feature-map.md).

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
