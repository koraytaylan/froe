//! The transducer writer, against bytes computed by hand from the
//! specification.
//!
//! `docs/analysis/lucene-4-7-codec.md` §7.6. These are hand-computed rather
//! than captured from froe, because a captured expectation pins whatever the
//! writer happened to do. Every byte below is derived from the Java quoted
//! in that section, and the derivation is in the comment beside it.
//!
//! Task 0911's conformance phase closes the loop the other way: the judge
//! loads the same corpus with Lucene's own reader and enumerates it.

use std::fmt::Write as _;

use froe::index::lucene::codec::fst::FstBuilder;

/// The header every transducer opens with: magic, "FST", version 4.
///
/// `CODEC_MAGIC` is `0x3fd76c17`; the name is a string, so a `VInt` 3 then
/// `F S T`; the version is a big-endian `Int`.
const HEADER: &[u8] = &[
    0x3f, 0xd7, 0x6c, 0x17, // magic
    0x03, b'F', b'S', b'T', // name
    0x00, 0x00, 0x00, 0x04, // VERSION_VINT_TARGET
];

fn built(keys: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut builder = FstBuilder::new();
    for (key, output) in keys {
        builder.add(key, output).expect("add");
    }
    builder.finish().expect("finish")
}

fn assert_bytes(what: &str, produced: &[u8], expected: &[u8]) {
    if produced == expected {
        return;
    }
    let first = produced
        .iter()
        .zip(expected.iter())
        .position(|(ours, theirs)| ours != theirs);
    panic!(
        "{what}: froe wrote {} bytes, the specification says {}{}\n  froe: {}\n  spec: {}",
        produced.len(),
        expected.len(),
        match first {
            Some(at) => format!(
                "; first difference at byte {at}: {:#04x} against {:#04x}",
                produced[at], expected[at]
            ),
            None => String::new(),
        },
        hex(produced),
        hex(expected)
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut rendered, byte| {
        let _ = write!(rendered, "{byte:02x}");
        rendered
    })
}

/// The minimal serializable shape: only the empty key.
///
/// The root has no arcs and is final, so `compile_node` answers
/// `FINAL_END_NODE`; `FST.finish` then forces the start node to 0 because
/// an empty output is present. The store is empty.
#[test]
fn a_transducer_carrying_only_the_empty_key() {
    let produced = built(&[(b"", b"xy")]);

    let mut expected = HEADER.to_vec();
    expected.push(0x00); // unpacked
    expected.push(0x01); // has an empty output
    // The empty output is `write(BytesRef)` — vint 2, then `x y` — and is
    // then reversed: `79 78 02`. Its own length goes in front.
    expected.extend_from_slice(&[0x03, 0x79, 0x78, 0x02]);
    expected.push(0x00); // BYTE1
    expected.push(0x00); // startNode, forced to 0
    expected.push(0x00); // nodeCount
    expected.push(0x00); // arcCount
    expected.push(0x00); // arcWithOutputCount
    // One byte, not none: the store opens with the pad every transducer
    // carries, which is the whole of this one.
    expected.push(0x01); // numBytes
    expected.push(0x00); // the pad
    assert_bytes("only the empty key", &produced, &expected);
}

