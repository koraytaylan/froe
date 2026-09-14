//! The `Lucene41` postings writer, against bytes computed by hand from the
//! specification.
//!
//! `docs/analysis/lucene-4-7-codec.md` §6. The small cases below are whole
//! byte streams derived from the Java quoted there, with the derivation in
//! the comment beside each. The large ones — the skip-level boundaries at
//! 1,023, 1,024, 1,025, 8,192 and 8,193 documents — are read back by a
//! parser written to **`MultiLevelSkipListReader`'s** framing rather than
//! the writer's, so the two rules meet in the middle: the writer decides
//! how high a point climbs, the reader decides how many levels to expect,
//! and the tests fail if they ever disagree.
//!
//! Task 0911's conformance phase closes the loop the other way, with
//! Lucene's own reader over froe's files.

use froe::index::lucene::codec::postings::{
    BLOCK_SIZE, IndexOptions, PostingsWriter, SegmentShape, TermMetadata,
};

/// What one run of the writer produced.
struct Written {
    document: Vec<u8>,
    position: Option<Vec<u8>>,
    payload: Option<Vec<u8>>,
    metadata: Vec<TermMetadata>,
}

fn write_with(
    options: IndexOptions,
    segment_documents: i32,
    body: impl FnOnce(&mut PostingsWriter<Vec<u8>>) -> Vec<TermMetadata>,
) -> Written {
    let shape = SegmentShape {
        document_count: segment_documents,
        any_field_has_positions: options.has_positions(),
        any_field_has_offsets: options.has_offsets(),
    };
    let mut writer = PostingsWriter::new(
        shape,
        Vec::new(),
        options.has_positions().then(Vec::new),
        options.has_offsets().then(Vec::new),
    )
    .expect("open the postings files");
    writer.set_field(options).expect("set the field");
    let metadata = body(&mut writer);
    let files = writer.finish();
    Written {
        document: files.document,
        position: files.position,
        payload: files.payload,
        metadata,
    }
}

/// A term over documents `0..count`, each occurring `occurrences` times,
/// with positions `0..occurrences` and offsets `(2p, 2p+1)`.
fn plain_term(
    writer: &mut PostingsWriter<Vec<u8>>,
    options: IndexOptions,
    count: i32,
    occurrences: i32,
) -> TermMetadata {
    writer.start_term();
    for document in 0..count {
        writer
            .start_document(document, occurrences)
            .expect("start the document");
        if options.has_positions() {
            for occurrence in 0..occurrences {
                writer
                    .add_position(occurrence, occurrence * 2, occurrence * 2 + 1)
                    .expect("add the position");
            }
        }
        writer.finish_document();
    }
    writer.finish_term().expect("finish the term")
}

/// `CodecUtil.writeHeader`: magic, the name as a string, the version as a
/// big-endian `Int`.
fn header(name: &str) -> Vec<u8> {
    let mut bytes = vec![0x3f, 0xd7, 0x6c, 0x17, name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&1i32.to_be_bytes());
    bytes
}

/// `.doc`'s header and the `ForUtil` format table that follows it: the
/// packed-integer version, then 32 entries of `PACKED << 5 | (width - 1)`,
/// which for `PACKED`'s id of zero are the widths less one.
fn document_preamble() -> Vec<u8> {
    let mut bytes = header("Lucene41PostingsWriterDoc");
    bytes.push(0x01);
    bytes.extend(0..32u8);
    bytes
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
        "{what}: froe wrote {} bytes, the specification says {}{}",
        produced.len(),
        expected.len(),
        match first {
            Some(at) => format!(
                "; first difference at byte {at}: {:#04x} against {:#04x}",
                produced[at], expected[at]
            ),
            None => String::new(),
        }
    );
}

/// The packed block a run of `[0, 1, 1, …]` of 128 values produces: one
/// byte of width 1, then the bits most significant first — a zero and then
/// 127 ones.
fn one_bit_block() -> Vec<u8> {
    let mut bytes = vec![0x01, 0x7f];
    bytes.extend(std::iter::repeat_n(0xffu8, 15));
    bytes
}

/// The all-equal escape: a width of zero, then the value as a `VInt`.
fn all_equal_block(value: u8) -> Vec<u8> {
    vec![0x00, value]
}

