//! The Lucene table-of-contents readers, against bytes built here.
//!
//! Two kinds of test. The *shape* tests build a well-formed structure and
//! read it back, which says the reader agrees with
//! `docs/analysis/index-lucene-storage.md` §8. The *hostile* tests feed it
//! bytes Lucene's own writer could not produce and require a typed error
//! naming the file and the offset — never a panic, and never an allocation
//! driven by a number the file chose.

use std::io::Cursor;

use froe::index::lucene::codec_header::{CODEC_MAGIC, read_codec_header};
use froe::index::lucene::compound::{read_table_of_contents, strip_segment_name};
use froe::index::lucene::read::{LuceneReadError, Reader};
use froe::index::lucene::segments::{
    commit_generation, generation_from_commit_file_name, read_commit_file, read_segment_info,
    read_segments_gen,
};

/// Builds bytes the way Lucene's own `DataOutput` does.
#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn int(&mut self, value: i32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn long(&mut self, value: i64) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn byte(&mut self, value: u8) -> &mut Self {
        self.bytes.push(value);
        self
    }

    fn variable_int(&mut self, value: i32) -> &mut Self {
        let mut remaining = value as u32;
        while remaining >= 0x80 {
            self.bytes.push((remaining as u8) | 0x80);
            remaining >>= 7;
        }
        self.bytes.push(remaining as u8);
        self
    }

    fn string(&mut self, value: &str) -> &mut Self {
        self.variable_int(value.len() as i32);
        self.bytes.extend_from_slice(value.as_bytes());
        self
    }

    fn string_set(&mut self, values: &[&str]) -> &mut Self {
        self.int(values.len() as i32);
        for value in values {
            self.string(value);
        }
        self
    }

    fn string_map(&mut self, entries: &[(&str, &str)]) -> &mut Self {
        self.int(entries.len() as i32);
        for (key, value) in entries {
            self.string(key);
            self.string(value);
        }
        self
    }

    fn header(&mut self, codec: &str, version: i32) -> &mut Self {
        self.int(CODEC_MAGIC).string(codec).int(version)
    }

    fn finish(&self) -> Vec<u8> {
        self.bytes.clone()
    }
}

/// A reader over `bytes`, named `name`.
fn reader(name: &str, bytes: Vec<u8>) -> Reader<Cursor<Vec<u8>>> {
    let length = bytes.len() as u64;
    Reader::new(Cursor::new(bytes), name, length)
}