/// One single-byte key with no output.
///
/// The root gets one arc labelled `a` whose target is the final end node.
/// That node has no arcs, so the arc carries `BIT_STOP_NODE`; it is the
/// only arc, so `BIT_LAST_ARC`; and the target is final, so
/// `BIT_FINAL_ARC`. No output, so no `BIT_ARC_HAS_OUTPUT`.
///
/// `BIT_TARGET_NEXT` is **not** set: `last_frozen_node` starts at 0 and the
/// target is `FINAL_END_NODE`, which is -1.
#[test]
fn a_single_key_with_no_output() {
    let produced = built(&[(b"a", b"")]);

    let flags = 0b0000_1011; // FINAL | LAST | STOP
    let mut expected = HEADER.to_vec();
    expected.push(0x00); // unpacked
    expected.push(0x00); // no empty output
    expected.push(0x00); // BYTE1
    // The root's address is the index of its **last** byte: the pad takes
    // index 0, the node takes 1 and 2. `addNode` returns
    // `bytes.getPosition() - 1`, and the root is the last node compiled.
    expected.push(0x02); // startNode
    expected.push(0x01); // nodeCount
    expected.push(0x01); // arcCount
    expected.push(0x00); // arcWithOutputCount
    expected.push(0x03); // numBytes
    expected.push(0x00); // the pad
    // The node is `flags, label` written forward and then reversed.
    expected.extend_from_slice(&[b'a', flags]);
    assert_bytes("a single key", &produced, &expected);
}

/// One key carrying an output.
#[test]
fn a_single_key_with_an_output() {
    let produced = built(&[(b"a", b"Z")]);

    let flags = 0b0001_1011; // FINAL | LAST | STOP | HAS_OUTPUT
    let mut expected = HEADER.to_vec();
    expected.push(0x00);
    expected.push(0x00);
    expected.push(0x00);
    expected.push(0x04); // startNode: the last byte of a five-byte store
    expected.push(0x01); // nodeCount
    expected.push(0x01); // arcCount
    expected.push(0x01); // arcWithOutputCount
    expected.push(0x05); // numBytes
    expected.push(0x00); // the pad
    // Forward: flags, 'a', vint 1, 'Z'. Reversed:
    expected.extend_from_slice(&[b'Z', 0x01, b'a', flags]);
    assert_bytes("a single key with an output", &produced, &expected);
}

/// Two keys sharing no prefix: the root has two arcs.
///
/// The arcs are written in the order they were added, so `a` first and `b`
/// second; only `b` carries `BIT_LAST_ARC`. Each target is the final end
/// node, so both carry `BIT_STOP_NODE` and neither carries
/// `BIT_TARGET_NEXT`.
#[test]
fn two_keys_sharing_no_prefix() {
    let produced = built(&[(b"a", b""), (b"b", b"")]);

    let first = 0b0000_1001; // FINAL | STOP
    let last = 0b0000_1011; // FINAL | LAST | STOP
    let mut expected = HEADER.to_vec();
    expected.push(0x00);
    expected.push(0x00);
    expected.push(0x00);
    expected.push(0x04); // startNode: the last byte of a five-byte store
    expected.push(0x01); // nodeCount
    expected.push(0x02); // arcCount
    expected.push(0x00); // arcWithOutputCount
    expected.push(0x05); // numBytes
    expected.push(0x00); // the pad
    // Forward: (first, 'a'), (last, 'b'). Reversed over the whole node:
    expected.extend_from_slice(&[b'b', last, b'a', first]);
    assert_bytes("two keys", &produced, &expected);
}

/// Out-of-order keys are refused rather than silently mis-built.
#[test]
fn keys_out_of_order_are_refused() {
    let mut builder = FstBuilder::new();
    builder.add(b"b", b"").expect("the first key");
    let error = builder
        .add(b"a", b"")
        .expect_err("a descending key must be refused");
    assert!(
        error.to_string().contains("ascending order"),
        "the refusal says what the rule is: {error}"
    );
}

/// The same key twice is refused: a byte-string output cannot merge.
#[test]
fn a_repeated_key_is_refused() {
    let mut builder = FstBuilder::new();
    builder.add(b"a", b"one").expect("the first");
    let error = builder
        .add(b"a", b"two")
        .expect_err("a repeated key must be refused");
    assert!(
        error.to_string().contains("added twice"),
        "the refusal names the problem: {error}"
    );
}

/// A transducer with no key at all has no serialized form.
#[test]
fn an_empty_transducer_is_refused() {
    let error = FstBuilder::new()
        .finish()
        .expect_err("nothing to serialize");
    assert!(
        error.to_string().contains("no key at all"),
        "the refusal says why: {error}"
    );
}

