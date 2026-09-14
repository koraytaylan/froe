# Scope — Plan 0009

> A dependency-free Rust writer for the Lucene 4.7.2 index format Oak selects as `oakCodec`: single-segment indexes that Lucene's own index checker accepts and that Lucene reads back term for term, posting for posting, value for value, identically to an index it built itself from the same documents.

Oak selects `oakCodec` for a definition only when it is fulltext-enabled — a rule with node aggregates or a fulltext-enabled property — or when an explicit `codec` property names it; every other Lucene definition is written with Lucene's default `Lucene46` codec, whose compressed stored fields and per-field postings and doc-values formats this plan does not write. The writer is therefore the writer for fulltext-enabled definitions — AEM's `cqPageLucene`, `damAssetLucene` and the default `lucene` shape — and plan 0010 refuses the rest by name.

## Why this plan

Everything before this point moves Lucene index data around; nothing produces it. Producing it is the difference between "froe can install an index someone built with a JVM" and "froe rebuilds the index" — the second half of oak-run's `--reindex`, and the capability an operator on a stopped AEM instance actually wants. Oak has pinned `oak-lucene` to Lucene 4.7.2 for a decade and vendors its sources at the pinned commit as version `4.7.2-oak2`, so the format is frozen, fully specified by the Java in that commit, and reachable by the same citation discipline this repository applies to `segment-tar`: task 0901 records it in `docs/analysis/lucene-4-7-codec.md`, and this plan states the format in froe's terms.

The plan is deliberately separated from Oak's document semantics (plan 0010): this writer takes documents whose fields are already tokenized and typed, and writes bytes. Its correctness oracle is therefore purely Lucene: the judge builds the same documents with Lucene inside the pinned image and both indexes are enumerated and compared. That keeps the two hard problems — the codec and the analyzer — from hiding each other's defects.

Risk is managed by a feasibility gate: the specification task ends with a recorded go/no-go verdict, and the first implementation task is gated on the maintainer's approval of that verdict.

## In scope

- **Specification** of every file a fresh index carries under `oakCodec` — `segments.gen`, `segments_N`, `.si`, `.fnm`, the uncompressed `Lucene40` stored fields `.fdt`/`.fdx`, the block-tree terms dictionary `.tim`/`.tip`, the `Lucene41` postings `.doc`/`.pos`/`.pay` with their frame-of-reference blocks and skip lists, the `Lucene45` doc values `.dvd`/`.dvm`, the `Lucene42` norms `.nvd`/`.nvm`, and the compound file `.cfs`/`.cfe` — plus the primitives beneath them: the output encodings, codec headers, packed integers, the block-packed and monotonic block-packed writers, the transducer byte format, and the norm the default similarity computes with its small-float quantization.
- **The writer**, producing one segment per index, from an in-memory document model with bounded-memory spilling of postings.
- **Field capabilities Oak's fields need**: `DOCS_ONLY`, `DOCS_AND_FREQS_AND_POSITIONS` and `…_AND_OFFSETS` index options, stored string and binary fields, norms present or omitted per field, `NUMERIC`, `SORTED` and `SORTED_SET` doc values, field boosts folded into norms.
- **Conformance evidence**: Lucene's own index checker clean; enumeration equality against a Lucene-built index; the interop phase `lucene_writer_conformance`.

## Out of scope

- Term vectors, payloads, live-document deletions, multiple segments and merging, `.del` files: Oak's fields never store term vectors or payloads, and a fresh offline build has nothing to delete or merge. The field infos still record these flags as false and the codec names remain those `oakCodec` declares.
- The `Lucene46` composition — LZ4-compressed stored fields in place of the uncompressed `Lucene40` ones, and per-field selection of the postings and doc-values formats — that Oak writes for definitions that are not fulltext-enabled. Its readers are accepted by plan 0008's check; writing it is a later extension.
- Tokenization, numeric term encoding, facets configuration and every other document-level rule (plan 0010).
- Reading postings, doc values or stored fields back in Rust. Reading the table of contents is plan 0008; a Rust reader for the rest is not required by any froe operation and is not built speculatively.
