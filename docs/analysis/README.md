# Analysis documents

The byte-exact specifications this workspace was implemented from,
extracted from the Apache Jackrabbit Oak `oak-segment-tar` Java sources
and the official storage documentation, then adversarially cross-verified
against the Java code. They are kept as contributor reference material;
the distilled format description lives in
[`../storage-format.md`](../storage-format.md) and the feature analysis
in [`../oak-segment-tar-feature-map.md`](../oak-segment-tar-feature-map.md).

| Document | Subsystem |
| --- | --- |
| [`tar-layer.md`](tar-layer.md) | Archive files: entries, index, graph, binary references, recovery |
| [`segment-layer.md`](segment-layer.md) | Segment headers, reference tables, record addressing |
| [`record-layer.md`](record-layer.md) | Value, list, and map record encodings |
| [`node-layer.md`](node-layer.md) | Node, template, and property records; super-root and checkpoints |
| [`filestore-layer.md`](filestore-layer.md) | Repository directory, journal, manifest, locking, opening |
| [`tooling-inventory.md`](tooling-inventory.md) | Complete feature inventory of `oak-segment-tar` |
| [`official-documentation.md`](official-documentation.md) | Distillation of the official Oak format documentation, as an independent witness |

Facts in these documents cite the Java file and constant they came from.
Where the official documentation and the code disagree, the code is
authoritative and the disagreement is called out.