/// A key that is a prefix of another, which is where `BIT_TARGET_NEXT`
/// first appears.
///
/// `a` and `ab`. The root's arc `a` reaches a node that is itself final —
/// `a` is a key — and carries one arc `b` to the final end node.
///
/// Traced: the `b` node is compiled first, landing at address 1; the root
/// is compiled next, and because `last_frozen_node` is then 1 and the
/// root's only arc targets 1, that arc carries `BIT_TARGET_NEXT` and
/// **writes no target at all**.
#[test]
fn a_key_that_is_a_prefix_of_another() {
    let produced = built(&[(b"a", b""), (b"ab", b"")]);

    // The `b` node: LAST | FINAL | STOP, no output, target is the final end
    // node so nothing follows the label.
    let b_flags = 0b0000_1011;
    // The root's arc: LAST | FINAL | TARGET_NEXT. Not STOP, because the
    // target has arcs; no target bytes, because TARGET_NEXT says where it
    // is.
    let a_flags = 0b0000_0111;

    let mut store = vec![0x00]; // the pad
    store.extend_from_slice(&[b'b', b_flags]); // reversed node at 1..3
    store.extend_from_slice(&[b'a', a_flags]); // reversed node at 3..5

    let mut expected = HEADER.to_vec();
    expected.extend_from_slice(&[0x00, 0x00, 0x00]); // unpacked, no empty output, BYTE1
    expected.push(0x04); // startNode: the root's last byte
    expected.push(0x02); // nodeCount
    expected.push(0x02); // arcCount
    expected.push(0x00); // arcWithOutputCount
    expected.push(0x05); // numBytes
    expected.extend_from_slice(&store);
    assert_bytes("a key that prefixes another", &produced, &expected);
}

/// Outputs that share a prefix: the shared part moves up to the arc every
/// key passes through.
///
/// `ab` → `XY` and `ac` → `XZ`. The common `X` ends on the root's `a` arc,
/// and `Y` and `Z` stay on the arcs below — so concatenating along either
/// path reproduces the output that was added.
#[test]
fn outputs_that_share_a_prefix_are_pushed_up() {
    let produced = built(&[(b"ab", b"XY"), (b"ac", b"XZ")]);

    // The two-arc node. `b` is not last; `c` is. Both are final, both stop,
    // both carry an output.
    let b_flags = 0b0001_1001; // FINAL | STOP | HAS_OUTPUT
    let c_flags = 0b0001_1011; // FINAL | LAST | STOP | HAS_OUTPUT
    let mut inner = Vec::new();
    inner.extend_from_slice(&[b_flags, b'b', 0x01, b'Y']);
    inner.extend_from_slice(&[c_flags, b'c', 0x01, b'Z']);
    inner.reverse();

    // The root: LAST | TARGET_NEXT | HAS_OUTPUT. Not final — `a` is not a
    // key — and not stop, because the target has arcs.
    let a_flags = 0b0001_0110;
    let mut root = vec![a_flags, b'a', 0x01, b'X'];
    root.reverse();

    let mut expected = HEADER.to_vec();
    expected.extend_from_slice(&[0x00, 0x00, 0x00]);
    expected.push(0x0c); // startNode: 12, the root's last byte
    expected.push(0x02); // nodeCount
    expected.push(0x03); // arcCount
    expected.push(0x03); // arcWithOutputCount — every arc has one
    expected.push(0x0d); // numBytes
    expected.push(0x00); // the pad
    expected.extend_from_slice(&inner);
    expected.extend_from_slice(&root);
    assert_bytes("outputs sharing a prefix", &produced, &expected);
}