#[test]
fn the_document_file_opens_with_its_header_and_the_format_table() {
    let written = write_with(IndexOptions::DocumentsAndFrequencies, 1, |writer| {
        vec![plain_term(
            writer,
            IndexOptions::DocumentsAndFrequencies,
            1,
            1,
        )]
    });
    assert_bytes("the .doc preamble", &written.document, &document_preamble());
    assert_eq!(written.document.len(), 67);
}

#[test]
fn the_three_files_carry_their_own_codec_names() {
    let options = IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets;
    let written = write_with(options, 1, |writer| vec![plain_term(writer, options, 1, 1)]);
    assert_bytes(
        "the .pos header",
        &written.position.expect("a .pos")[..34],
        &header("Lucene41PostingsWriterPos"),
    );
    assert_bytes(
        "the .pay header",
        &written.payload.expect("a .pay"),
        &header("Lucene41PostingsWriterPay"),
    );
}

#[test]
fn a_single_document_term_is_pulsed_in_every_index_option() {
    for options in [
        IndexOptions::Documents,
        IndexOptions::DocumentsAndFrequencies,
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets,
    ] {
        let written = write_with(options, 16, |writer| {
            writer.start_term();
            writer.start_document(7, 1).expect("start");
            if options.has_positions() {
                writer.add_position(0, 0, 1).expect("position");
            }
            writer.finish_document();
            vec![writer.finish_term().expect("finish")]
        });
        // §6.3: nothing at all reaches `.doc`, whatever the options.
        assert_bytes(
            &format!("the .doc of a pulsed term under {options:?}"),
            &written.document,
            &document_preamble(),
        );
        let metadata = written.metadata[0];
        assert_eq!(metadata.singleton_document, Some(7), "{options:?}");
        assert_eq!(metadata.skip_offset, None, "{options:?}");
        assert_eq!(metadata.document_frequency, 1, "{options:?}");
        assert_eq!(
            metadata.total_term_frequency,
            if options.has_frequencies() { 1 } else { -1 },
            "{options:?}"
        );
    }
}

#[test]
fn a_pulsed_term_in_a_positions_and_offsets_field_still_writes_its_positions() {
    let options = IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets;
    let written = write_with(options, 16, |writer| {
        writer.start_term();
        writer.start_document(5, 2).expect("start");
        writer.add_position(0, 0, 4).expect("first position");
        writer.add_position(1, 5, 9).expect("second position");
        writer.finish_document();
        vec![writer.finish_term().expect("finish")]
    });

    // The tail, per §6.5, with no payloads: a bare position delta, then a
    // shifted offset start delta whose low bit says a length follows.
    //   position 0: delta 0                     → 00
    //               start delta 0, length 4,
    //               and -1 forces the length    → 01 04
    //   position 1: delta 1                     → 01
    //               start delta 5, length 4
    //               unchanged                   → 0a
    let mut expected = header("Lucene41PostingsWriterPos");
    expected.extend_from_slice(&[0x00, 0x01, 0x04, 0x01, 0x0a]);
    assert_bytes(
        "the .pos of a pulsed term",
        &written.position.expect("a .pos"),
        &expected,
    );
    assert_bytes(
        "the .doc of a pulsed term",
        &written.document,
        &document_preamble(),
    );
    assert_eq!(written.metadata[0].singleton_document, Some(5));
    assert_eq!(written.metadata[0].last_position_block_offset, None);
}

#[test]
fn a_term_below_the_block_size_is_a_vint_tail_alone() {
    let options = IndexOptions::DocumentsAndFrequencies;
    let written = write_with(options, 127, |writer| {
        vec![plain_term(writer, options, 127, 1)]
    });

    // §6.2: with frequencies, the delta shifts left one and bit 0 means a
    // frequency of one. The first document's delta is 0 → 0x01; every
    // later delta is 1 → 0x03.
    let mut expected = document_preamble();
    expected.push(0x01);
    expected.extend(std::iter::repeat_n(0x03u8, 126));
    assert_bytes("a 127-document term", &written.document, &expected);
    assert_eq!(written.metadata[0].skip_offset, None);
    assert_eq!(written.metadata[0].total_term_frequency, 127);
}