/// A well-formed `.si` for one segment.
fn segment_info_bytes(document_count: i32, files: &[&str]) -> Vec<u8> {
    let mut writer = Writer::default();
    writer
        .header("Lucene46SegmentInfo", 0)
        .string("4.7.2")
        .int(document_count)
        .byte(1)
        .string_map(&[("os", "Linux")])
        .string_set(files);
    writer.finish()
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

#[test]
fn a_segment_descriptor_reads_back_every_field() {
    let bytes = segment_info_bytes(7, &["_0.cfs", "_0.cfe", "_0.si"]);
    let info = read_segment_info(&mut reader("_0.si", bytes)).expect("read the descriptor");
    assert_eq!(info.lucene_version, "4.7.2");
    assert_eq!(info.document_count, 7);
    assert!(info.compound, "the compound flag is byte 1");
    assert_eq!(
        info.diagnostics,
        vec![("os".to_owned(), "Linux".to_owned())]
    );
    assert_eq!(info.files.len(), 3);
}

#[test]
fn a_commit_file_opens_each_segment_descriptor_mid_record() {
    // The ordering claim of §8.4: the `.si` is opened between the codec
    // name and the deletion generation. A reader that consumed the record
    // as one run of fields would misparse everything after segment one, so
    // two segments is the smallest fixture that can catch it.
    let mut writer = Writer::default();
    writer
        .header("segments", 1)
        .long(42)
        .int(2)
        .int(2)
        // Segment one.
        .string("_0")
        .string("oakCodec")
        .long(-1)
        .int(0)
        .long(-1)
        .int(0)
        // Segment two.
        .string("_1")
        .string("oakCodec")
        .long(-1)
        .int(1)
        .long(-1)
        .int(0)
        .string_map(&[("userKey", "userValue")])
        .long(0);

    let descriptors = |name: &str| {
        let count = if name == "_0.si" { 5 } else { 9 };
        Ok(reader(name, segment_info_bytes(count, &[])))
    };
    let commit = read_commit_file(
        &mut reader("segments_2", writer.finish()),
        "segments_2",
        descriptors,
    )
    .expect("read the commit file");

    assert_eq!(commit.generation, 2);
    assert_eq!(commit.version, 42);
    assert_eq!(commit.segments.len(), 2);
    assert_eq!(commit.segments[0].name, "_0");
    assert_eq!(commit.segments[0].info.document_count, 5);
    assert_eq!(commit.segments[1].name, "_1");
    assert_eq!(commit.segments[1].info.document_count, 9);
    assert_eq!(commit.segments[1].deletion_count, 1);
    assert_eq!(
        commit.user_data,
        vec![("userKey".to_owned(), "userValue".to_owned())]
    );
    // 5 live from the first, 9 - 1 from the second.
    assert_eq!(commit.live_document_count(), 13);
}

#[test]
fn a_commit_generation_comes_from_the_name_in_base_thirty_six() {
    assert_eq!(generation_from_commit_file_name("segments"), Some(0));
    assert_eq!(generation_from_commit_file_name("segments_1"), Some(1));
    // Base 36, so `z` is 35 and `10` is 36.
    assert_eq!(generation_from_commit_file_name("segments_z"), Some(35));
    assert_eq!(generation_from_commit_file_name("segments_10"), Some(36));
    assert_eq!(generation_from_commit_file_name("_0.si"), None);
}

#[test]
fn the_generation_hint_can_only_raise_the_listing_and_never_lowers_it() {
    let listing = vec!["segments_3".to_owned(), "segments.gen".to_owned()];
    assert_eq!(commit_generation(&listing, None), Some(3));
    assert_eq!(commit_generation(&listing, Some(5)), Some(5));
    // A hint below the listing is ignored rather than believed.
    assert_eq!(commit_generation(&listing, Some(1)), Some(3));
}

#[test]
fn a_generation_hint_counts_only_when_its_two_copies_agree() {
    let mut agreeing = Writer::default();
    agreeing.int(-2).long(4).long(4);
    assert_eq!(
        read_segments_gen(&mut reader("segments.gen", agreeing.finish())),
        Some(4)
    );

    let mut disagreeing = Writer::default();
    disagreeing.int(-2).long(4).long(5);
    assert_eq!(
        read_segments_gen(&mut reader("segments.gen", disagreeing.finish())),
        None,
        "Oak writes this file best-effort, so disagreement is ignored rather than a fault"
    );

    // Truncated, and still not a fault.
    let mut truncated = Writer::default();
    truncated.int(-2).long(4);
    assert_eq!(
        read_segments_gen(&mut reader("segments.gen", truncated.finish())),
        None
    );
}

#[test]
fn a_table_of_contents_reads_back_and_looks_up_either_spelling() {
    let mut writer = Writer::default();
    writer
        .header("CompoundFileWriterEntries", 0)
        .variable_int(2)
        .string(".fdt")
        .long(0)
        .long(100)
        .string(".tim")
        .long(100)
        .long(50);
    let table = read_table_of_contents(&mut reader("_0.cfe", writer.finish()), 150)
        .expect("read the table of contents");

    assert_eq!(table.entries.len(), 2);
    // §8.6: a caller holding either spelling reaches the same entry.
    assert_eq!(table.entry(".fdt").map(|entry| entry.length), Some(100));
    assert_eq!(table.entry("_0.fdt").map(|entry| entry.length), Some(100));
    assert_eq!(table.entry(".tim").map(|entry| entry.offset), Some(100));
}

#[test]
fn the_segment_prefix_is_stripped_the_way_lucene_strips_it() {
    assert_eq!(strip_segment_name("_0.fdt"), ".fdt");
    assert_eq!(strip_segment_name("_0_Lucene41_0.tim"), "_Lucene41_0.tim");
    assert_eq!(strip_segment_name(".fdt"), ".fdt");
    assert_eq!(strip_segment_name("segments_1"), "segments_1");
}

// ---------------------------------------------------------------------------
// Hostile
// ---------------------------------------------------------------------------

#[test]
fn a_wrong_magic_says_the_file_is_not_a_lucene_file() {
    let mut writer = Writer::default();
    writer.int(0x0bad_f00d).string("segments").int(1);
    let error = read_codec_header(&mut reader("segments_1", writer.finish()), "segments", 0, 1)
        .expect_err("a wrong magic must be refused");
    assert!(
        matches!(error, LuceneReadError::NotALuceneFile { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("segments_1"), "{error}");
}

#[test]
fn a_wrong_codec_name_is_distinct_from_a_wrong_magic() {
    // The operator's next move differs: this is a Lucene file, but the
    // wrong kind of one.
    let mut writer = Writer::default();
    writer.header("CompoundFileWriterEntries", 0);
    let error = read_codec_header(
        &mut reader("_0.si", writer.finish()),
        "Lucene46SegmentInfo",
        0,
        0,
    )
    .expect_err("a wrong codec name must be refused");
    assert!(
        matches!(error, LuceneReadError::WrongCodecName { .. }),
        "{error}"
    );
}

#[test]
fn a_version_outside_the_range_is_its_own_refusal() {
    let mut writer = Writer::default();
    writer.header("Lucene46SegmentInfo", 7);
    let error = read_codec_header(
        &mut reader("_0.si", writer.finish()),
        "Lucene46SegmentInfo",
        0,
        0,
    )
    .expect_err("an unsupported version must be refused");
    assert!(
        matches!(error, LuceneReadError::UnsupportedFormatVersion { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("0..=0"), "{error}");
}

#[test]
fn a_truncated_header_names_the_file_and_what_was_needed() {
    let error = read_codec_header(&mut reader("_0.si", vec![0x3f, 0xd7]), "anything", 0, 0)
        .expect_err("a truncated header must be refused");
    assert!(
        matches!(error, LuceneReadError::Truncated { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("_0.si"), "{error}");
}

#[test]
fn a_string_longer_than_the_file_is_refused_before_it_is_allocated() {
    // The length prefix is attacker-controlled and in bytes. A reader that
    // allocated it first would turn a 20-byte file into a 2 GiB
    // allocation; this must be an error, and a fast one.
    let mut writer = Writer::default();
    writer
        .header("Lucene46SegmentInfo", 0)
        .variable_int(0x7fff_ffff);
    let error = read_segment_info(&mut reader("_0.si", writer.finish()))
        .expect_err("an implausible string length must be refused");
    assert!(
        matches!(error, LuceneReadError::ImplausibleLength { .. }),
        "{error}"
    );
}

#[test]
fn a_string_set_count_beyond_the_file_is_refused_before_it_is_reserved() {
    let mut writer = Writer::default();
    writer
        .header("Lucene46SegmentInfo", 0)
        .string("4.7.2")
        .int(1)
        .byte(1)
        .string_map(&[])
        // A count of two billion, in a file with a handful of bytes left.
        .int(2_000_000_000);
    let error = read_segment_info(&mut reader("_0.si", writer.finish()))
        .expect_err("an implausible set count must be refused");
    assert!(
        matches!(error, LuceneReadError::ImplausibleLength { .. }),
        "{error}"
    );
}

#[test]
fn a_negative_document_count_is_refused() {
    let mut writer = Writer::default();
    writer
        .header("Lucene46SegmentInfo", 0)
        .string("4.7.2")
        .int(-1)
        .byte(1)
        .string_map(&[])
        .string_set(&[]);
    let error = read_segment_info(&mut reader("_0.si", writer.finish()))
        .expect_err("a negative document count must be refused");
    assert!(error.to_string().contains("negative"), "{error}");
}

#[test]
fn trailing_bytes_in_a_segment_descriptor_are_refused() {
    // `Lucene46SegmentInfoReader.read` requires the file to be consumed
    // exactly, so a `.si` with anything after the file set is one Lucene
    // itself will not open.
    let mut bytes = segment_info_bytes(1, &[]);
    bytes.extend_from_slice(b"trailing");
    let error =
        read_segment_info(&mut reader("_0.si", bytes)).expect_err("trailing bytes must be refused");
    assert!(error.to_string().contains("trailing bytes"), "{error}");
}

#[test]
fn a_deletion_count_above_the_document_count_is_refused() {
    let mut writer = Writer::default();
    writer
        .header("segments", 1)
        .long(1)
        .int(1)
        .int(1)
        .string("_0")
        .string("oakCodec")
        .long(-1)
        // Five deletions in a segment of three documents.
        .int(5)
        .long(-1)
        .int(0)
        .string_map(&[])
        .long(0);
    let error = read_commit_file(
        &mut reader("segments_1", writer.finish()),
        "segments_1",
        |name| Ok(reader(name, segment_info_bytes(3, &[]))),
    )
    .expect_err("a deletion count above the document count must be refused");
    assert!(error.to_string().contains("outside 0..=3"), "{error}");
}

#[test]
fn a_negative_segment_count_is_refused() {
    let mut writer = Writer::default();
    writer.header("segments", 1).long(1).int(1).int(-3);
    let error = read_commit_file(
        &mut reader("segments_1", writer.finish()),
        "segments_1",
        |name| Ok(reader(name, segment_info_bytes(1, &[]))),
    )
    .expect_err("a negative segment count must be refused");
    assert!(error.to_string().contains("negative"), "{error}");
}

#[test]
fn a_three_x_commit_file_is_named_as_such_rather_than_misparsed() {
    let mut writer = Writer::default();
    // Lucene 3.x commit files open with a format number, not the magic.
    writer.int(-11).long(1);
    let error = read_commit_file(
        &mut reader("segments_1", writer.finish()),
        "segments_1",
        |name| Ok(reader(name, segment_info_bytes(1, &[]))),
    )
    .expect_err("a 3.x commit file must be refused");
    assert!(error.to_string().contains("Lucene 3.x"), "{error}");
}

#[test]
fn a_compound_entry_past_the_end_of_the_data_file_is_refused() {
    let mut writer = Writer::default();
    writer
        .header("CompoundFileWriterEntries", 0)
        .variable_int(1)
        .string(".fdt")
        .long(90)
        .long(100);
    // The entry spans 90..190 of a file 150 bytes long.
    let error = read_table_of_contents(&mut reader("_0.cfe", writer.finish()), 150)
        .expect_err("an entry past the end must be refused");
    assert!(error.to_string().contains("spans 90..190"), "{error}");
}

#[test]
fn a_compound_entry_carrying_its_segment_prefix_is_refused() {
    // Lucene's own writer strips the prefix. One that carries it would let
    // a crafted directory address the same bytes under two names.
    let mut writer = Writer::default();
    writer
        .header("CompoundFileWriterEntries", 0)
        .variable_int(1)
        .string("_0.fdt")
        .long(0)
        .long(10);
    let error = read_table_of_contents(&mut reader("_0.cfe", writer.finish()), 100)
        .expect_err("a prefixed entry name must be refused");
    assert!(
        error.to_string().contains("carries a segment prefix"),
        "{error}"
    );
}

#[test]
fn a_duplicate_compound_entry_is_refused() {
    let mut writer = Writer::default();
    writer
        .header("CompoundFileWriterEntries", 0)
        .variable_int(2)
        .string(".fdt")
        .long(0)
        .long(10)
        .string(".fdt")
        .long(10)
        .long(10);
    let error = read_table_of_contents(&mut reader("_0.cfe", writer.finish()), 100)
        .expect_err("a duplicate entry must be refused");
    assert!(error.to_string().contains("duplicate entry"), "{error}");
}

#[test]
fn an_implausible_entry_count_is_refused_before_it_is_reserved() {
    let mut writer = Writer::default();
    writer
        .header("CompoundFileWriterEntries", 0)
        .variable_int(1_000_000_000);
    let error = read_table_of_contents(&mut reader("_0.cfe", writer.finish()), 100)
        .expect_err("an implausible entry count must be refused");
    assert!(
        matches!(error, LuceneReadError::ImplausibleLength { .. }),
        "{error}"
    );
}