/// The shape every `.tip` takes: an empty key carrying the root block's
/// code, beside ordinary keys.
///
/// The terms writer stores the root block's code as the transducer's empty
/// output, so this is not a corner case — it is the common one.
#[test]
fn an_empty_key_beside_ordinary_keys() {
    let produced = built(&[(b"", b"root"), (b"a", b"")]);

    // The empty output is `vint 4, r o o t`, then reversed, then written
    // with its own length in front.
    let mut empty = vec![0x04, b'r', b'o', b'o', b't'];
    empty.reverse();

    let flags = 0b0000_1011; // FINAL | LAST | STOP

    let mut expected = HEADER.to_vec();
    expected.push(0x00); // unpacked
    expected.push(0x01); // has an empty output
    expected.push(0x05); // its serialized length
    expected.extend_from_slice(&empty);
    expected.push(0x00); // BYTE1
    expected.push(0x02); // startNode
    expected.push(0x01); // nodeCount
    expected.push(0x01); // arcCount
    expected.push(0x00); // arcWithOutputCount
    expected.push(0x03); // numBytes
    expected.push(0x00); // the pad
    expected.extend_from_slice(&[b'a', flags]);
    assert_bytes("an empty key beside ordinary keys", &produced, &expected);
}

/// A shared suffix is written once.
///
/// `ax` and `bx` both end in a node whose single arc is `x` to the final
/// end node. Those two nodes are structurally identical, and the terms
/// writer's builder shares single-arc tails — so the second is not written,
/// and both root arcs point at the same address.
#[test]
fn a_shared_single_arc_tail_is_written_once() {
    let produced = built(&[(b"ax", b""), (b"bx", b"")]);

    // Two nodes, not three: the `x` tail once, then the root. And three
    // arcs — two out of the root, one out of the shared tail — so what is
    // shared is the *node*, not the arcs that reach it.
    let counts = trailer_counts(&produced);
    assert_eq!(
        counts.node_count,
        2,
        "the shared tail must be written once: {}",
        hex(&produced)
    );
    assert_eq!(counts.arc_count, 3, "{}", hex(&produced));
}

/// What the serialized trailer says, parsed rather than indexed.
struct TrailerCounts {
    node_count: u64,
    arc_count: u64,
}

/// Reads the counts, so a test asserting on them does not depend on the
/// length of anything before them.
fn trailer_counts(bytes: &[u8]) -> TrailerCounts {
    let mut at = HEADER.len();
    at += 1; // the packed flag
    let has_empty_output = bytes[at];
    at += 1;
    if has_empty_output == 1 {
        let (length, width) = read_vint(bytes, at);
        at += width + length as usize;
    }
    at += 1; // the input type
    let (_start_node, width) = read_vlong(bytes, at);
    at += width;
    let (node_count, width) = read_vlong(bytes, at);
    at += width;
    let (arc_count, _width) = read_vlong(bytes, at);
    TrailerCounts {
        node_count,
        arc_count,
    }
}

fn read_vint(bytes: &[u8], at: usize) -> (u32, usize) {
    let (value, width) = read_vlong(bytes, at);
    (value as u32, width)
}

fn read_vlong(bytes: &[u8], at: usize) -> (u64, usize) {
    let mut value = 0u64;
    let mut shift = 0;
    let mut width = 0;
    loop {
        let byte = bytes[at + width];
        value |= u64::from(byte & 0x7F) << shift;
        width += 1;
        if byte & 0x80 == 0 {
            return (value, width);
        }
        shift += 7;
    }
}

// ---------------------------------------------------------------------------
// The corpus the judge reads back
// ---------------------------------------------------------------------------