#[test]
fn a_tail_without_frequencies_writes_the_delta_unshifted() {
    let options = IndexOptions::Documents;
    let written = write_with(options, 127, |writer| {
        vec![plain_term(writer, options, 127, 1)]
    });

    let mut expected = document_preamble();
    expected.push(0x00);
    expected.extend(std::iter::repeat_n(0x01u8, 126));
    assert_bytes(
        "a 127-document term without frequencies",
        &written.document,
        &expected,
    );
    assert_eq!(written.metadata[0].total_term_frequency, -1);
}

#[test]
fn a_frequency_above_one_takes_a_second_vint() {
    let options = IndexOptions::DocumentsAndFrequencies;
    let written = write_with(options, 4, |writer| {
        writer.start_term();
        for (document, frequency) in [(0, 1), (1, 2), (2, 1)] {
            writer.start_document(document, frequency).expect("start");
            writer.finish_document();
        }
        vec![writer.finish_term().expect("finish")]
    });

    // 0x01 = delta 0 with the frequency-is-one bit; 0x02 0x02 = delta 1
    // unshifted-low-bit then the frequency; 0x03 = delta 1 again.
    let mut expected = document_preamble();
    expected.extend_from_slice(&[0x01, 0x02, 0x02, 0x03]);
    assert_bytes("a mixed-frequency tail", &written.document, &expected);
    assert_eq!(written.metadata[0].total_term_frequency, 4);
}

#[test]
fn a_term_of_exactly_one_block_writes_no_tail() {
    let options = IndexOptions::DocumentsAndFrequencies;
    let written = write_with(options, 128, |writer| {
        vec![plain_term(writer, options, 128, 1)]
    });

    // One packed block of deltas, one all-equal block of frequencies, and
    // nothing else: `finish_document` cleared the buffer that `start_document`
    // deliberately left full. Without that clear the 128 deltas would follow
    // a second time as a tail.
    let mut expected = document_preamble();
    expected.extend(one_bit_block());
    expected.extend(all_equal_block(0x01));
    assert_bytes("a 128-document term", &written.document, &expected);
    assert_eq!(written.document.len(), 67 + 19);
    assert_eq!(
        written.metadata[0].skip_offset, None,
        "128 is not above 128"
    );
    assert_eq!(written.metadata[0].singleton_document, None);
}

#[test]
fn a_term_just_above_a_block_adds_a_tail_and_a_skip_list() {
    let options = IndexOptions::DocumentsAndFrequencies;
    let written = write_with(options, 129, |writer| {
        vec![plain_term(writer, options, 129, 1)]
    });

    let mut expected = document_preamble();
    expected.extend(one_bit_block());
    expected.extend(all_equal_block(0x01));
    // The 129th document's tail entry: delta 1, frequency 1.
    expected.push(0x03);
    // One skip point, buffered when the 129th document started: the last
    // document of the closed block (127) and the `.doc` offset at that
    // moment (19 bytes of blocks past the term's start).
    expected.extend_from_slice(&[0x7f, 0x13]);
    assert_bytes("a 129-document term", &written.document, &expected);
    assert_eq!(written.metadata[0].skip_offset, Some(20));
}

#[test]
fn a_filled_position_block_sends_its_offsets_to_the_payload_file() {
    let options = IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets;
    let written = write_with(options, 1, |writer| {
        vec![plain_term(writer, options, 1, 129)]
    });

    // A single document with 129 occurrences: pulsed in `.doc`, one filled
    // block in `.pos` and `.pay`, and a three-value tail in `.pos` alone.
    let mut position = header("Lucene41PostingsWriterPos");
    position.extend(one_bit_block());
    // The 129th position: delta 1, then a start delta of 2 with the
    // length-follows bit and the length, because the tail's running length
    // starts at -1.
    position.extend_from_slice(&[0x01, 0x05, 0x01]);
    assert_bytes("the .pos", &written.position.expect("a .pos"), &position);

    // `.pay` takes the offset start deltas — 0 then 127 twos, two bits
    // each — and then the lengths, which are all one.
    let mut payload = header("Lucene41PostingsWriterPay");
    payload.push(0x02);
    payload.push(0x2a);
    payload.extend(std::iter::repeat_n(0xaau8, 31));
    payload.extend(all_equal_block(0x01));
    assert_bytes("the .pay", &written.payload.expect("a .pay"), &payload);

    assert_bytes("the .doc", &written.document, &document_preamble());
    let metadata = written.metadata[0];
    assert_eq!(metadata.singleton_document, Some(0));
    assert_eq!(metadata.total_term_frequency, 129);
    // §6.5: read before the tail, so it points at the tail's first byte —
    // the width byte and sixteen packed bytes past the term's start.
    assert_eq!(metadata.last_position_block_offset, Some(17));
}

