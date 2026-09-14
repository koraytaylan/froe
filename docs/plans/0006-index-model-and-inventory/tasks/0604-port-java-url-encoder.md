---
id: port-java-url-encoder
title: Port Java's URL Encoder For Property Index Keys
workstream: "0006"
kind: task
depends_on: [specify-property-index-storage]
gated: false
touches:
  - crates/froe/src/java/url_encoder.rs
  - crates/froe/src/java/mod.rs
  - crates/froe/tests/fixtures/java-url-encoder-vectors.tsv
status: done
merged_as: "30f8c8813b041870e962f839ddd5bc6d72038cb2"
---
# Port Java's URL Encoder For Property Index Keys

Implement `java::url_encode` (the function `url_encode` in `java/url_encoder.rs`, reached through the module's glob re-export like the functions of `java::numbers`), the exact semantics of Java's URL encoding of a string as UTF-8, which is the last step of property-index key derivation: alphanumerics and `.`, `-`, `*`, `_` pass through, a space becomes `+`, every other character is emitted as `%XX` per UTF-8 byte with upper-case hexadecimal digits, a surrogate pair encodes as the four bytes of its code point, and a lone surrogate — which Oak's 100-unit truncation can produce — encodes as `%3F`. The function operates on UTF-16 code units because the Java it mirrors does; the caller, task 0606's `keys_for_property`, truncates before encoding. The `froe::java` module is crate-private (`mod java;` in `lib.rs`, `pub(crate)` re-exports in `java/mod.rs`), and this task keeps it so: the encoder is `pub(crate)`, its only callers are inside the crate, and its tests are unit tests in the module — the `java/mod.rs` doc, which says the tests live with the callers, is rewritten to distinguish caller-side tests for the two file-format helpers from in-module vector replays for the Java-semantics primitives the index plans add — so `lib.rs` stays in task 0605's footprint alone.

**Steps:**

1. Add `crates/froe/src/java/url_encoder.rs` beside `numbers.rs`, `properties.rs` and `split.rs`, register it in `java/mod.rs` with a `pub(crate)` re-export, with a module comment recording that it reproduces Java's URL encoding as UTF-8 and pointing at `docs/analysis/index-property-storage.md` for the key derivation that calls it, and an input type that makes the UTF-16 contract explicit (an iterator of code units, so a lone surrogate is representable).
2. Generate the vector file with the pinned image's JDK — `podman run --rm --entrypoint jshell` over a fixed list of inputs covering every character class, multi-byte sequences, an astral code point, an empty string, and a lone surrogate — and commit it as `crates/froe/tests/fixtures/java-url-encoder-vectors.tsv` with the generating script recorded in its header comment.
3. Unit tests in the module: one test per character class named for the property it pins, and one test that replays every committed vector through `include_str!`.
4. Run the stable host gate.

- **Done when:** every committed vector round-trips through `java::url_encode`, the lone-surrogate vector yields `%3F`, and the stable host gate passes.