/// The cases the corpus carries, and the corpus is regenerated from these.
///
/// One shape per line of `fixtures/lucene-fst-corpus.tsv`: the transducer
/// froe wrote, and the key/output pairs it was built from. Task 0911's
/// conformance phase loads each with Lucene's own reader and enumerates it.
type CorpusCase = (&'static str, Vec<(&'static [u8], &'static [u8])>);

/// The cases, in corpus order.
fn corpus_cases() -> Vec<CorpusCase> {
    vec![
        // The shape a field with a single root block produces, which is
        // every field of fewer than 49 terms: the root block's code as the
        // empty output and no other key. It is first because it is the
        // common case, not a corner one.
        ("empty-key-only", vec![(&b""[..], &b"xy"[..])]),
        ("single", vec![(&b"a"[..], &b""[..])]),
        ("single-with-output", vec![(&b"a"[..], &b"Z"[..])]),
        (
            "two-keys",
            vec![(&b"a"[..], &b""[..]), (&b"b"[..], &b""[..])],
        ),
        (
            "prefix",
            vec![(&b"a"[..], &b""[..]), (&b"ab"[..], &b""[..])],
        ),
        (
            "shared-outputs",
            vec![(&b"ab"[..], &b"XY"[..]), (&b"ac"[..], &b"XZ"[..])],
        ),
        (
            "shared-tail",
            vec![(&b"ax"[..], &b""[..]), (&b"bx"[..], &b""[..])],
        ),
        (
            "tip-shape",
            vec![
                (&b""[..], &b"root"[..]),
                (&b"alpha"[..], &b"1"[..]),
                (&b"alpine"[..], &b"2"[..]),
                (&b"beta"[..], &b"3"[..]),
            ],
        ),
        (
            "many-arcs",
            vec![
                (&b"a"[..], &b"0"[..]),
                (&b"b"[..], &b"1"[..]),
                (&b"c"[..], &b"2"[..]),
                (&b"d"[..], &b"3"[..]),
                (&b"e"[..], &b"4"[..]),
                (&b"f"[..], &b"5"[..]),
                (&b"g"[..], &b"6"[..]),
                (&b"h"[..], &b"7"[..]),
                (&b"i"[..], &b"8"[..]),
                (&b"j"[..], &b"9"[..]),
                (&b"k"[..], &b"10"[..]),
                (&b"l"[..], &b"11"[..]),
            ],
        ),
    ]
}

/// The corpus, rendered.
fn render_corpus() -> String {
    let mut rendered = String::from(
        "# Transducers froe wrote, for Lucene's own reader to enumerate.\n\
         #\n\
         # Regenerated by `cargo test --test lucene_fst_tests`, which also\n\
         # asserts this file is unchanged — so the committed corpus is the\n\
         # corpus the judge checks. Task 0911's conformance phase runs\n\
         # `FstCheck fst-check` over it inside the pinned image.\n\
         #\n\
         # Columns: <name>\\t<fst bytes in hex>\\t<key>=<output>,… with both\n\
         # sides of a pair in hex, and an empty key or output written as an\n\
         # empty field.\n",
    );
    for (name, pairs) in corpus_cases() {
        let mut builder = FstBuilder::new();
        for (key, output) in &pairs {
            builder.add(key, output).expect("add");
        }
        let bytes = builder.finish().expect("finish");
        let rendered_pairs: Vec<String> = pairs
            .iter()
            .map(|(key, output)| format!("{}={}", hex(key), hex(output)))
            .collect();
        let _ = writeln!(
            rendered,
            "{name}\t{}\t{}",
            hex(&bytes),
            rendered_pairs.join(",")
        );
    }
    rendered
}

/// The committed corpus is what these cases produce.
///
/// Regenerating and comparing rather than only reading keeps the file and
/// the cases from drifting apart: a change to the writer that alters a byte
/// fails here rather than silently leaving the judge checking an older
/// transducer.
#[test]
fn the_committed_corpus_is_the_one_these_cases_produce() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/lucene-fst-corpus.tsv");
    let rendered = render_corpus();
    if std::env::var_os("FROE_REGENERATE_FIXTURES").is_some() {
        std::fs::write(&path, &rendered).expect("write the corpus");
        return;
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_eq!(
        committed, rendered,
        "the committed corpus differs from what these cases produce; rerun with \
         FROE_REGENERATE_FIXTURES=1 once the change is intended"
    );
}