#[test]
fn a_skip_point_in_a_positions_field_carries_the_position_and_payload_state() {
    let options = IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets;
    let written = write_with(options, 129, |writer| {
        vec![plain_term(writer, options, 129, 1)]
    });

    let mut expected = document_preamble();
    expected.extend(one_bit_block());
    expected.extend(all_equal_block(0x01));
    expected.push(0x03);
    // One skip point: the closed block's last document (127), the `.doc`
    // offset (19), the `.pos` offset (an all-equal block of 128 zero
    // deltas, two bytes), the position buffer offset (cleared to 0 when
    // that block flushed) and the `.pay` offset (two all-equal blocks).
    expected.extend_from_slice(&[0x7f, 0x13, 0x02, 0x00, 0x04]);
    assert_bytes(
        "a 129-document positions term",
        &written.document,
        &expected,
    );
    assert_eq!(written.metadata[0].skip_offset, Some(20));
}

#[test]
fn the_long_count_follows_the_field_options() {
    for (options, expected) in [
        (IndexOptions::Documents, 1),
        (IndexOptions::DocumentsAndFrequencies, 1),
        (IndexOptions::DocumentsAndFrequenciesAndPositions, 2),
        (
            IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets,
            3,
        ),
    ] {
        let shape = SegmentShape {
            document_count: 1,
            any_field_has_positions: options.has_positions(),
            any_field_has_offsets: options.has_offsets(),
        };
        let mut writer = PostingsWriter::new(
            shape,
            Vec::new(),
            options.has_positions().then(Vec::new),
            options.has_offsets().then(Vec::new),
        )
        .expect("open");
        assert_eq!(
            writer.set_field(options).expect("set field").value(),
            expected,
            "{options:?}"
        );
    }
}

/// One parsed skip entry, and where it ends within its level's region — the
/// offset the level above records as a child pointer.
#[derive(Debug)]
struct SkipEntry {
    document_delta: i32,
    document_pointer_delta: u64,
    position_pointer_delta: u64,
    position_buffer_upto: i32,
    payload_pointer_delta: u64,
    child_pointer: u64,
    end: usize,
}

fn read_vint(bytes: &[u8], at: usize) -> (i32, usize) {
    let (value, next) = read_vlong(bytes, at);
    (value as i32, next)
}

fn read_vlong(bytes: &[u8], at: usize) -> (u64, usize) {
    let mut value = 0u64;
    let mut shift = 0;
    let mut cursor = at;
    loop {
        let byte = bytes[cursor];
        cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return (value, cursor);
        }
        shift += 7;
    }
}

/// `Lucene41SkipReader.trim` then `MultiLevelSkipListReader.loadSkipLevels`:
/// how many levels **the reader** expects for a term of this document
/// frequency. The writer decides independently, from how many times eight
/// divides each block number, and §6.6 is the claim that they agree.
fn reader_level_count(document_frequency: i32) -> usize {
    let trimmed = if document_frequency % BLOCK_SIZE as i32 == 0 {
        document_frequency - 1
    } else {
        document_frequency
    };
    if trimmed <= BLOCK_SIZE as i32 {
        return 1;
    }
    let mut value = trimmed as usize / BLOCK_SIZE;
    let mut levels = 1;
    while value >= 8 {
        value /= 8;
        levels += 1;
    }
    levels.min(10)
}

/// Splits the skip packet the way the reader does — highest level first,
/// each above level 0 prefixed by its `VLong` length — and parses every
/// entry, refusing a level whose bytes do not come out even.
fn parse_skip_list(
    region: &[u8],
    document_frequency: i32,
    options: IndexOptions,
) -> Vec<Vec<SkipEntry>> {
    let levels = reader_level_count(document_frequency);
    let mut at = 0usize;
    let mut bounds = vec![(0usize, 0usize); levels];
    for level in (1..levels).rev() {
        let (length, next) = read_vlong(region, at);
        at = next;
        bounds[level] = (at, at + length as usize);
        at += length as usize;
    }
    bounds[0] = (at, region.len());
    bounds
        .iter()
        .enumerate()
        .map(|(level, (start, end))| parse_level(&region[*start..*end], level, options))
        .collect()
}

