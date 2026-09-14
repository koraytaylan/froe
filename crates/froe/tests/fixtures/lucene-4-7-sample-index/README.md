# A real Lucene 4.7.2 `oakCodec` index

Five documents, one segment, compound. Written by Oak's own
`IndexWriter` — not by froe — so froe's readers are held to bytes Oak
produced rather than to bytes froe produced.

## How it was generated

Inside the pinned Sling image, through this repository's judge:

```console
$ cargo test -p froe-cli --features interop --release -- \
      --ignored --test-threads=1 judge_smoke     # compiles the judge
$ podman run --rm --user 0 --entrypoint /bin/sh \
      -v <judge-classes>:/classes:ro -v <output>:/out:rw \
      docker.io/apache/sling@sha256:8722cd66ae0758e50784ac21df836c8f8d9e443d105e1a4292a4cb7f810a8cc9 \
      -c 'CP=/classes:$(find /opt -name "*.jar" | tr "\n" ":"); \
          java -cp "$CP" LuceneJudge sample-index /out'
```

`LuceneJudge.sampleIndex` builds a `lucene` definition naming
`codec = oakCodec`, asks Oak for the `IndexWriterConfig` that definition
selects, and **refuses unless Oak chose `oakCodec`** — so a sample that did
not exercise the codec froe has to read is never captured. It writes five
documents and does not `forceMerge`: merging would rewrite `_0` into `_1`
and leave the merged segment uncompounded, which is a different file set and
a worse sample.

## What the judge reported when this was captured

```console
$ java -cp "$CP" LuceneJudge numdocs /out
5
```

`lucene_segments_tests.rs` pins that number. If froe's reader ever computes
something else from these bytes, the difference is froe's.

## The files

| File | Bytes | What it is |
| --- | --- | --- |
| `segments_1` | 81 | the commit file, generation 1 |
| `segments.gen` | 20 | the generation hint |
| `_0.si` | 225 | the segment descriptor |
| `_0.cfe` | 161 | the compound file's table of contents |
| `_0.cfs` | 711 | the compound file's data |

The whole directory is 1,198 bytes. The interop fixture's own `_0.cfs` is
1.9 MB and is deliberately **not** committed; this is a purpose-built sample
small enough to live in the repository.
