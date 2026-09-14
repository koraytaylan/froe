---
id: render-definitions-json
title: Render Index Definitions In oak-run's JSON Form
workstream: "0006"
kind: task
depends_on: [model-index-definitions, specify-index-definitions-and-lanes]
gated: false
touches:
  - crates/froe/src/index/definitions_json.rs
  - crates/froe/tests/index_definitions_json_tests.rs
status: done
merged_as: "019489b3cd63e6d7fe4f4e331f78a4183fc7d8ea"
---
# Render Index Definitions In oak-run's JSON Form

Implement the rendering oak-run's definition printer produces, as task 0601's specification records it: one JSON object keyed by index path in the order task 0605's `index_paths` yields — the printer's own order — or, when the caller supplied an explicit selection, in the order the caller gave, with `index_paths` not consulted and so its nodetype precondition not evaluated, which is how oak-run behaves when it is given `--index-paths`, stored child order or the node-type index's mirror walk, never a sort — each value the definition node serialized under the filter `{"properties":["*","-:childOrder"],"nodes":["*","-:*"]}`: every property except `:childOrder`, hidden properties such as `:version` and `:originalType` included (the fixture's `lucene` definition carries `:version`, so a renderer that skipped hidden properties would fail 0615's byte comparison), then the children `:childOrder` names when that property exists and every visible child in stored order otherwise, recursively, hidden children omitted — with a type code on every non-string value, base64 for binaries, and Oak's own string escaping and pretty-printed layout. This is the file `oak-run index --index-definitions-file` and Oak's own definition updater consume, so the output must be a byte-for-byte plausible input to them: the test oracle is the judge running Oak's own printer over the same store (task 0615).

**Steps:**

1. Implement the node serializer against `docs/analysis/index-definitions.md`: the child-order rule above; values rendered with booleans and longs unquoted, doubles in Java's own textual form with `dou:` codes for the non-finite ones, a `str:` prefix on any plain string the type-code splitter recognizes a prefix in — one starting with `:blobId:`, or of length four or more with `:` at index 3, the code known or not, so `jcr:title` becomes `str:jcr:title` — and `[0]:<Type>` for empty typed arrays of every type except `STRING`, whose empty array renders `[]`; the type code for every other type; the base64 form for a binary and its size refusal, a binary at or above `oak.serializer.maxBlobSize`, 1 MiB by default, being a typed error because Oak refuses it rather than encoding it; and Oak's two-phase string escaping implemented here — the first phase appends the string raw when no character trips the escape scan, which looks for `"`, `\`, characters below U+0020 and high surrogates while letting DEL and the C1 range pass raw, and otherwise the second phase emits `\"` and `\\` first, then `\b \f \n \r \t`, lower-case `\uXXXX` for other controls and for either half of an unpaired surrogate, and every other character raw — because froe's only JSON escaper lives in `froe-export`, which depends on this crate; reuse `double_to_text` from `content/property.rs`.
2. Implement the pretty-printed layout exactly as Oak's builder produces it — a two-space indent unit, one member per line for objects, `{}` inline for an object with no members, arrays entirely on one line with `", "` between elements, and a space after every other token, the `:` rule included — so a diff against Oak's output is line-for-line.
3. Provide `render(definitions, filter)` where the filter is the printer's default, plus the variant an oak-run out-of-band build dumps with, which drops `:index-definition`, `:data` and `:suggest-data` and keeps `:status`; plan 0008's dump writes the variant so its output is an `index-definitions.json` oak-run's importer accepts.
4. Tests over the synthetic definition store comparing against hand-written expected JSON for every type code, an empty `String[]` and an empty `Name[]`, a multi-valued property rendered inline, an empty visible child rendered `{}`, a binary property, a binary at the base64 size boundary refused with the typed error and one byte inside it encoded, a nested child, a hidden property on the definition node, a control character, a value holding a double quote and a backslash (the two escapes the second phase emits before the named five), a non-ASCII string, a `STRING` shaped like `jcr:title` (fourth character `:`, rendered `str:jcr:title`), a lone low surrogate with and without a trigger character, a definition with `:childOrder` naming only some children, a store whose definitions are deliberately not in name order, and two renderings of one store compared byte for byte.

- **Done when:** the hand-written expectations match, `froe digest`-style determinism holds across two renderings, and the stable host gate passes; the byte comparison against Oak's printer is task 0615's acceptance.