fn parse_level(bytes: &[u8], level: usize, options: IndexOptions) -> Vec<SkipEntry> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let (document_delta, next) = read_vint(bytes, at);
        let (document_pointer_delta, next) = read_vlong(bytes, next);
        let mut entry = SkipEntry {
            document_delta,
            document_pointer_delta,
            position_pointer_delta: 0,
            position_buffer_upto: 0,
            payload_pointer_delta: 0,
            child_pointer: 0,
            end: 0,
        };
        let mut cursor = next;
        if options.has_positions() {
            let (position_pointer_delta, next) = read_vlong(bytes, cursor);
            let (position_buffer_upto, next) = read_vint(bytes, next);
            entry.position_pointer_delta = position_pointer_delta;
            entry.position_buffer_upto = position_buffer_upto;
            cursor = next;
            if options.has_offsets() {
                let (payload_pointer_delta, next) = read_vlong(bytes, cursor);
                entry.payload_pointer_delta = payload_pointer_delta;
                cursor = next;
            }
        }
        // The child pointer trails the entry at every level above zero, and
        // never at level zero.
        entry.end = cursor;
        if level != 0 {
            let (child_pointer, next) = read_vlong(bytes, cursor);
            entry.child_pointer = child_pointer;
            cursor = next;
        }
        entries.push(entry);
        at = cursor;
    }
    assert_eq!(at, bytes.len(), "level {level} did not come out even");
    entries
}

#[test]
fn the_skip_levels_follow_the_block_boundaries() {
    for options in [
        IndexOptions::Documents,
        IndexOptions::DocumentsAndFrequencies,
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets,
    ] {
        for count in [129, 1023, 1024, 1025, 8192, 8193] {
            check_skip_list(options, count);
        }
    }
}

#[allow(
    clippy::cognitive_complexity,
    reason = "one assertion per rule in §6.6, each naming the rule it checks"
)]
fn check_skip_list(options: IndexOptions, count: i32) {
    let written = write_with(options, count, |writer| {
        vec![plain_term(writer, options, count, 1)]
    });
    let metadata = written.metadata[0];
    let what = format!("{options:?} over {count} documents");

    let skip_offset = metadata.skip_offset.expect("a term above one block skips");
    let start = (metadata.document_start + skip_offset) as usize;
    let levels = parse_skip_list(&written.document[start..], count, options);

    // A point per closed block, and a block closes only when a document
    // follows it — so the last full block of a term whose count is a
    // multiple of 128 gets none.
    let points = (count - 1) as usize / BLOCK_SIZE;
    assert_eq!(
        levels.len(),
        1 + points.ilog(8) as usize,
        "{what}: the level count the reader derives"
    );

    // Per block, `.doc` holds one packed block of deltas and, with
    // frequencies, one all-equal block. Only the first term block carries a
    // zero delta and so needs a packed body; the rest are all ones.
    let frequency_block = usize::from(options.has_frequencies()) * 2;
    let first_block = 17 + frequency_block;
    let later_block = 2 + frequency_block;

    for (level, entries) in levels.iter().enumerate() {
        let stride = 8usize.pow(level as u32);
        assert_eq!(
            entries.len(),
            points / stride,
            "{what}: entries at level {level}"
        );

        let mut document = 0i32;
        let mut document_pointer = 0u64;
        let mut position_pointer = 0u64;
        let mut payload_pointer = 0u64;
        for (index, entry) in entries.iter().enumerate() {
            document += entry.document_delta;
            document_pointer += entry.document_pointer_delta;
            position_pointer += entry.position_pointer_delta;
            payload_pointer += entry.payload_pointer_delta;

            let block = (index + 1) * stride;
            assert_eq!(
                document,
                (block * BLOCK_SIZE) as i32 - 1,
                "{what}: the last document of block {block} at level {level}"
            );
            assert_eq!(
                document_pointer,
                (first_block + later_block * (block - 1)) as u64,
                "{what}: the .doc offset at block {block} at level {level}"
            );
            if options.has_positions() {
                // One position per document, so a position block closes
                // with each document block: an all-equal block of zero
                // deltas, two bytes, and a buffer left empty behind it.
                assert_eq!(
                    position_pointer,
                    (block * 2) as u64,
                    "{what}: the .pos offset at block {block}"
                );
                assert_eq!(entry.position_buffer_upto, 0, "{what}: the buffer offset");
            }
            if options.has_offsets() {
                // Two all-equal blocks per position block: the start deltas
                // and the lengths.
                assert_eq!(
                    payload_pointer,
                    (block * 4) as u64,
                    "{what}: the .pay offset at block {block}"
                );
            }
            if level != 0 {
                // The child pointer is the offset just past the entry the
                // same buffering call wrote one level down.
                let below = &levels[level - 1][(index + 1) * 8 - 1];
                assert_eq!(
                    entry.child_pointer, below.end as u64,
                    "{what}: the child pointer of entry {index} at level {level}"
                );
            }
        }
    }
}

#[test]
fn documents_out_of_order_are_refused() {
    let shape = SegmentShape {
        document_count: 4,
        any_field_has_positions: false,
        any_field_has_offsets: false,
    };
    let mut writer = PostingsWriter::new(shape, Vec::new(), None, None).expect("open");
    writer
        .set_field(IndexOptions::DocumentsAndFrequencies)
        .expect("set field");
    writer.start_term();
    writer.start_document(3, 1).expect("the first document");
    writer.finish_document();
    let refusal = writer
        .start_document(3, 1)
        .expect_err("a repeated document is not increasing");
    assert!(
        refusal.to_string().contains("increasing document order"),
        "{refusal}"
    );
}

#[test]
fn a_position_in_a_field_without_positions_is_refused() {
    let shape = SegmentShape {
        document_count: 4,
        any_field_has_positions: false,
        any_field_has_offsets: false,
    };
    let mut writer = PostingsWriter::new(shape, Vec::new(), None, None).expect("open");
    writer
        .set_field(IndexOptions::DocumentsAndFrequencies)
        .expect("set field");
    writer.start_term();
    writer.start_document(0, 1).expect("the document");
    let refusal = writer
        .add_position(0, 0, 1)
        .expect_err("there is no .pos to write into");
    assert!(refusal.to_string().contains("index options"), "{refusal}");
}

#[test]
fn offsets_that_move_backwards_are_refused() {
    let options = IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets;
    let shape = SegmentShape {
        document_count: 4,
        any_field_has_positions: true,
        any_field_has_offsets: true,
    };
    let mut writer =
        PostingsWriter::new(shape, Vec::new(), Some(Vec::new()), Some(Vec::new())).expect("open");
    writer.set_field(options).expect("set field");
    writer.start_term();
    writer.start_document(0, 2).expect("the document");
    writer.add_position(0, 10, 12).expect("the first position");
    let refusal = writer
        .add_position(1, 4, 6)
        .expect_err("a start offset behind the last one");
    assert!(
        refusal.to_string().contains("offsets increase"),
        "{refusal}"
    );
}

#[test]
fn a_file_set_that_does_not_match_the_segment_is_refused() {
    let shape = SegmentShape {
        document_count: 4,
        any_field_has_positions: true,
        any_field_has_offsets: false,
    };
    let Err(refusal) = PostingsWriter::new(shape, Vec::new(), None, None) else {
        panic!("a segment with positions needs a .pos");
    };
    assert!(refusal.to_string().contains(".pos sink"), "{refusal}");

    let shape = SegmentShape {
        document_count: 4,
        any_field_has_positions: false,
        any_field_has_offsets: true,
    };
    let Err(refusal) = PostingsWriter::new(shape, Vec::new(), None, Some(Vec::new())) else {
        panic!("offsets without positions is not an index option");
    };
    assert!(
        refusal.to_string().contains("without positions"),
        "{refusal}"
    );
}

#[test]
fn a_term_with_no_documents_is_refused() {
    let shape = SegmentShape {
        document_count: 4,
        any_field_has_positions: false,
        any_field_has_offsets: false,
    };
    let mut writer = PostingsWriter::new(shape, Vec::new(), None, None).expect("open");
    writer
        .set_field(IndexOptions::DocumentsAndFrequencies)
        .expect("set field");
    writer.start_term();
    let refusal = writer.finish_term().expect_err("no postings to finish");
    assert!(refusal.to_string().contains("no documents"), "{refusal}");
}
